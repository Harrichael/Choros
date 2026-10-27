//! Instant mode: how it reaches main, and what it put there.
//!
//! How: `<trunk>/.choros-config/instant.toml`, overridable by a branch-space's
//! `landing.toml` and per workspace in its `.choros-landing.toml`, nearest
//! first.
//!
//! What: each land is a line in `<trunk>/.choros-config/instant.jsonl`. It
//! makes no landing-set, so this log is the only record of its labels and
//! sessions, which `choros status --json` reports for other tools.

use std::io::Write;
use std::path::{Path, PathBuf};

use choros_core::choros::CONFIG_DIR;
use color_eyre::eyre::Result;
use serde::{Deserialize, Serialize};

use crate::workspace;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    /// Push straight to main.
    #[default]
    Push,
    /// Open a PR and merge it at once: a PR record, and required checks run.
    Pr,
}

#[derive(Debug, Default, Deserialize)]
struct InstantFile {
    #[serde(default)]
    instant: Section,
}

#[derive(Debug, Default, Deserialize)]
struct Section {
    #[serde(default)]
    via: Via,
}

pub fn via(trunk: &Path, ws: &Path) -> Result<Via> {
    if let Some(v) = workspace::read(ws)?.instant {
        return Ok(v);
    }
    if let Some(space) = ws.parent().and_then(choros_core::space::find_space)
        && let Some(v) = crate::branch::default_instant(&space)?
    {
        return Ok(v);
    }
    let path = trunk.join(CONFIG_DIR).join("instant.toml");
    if !path.exists() {
        return Ok(Via::default());
    }
    let file: InstantFile = toml::from_str(&std::fs::read_to_string(path)?)?;
    Ok(file.instant.via)
}

/// A land that went straight to main.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Landed {
    pub repo: String,
    pub workspace: PathBuf,
    /// The last commit it put on main, as rebased in the workspace.
    pub commit: String,
    /// The PR it merged through, with `via = "pr"`.
    pub pr: Option<u64>,
    pub labels: Vec<String>,
    /// The Claude Code sessions the work came from.
    pub sessions: Vec<String>,
}

fn log_path(trunk: &Path) -> PathBuf {
    trunk.join(CONFIG_DIR).join("instant.jsonl")
}

/// One line per land, appended whole, so concurrent lands need no lock.
pub fn record(trunk: &Path, landed: &Landed) -> Result<()> {
    let mut line = serde_json::to_string(landed)?;
    line.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(trunk))?
        .write_all(line.as_bytes())?;
    Ok(())
}

pub fn read(trunk: &Path) -> Result<Vec<Landed>> {
    let path = log_path(trunk);
    if !path.exists() {
        return Ok(Vec::new());
    }
    std::fs::read_to_string(path)?
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| Ok(serde_json::from_str(l)?))
        .collect()
}
