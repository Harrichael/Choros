use std::path::{Path, PathBuf};

use color_eyre::eyre::{eyre, Result};

const REGISTRY_REL: &str = ".ws-config/registry";

pub fn registry_dir(root: &Path) -> PathBuf {
    root.join(REGISTRY_REL)
}

pub fn init_dirs(root: &Path) -> Result<PathBuf> {
    let dir = registry_dir(root);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn repo_path(root: &Path, name: &str) -> PathBuf {
    registry_dir(root).join(name)
}

pub fn scan(root: &Path) -> Result<Vec<String>> {
    let dir = registry_dir(root);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if !path.join(".git").exists() && !path.join("HEAD").exists() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

pub fn ensure_repo_exists(root: &Path, name: &str) -> Result<PathBuf> {
    let path = repo_path(root, name);
    if !path.exists() {
        return Err(eyre!("registry repo {:?} does not exist", path));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_repo(parent: &Path, name: &str) {
        let dir = parent.join(name);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
    }

    #[test]
    fn scan_empty_when_no_registry() {
        let tmp = tempdir();
        assert!(scan(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn scan_finds_git_dirs_only() {
        let tmp = tempdir();
        let reg = registry_dir(tmp.path());
        std::fs::create_dir_all(&reg).unwrap();
        fake_repo(&reg, "alpha");
        fake_repo(&reg, "beta");
        std::fs::create_dir_all(reg.join("not-a-repo")).unwrap();
        std::fs::write(reg.join("README"), b"ignore me").unwrap();

        let mut found = scan(tmp.path()).unwrap();
        found.sort();
        assert_eq!(found, vec!["alpha".to_string(), "beta".to_string()]);
    }

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }
}
