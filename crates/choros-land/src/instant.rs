//! How instant mode reaches main: `<trunk>/.choros-config/instant.toml`,
//! overridable by a branch-space's `landing.toml` and per workspace in its
//! `.choros-landing.toml`, nearest first.

use std::path::Path;

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
