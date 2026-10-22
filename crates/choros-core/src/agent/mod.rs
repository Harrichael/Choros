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

pub fn templates_dir(root: &Path) -> PathBuf {
    root.join(TEMPLATES_REL)
}

/// Set up everything agent-related in a fresh workspace: the choros-archive
/// skill plus any per-agent baselines staged under `.choros-config/templates/`.
pub fn provision(root: &Path, workspace: &Path) -> Result<()> {
    write_skill(workspace)?;
    copy_templates(&templates_dir(root), workspace)
        .wrap_err("copying .choros-config/templates into workspace")
}

fn write_skill(workspace: &Path) -> Result<()> {
    let skill_dir = workspace.join(".claude/skills/choros-archive");
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(skill_dir.join("SKILL.md"), CHOROS_ARCHIVE_SKILL)?;
    Ok(())
}

pub fn init_templates(root: &Path) -> Result<PathBuf> {
    let dir = templates_dir(root);
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

fn copy_templates(templates_root: &Path, workspace: &Path) -> Result<()> {
    if !templates_root.exists() {
        return Ok(());
    }
    for agent in Agent::ALL {
        for tf in agent.template_files() {
            let src = templates_root.join(tf.rel);
            if !src.exists() {
                continue;
            }
            let dest = workspace.join(tf.rel);
            if dest.exists() {
                return Err(eyre!(
                    "template collision: {:?} already exists in workspace; refusing to overwrite",
                    dest
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn copy_templates_errors_on_collision() {
        let tmp = tempdir();
        let root = tmp.path();

        let templates = root.join("templates");
        let claude_t = templates.join(".claude");
        std::fs::create_dir_all(&claude_t).unwrap();
        std::fs::write(claude_t.join("settings.json"), b"from-template").unwrap();

        let workspace = root.join("ws");
        let claude_w = workspace.join(".claude");
        std::fs::create_dir_all(&claude_w).unwrap();
        std::fs::write(claude_w.join("settings.json"), b"from-workspace").unwrap();

        let err = copy_templates(&templates, &workspace).unwrap_err();
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
        let root = tmp.path();
        init_templates(root).unwrap();
        let settings = root.join(".choros-config/templates/.claude/settings.json");
        std::fs::write(&settings, b"user-edited").unwrap();
        init_templates(root).unwrap();
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "user-edited");
    }
}
