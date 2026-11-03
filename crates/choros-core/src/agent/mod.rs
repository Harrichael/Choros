//! Everything choros knows about coding agents (Claude Code, Cursor CLI):
//! which files each agent reads, the per-workspace baselines seeded from
//! `.choros-config/templates/`, the choros-archive skill, settings promotion,
//! and session discovery. Workspace lifecycle (`crate::choros`) only calls
//! `provision`; agent specifics stay behind this module.

pub mod ls;
pub mod settings;

use std::path::{Path, PathBuf};

use color_eyre::eyre::{eyre, Context, Result};

pub const TEMPLATES_REL: &str = ".choros-config/templates";
pub const CLAUDE_SETTINGS_REL: &str = ".claude/settings.json";

const CHOROS_ARCHIVE_SKILL: &str = include_str!("../skills/choros-archive.md");
const CHOROS_DEFAULT_CLAUDE_SETTINGS: &str = include_str!("../templates/claude-settings.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Cursor,
}

pub struct TemplateFile {
    pub rel: &'static str,
    pub default_content: Option<&'static str>,
}

impl Agent {
    pub const ALL: &'static [Agent] = &[Agent::Claude, Agent::Cursor];

    pub fn template_files(&self) -> &'static [TemplateFile] {
        match self {
            Agent::Claude => &[TemplateFile {
                rel: CLAUDE_SETTINGS_REL,
                default_content: Some(CHOROS_DEFAULT_CLAUDE_SETTINGS),
            }],
            // Cursor's project-level `.cursor/cli.json` *shadows* the user's
            // global allowlist, so we deliberately don't seed a default.
            Agent::Cursor => &[TemplateFile {
                rel: ".cursor/cli.json",
                default_content: None,
            }],
        }
    }
}

pub fn templates_dir(trunk: &Path) -> PathBuf {
    trunk.join(TEMPLATES_REL)
}

/// A branch-space's own templates, which win over the trunk's file by file.
pub const BRANCH_TEMPLATES_REL: &str = ".choros-branch-config/templates";

/// Where a workspace's templates come from, nearest space first.
fn template_dirs(workspace: &Path) -> Result<Vec<PathBuf>> {
    let space = workspace.parent().and_then(crate::space::find_space);
    let Some(space) = space else {
        return Ok(Vec::new());
    };
    Ok(crate::space::chain(&space)?
        .into_iter()
        .map(|s| {
            if crate::space::is_trunk(&s) {
                templates_dir(&s)
            } else {
                s.join(BRANCH_TEMPLATES_REL)
            }
        })
        .collect())
}

/// Set up everything agent-related in a fresh workspace: the choros-archive
/// skill plus any per-agent baselines staged under `.choros-config/templates/`,
/// or a branch-space's `.choros-branch-config/templates/` above it.
pub fn provision(workspace: &Path) -> Result<()> {
    write_skill(workspace)?;
    copy_templates(&template_dirs(workspace)?, workspace)
        .wrap_err("copying templates into workspace")
}

fn write_skill(workspace: &Path) -> Result<()> {
    let skill_dir = workspace.join(".claude/skills/choros-archive");
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(skill_dir.join("SKILL.md"), CHOROS_ARCHIVE_SKILL)?;
    Ok(())
}

pub fn init_templates(trunk: &Path) -> Result<PathBuf> {
    let dir = templates_dir(trunk);
    std::fs::create_dir_all(&dir)?;
    for agent in Agent::ALL {
        for tf in agent.template_files() {
            let Some(default) = tf.default_content else {
                continue;
            };
            let path = dir.join(tf.rel);
            if path.exists() {
                continue;
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, default)?;
        }
    }
    Ok(dir)
}

fn copy_templates(dirs: &[PathBuf], workspace: &Path) -> Result<()> {
    for agent in Agent::ALL {
        for tf in agent.template_files() {
            let Some(src) = dirs.iter().map(|d| d.join(tf.rel)).find(|p| p.exists()) else {
                continue;
            };
            let dest = workspace.join(tf.rel);
            if dest.exists() {
                return Err(eyre!(
                    "template collision: {} already exists in the workspace; refusing to overwrite",
                    dest.display()
                ));
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src, &dest)?;
        }
    }
    Ok(())
}

/// Carry Claude Code's sessions for `from`, and for folders below it, over
/// to `to`, a folder just moved there. Claude Code keeps a folder's sessions
/// under its path, so a moved folder's sessions would not resume from it.
pub fn move_claude_sessions(home: &Path, from: &Path, to: &Path) -> Result<()> {
    let projects = home.join(".claude/projects");
    let Ok(entries) = std::fs::read_dir(&projects) else { return Ok(()) };
    let (old, new) = (ls::encode_cwd(from), ls::encode_cwd(to));
    let (from, to) = (from.to_string_lossy(), to.to_string_lossy());
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(&old).filter(|r| r.is_empty() || r.starts_with('-')) else {
            continue;
        };
        let dst = projects.join(format!("{new}{rest}"));
        if dst.exists() {
            continue;
        }
        // Rewritten before the rename, so a failure leaves the folder to retry.
        for file in std::fs::read_dir(entry.path())?.flatten() {
            let path = file.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            // Bounded by what can follow a path, so `landing-1` spares `landing-10`.
            let text = std::fs::read_to_string(&path)?;
            let moved = text
                .replace(&format!("{from}/"), &format!("{to}/"))
                .replace(&format!("{from}\""), &format!("{to}\""));
            if moved != text {
                std::fs::write(&path, moved)?;
            }
        }
        std::fs::rename(entry.path(), &dst)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn copy_templates_errors_on_collision() {
        let tmp = tempdir();
        let trunk = tmp.path();

        let templates = trunk.join("templates");
        let claude_t = templates.join(".claude");
        std::fs::create_dir_all(&claude_t).unwrap();
        std::fs::write(claude_t.join("settings.json"), b"from-template").unwrap();

        let workspace = trunk.join("ws");
        let claude_w = workspace.join(".claude");
        std::fs::create_dir_all(&claude_w).unwrap();
        std::fs::write(claude_w.join("settings.json"), b"from-workspace").unwrap();

        let err = copy_templates(&[templates], &workspace).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("template collision"), "got: {msg}");

        // Destination unchanged.
        assert_eq!(
            std::fs::read_to_string(claude_w.join("settings.json")).unwrap(),
            "from-workspace"
        );
    }

    #[test]
    fn init_templates_preserves_existing_settings() {
        let tmp = tempdir();
        let trunk = tmp.path();
        init_templates(trunk).unwrap();
        let settings = trunk.join(".choros-config/templates/.claude/settings.json");
        std::fs::write(&settings, b"user-edited").unwrap();
        init_templates(trunk).unwrap();
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "user-edited");
    }

    /// A moved folder's sessions, and those of folders below it, follow it
    /// with the paths inside rewritten; a sibling that shares a prefix stays.
    #[test]
    fn claude_sessions_follow_a_moved_folder() {
        let home = tempdir();
        let projects = home.path().join(".claude/projects");
        let (from, to) = (Path::new("/t/landing/q/landing-1"), Path::new("/t/.cfg/q/landing/landing-1"));
        let session = |dir: &Path, cwd: &str| {
            std::fs::create_dir_all(projects.join(ls::encode_cwd(dir))).unwrap();
            let line = format!("{{\"cwd\":\"{cwd}\"}}\n");
            std::fs::write(projects.join(ls::encode_cwd(dir)).join("s.jsonl"), line).unwrap();
        };
        session(from, "/t/landing/q/landing-1");
        session(&from.join("repo"), "/t/landing/q/landing-1/repo");
        session(Path::new("/t/landing/q/landing-10"), "/t/landing/q/landing-10");

        move_claude_sessions(home.path(), from, to).unwrap();

        let read = |dir: &Path| std::fs::read_to_string(projects.join(ls::encode_cwd(dir)).join("s.jsonl")).unwrap();
        assert_eq!(read(to), "{\"cwd\":\"/t/.cfg/q/landing/landing-1\"}\n");
        assert_eq!(read(&to.join("repo")), "{\"cwd\":\"/t/.cfg/q/landing/landing-1/repo\"}\n");
        assert_eq!(read(Path::new("/t/landing/q/landing-10")), "{\"cwd\":\"/t/landing/q/landing-10\"}\n");
        assert!(!projects.join(ls::encode_cwd(from)).exists());
    }
}
