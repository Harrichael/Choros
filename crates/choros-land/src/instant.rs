//! How instant mode reaches main: `<root>/.choros-config/instant.toml`,
//! overridable per workspace in its `.choros-landing.toml`.

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

pub fn via(root: &Path, ws: &Path) -> Result<Via> {
    if let Some(v) = workspace::read(ws)?.instant {
        return Ok(v);
    }
    let path = root.join(CONFIG_DIR).join("instant.toml");
    if !path.exists() {
        return Ok(Via::default());
    }
    let file: InstantFile = toml::from_str(&std::fs::read_to_string(path)?)?;
    Ok(file.instant.via)
}
