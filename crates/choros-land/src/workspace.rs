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
    /// Instant mode's route to main for this workspace, overriding the root's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instant: Option<crate::instant::Via>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoLanding {
    /// Absent: instant mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
    /// The commit the repo's branch currently builds on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Everything up to here has been landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed_through: Option<String>,
    /// The merge head content `base` had (`Composition::key`), so the prompt
    /// can tell "needs sync" from files alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition: Option<String>,
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
