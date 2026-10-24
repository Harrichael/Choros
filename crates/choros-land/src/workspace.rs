//! A workspace's landing bookkeeping, in `.choros-landing.toml` beside its
//! `.choros-meta.toml`: which queue each repo lands into, what each repo's
//! branch builds on, and how far it has been landed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use color_eyre::eyre::Result;
use serde::{Deserialize, Serialize};

use crate::state::write_atomic;

pub const LANDING_FILE: &str = ".choros-landing.toml";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceLanding {
    #[serde(default)]
    pub repo: BTreeMap<String, RepoLanding>,
    /// Set when this workspace is itself a landing folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landing: Option<LandingOf>,
    /// Instant mode's route to main for this workspace, overriding the trunk's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instant: Option<crate::instant::Via>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoLanding {
    /// Absent: instant mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
    /// Everything up to here has been landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed_through: Option<String>,
    /// The merge head content `base` had (`Composition::key`), so the prompt
    /// can tell "needs sync" from files alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition: Option<String>,
    /// Set while the workspace is being rebased onto this commit (a sync, or
    /// a fork's base). Once HEAD contains it with no rebase in progress, it
    /// becomes `landed_through`; a conflict resolved by hand therefore
    /// finishes the sync rather than looking like rewritten history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_composition: Option<String>,
    /// Sets a fork land moved this repo onto: the commits now build on them,
    /// so the fork must keep depending on them when the land is re-run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fork_deps: Vec<u32>,
    /// Unix seconds of the last land, so a raw-terminal land forks only the
    /// sessions active since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_landed: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LandingOf {
    pub queue: String,
    pub n: u32,
    pub parent: PathBuf,
}

pub fn read(workspace: &Path) -> Result<WorkspaceLanding> {
    let path = workspace.join(LANDING_FILE);
    if !path.exists() {
        return Ok(WorkspaceLanding::default());
    }
    Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
}

pub fn write(workspace: &Path, landing: &WorkspaceLanding) -> Result<()> {
    write_atomic(
        &workspace.join(LANDING_FILE),
        toml::to_string_pretty(landing)?.as_bytes(),
    )
}
