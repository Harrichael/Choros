use std::cmp::Reverse;
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use color_eyre::eyre::Result;
use serde_json::Value;
use time::OffsetDateTime;

use crate::choros;

pub struct Session {
    pub agent: &'static str,
    pub id: String,
    pub modified: SystemTime,
    pub preview: String,
    /// Path the session was invoked from, relative to `base` (the workspace
    /// root if cwd is inside one, else cwd itself). `"."` for sessions
    /// invoked at `base` itself.
    pub path: String,
}

pub fn run(cwd: &Path) -> Result<()> {
    let home = match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h),
        None => {
            eprintln!("HOME is not set; cannot locate agent session dirs");
            return Ok(());
        }
    };
    // Prefer the choros workspace root as the base so the PATH column is
    // workspace-relative. Fall back to cwd if we're not inside a workspace.
    let base = match choros::resolve_target(cwd, None) {
        Ok((root, name)) => root.join(name),
        Err(_) => cwd.to_path_buf(),
    };
    let mut stdout = std::io::stdout().lock();
    run_inner(&home, &base, &mut stdout)
}

fn run_inner(home: &Path, base: &Path, out: &mut dyn Write) -> Result<()> {
    let mut sessions = Vec::new();
    sessions.extend(claude_sessions(home, base)?);
    sessions.extend(cursor_sessions(home, base)?);
    sessions.sort_by_key(|s| Reverse(s.modified));

    writeln!(out, "sessions under {}:", base.display())?;
    writeln!(out)?;
    if sessions.is_empty() {
        writeln!(out, "  (none found)")?;
        return Ok(());
    }
    writeln!(
        out,
        "{:<6}  {:<16}  {:<36}  {:<20}  {}",
        "AGENT", "MODIFIED (UTC)", "ID", "PATH", "PREVIEW"
    )?;
    for s in &sessions {
        writeln!(
            out,
            "{:<6}  {:<16}  {:<36}  {:<20}  {}",
            s.agent,
            format_time(s.modified),
            s.id,
            truncate(&s.path, 20),
            truncate(&s.preview, 80),
        )?;
    }
    Ok(())
}

/// Encode a filesystem path the way Claude Code / Cursor name their per-cwd
/// session dirs: replace every character that is not `[A-Za-z0-9-]` with `-`.
/// The encoding is lossy (so e.g. `dev_choros` and `dev/choros` collide), but
/// it matches what the agents themselves write.
pub fn encode_cwd(p: &Path) -> String {
    p.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect()
}

/// True if `dir_name` is the encoded form of `cwd` itself or of a path
/// beneath it. Adds a `-` boundary check so `/foo/bar` doesn't match `/foo/barbar`.
fn matches_cwd(dir_name: &str, encoded_cwd: &str) -> bool {
    if dir_name == encoded_cwd {
        return true;
    }
    match dir_name.strip_prefix(encoded_cwd) {
        Some(rest) => rest.starts_with('-'),
        None => false,
    }
}

fn claude_sessions(home: &Path, base: &Path) -> Result<Vec<Session>> {
    let session_base = home.join(".claude/projects");
    let encoded_base = encode_cwd(base);
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&session_base) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !matches_cwd(name, &encoded_base) {
            continue;
        }
        let rel = relative_path(base, name, &encoded_base);
        collect_claude_dir(&path, &rel, &mut out)?;
    }
    Ok(out)
}

fn collect_claude_dir(dir: &Path, rel_path: &str, out: &mut Vec<Session>) -> Result<()> {
    for entry in fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        if path.extension() != Some(OsStr::new("jsonl")) {
            continue;
        }
        let id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let modified = entry.metadata()?.modified()?;
        let preview = first_user_message_claude(&path).unwrap_or_default();
        out.push(Session {
            agent: "claude",
            id,
            modified,
            preview,
            path: rel_path.to_string(),
        });
    }
    Ok(())
}

fn first_user_message_claude(path: &Path) -> Option<String> {
    let f = fs::File::open(path).ok()?;
    let reader = BufReader::new(f);
    for line in reader.lines().take(500) {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        let content = v.get("message").and_then(|m| m.get("content"))?;
        if let Some(s) = content.as_str() {
            return Some(s.to_string());
        }
        if let Some(arr) = content.as_array() {
            for block in arr {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                        return Some(text.to_string());
                    }
                }
            }
        }
    }
    None
}

fn cursor_sessions(home: &Path, base: &Path) -> Result<Vec<Session>> {
    // Cursor CLI stores per-project data under ~/.cursor/projects/ and chat
    // transcripts under ~/.cursor/chats/. The exact on-disk format isn't
    // publicly documented, so we treat both as candidate dirs and best-effort
    // pull out a preview from whatever JSON / JSONL we find.
    let encoded_base = encode_cwd(base);
    let mut out = Vec::new();
    for sub in ["projects", "chats"] {
        let session_base = home.join(".cursor").join(sub);
        let Ok(entries) = fs::read_dir(&session_base) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            if !matches_cwd(name, &encoded_base) {
                continue;
            }
            let rel = relative_path(base, name, &encoded_base);
            collect_cursor_dir(&path, &rel, &mut out)?;
        }
    }
    Ok(out)
}

fn collect_cursor_dir(dir: &Path, rel_path: &str, out: &mut Vec<Session>) -> Result<()> {
    for entry in fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let modified = entry.metadata()?.modified()?;
        let preview = first_user_message_cursor(&path).unwrap_or_default();
        out.push(Session {
            agent: "cursor",
            id,
            modified,
            preview,
            path: rel_path.to_string(),
        });
    }
    Ok(())
}

/// Decode the encoded suffix of a session dir back into a path relative to
/// `base`. The encoding is lossy (`/`, `_`, `.` all become `-`), so we recover
/// the original by walking the actual filesystem under `base` and finding a
/// path whose component-wise encoding matches the suffix. Returns the encoded
/// suffix verbatim as a last-resort fallback when no real path matches.
fn relative_path(base: &Path, dir_name: &str, encoded_base: &str) -> String {
    if dir_name == encoded_base {
        return ".".into();
    }
    let suffix = match dir_name.strip_prefix(encoded_base).and_then(|r| r.strip_prefix('-')) {
        Some(s) if !s.is_empty() => s,
        _ => return ".".into(),
    };
    probe_path(base, suffix).unwrap_or_else(|| suffix.to_string())
}

fn probe_path(base: &Path, encoded_remainder: &str) -> Option<String> {
    if encoded_remainder.is_empty() {
        return Some(String::new());
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(base)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();
    for path in entries {
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let enc = encode_segment(name);
        if encoded_remainder == enc {
            return Some(name.to_string());
        }
        if let Some(rest) = encoded_remainder
            .strip_prefix(&enc)
            .and_then(|r| r.strip_prefix('-'))
        {
            if let Some(sub) = probe_path(&path, rest) {
                let joined = if sub.is_empty() {
                    name.to_string()
                } else {
                    format!("{name}/{sub}")
                };
                return Some(joined);
            }
        }
    }
    None
}

fn encode_segment(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect()
}

fn first_user_message_cursor(path: &Path) -> Option<String> {
    let body = fs::read_to_string(path).ok()?;
    // Try as single JSON object first.
    if let Ok(v) = serde_json::from_str::<Value>(&body) {
        if let Some(t) = extract_user_text(&v) {
            return Some(t);
        }
    }
    // Fall back to JSONL.
    for line in body.lines().take(500) {
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if let Some(t) = extract_user_text(&v) {
                return Some(t);
            }
        }
    }
    None
}

fn extract_user_text(v: &Value) -> Option<String> {
    // Direct `{role:"user", content:"..."}` shape.
    if v.get("role").and_then(|r| r.as_str()) == Some("user") {
        if let Some(s) = v.get("content").and_then(|c| c.as_str()) {
            return Some(s.to_string());
        }
    }
    // Container `{messages: [{role:"user", content:"..."}, ...]}`.
    if let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) {
        for msg in msgs {
            if msg.get("role").and_then(|r| r.as_str()) == Some("user") {
                if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

fn format_time(t: SystemTime) -> String {
    let dt: OffsetDateTime = t.into();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute()
    )
}

fn truncate(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    for c in s.chars() {
        let c = if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c };
        if count + 1 > max {
            out.push('…');
            return out;
        }
        out.push(c);
        count += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn encode_cwd_replaces_separators_and_specials() {
        let s = encode_cwd(Path::new("/home/harrichael/xsrc/dev_choros/Choros"));
        assert_eq!(s, "-home-harrichael-xsrc-dev-choros-Choros");
    }

    #[test]
    fn encode_cwd_preserves_alphanum_and_dash() {
        let s = encode_cwd(Path::new("/home/me/claude-help"));
        assert_eq!(s, "-home-me-claude-help");
    }

    #[test]
    fn encode_cwd_encodes_dots() {
        let s = encode_cwd(Path::new("/x/.choros-config"));
        assert_eq!(s, "-x--choros-config");
    }

    #[test]
    fn matches_cwd_exact_and_prefix_with_dash() {
        assert!(matches_cwd("-foo-bar", "-foo-bar"));
        assert!(matches_cwd("-foo-bar-baz", "-foo-bar"));
        assert!(!matches_cwd("-foo-barbar", "-foo-bar"));
        assert!(!matches_cwd("-foo", "-foo-bar"));
    }

    #[test]
    fn truncate_keeps_short_strings() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_caps_long_strings() {
        let s = "a".repeat(200);
        let t = truncate(&s, 10);
        assert!(t.ends_with('…'));
        assert_eq!(t.chars().count(), 11);
    }

    #[test]
    fn truncate_replaces_whitespace_with_space() {
        assert_eq!(truncate("a\nb\tc", 80), "a b c");
    }

    #[test]
    fn claude_sessions_found_for_exact_cwd() {
        let home = tempdir();
        let cwd = Path::new("/home/u/proj");
        let encoded = encode_cwd(cwd);
        let dir = home.path().join(".claude/projects").join(&encoded);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("aaa.jsonl"),
            br#"{"type":"permission-mode","sessionId":"aaa"}
{"type":"user","message":{"role":"user","content":"hello there"}}
"#,
        )
        .unwrap();

        let sessions = claude_sessions(home.path(), cwd).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "aaa");
        assert_eq!(sessions[0].preview, "hello there");
        assert_eq!(sessions[0].path, ".");
    }

    #[test]
    fn claude_sessions_found_for_subdir_of_cwd() {
        let home = tempdir();
        let cwd = Path::new("/home/u/proj");
        let sub_encoded = encode_cwd(Path::new("/home/u/proj/PROJ-1"));
        let dir = home.path().join(".claude/projects").join(&sub_encoded);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("bbb.jsonl"),
            br#"{"type":"user","message":{"role":"user","content":"sub session"}}"#,
        )
        .unwrap();

        let sessions = claude_sessions(home.path(), cwd).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "bbb");
        // /home/u/proj doesn't exist on disk so probe can't recover the real
        // path; fall back to the encoded suffix.
        assert_eq!(sessions[0].path, "PROJ-1");
    }

    #[test]
    fn claude_sessions_path_decoded_via_real_workspace() {
        let home = tempdir();
        let workspace = tempdir();
        // Real workspace with a subdir whose name uses `_` (encoded as `-`).
        let repo = workspace.path().join("api_server").join("src");
        std::fs::create_dir_all(&repo).unwrap();

        let encoded = encode_cwd(&repo);
        let sdir = home.path().join(".claude/projects").join(&encoded);
        std::fs::create_dir_all(&sdir).unwrap();
        std::fs::write(
            sdir.join("ccc.jsonl"),
            br#"{"type":"user","message":{"role":"user","content":"deep"}}"#,
        )
        .unwrap();

        let sessions = claude_sessions(home.path(), workspace.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        // probe recovers `api_server/src`, not the lossy `api-server-src`.
        assert_eq!(sessions[0].path, "api_server/src");
    }

    #[test]
    fn claude_sessions_skips_unrelated_dirs() {
        let home = tempdir();
        // Cwd encodes to `-home-u-proj`; this dir encodes to `-home-u-projother`.
        let dir = home.path().join(".claude/projects/-home-u-projother");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("zzz.jsonl"), b"").unwrap();

        let sessions = claude_sessions(home.path(), Path::new("/home/u/proj")).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn claude_sessions_handles_missing_projects_dir() {
        let home = tempdir();
        let sessions = claude_sessions(home.path(), Path::new("/x/y")).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn cursor_sessions_reads_chat_json() {
        let home = tempdir();
        let cwd = Path::new("/home/u/proj");
        let encoded = encode_cwd(cwd);
        let dir = home.path().join(".cursor/chats").join(&encoded);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("session-7.json"),
            br#"{"messages":[{"role":"user","content":"refactor auth please"},{"role":"assistant","content":"ok"}]}"#,
        )
        .unwrap();

        let sessions = cursor_sessions(home.path(), cwd).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "session-7");
        assert_eq!(sessions[0].preview, "refactor auth please");
        assert_eq!(sessions[0].path, ".");
    }

    #[test]
    fn run_inner_empty_prints_none_found() {
        let home = tempdir();
        let mut buf = Vec::new();
        run_inner(home.path(), Path::new("/nowhere"), &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("none found"), "got: {s}");
    }

    #[test]
    fn run_inner_sorts_most_recent_first() {
        let home = tempdir();
        let cwd = Path::new("/home/u/proj");
        let encoded = encode_cwd(cwd);

        let cdir = home.path().join(".claude/projects").join(&encoded);
        std::fs::create_dir_all(&cdir).unwrap();
        std::fs::write(
            cdir.join("older.jsonl"),
            br#"{"type":"user","message":{"role":"user","content":"older one"}}"#,
        )
        .unwrap();

        // Filesystem mtime resolution is coarse (1s on some FS); sleep so the
        // second write lands in a strictly later second.
        std::thread::sleep(std::time::Duration::from_millis(1100));

        let xdir = home.path().join(".cursor/chats").join(&encoded);
        std::fs::create_dir_all(&xdir).unwrap();
        std::fs::write(
            xdir.join("newer.json"),
            br#"{"messages":[{"role":"user","content":"newer one"}]}"#,
        )
        .unwrap();

        let mut buf = Vec::new();
        run_inner(home.path(), cwd, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        let newer_pos = s.find("newer").unwrap();
        let older_pos = s.find("older").unwrap();
        assert!(
            newer_pos < older_pos,
            "expected newer session listed before older; got:\n{s}"
        );
    }
}
