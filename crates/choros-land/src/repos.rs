//! Settings for a repo wherever it lands, in
//! `<trunk>/.choros-config/repos.toml`, keyed by registry name:
//!
//! ```toml
//! [app]
//! check = "cargo check --workspace"   # every set must pass it on its own tip
//! ```
//!
//! A repo can't land into a queue without a check, short of `--force`: a
//! land skips past queued work it doesn't conflict with, and the check is
//! what shows it didn't need that work.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use choros_core::choros::CONFIG_DIR;
use color_eyre::eyre::{eyre, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    pub check: Option<String>,
    /// Gone (a plain land is what `deps = "auto"` was); read only to say so.
    #[serde(default)]
    deps: Option<toml::Value>,
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
    if let Some(old) = all.iter().find_map(|(name, c)| c.deps.is_some().then_some(name)) {
        return Err(eyre!(
            "{}: drop `deps` under [{old}]; a plain land already lands on as little as it needs",
            path.display()
        ));
    }
    Ok(all.remove(repo).unwrap_or_default())
}
