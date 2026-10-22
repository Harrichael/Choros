//! The machine-wide list of choros roots, so `chorosd` and `choros status`
//! can find every queue without being told where to look.

use std::path::{Path, PathBuf};

use color_eyre::eyre::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct RootsFile {
    #[serde(default)]
    roots: Vec<PathBuf>,
}

pub fn roots_file(config_home: &Path) -> PathBuf {
    config_home.join("choros/roots.toml")
}

/// `$XDG_CONFIG_HOME`, else `~/.config`.
pub fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

pub fn list(config_home: &Path) -> Result<Vec<PathBuf>> {
    let path = roots_file(config_home);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file: RootsFile = toml::from_str(&std::fs::read_to_string(path)?)?;
    Ok(file.roots)
}

pub fn register(config_home: &Path, root: &Path) -> Result<()> {
    let root = root.canonicalize()?;
    let mut roots = list(config_home)?;
    if roots.contains(&root) {
        return Ok(());
    }
    roots.push(root);
    let path = roots_file(config_home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, toml::to_string_pretty(&RootsFile { roots })?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registering_a_root_twice_lists_it_once() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("psrc");
        std::fs::create_dir_all(&root).unwrap();
        let config = tmp.path().join("config");
        assert!(list(&config).unwrap().is_empty());
        register(&config, &root).unwrap();
        register(&config, &root).unwrap();
        assert_eq!(list(&config).unwrap(), [root.canonicalize().unwrap()]);
    }
}
