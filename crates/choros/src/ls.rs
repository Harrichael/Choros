//! `choros ls`: the active workspaces under a choros root, most recently
//! worked-in first.

use std::cmp::Reverse;
use std::io::Write;
use std::path::Path;
use std::time::SystemTime;

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

pub fn run(cwd: &Path) -> Result<()> {
    if let Ok((root, name)) = choros::resolve_target(cwd, None) {
        return Err(eyre!(
            "inside workspace '{name}'; run `choros ls` from the choros root ({})",
            root.display()
        ));
    }
    // Must run before anything spawns a thread: on Unix, `time` refuses to
    // read the local offset from a multithreaded process.
    let offset = UtcOffset::current_local_offset().ok();
    let mut stdout = std::io::stdout().lock();
    run_inner(cwd, offset, SystemTime::now(), &mut stdout)
}

fn run_inner(
    root: &Path,
    offset: Option<UtcOffset>,
    now: SystemTime,
    out: &mut dyn Write,
) -> Result<()> {
    let mut rows: Vec<Row> = choros::scan(root)?
        .into_iter()
        .map(|info| Row {
            last_active: choros::last_active(&info.path),
            created: OffsetDateTime::parse(&info.meta.created_at, &Rfc3339)
                .map_err(|_| info.meta.created_at.clone()),
            changed: info
                .meta
                .repos
                .iter()
                .map(|r| git::changed_count(&info.path.join(r)).ok())
                .sum(),
            name: info.meta.name,
            repos: info.meta.repos,
        })
        .collect();
    rows.sort_by_key(|r| Reverse(r.last_active));

    if rows.is_empty() {
        writeln!(out, "(no workspaces)")?;
        return Ok(());
    }
    let tz = if offset.is_some() { "" } else { " (UTC)" };
    let last_active_header = format!("LAST ACTIVE{tz}");
    let created_header = format!("CREATED{tz}");
    let offset = offset.unwrap_or(UtcOffset::UTC);
    let name_w = rows.iter().map(|r| r.name.chars().count()).max().unwrap_or(0).max(4);
    writeln!(
        out,
        "{:<name_w$}  {:<17}  {:<9}  {:<17}  {:<7}  REPOS",
        "NAME", last_active_header, "AGO", created_header, "CHANGED"
    )?;
    for r in &rows {
        writeln!(
            out,
            "{:<name_w$}  {:<17}  {:<9}  {:<17}  {:<7}  {}",
            r.name,
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
        )?;
    }
    Ok(())
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
    fn workspace(root: &Path, name: &str, at: SystemTime) -> std::path::PathBuf {
        let ws = root.join(name);
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

    fn ls(root: &Path, now: SystemTime) -> String {
        let mut buf = Vec::new();
        run_inner(root, Some(UtcOffset::UTC), now, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    /// Workspaces are listed most recently worked-in first, with how stale
    /// each one is and how much uncommitted work it holds; archived ones are
    /// left out.
    #[test]
    fn lists_active_workspaces_most_recent_first() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let now = SystemTime::now();
        workspace(root, "stale", now - 20 * DAY);
        let fresh = workspace(root, "fresh", now - 3 * DAY);
        std::fs::write(fresh.join("app/main.rs"), "fn main() { todo!() }").unwrap();
        std::fs::write(fresh.join("app/notes.md"), "wip").unwrap();
        workspace(root, "shelved", now - DAY);
        choros::archive(root, "shelved").unwrap();

        let out = ls(root, now);
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

    #[test]
    fn refuses_to_run_inside_a_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = workspace(tmp.path(), "ws", SystemTime::now());
        let err = run(&ws.join("app")).unwrap_err();
        assert!(err.to_string().contains("inside workspace 'ws'"), "{err}");
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
