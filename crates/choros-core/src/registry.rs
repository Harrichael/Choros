use std::path::{Path, PathBuf};

use color_eyre::eyre::{eyre, Result};

use crate::git;

const REGISTRY_REL: &str = ".choros-config/registry";

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
    // Repo args double as clone destinations inside the workspace; a path-ish
    // name (`../x`, `/abs`) would escape both the registry and the workspace.
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.starts_with('.') {
        return Err(eyre!("invalid repo name {:?}", name));
    }
    let path = repo_path(root, name);
    if !path.exists() {
        return Err(eyre!("registry repo {:?} does not exist", path));
    }
    Ok(path)
}

/// Parse an SSH remote (`ssh://[user@]host[:port]/path` or the scp-like
/// `[user@]host:path` shorthand) and derive the bare repo name from its final
/// path segment. Registry entries only ever come from SSH remotes: they're
/// cloned once here and then every workspace clone references this copy via
/// `git clone --reference-if-able`, which needs a stable local source, not an
/// HTTP(S) endpoint.
fn repo_name_from_ssh_url(url: &str) -> Result<String> {
    // A leading `-` would let git's arg parser read the url as an option
    // instead of a positional url; reject outright rather than relying only
    // on git.rs's `--` separator.
    if url.starts_with('-') {
        return Err(eyre!("url cannot start with '-', got {:?}", url));
    }
    let path = if let Some(rest) = url.strip_prefix("ssh://") {
        rest.split_once('/')
            .map(|(_, path)| path)
            .ok_or_else(|| eyre!("not a valid ssh:// URL: {:?}", url))?
    } else if url.contains("://") {
        return Err(eyre!("only ssh URLs are supported, got {:?}", url));
    } else {
        // scp-like shorthand (`git@host:org/repo.git`) only counts if the
        // colon comes before any slash — otherwise this is a local path that
        // happens to contain a colon further in, not a remote.
        let colon = url.find(':');
        let slash = url.find('/');
        match (colon, slash) {
            (Some(c), slash) if slash.is_none_or(|s| c < s) => &url[c + 1..],
            _ => {
                return Err(eyre!(
                    "only ssh URLs are supported (scp-like `git@host:org/repo.git` or `ssh://...`), got {:?}",
                    url
                ))
            }
        }
    };

    let path = path.strip_suffix('/').unwrap_or(path);
    let name = path.rsplit('/').next().unwrap_or(path);
    let name = name.strip_suffix(".git").unwrap_or(name);
    if name.is_empty() || name == "." || name == ".." || name.starts_with('.') {
        return Err(eyre!("could not derive a usable repo name from {:?}", url));
    }
    Ok(name.to_string())
}

/// Clone `url` (an SSH remote) into the registry, deriving the directory name
/// from the URL. Returns the derived name and its path under the registry.
pub fn add(root: &Path, url: &str) -> Result<(String, PathBuf)> {
    let name = repo_name_from_ssh_url(url)?;
    init_dirs(root)?;
    let dest = repo_path(root, &name);
    if dest.exists() {
        return Err(eyre!("'{}' already exists in the registry", name));
    }
    git::clone(url, &dest)?;
    Ok((name, dest))
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

    #[test]
    fn ensure_repo_exists_rejects_path_like_names() {
        let tmp = tempdir();
        let reg = registry_dir(tmp.path());
        fake_repo(&reg, "alpha");

        assert!(ensure_repo_exists(tmp.path(), "alpha").is_ok());
        for bad in ["../alpha", "/etc", "a/b", "..", ".hidden", ""] {
            assert!(
                ensure_repo_exists(tmp.path(), bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn repo_name_from_ssh_url_accepts_valid_forms() {
        let cases = [
            ("git@github.com:org/repo.git", "repo"),
            ("host:repo.git", "repo"),
            ("git@github.com:org/nested/repo", "repo"),
            ("ssh://git@github.com/org/repo.git", "repo"),
            ("ssh://git@github.com:2222/org/repo.git", "repo"),
            ("ssh://host/repo.git", "repo"),
            ("git@github.com:org/repo.git/", "repo"),
        ];
        for (url, expected) in cases {
            assert_eq!(
                repo_name_from_ssh_url(url).unwrap(),
                expected,
                "for url {url:?}"
            );
        }
    }

    #[test]
    fn repo_name_from_ssh_url_rejects_non_ssh_and_bad_names() {
        let cases = [
            "https://github.com/org/repo.git",
            "http://github.com/org/repo.git",
            "git://github.com/org/repo.git",
            "file:///abs/path/repo.git",
            "./weird:name",
            "/abs/local/path",
            "relative/local/path",
            "git@github.com:org/..",
            "git@github.com:org/.",
            "git@github.com:org/.hidden",
            "git@github.com:",
            "--upload-pack=touch /tmp/pwned;",
            "-oProxyCommand=touch /tmp/pwned",
        ];
        for url in cases {
            assert!(
                repo_name_from_ssh_url(url).is_err(),
                "expected {url:?} to be rejected"
            );
        }
    }

    #[test]
    fn add_rejects_non_ssh_url_without_touching_registry() {
        let tmp = tempdir();
        let root = tmp.path();
        let err = add(root, "https://github.com/org/repo.git").unwrap_err();
        assert!(err.to_string().contains("ssh"), "got: {err}");
        assert!(!registry_dir(root).join("repo").exists());
    }

    #[test]
    fn add_rejects_duplicate_name() {
        let tmp = tempdir();
        let root = tmp.path();
        let reg = registry_dir(root);
        fake_repo(&reg, "repo");

        let err = add(root, "git@github.com:org/repo.git").unwrap_err();
        assert!(err.to_string().contains("already exists"), "got: {err}");
        // Pre-existing fake repo must be untouched, not overwritten by a clone attempt.
        assert!(reg.join("repo/.git").exists());
    }
}
