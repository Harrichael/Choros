//! Settings for a repo wherever it lands, in
//! `<trunk>/.choros-config/repos.toml`, keyed by registry name:
//!
//! ```toml
//! [app]
//! check = "cargo check --workspace"   # run on a set before an auto land trusts it
//! deps = "auto"                       # lands pick their own dependencies
//! ```
//!
//! `deps = "auto"` needs a `check`: lands that skip past queued work must
//! show they still build without it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use choros_core::choros::CONFIG_DIR;
use color_eyre::eyre::{eyre, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    pub check: Option<String>,
    #[serde(default)]
    pub deps: DepsDefault,
}

/// What a plain `land` depends on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DepsDefault {
    /// Everything queued in the repo.
    #[default]
    Queue,
    /// As little as the commits need; see `land::land_auto`.
    Auto,
}

pub fn path(trunk: &Path) -> PathBuf {
    trunk.join(CONFIG_DIR).join("repos.toml")
}

pub fn read(trunk: &Path, repo: &str) -> Result<RepoConfig> {
    let path = path(trunk);
    if !path.exists() {
        return Ok(RepoConfig::default());
    }
    let mut all: BTreeMap<String, RepoConfig> = toml::from_str(&std::fs::read_to_string(&path)?)
        .map_err(|e| eyre!("{}: {e}", path.display()))?;
    let config = all.remove(repo).unwrap_or_default();
    if config.deps == DepsDefault::Auto && config.check.is_none() {
        return Err(eyre!(
            "{}: {repo} has deps = \"auto\" but no check; auto lands need one",
            path.display()
        ));
    }
    Ok(config)
}
