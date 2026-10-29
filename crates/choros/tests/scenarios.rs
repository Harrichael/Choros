//! Scenario tests. Each file in `tests/scenarios/` is a transcript of shell
//! commands and the exact output they must produce, run against the real
//! `choros` binary: what the person at the terminal sees.
//!
//! ```text
//! # Comments start with '#'.
//! $ choros work ws app
//! ...
//! $ cd trunk/ws/app
//! $ echo a > a.txt && git add a.txt && git commit -qm "write a"
//! $ choros land continue
//! app → appq/landing-1, at the front: https://github.test/pull/1
//! ...
//! ```
//!
//! - `$ <command>` runs it with `sh -c`. `$ cd <dir>` changes directory,
//!   relative to the scenario's root.
//! - The expected output is every line up to the next `$ ` line, minus
//!   trailing blank and comment lines. It's stdout, then stderr, then
//!   `[exit N]` if the command failed. A line of just `...` matches any
//!   number of lines. The scenario root is shown as `$ROOT`.
//!
//! Every scenario starts in `$ROOT/trunk`, a trunk (`choros init`) whose
//! registry holds `app`, cloned from `$ROOT/app.git` with one commit on
//! main, and a queue `appq` for it, whose lands are checked with `true`
//! (in `.choros-config/repos.toml`). Git's config and clock are fixed, so
//! commit hashes are the same on every run. `gh` is a fake that keeps PRs in
//! files and really merges them into the origin (`gh pr merge N --squash`,
//! `gh pr close N`); `claude` does nothing.
//!
//! Run with `CHOROS_BLESS=1` to rewrite failing expectations with the actual
//! output, and `CHOROS_SCENARIO=<name>` to run only matching files.

use std::path::{Path, PathBuf};
use std::process::Command;

const FAKE_GH: &str = r#"#!/bin/sh
set -e
db="$FAKE_GH"; mkdir -p "$db"
case "$1 $2" in
  "api repos/{owner}/{repo}") echo '{"allow_merge_commit":true,"allow_squash_merge":true}' ;;
  api*) echo 'Not Found' >&2; exit 1 ;;
  "repo view") echo '{"pullRequestTemplates":[]}' ;;
  "pr create")
    shift 2
    while [ $# -gt 0 ]; do case "$1" in --head) head="$2"; shift 2 ;; --draft) shift ;; *) shift 2 ;; esac; done
    git ls-remote --exit-code --heads origin "$head" >/dev/null || { echo "no branch $head on GitHub" >&2; exit 1; }
    n=$(( $(ls "$db" | grep -c '\.head$' || true) + 1 ))
    echo "$head" > "$db/$n.head"; git remote get-url origin > "$db/$n.origin"; echo OPEN > "$db/$n.state"
    echo "https://github.test/pull/$n" ;;
  "pr list")
    shift 2; while [ $# -gt 0 ]; do case "$1" in --head) head="$2"; shift 2 ;; *) shift ;; esac; done
    out='[]'
    for f in "$db"/*.head; do
      [ -e "$f" ] || continue; n=$(basename "$f" .head)
      if [ "$(cat "$f")" = "$head" ] && [ "$(cat "$db/$n.state")" = OPEN ]; then
        out="[{\"number\":$n,\"url\":\"https://github.test/pull/$n\"}]"
      fi
    done
    echo "$out" ;;
  "pr view")
    n=$3; s=$(cat "$db/$n.state")
    # As GitHub does: an open PR's head is kept at refs/pull/N/head.
    if [ "$s" = OPEN ]; then git -C "$(cat "$db/$n.origin")" update-ref "refs/pull/$n/head" "refs/heads/$(cat "$db/$n.head")"; fi
    echo "{\"state\":\"$s\"}" ;;
  "pr merge")
    n=$3; tmp=$(mktemp -d); h=$(cat "$db/$n.head")
    git clone -q "$(cat "$db/$n.origin")" "$tmp/w"
    if [ "$4" = --squash ]; then
      git -C "$tmp/w" merge -q --squash "origin/$h" >/dev/null; git -C "$tmp/w" commit -qm "Squashed #$n"
    else
      git -C "$tmp/w" merge -q --no-ff -m "Merge #$n" "origin/$h"
    fi
    git -C "$tmp/w" push -q origin main; rm -rf "$tmp"; echo MERGED > "$db/$n.state" ;;
  "pr close") echo CLOSED > "$db/$3.state" ;;
  *) echo "fake gh: unsupported: $*" >&2; exit 1 ;;
esac
"#;

const GITCONFIG: &str = "[init]\n\tdefaultBranch = main\n[user]\n\tname = Test\n\temail = test@example.test\n\
                         [advice]\n\tdetachedHead = false\n";
const DATE: &str = "2025-10-09T08:53:20Z";

struct Step {
    /// Lines before the command: comments and blank lines.
    preamble: Vec<String>,
    command: String,
    expected: Vec<String>,
}

fn parse(text: &str) -> (Vec<String>, Vec<Step>) {
    let mut header = Vec::new();
    let mut steps: Vec<Step> = Vec::new();
    let mut pending = Vec::new();
    for line in text.lines() {
        if let Some(cmd) = line.strip_prefix("$ ") {
            if let Some(last) = steps.last_mut() {
                // Trailing blank and comment lines belong to the next step.
                while last
                    .expected
                    .last()
                    .is_some_and(|l| l.trim().is_empty() || l.starts_with('#'))
                {
                    pending.insert(0, last.expected.pop().unwrap());
                }
            } else {
                header.append(&mut pending);
            }
            steps.push(Step {
                preamble: std::mem::take(&mut pending),
                command: cmd.to_string(),
                expected: Vec::new(),
            });
        } else if let Some(last) = steps.last_mut() {
            last.expected.push(line.to_string());
        } else {
            pending.push(line.to_string());
        }
    }
    header.append(&mut pending);
    if let Some(last) = steps.last_mut() {
        while last.expected.last().is_some_and(|l| l.trim().is_empty()) {
            last.expected.pop();
        }
    }
    (header, steps)
}

/// Whether `actual` matches `expected`, where a `...` line matches any run
/// of lines.
fn matches(expected: &[String], actual: &[String]) -> bool {
    match expected.split_first() {
        None => actual.is_empty(),
        Some((e, rest)) if e == "..." => (0..=actual.len()).any(|i| matches(rest, &actual[i..])),
        Some((e, rest)) => actual
            .split_first()
            .is_some_and(|(a, arest)| a == e && matches(rest, arest)),
    }
}

struct Sandbox {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    env: Vec<(String, String)>,
    cwd: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        // choros prints paths with symlinks resolved (on macOS the temp dir
        // is under /var, a link to /private/var).
        let root = tmp.path().canonicalize().unwrap().join("root");
        let bin = tmp.path().join("bin");
        for dir in [&root, &bin, &tmp.path().join("home")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        for (name, script) in [("gh", FAKE_GH), ("claude", "#!/bin/sh\nexit 0\n")] {
            std::fs::write(bin.join(name), script).unwrap();
            std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(tmp.path().join("gitconfig"), GITCONFIG).unwrap();
        let choros_dir = Path::new(env!("CARGO_BIN_EXE_choros")).parent().unwrap();
        let path = format!(
            "{}:{}:{}",
            bin.display(),
            choros_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let t = tmp.path();
        let env = [
            ("PATH", path),
            ("HOME", t.join("home").display().to_string()),
            ("GIT_CONFIG_GLOBAL", t.join("gitconfig").display().to_string()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("GIT_AUTHOR_DATE", DATE.into()),
            ("GIT_COMMITTER_DATE", DATE.into()),
            ("FAKE_GH", t.join("gh").display().to_string()),
            ("TZ", "UTC".into()),
            ("NO_COLOR", "1".into()),
            ("LANG", "C.UTF-8".into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let mut sandbox = Sandbox {
            cwd: root.clone(),
            root,
            env,
            _tmp: tmp,
        };
        sandbox.setup();
        sandbox
    }

    fn setup(&mut self) {
        for cmd in [
            "git init -q --bare -b main app.git",
            "git clone -q app.git seed 2>/dev/null",
            "cd seed && echo app > README && git add README && git commit -qm init && git push -q origin main",
            "rm -rf seed && mkdir trunk",
            "cd trunk && choros init >/dev/null 2>&1 && git clone -q ../app.git .choros-config/registry/app",
            "cd trunk && choros queue new appq >/dev/null 2>&1",
            r#"cd trunk && printf '[app]\ncheck = "true"\n' > .choros-config/repos.toml"#,
        ] {
            let out = self.shell(cmd);
            assert!(out.is_empty(), "setup `{cmd}`: {out:?}");
        }
        self.cwd = self.root.join("trunk");
    }

    fn run(&mut self, command: &str) -> Vec<String> {
        if let Some(dir) = command.strip_prefix("cd ") {
            self.cwd = self.root.join(dir.trim());
            assert!(self.cwd.is_dir(), "no directory {}", self.cwd.display());
            return Vec::new();
        }
        self.shell(command)
    }

    fn shell(&self, command: &str) -> Vec<String> {
        let out = Command::new("sh")
            .args(["-c", command])
            .current_dir(&self.cwd)
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .output()
            .unwrap();
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        if !out.status.success() {
            text.push_str(&format!("[exit {}]\n", out.status.code().unwrap_or(-1)));
        }
        let text = text.replace(self.root.to_str().unwrap(), "$ROOT");
        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        lines
    }
}

/// Runs one scenario, returning a failure report if any step mismatched.
fn run_scenario(path: &Path, bless: bool) -> Option<String> {
    let text = std::fs::read_to_string(path).unwrap();
    let (header, mut steps) = parse(&text);
    let mut sandbox = Sandbox::new();
    let mut failure = None;
    for step in &mut steps {
        let actual = sandbox.run(&step.command);
        if matches(&step.expected, &actual) {
            continue;
        }
        if failure.is_none() {
            failure = Some(format!(
                "{}\n$ {}\n--- expected\n{}\n--- actual\n{}\n",
                path.display(),
                step.command,
                step.expected.join("\n"),
                actual.join("\n")
            ));
        }
        if !bless {
            break;
        }
        step.expected = actual;
    }
    if bless && failure.is_some() {
        let mut out = header.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        for step in &steps {
            for line in step.preamble.iter().chain([&format!("$ {}", step.command)]) {
                out.push_str(line);
                out.push('\n');
            }
            for line in &step.expected {
                out.push_str(line);
                out.push('\n');
            }
        }
        std::fs::write(path, out).unwrap();
    }
    failure
}

#[test]
fn scenarios() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios");
    let filter = std::env::var("CHOROS_SCENARIO").ok();
    let bless = std::env::var_os("CHOROS_BLESS").is_some();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .filter(|p| filter.as_ref().is_none_or(|f| p.to_string_lossy().contains(f)))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no scenarios found in {}", dir.display());

    let failures: Vec<String> = paths.iter().filter_map(|p| run_scenario(p, bless)).collect();
    if !failures.is_empty() {
        let verb = if bless { "blessed" } else { "failed" };
        panic!("{} scenario(s) {verb}:\n\n{}", failures.len(), failures.join("\n"));
    }
}

#[test]
fn runner_parsing() {
    let lines = |s: &str| s.lines().map(String::from).collect::<Vec<_>>();
    assert!(matches(&lines("a\n...\nd"), &lines("a\nb\nc\nd")));
    assert!(matches(&lines("a\n..."), &lines("a")));
    assert!(!matches(&lines("a\n...\nd"), &lines("a\nb")));

    let (header, steps) = parse("# intro\n\n$ choros x\nout\n\n# next\n$ choros y\n");
    assert_eq!(header, ["# intro", ""]);
    assert_eq!(steps[0].expected, ["out"]);
    assert_eq!(steps[1].preamble, ["", "# next"]);
}
