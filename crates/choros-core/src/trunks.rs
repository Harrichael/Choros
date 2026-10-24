//! The machine-wide list of choros trunks, so `chorosd` and `choros status`
//! can find every queue without being told where to look.

use std::path::{Path, PathBuf};

use color_eyre::eyre::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrunksFile {
    #[serde(default)]
    trunks: Vec<PathBuf>,
}

/// What choros wrote before trunks were called roots; still read so older
/// setups keep their queues watched, never written.
#[derive(Debug, Default, Deserialize)]
struct LegacyRootsFile {
    #[serde(default)]
    roots: Vec<PathBuf>,
}

pub fn trunks_file(config_home: &Path) -> PathBuf {
    config_home.join("choros/trunks.toml")
}

/// `$XDG_CONFIG_HOME`, else `~/.config`.
pub fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

pub fn list(config_home: &Path) -> Result<Vec<PathBuf>> {
    let mut trunks = Vec::new();
    let path = trunks_file(config_home);
    if path.exists() {
        let file: TrunksFile = toml::from_str(&std::fs::read_to_string(path)?)?;
        trunks = file.trunks;
    }
    let legacy = config_home.join("choros/roots.toml");
    if legacy.exists() {
        let file: LegacyRootsFile = toml::from_str(&std::fs::read_to_string(legacy)?)?;
        for t in file.roots {
            if !trunks.contains(&t) {
                trunks.push(t);
            }
        }
    }
    Ok(trunks)
}

pub fn register(config_home: &Path, trunk: &Path) -> Result<()> {
    let trunk = trunk.canonicalize()?;
    let mut trunks = list(config_home)?;
    if trunks.contains(&trunk) {
        return Ok(());
    }
    trunks.push(trunk);
    // Legacy entries move into trunks.toml with the new one; roots.toml is
    // left as it was.
    let path = trunks_file(config_home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, toml::to_string_pretty(&TrunksFile { trunks })?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registering_a_trunk_twice_lists_it_once() {
        let tmp = tempfile::tempdir().unwrap();
        let trunk = tmp.path().join("psrc");
        std::fs::create_dir_all(&trunk).unwrap();
        let config = tmp.path().join("config");
        assert!(list(&config).unwrap().is_empty());
        register(&config, &trunk).unwrap();
        register(&config, &trunk).unwrap();
        assert_eq!(list(&config).unwrap(), [trunk.canonicalize().unwrap()]);
    }

    #[test]
    fn trunks_registered_as_roots_are_still_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        std::fs::create_dir_all(config.join("choros")).unwrap();
        std::fs::write(config.join("choros/roots.toml"), "roots = [\"/old\"]\n").unwrap();
        let new = tmp.path().join("new");
        std::fs::create_dir_all(&new).unwrap();
        register(&config, &new).unwrap();
        let listed = list(&config).unwrap();
        assert_eq!(listed, [PathBuf::from("/old"), new.canonicalize().unwrap()]);
    }
}
