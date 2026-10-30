//! The engine's view of GitHub: open, read and merge PRs, and read the repo
//! settings that decide how merging behaves. [`Gh`] shells out to `gh`.

use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::eyre::{Result, eyre};
use serde_json::Value;

use crate::state::PrRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

#[derive(Debug, Clone)]
pub struct RepoSettings {
    /// Allowed methods, in the order Choros prefers them.
    pub methods: Vec<MergeMethod>,
    pub require_up_to_date: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

/// Where an open PR's review stands.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrReview {
    pub draft: bool,
    /// Someone besides the PR's author commented or reviewed after the
    /// author last did, or last wrote a commit. An approval with nothing
    /// written in it needs no answer, so it doesn't count.
    pub unanswered: bool,
    pub approved: bool,
}

/// An open GitHub stack: its PRs bottom to top, merged and closed ones
/// included, each with the branch it is based on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack {
    pub number: u64,
    pub prs: Vec<(u64, String)>,
}

pub struct PrRequest<'a> {
    pub head: &'a str,
    pub base: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub draft: bool,
}

/// `repo` is a local clone whose `origin` names the GitHub repository.
pub trait Forge {
    fn settings(&self, repo: &Path, branch: &str) -> Result<RepoSettings>;
    fn create_pr(&self, repo: &Path, req: &PrRequest) -> Result<PrRef>;
    fn find_open_pr(&self, repo: &Path, head: &str) -> Result<Option<PrRef>>;
    fn pr_state(&self, repo: &Path, number: u64) -> Result<PrState>;
    fn pr_review(&self, repo: &Path, number: u64) -> Result<PrReview>;
    fn merge_pr(&self, repo: &Path, number: u64, method: MergeMethod) -> Result<()>;
    /// The repo's PR template, the org's default included.
    fn pr_template(&self, repo: &Path) -> Result<Option<String>>;
    fn close_pr(&self, repo: &Path, number: u64, comment: &str) -> Result<()>;
    /// GitHub refuses this for a PR in a stack.
    fn set_pr_base(&self, repo: &Path, number: u64, base: &str) -> Result<()>;
    /// Err when stacks can't be made from here (`gh stack` missing).
    fn stacks_ready(&self, repo: &Path) -> Result<()>;
    fn stacks(&self, repo: &Path) -> Result<Vec<Stack>>;
    /// Put `prs`, bottom to top, on top of `stack`, or into a new stack
    /// when `None`. GitHub refuses a link that would leave out a member of
    /// a stack already holding one of them. Returns the stack's number.
    fn link_stack(&self, repo: &Path, stack: Option<u64>, prs: &[u64]) -> Result<u64>;
    /// Dissolve the stack; its PRs stay open.
    fn unstack(&self, repo: &Path, stack: u64) -> Result<()>;
}

pub struct Gh {
    /// `<trunk>/.choros-config/gh` when the trunk has its own account.
    pub config_dir: Option<PathBuf>,
    /// Tests swap in a stand-in that answers as GitHub did.
    bin: PathBuf,
}

impl Gh {
    pub fn new(config_dir: Option<PathBuf>) -> Gh {
        Gh {
            config_dir,
            bin: "gh".into(),
        }
    }

    fn run(&self, repo: &Path, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new(&self.bin);
        cmd.current_dir(repo).args(args);
        if let Some(dir) = &self.config_dir {
            cmd.env("GH_CONFIG_DIR", dir);
        }
        let out = cmd.output()?;
        if !out.status.success() {
            return Err(eyre!(
                "gh {} failed:\n{}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// `gh stack` is an extension, and it can't read a token kept in the
    /// keychain under `GH_CONFIG_DIR`; hand it one.
    fn run_stack(&self, repo: &Path, args: &[&str]) -> Result<String> {
        let token = self.run(repo, &["auth", "token"])?;
        let mut cmd = Command::new(&self.bin);
        cmd.current_dir(repo).arg("stack").args(args).env("GH_TOKEN", token);
        if let Some(dir) = &self.config_dir {
            cmd.env("GH_CONFIG_DIR", dir);
        }
        let out = cmd.output()?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        if !out.status.success() {
            return Err(eyre!("gh stack {} failed:\n{}", args.join(" "), text.trim()));
        }
        Ok(text)
    }
}

impl Forge for Gh {
    fn pr_template(&self, repo: &Path) -> Result<Option<String>> {
        let v: Value = serde_json::from_str(&self.run(repo, &["repo", "view", "--json", "pullRequestTemplates"])?)?;
        Ok(v["pullRequestTemplates"]
            .as_array()
            .and_then(|t| t.first())
            .and_then(|t| t["body"].as_str())
            .map(str::to_string))
    }

    fn settings(&self, repo: &Path, branch: &str) -> Result<RepoSettings> {
        let v: Value = serde_json::from_str(&self.run(repo, &["api", "repos/{owner}/{repo}"])?)?;
        let allowed = |k: &str| v[k].as_bool().unwrap_or(false);
        let mut methods = Vec::new();
        for (key, method) in [
            ("allow_merge_commit", MergeMethod::Merge),
            ("allow_squash_merge", MergeMethod::Squash),
            ("allow_rebase_merge", MergeMethod::Rebase),
        ] {
            if allowed(key) {
                methods.push(method);
            }
        }
        // No protection (a 404) means no up-to-date requirement.
        let protection = self
            .run(repo, &["api", &format!("repos/{{owner}}/{{repo}}/branches/{branch}/protection")])
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok());
        let require_up_to_date = protection
            .is_some_and(|p| p["required_status_checks"]["strict"].as_bool().unwrap_or(false));
        Ok(RepoSettings {
            methods,
            require_up_to_date,
        })
    }

    fn create_pr(&self, repo: &Path, req: &PrRequest) -> Result<PrRef> {
        let mut args = vec![
            "pr", "create", "--head", req.head, "--base", req.base, "--title", req.title, "--body",
            req.body,
        ];
        if req.draft {
            args.push("--draft");
        }
        let url = self.run(repo, &args)?;
        let url = url.lines().last().unwrap_or_default().to_string();
        let number = url
            .rsplit('/')
            .next()
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| eyre!("could not read a PR number from `{url}`"))?;
        Ok(PrRef { number, url })
    }

    fn find_open_pr(&self, repo: &Path, head: &str) -> Result<Option<PrRef>> {
        let v: Value = serde_json::from_str(&self.run(
            repo,
            &["pr", "list", "--head", head, "--state", "open", "--json", "number,url"],
        )?)?;
        Ok(v.as_array().and_then(|a| a.first()).and_then(|pr| {
            Some(PrRef {
                number: pr["number"].as_u64()?,
                url: pr["url"].as_str()?.to_string(),
            })
        }))
    }

    fn pr_state(&self, repo: &Path, number: u64) -> Result<PrState> {
        let v: Value = serde_json::from_str(&self.run(
            repo,
            &["pr", "view", &number.to_string(), "--json", "state"],
        )?)?;
        Ok(match v["state"].as_str() {
            Some("MERGED") => PrState::Merged,
            Some("CLOSED") => PrState::Closed,
            _ => PrState::Open,
        })
    }

    fn pr_review(&self, repo: &Path, number: u64) -> Result<PrReview> {
        let v: Value = serde_json::from_str(&self.run(
            repo,
            &["pr", "view", &number.to_string(), "--json", "isDraft,author,comments,reviews,commits,reviewDecision"],
        )?)?;
        Ok(PrReview {
            draft: v["isDraft"].as_bool().unwrap_or(false),
            unanswered: unanswered(&v),
            approved: approved(&v),
        })
    }

    fn close_pr(&self, repo: &Path, number: u64, comment: &str) -> Result<()> {
        self.run(repo, &["pr", "close", &number.to_string(), "--comment", comment])?;
        Ok(())
    }

    fn set_pr_base(&self, repo: &Path, number: u64, base: &str) -> Result<()> {
        self.run(repo, &["pr", "edit", &number.to_string(), "--base", base])?;
        Ok(())
    }

    fn stacks_ready(&self, repo: &Path) -> Result<()> {
        self.run(repo, &["stack", "--help"])
            .map(|_| ())
            .map_err(|_| eyre!("`gh stack` is missing; install it with `gh extension install github/gh-stack`"))
    }

    fn stacks(&self, repo: &Path) -> Result<Vec<Stack>> {
        // The listing leaves out each PR's base; only one stack's own page has it.
        let v: Value = serde_json::from_str(&self.run(repo, &["api", "--paginate", "--slurp", "repos/{owner}/{repo}/stacks"])?)?;
        let open: Vec<u64> = v
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|page| page.as_array().into_iter().flatten())
            .filter(|s| s["open"].as_bool() == Some(true))
            .filter_map(|s| s["number"].as_u64())
            .collect();
        let mut out = Vec::new();
        for n in open {
            let v: Value = serde_json::from_str(&self.run(repo, &["api", &format!("repos/{{owner}}/{{repo}}/stacks/{n}")])?)?;
            out.extend(parse_stack(&v));
        }
        Ok(out)
    }

    fn link_stack(&self, repo: &Path, stack: Option<u64>, prs: &[u64]) -> Result<u64> {
        let mut args: Vec<String> = vec!["link".into()];
        args.extend(stack.map(|s| s.to_string()));
        args.extend(prs.iter().map(u64::to_string));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.run_stack(repo, &args)?;
        if let Some(n) = stack {
            return Ok(n);
        }
        // "✓ Created stack with 2 PRs (stack #16 (https://…))"
        out.split("stack #")
            .nth(1)
            .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| eyre!("could not read a stack number from `gh stack link`:\n{}", out.trim()))
    }

    fn unstack(&self, repo: &Path, stack: u64) -> Result<()> {
        self.run_stack(repo, &["unstack", &stack.to_string()])?;
        Ok(())
    }

    fn merge_pr(&self, repo: &Path, number: u64, method: MergeMethod) -> Result<()> {
        let flag = match method {
            MergeMethod::Merge => "--merge",
            MergeMethod::Squash => "--squash",
            MergeMethod::Rebase => "--rebase",
        };
        self.run(repo, &["pr", "merge", &number.to_string(), flag])?;
        Ok(())
    }
}

fn parse_stack(v: &Value) -> Option<Stack> {
    Some(Stack {
        number: v["number"].as_u64()?,
        prs: v["pull_requests"]
            .as_array()?
            .iter()
            .filter_map(|p| Some((p["number"].as_u64()?, p["base"]["ref"].as_str()?.to_string())))
            .collect(),
    })
}

/// Commits count by when they were written, not committed: a rebase, like a
/// re-sync, rewrites committer dates but answers no one. GitHub's timestamps
/// are all UTC in one format, so they compare as strings.
fn unanswered(pr: &Value) -> bool {
    let author = pr["author"]["login"].as_str();
    let items = |k: &str| pr[k].as_array().cloned().unwrap_or_default();
    let mut theirs: Option<String> = None;
    let mut mine: Option<String> = None;
    let mut saw = |by_author: bool, at: Option<&str>| {
        let Some(at) = at else { return };
        let last = if by_author { &mut mine } else { &mut theirs };
        if last.as_deref().is_none_or(|l| l < at) {
            *last = Some(at.to_string());
        }
    };
    let by_author = |v: &Value| author.is_some() && v["author"]["login"].as_str() == author;
    for c in items("comments") {
        if c["isMinimized"].as_bool() != Some(true) {
            saw(by_author(&c), c["createdAt"].as_str());
        }
    }
    for r in items("reviews") {
        let bare_approval = r["state"] == "APPROVED" && r["body"].as_str().unwrap_or("").trim().is_empty();
        if !bare_approval {
            saw(by_author(&r), r["submittedAt"].as_str());
        }
    }
    for c in items("commits") {
        saw(true, c["authoredDate"].as_str());
    }
    match (theirs, mine) {
        (Some(t), Some(m)) => t > m,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// GitHub decides only where the repo requires reviews; elsewhere a PR is
/// approved when some reviewer's latest verdict approves and none asks for
/// changes.
fn approved(pr: &Value) -> bool {
    match pr["reviewDecision"].as_str().unwrap_or("") {
        "" => {}
        decision => return decision == "APPROVED",
    }
    let mut verdicts: std::collections::BTreeMap<&str, &str> = Default::default();
    for r in pr["reviews"].as_array().into_iter().flatten() {
        let (Some(who), Some(state)) = (r["author"]["login"].as_str(), r["state"].as_str()) else {
            continue;
        };
        if matches!(state, "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED") {
            verdicts.insert(who, state);
        }
    }
    verdicts.values().any(|&v| v == "APPROVED") && !verdicts.values().any(|&v| v == "CHANGES_REQUESTED")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A review comes in, the author pushes a fix, the reviewer replies, the
    /// author answers in a comment. Each turn flips whose move it is. A
    /// re-sync's rebase keeps the fix's written date, so it answers no one.
    #[test]
    fn the_last_word_decides_whether_a_pr_is_unanswered() {
        let pr = |comments: Vec<Value>, reviews: Vec<Value>, commits: Vec<&str>| {
            let commits: Vec<Value> = commits
                .into_iter()
                .map(|at| json!({"authoredDate": at, "committedDate": "2026-10-09T00:00:00Z"}))
                .collect();
            json!({"author": {"login": "me"}, "comments": comments, "reviews": reviews, "commits": commits})
        };
        let by = |who: &str, at: &str| json!({"author": {"login": who}, "createdAt": at});
        let review = |who: &str, at: &str| json!({"author": {"login": who}, "submittedAt": at});

        assert!(!unanswered(&pr(vec![], vec![], vec!["2026-10-01T00:00:00Z"])), "no one has spoken");

        let reviewed = pr(vec![], vec![review("ann", "2026-10-02T00:00:00Z")], vec!["2026-10-01T00:00:00Z"]);
        assert!(unanswered(&reviewed));

        let fixed = pr(
            vec![],
            vec![review("ann", "2026-10-02T00:00:00Z")],
            vec!["2026-10-01T00:00:00Z", "2026-10-03T00:00:00Z"],
        );
        assert!(!unanswered(&fixed));

        let replied = pr(
            vec![by("ann", "2026-10-04T00:00:00Z")],
            vec![review("ann", "2026-10-02T00:00:00Z")],
            vec!["2026-10-01T00:00:00Z", "2026-10-03T00:00:00Z"],
        );
        assert!(unanswered(&replied));

        let answered = pr(
            vec![by("ann", "2026-10-04T00:00:00Z"), by("me", "2026-10-05T00:00:00Z")],
            vec![review("ann", "2026-10-02T00:00:00Z")],
            vec!["2026-10-01T00:00:00Z", "2026-10-03T00:00:00Z"],
        );
        assert!(!unanswered(&answered));

        let approved = pr(
            vec![],
            vec![json!({"author": {"login": "ann"}, "submittedAt": "2026-10-04T00:00:00Z", "state": "APPROVED", "body": ""})],
            vec!["2026-10-01T00:00:00Z"],
        );
        assert!(!unanswered(&approved), "a bare approval needs no answer");
    }

    /// `gh` as GitHub answered it when tried (trimmed): the stack listing
    /// leaves out each PR's base, which only a stack's own page gives. A
    /// closed PR in the middle keeps its place, and the PR above it stays
    /// on its branch.
    #[test]
    fn gh_reads_each_prs_base_from_its_stacks_page() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("gh");
        let pr = |n: u64, state: &str, head: &str| {
            json!({"number": n, "state": state, "draft": false, "merged_at": null,
                   "head": {"ref": head, "sha": "48ded4b775619006d72e4b9e005c18788dc0df11"}})
        };
        let listing = json!([[
            {"number": 15, "open": true, "base": {"ref": "main"},
             "pull_requests": [pr(12, "open", "q/landing-1"), pr(13, "closed", "q/landing-2"), pr(14, "open", "q/landing-3")]},
            {"number": 11, "open": false, "base": {"ref": "main"}, "pull_requests": []},
        ]]);
        let with_base = |n: u64, state: &str, head: &str, base: &str| {
            let mut p = pr(n, state, head);
            p["base"] = json!({"ref": base, "sha": "d55dd64e52154092b0ea0f40f8b5d33782a41f60"});
            p
        };
        let page = json!({"number": 15, "open": true, "base": {"ref": "main"}, "pull_requests": [
            with_base(12, "open", "q/landing-1", "main"),
            with_base(13, "closed", "q/landing-2", "q/landing-1"),
            with_base(14, "open", "q/landing-3", "q/landing-2"),
        ]});
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n\
                 'auth token') echo tok ;;\n\
                 'api --paginate --slurp repos/{{owner}}/{{repo}}/stacks') echo '{listing}' ;;\n\
                 'api repos/{{owner}}/{{repo}}/stacks/15') echo '{page}' ;;\n\
                 'stack link 12 14') [ \"$GH_TOKEN\" = tok ] || exit 4\n\
                   echo '\u{2713} Created stack with 2 PRs (stack #16 (https://github.com/o/r/stack/16))' ;;\n\
                 *) echo \"unexpected: $*\" >&2; exit 1 ;;\nesac\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let gh = Gh { config_dir: None, bin };

        assert_eq!(
            gh.stacks(dir.path()).unwrap(),
            [Stack {
                number: 15,
                prs: vec![(12, "main".into()), (13, "q/landing-1".into()), (14, "q/landing-2".into())],
            }]
        );
        assert_eq!(gh.link_stack(dir.path(), None, &[12, 14]).unwrap(), 16);
    }

    /// Without required reviews GitHub gives no decision, so each
    /// reviewer's latest verdict stands: one asking for changes holds back
    /// another's approval until they approve too.
    #[test]
    fn a_pr_is_approved_when_no_reviewer_still_wants_changes() {
        let verdict = |who: &str, state: &str| json!({"author": {"login": who}, "state": state});
        let pr = |reviews: Vec<Value>| json!({"reviewDecision": "", "reviews": reviews});

        assert!(!approved(&pr(vec![])));
        assert!(approved(&pr(vec![verdict("ann", "APPROVED")])));
        assert!(!approved(&pr(vec![verdict("ann", "APPROVED"), verdict("bo", "CHANGES_REQUESTED")])));
        assert!(approved(&pr(vec![
            verdict("bo", "CHANGES_REQUESTED"),
            verdict("bo", "COMMENTED"),
            verdict("bo", "APPROVED"),
        ])));
        assert!(!approved(&json!({"reviewDecision": "REVIEW_REQUIRED", "reviews": [verdict("ann", "APPROVED")]})));
    }
}
