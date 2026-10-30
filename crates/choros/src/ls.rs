//! `choros ls`: the workspaces in a space and the branch-spaces below it, as a
//! tree, most recently worked-in first, with what each space has landing.

use std::cmp::Reverse;
use std::io::Write;
use std::path::Path;
use std::time::SystemTime;

use choros_core::space::{self, Tree};
use choros_land::queue::{self, Queue};
use color_eyre::eyre::{eyre, Result};
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

use crate::{choros, git};

struct Row {
    name: String,
    last_active: Option<SystemTime>,
    /// The raw meta string when it isn't RFC 3339, e.g. `scan`'s
    /// "(unreadable meta)" placeholder.
    created: Result<OffsetDateTime, String>,
    /// `None` when any repo couldn't be read by `git status`.
    changed: Option<usize>,
    repos: Vec<String>,
}

enum Line {
    Workspace(usize, Row),
    Branch(usize, String, usize),
    Landing(usize, String),
}

/// From the trunk, or with `all`: the whole tree. In a branch-space, or a
/// workspace in one: that space and below.
pub fn run(cwd: &Path, all: bool) -> Result<()> {
    let here = space::find_space(cwd).ok_or_else(|| eyre!("not in a choros trunk (no .choros-config above here)"))?;
    let space = if all { space::find_trunk(&here).unwrap_or(here) } else { here };
    // Must run before anything spawns a thread: on Unix, `time` refuses to
    // read the local offset from a multithreaded process.
    let offset = UtcOffset::current_local_offset().ok();
    let mut stdout = std::io::stdout().lock();
    run_inner(&space, offset, SystemTime::now(), &mut stdout)
}

fn run_inner(space: &Path, offset: Option<UtcOffset>, now: SystemTime, out: &mut dyn Write) -> Result<()> {
    let tree = space::tree(space)?;
    let landings = landings_by_space(space);
    let mut lines = Vec::new();
    push_tree(&tree, 0, &landings, &mut lines);

    if lines.is_empty() {
        writeln!(out, "(no workspaces)")?;
        return Ok(());
    }
    let tz = if offset.is_some() { "" } else { " (UTC)" };
    let last_active_header = format!("LAST ACTIVE{tz}");
    let created_header = format!("CREATED{tz}");
    let offset = offset.unwrap_or(UtcOffset::UTC);
    let name_w = lines
        .iter()
        .map(|l| match l {
            Line::Workspace(d, r) => 2 * d + r.name.chars().count(),
            Line::Branch(d, n, _) => 2 * d + n.chars().count() + 1,
            Line::Landing(..) => 0,
        })
        .max()
        .unwrap_or(0)
        .max(4);
    writeln!(
        out,
        "{:<name_w$}  {:<17}  {:<9}  {:<17}  {:<7}  REPOS",
        "NAME", last_active_header, "AGO", created_header, "CHANGED"
    )?;
    for line in &lines {
        match line {
            Line::Workspace(depth, r) => writeln!(
                out,
                "{:<name_w$}  {:<17}  {:<9}  {:<17}  {:<7}  {}",
                format!("{}{}", "  ".repeat(*depth), r.name),
                r.last_active
                    .map(|t| format_time(t.into(), offset))
                    .unwrap_or_else(|| "-".into()),
                r.last_active.map(|t| ago(t, now)).unwrap_or_else(|| "-".into()),
                match &r.created {
                    Ok(t) => format_time(*t, offset),
                    Err(raw) => raw.clone(),
                },
                match r.changed {
                    Some(0) => "clean".to_string(),
                    Some(n) => n.to_string(),
                    None => "?".to_string(),
                },
                r.repos.join(", "),
            )?,
            Line::Branch(depth, name, n) => writeln!(
                out,
                "{:<name_w$}  ({n} workspace{})",
                format!("{}{name}/", "  ".repeat(*depth)),
                if *n == 1 { "" } else { "s" }
            )?,
            Line::Landing(depth, text) => writeln!(out, "{}↳ {text}", "  ".repeat(*depth))?,
        }
    }
    Ok(())
}

/// A space's workspaces (most recent first), what it has landing, then its
/// branch-spaces (most recent activity first), each indented under it.
fn push_tree(tree: &Tree, depth: usize, landings: &[(std::path::PathBuf, String)], out: &mut Vec<Line>) {
    let mut rows: Vec<Row> = tree.workspaces.iter().map(row).collect();
    rows.sort_by_key(|r| Reverse(r.last_active));
    out.extend(rows.into_iter().map(|r| Line::Workspace(depth, r)));
    for (space, text) in landings {
        if *space == tree.path {
            out.push(Line::Landing(depth, text.clone()));
        }
    }
    let mut branches: Vec<&Tree> = tree.branches.iter().collect();
    branches.sort_by_key(|b| Reverse(b.all_workspaces().iter().filter_map(|w| choros::last_active(&w.path)).max()));
    for b in branches {
        let name = b.path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        out.push(Line::Branch(depth, name, b.all_workspaces().len()));
        push_tree(b, depth + 1, landings, out);
    }
}

fn row(info: &choros::ChorosInfo) -> Row {
    Row {
        last_active: choros::last_active(&info.path),
        created: OffsetDateTime::parse(&info.meta.created_at, &Rfc3339).map_err(|_| info.meta.created_at.clone()),
        changed: info
            .meta
            .repos
            .iter()
            .map(|r| git::changed_count(&info.path.join(r)).ok())
            .sum(),
        name: info.meta.name.clone(),
        repos: info.meta.repos.clone(),
    }
}

/// Each open landing-set under `space`, keyed by the space its workspace was
/// in when it landed: the set's files live at the trunk, but it belongs to the
/// work that made it.
fn landings_by_space(space: &Path) -> Vec<(std::path::PathBuf, String)> {
    let Some(trunk) = space::find_trunk(space) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name in queue::list(&trunk).unwrap_or_default() {
        let Ok(q) = Queue::open(&trunk, &name) else { continue };
        let Ok(st) = q.read_state() else { continue };
        for set in st.sets.values().filter(|s| s.is_open()) {
            let Some(from) = set.source.workspace.parent() else { continue };
            if !from.starts_with(space) {
                continue;
            }
            let ws = set.source.workspace.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            let state = if set.park.is_some() {
                "parked"
            } else if set.pr_base.is_some() {
                "stacked"
            } else if set.pr.is_some() {
                "front"
            } else {
                "queued"
            };
            let title = set.title.clone().unwrap_or_default();
            out.push((from.to_path_buf(), format!("{} {state} from {ws}  {title}", set.branch(&name)).trim_end().to_string()));
        }
    }
    out
}

fn format_time(t: OffsetDateTime, offset: UtcOffset) -> String {
    t.to_offset(offset)
        .format(format_description!("[year]-[month]-[day] [hour]:[minute]"))
        .unwrap_or_else(|_| "?".into())
}

fn ago(then: SystemTime, now: SystemTime) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    const MONTH: u64 = 30 * DAY;
    // A future mtime (clock skew, restored backups) reads as "just now".
    let secs = now.duration_since(then).map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        0..MIN => "just now".into(),
        MIN..HOUR => format!("{}m ago", secs / MIN),
        HOUR..DAY => format!("{}h ago", secs / HOUR),
        _ if secs < 14 * DAY => format!("{}d ago", secs / DAY),
        _ if secs < 2 * MONTH => format!("{}w ago", secs / WEEK),
        _ => format!("{}mo ago", secs / MONTH),
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::time::Duration;

    use super::*;
    use crate::choros::{write_meta, ChorosMeta};

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    fn git(dir: &Path, args: &[&str]) {
        // `git commit` otherwise spawns a detached `maintenance run --auto`
        // whose lock files appear and vanish under `backdate`'s feet.
        let ok = Command::new("git")
            .args(["-c", "maintenance.auto=false", "-C"])
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(ok.success(), "git {args:?} failed");
    }

    /// A workspace with one committed repo, every file's mtime set to `at`.
    fn workspace(trunk: &Path, name: &str, at: SystemTime) -> std::path::PathBuf {
        let ws = trunk.join(name);
        let repo = ws.join("app");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "--quiet", "-m", "init"]);
        write_meta(
            &ws,
            &ChorosMeta {
                name: name.into(),
                created_at: "2026-01-01T12:00:00Z".into(),
                repos: vec!["app".into()],
                branch: None,
            },
        )
        .unwrap();
        backdate(&ws, at);
        ws
    }

    fn backdate(dir: &Path, at: SystemTime) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                backdate(&path, at);
            } else {
                touch(&path, at);
            }
        }
    }

    fn touch(path: &Path, at: SystemTime) {
        // Read-only open: git objects are 0444, and setting times only
        // needs ownership.
        std::fs::File::open(path).unwrap().set_modified(at).unwrap();
    }

    fn ls(trunk: &Path, now: SystemTime) -> String {
        let mut buf = Vec::new();
        run_inner(trunk, Some(UtcOffset::UTC), now, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    /// Workspaces are listed most recently worked-in first, with how stale
    /// each one is and how much uncommitted work it holds; archived ones are
    /// left out.
    fn trunk() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(choros::CONFIG_DIR)).unwrap();
        tmp
    }

    #[test]
    fn lists_active_workspaces_most_recent_first() {
        let tmp = trunk();
        let trunk = tmp.path();
        let now = SystemTime::now();
        workspace(trunk, "stale", now - 20 * DAY);
        let fresh = workspace(trunk, "fresh", now - 3 * DAY);
        std::fs::write(fresh.join("app/main.rs"), "fn main() { todo!() }").unwrap();
        std::fs::write(fresh.join("app/notes.md"), "wip").unwrap();
        workspace(trunk, "shelved", now - DAY);
        choros::archive(trunk, "shelved").unwrap();

        let out = ls(trunk, now);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "got:\n{out}");
        assert!(lines[0].starts_with("NAME"));
        let fresh_row: Vec<&str> = lines[1].split_whitespace().collect();
        assert_eq!(fresh_row[0], "fresh");
        assert_eq!(&fresh_row[3..], ["just", "now", "2026-01-01", "12:00", "2", "app"]);
        let stale_row: Vec<&str> = lines[2].split_whitespace().collect();
        assert_eq!(stale_row[0], "stale");
        assert_eq!(&stale_row[3..], ["2w", "ago", "2026-01-01", "12:00", "clean", "app"]);
    }

    /// Build output and git's own bookkeeping don't count as work: only
    /// files git would track, plus the reflog, move the last-active time.
    #[test]
    fn last_active_ignores_build_output_and_git_internals() {
        let tmp = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let ws = workspace(tmp.path(), "ws", now - 10 * DAY);
        let build = ws.join("app/target/debug");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(build.join("app"), "binary").unwrap();
        touch(&ws.join("app/.git/index"), now);
        assert_eq!(choros::last_active(&ws), Some(now - 10 * DAY));

        touch(&ws.join("app/.git/logs/HEAD"), now - DAY);
        assert_eq!(choros::last_active(&ws), Some(now - DAY));
    }

    /// Branch-spaces show as headings over their own workspaces, indented,
    /// and a branch-space lists only itself and what's below it.
    #[test]
    fn branch_spaces_show_as_a_tree() {
        let tmp = trunk();
        let trunk = tmp.path();
        let now = SystemTime::now();
        workspace(trunk, "typo", now - DAY);
        let epic = space::create_branch(trunk, "epic-x").unwrap();
        workspace(&epic, "login", now - 2 * DAY);
        let auth = space::create_branch(&epic, "auth").unwrap();
        workspace(&auth, "token", now - 3 * DAY);

        let whole = ls(trunk, now);
        let names: Vec<String> = whole
            .lines()
            .skip(1)
            .map(|l| {
                let indent = l.len() - l.trim_start().len();
                format!("{}{}", " ".repeat(indent), l.split_whitespace().next().unwrap())
            })
            .collect();
        assert_eq!(names, ["typo", "epic-x/", "  login", "  auth/", "    token"], "got:\n{whole}");
        assert!(whole.contains("(2 workspaces)"), "got:\n{whole}");

        let narrowed = ls(&epic, now);
        assert!(!narrowed.contains("typo"), "got:\n{narrowed}");
        assert!(narrowed.lines().nth(1).unwrap().starts_with("login"), "got:\n{narrowed}");
    }

    #[test]
    fn ago_reads_naturally() {
        let now = SystemTime::now();
        let cases = [
            (Duration::from_secs(5), "just now"),
            (Duration::from_secs(5 * 60), "5m ago"),
            (Duration::from_secs(3 * 60 * 60), "3h ago"),
            (3 * DAY, "3d ago"),
            (20 * DAY, "2w ago"),
            (90 * DAY, "3mo ago"),
        ];
        for (age, want) in cases {
            assert_eq!(ago(now - age, now), want);
        }
        assert_eq!(ago(now + DAY, now), "just now");
    }
}
