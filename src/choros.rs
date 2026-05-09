use std::path::{Path, PathBuf};

use color_eyre::eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::{git, registry};

pub trait ProgressSink {
    fn status(&self, msg: String);
}

impl ProgressSink for () {
    fn status(&self, _: String) {}
}

pub const META_FILE: &str = ".choros-meta.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChorosMeta {
    pub name: String,
    pub created_at: String,
    pub repos: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ChorosInfo {
    pub path: PathBuf,
    pub meta: ChorosMeta,
}

pub fn meta_path(choros_dir: &Path) -> PathBuf {
    choros_dir.join(META_FILE)
}

pub fn read_meta(choros_dir: &Path) -> Result<ChorosMeta> {
    let body = std::fs::read_to_string(meta_path(choros_dir))?;
    Ok(toml::from_str(&body)?)
}

pub fn write_meta(choros_dir: &Path, meta: &ChorosMeta) -> Result<()> {
    let body = toml::to_string_pretty(meta)?;
    std::fs::write(meta_path(choros_dir), body)?;
    Ok(())
}

pub fn scan(root: &Path) -> Result<Vec<ChorosInfo>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with('.'))
            .unwrap_or(true)
        {
            continue;
        }
        let meta_file = meta_path(&path);
        if !meta_file.exists() {
            continue;
        }
        match read_meta(&path) {
            Ok(meta) => out.push(ChorosInfo { path, meta }),
            Err(_) => continue,
        }
    }
    out.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
    Ok(out)
}

pub fn validate_name(root: &Path, name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(eyre!("name cannot be empty"));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(eyre!("name cannot contain slashes"));
    }
    if name.starts_with('.') {
        return Err(eyre!("name cannot start with '.'"));
    }
    let target = root.join(name);
    if target.exists() {
        return Err(eyre!("'{}' already exists", name));
    }
    Ok(())
}

pub fn create<P: ProgressSink>(
    root: &Path,
    name: &str,
    repos: &[String],
    progress: &P,
) -> Result<ChorosInfo> {
    validate_name(root, name)?;
    let target = root.join(name);
    progress.status(format!("mkdir {}", target.display()));
    std::fs::create_dir_all(&target)?;

    for repo in repos {
        progress.status(format!("cloning {repo}…"));
        let reg_path = registry::ensure_repo_exists(root, repo)?;
        let url = git::remote_get_url(&reg_path, "origin")?;
        if let Err(e) = git::fetch(&reg_path, "origin") {
            progress.status(format!("fetch warning ({repo}): {e}; continuing"));
        }
        let dest = target.join(repo);
        git::clone_with_reference(&reg_path, &url, &dest)?;
    }

    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".into());
    let meta = ChorosMeta {
        name: name.to_string(),
        created_at: now,
        repos: repos.to_vec(),
    };
    write_meta(&target, &meta)?;
    Ok(ChorosInfo {
        path: target,
        meta,
    })
}

pub fn delete(choros: &ChorosInfo) -> Result<()> {
    if !choros.path.join(META_FILE).exists() {
        return Err(eyre!(
            "refusing to delete: {:?} is not a choros (no {})",
            choros.path,
            META_FILE
        ));
    }
    std::fs::remove_dir_all(&choros.path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn meta_round_trip() {
        let tmp = tempdir();
        let dir = tmp.path().join("PROJ-1");
        std::fs::create_dir_all(&dir).unwrap();
        let meta = ChorosMeta {
            name: "PROJ-1".into(),
            created_at: "2026-05-08T12:00:00Z".into(),
            repos: vec!["a".into(), "b".into()],
        };
        write_meta(&dir, &meta).unwrap();
        let back = read_meta(&dir).unwrap();
        assert_eq!(back.name, "PROJ-1");
        assert_eq!(back.repos, vec!["a", "b"]);
    }

    #[test]
    fn scan_finds_choros() {
        let tmp = tempdir();
        let dir = tmp.path().join("PROJ-1");
        std::fs::create_dir_all(&dir).unwrap();
        write_meta(
            &dir,
            &ChorosMeta {
                name: "PROJ-1".into(),
                created_at: "2026-05-08T12:00:00Z".into(),
                repos: vec!["a".into()],
            },
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("not-a-choros")).unwrap();
        std::fs::create_dir_all(tmp.path().join(".choros-config")).unwrap();

        let found = scan(tmp.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].meta.name, "PROJ-1");
    }

    fn git(args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {:?} failed", args);
    }

    fn make_source_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        git(&["init", "--quiet", path.to_str().unwrap()]);
        git(&["-C", path.to_str().unwrap(), "config", "user.email", "t@t"]);
        git(&["-C", path.to_str().unwrap(), "config", "user.name", "t"]);
        std::fs::write(path.join("hello.txt"), b"hi").unwrap();
        git(&["-C", path.to_str().unwrap(), "add", "."]);
        git(&[
            "-C",
            path.to_str().unwrap(),
            "commit",
            "--quiet",
            "-m",
            "init",
        ]);
    }

    #[test]
    fn end_to_end_create_choros() {
        let tmp = tempdir();
        let root = tmp.path();

        let src_a = root.join("origins/alpha.git-src");
        let src_b = root.join("origins/beta.git-src");
        make_source_repo(&src_a);
        make_source_repo(&src_b);

        let registry = root.join(".choros-config/registry");
        std::fs::create_dir_all(&registry).unwrap();
        git(&[
            "clone",
            "--quiet",
            src_a.to_str().unwrap(),
            registry.join("alpha").to_str().unwrap(),
        ]);
        git(&[
            "clone",
            "--quiet",
            src_b.to_str().unwrap(),
            registry.join("beta").to_str().unwrap(),
        ]);

        let info = create(
            root,
            "PROJ-1",
            &["alpha".to_string(), "beta".to_string()],
            &(),
        )
        .unwrap();

        assert_eq!(info.meta.name, "PROJ-1");
        assert!(root.join("PROJ-1/alpha/.git").exists());
        assert!(root.join("PROJ-1/beta/.git").exists());
        assert!(root.join("PROJ-1/.choros-meta.toml").exists());
        assert!(root.join("PROJ-1/alpha/hello.txt").exists());

        let origin = crate::git::remote_get_url(&root.join("PROJ-1/alpha"), "origin").unwrap();
        assert_eq!(origin, src_a.to_str().unwrap());

        let scanned = scan(root).unwrap();
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].meta.name, "PROJ-1");

        delete(&info).unwrap();
        assert!(!root.join("PROJ-1").exists());
        assert!(scan(root).unwrap().is_empty());
    }

    #[test]
    fn validate_rejects_bad_names() {
        let tmp = tempdir();
        assert!(validate_name(tmp.path(), "").is_err());
        assert!(validate_name(tmp.path(), "a/b").is_err());
        assert!(validate_name(tmp.path(), ".hidden").is_err());
        std::fs::create_dir(tmp.path().join("exists")).unwrap();
        assert!(validate_name(tmp.path(), "exists").is_err());
        assert!(validate_name(tmp.path(), "PROJ-1").is_ok());
    }
}
