use std::path::{Path, PathBuf};
use std::time::SystemTime;

use color_eyre::eyre::{eyre, Context, Result};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::{agent, cache, git, registry, space};

pub trait ProgressSink {
    fn status(&self, msg: String);
}

impl ProgressSink for () {
    fn status(&self, _: String) {}
}

pub const META_FILE: &str = ".choros-meta.toml";
pub const ARCHIVE_REL: &str = ".choros-config/archive";
pub const CONFIG_DIR: &str = ".choros-config";
pub const LANDING_DIR: &str = "landing";
pub const QUEUES_REL: &str = ".choros-config/queues";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChorosMeta {
    pub name: String,
    pub created_at: String,
    pub repos: Vec<String>,
    /// The git branch every clone is on, fixed at creation: the space's path
    /// plus the name (`epic-x/fix-login`). Absent in workspaces made before
    /// branch-spaces, whose branch is their name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

impl ChorosMeta {
    pub fn branch(&self) -> &str {
        self.branch.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Debug, Clone)]
pub struct ChorosInfo {
    pub path: PathBuf,
    pub meta: ChorosMeta,
}

impl ChorosInfo {
    /// Directory the `work` flow should cd into: the single repo if there's
    /// exactly one, otherwise the workspace root.
    pub fn cd_target(&self) -> PathBuf {
        match self.meta.repos.as_slice() {
            [only] => self.path.join(only),
            _ => self.path.clone(),
        }
    }
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

pub fn scan(trunk: &Path) -> Result<Vec<ChorosInfo>> {
    let mut out = Vec::new();
    if !trunk.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(trunk)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if dir_name.starts_with('.') {
            continue;
        }
        let meta_file = meta_path(&path);
        if !meta_file.exists() {
            continue;
        }
        // A corrupt meta file must not make the workspace invisible — that
        // would leave it undeletable from the TUI while still holding the
        // name. Surface it under its directory name instead.
        let meta = read_meta(&path).unwrap_or_else(|_| ChorosMeta {
            name: dir_name.to_string(),
            created_at: "(unreadable meta)".into(),
            repos: Vec::new(),
            branch: None,
        });
        out.push(ChorosInfo { path, meta });
    }
    out.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
    Ok(out)
}

/// When the workspace was last worked in: the newest mtime of any file git
/// doesn't ignore, or of a repo's reflog (commits, checkouts, pulls). Ignored
/// files are skipped so build output doesn't read as work, and `.git/` is
/// skipped because read-only commands like `git status` rewrite the index.
pub fn last_active(workspace: &Path) -> Option<SystemTime> {
    let files = ignore::WalkBuilder::new(workspace)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|e| e.metadata().ok()?.modified().ok());
    let reflogs = std::fs::read_dir(workspace)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            std::fs::metadata(e.path().join(".git/logs/HEAD"))
                .ok()?
                .modified()
                .ok()
        });
    files.chain(reflogs).max()
}

/// Can `name` be a new workspace or branch-space in `space`? Besides being a
/// legal git branch component and a free folder name, a name at the trunk
/// must not be a queue's: queues push `<queue>/landing-N`, which would clash
/// with the branch `<name>` or `<name>/…` on origin.
pub fn validate_name(space: &Path, name: &str) -> Result<()> {
    validate_branch_name(name)?;
    if name == LANDING_DIR {
        return Err(eyre!("'{LANDING_DIR}' is reserved for landed changes"));
    }
    let target = space.join(name);
    if target.exists() {
        return Err(eyre!("'{}' already exists", name));
    }
    if space::is_trunk(space) && space.join(QUEUES_REL).join(name).exists() {
        return Err(eyre!("'{name}' is a landing queue's name"));
    }
    Ok(())
}

/// Validate that `name` is usable as both a directory name and a git branch name.
///
/// Rules follow `git check-ref-format` for a single-component branch name, so the
/// workspace name can be passed directly to `git checkout -b <name>` in every clone.
fn validate_branch_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(eyre!("name cannot be empty"));
    }
    if name == "@" {
        return Err(eyre!("name cannot be '@'"));
    }
    if name.starts_with('.') || name.starts_with('-') {
        return Err(eyre!("name cannot start with '.' or '-'"));
    }
    if name.ends_with('.') || name.ends_with(".lock") {
        return Err(eyre!("name cannot end with '.' or '.lock'"));
    }
    // Slashes/backslashes are rejected outright: choros names also map to a
    // single directory under the project trunk, so we don't allow nested forms.
    if name.contains('/') || name.contains('\\') {
        return Err(eyre!("name cannot contain '/' or '\\\\'"));
    }
    if name.contains("..") || name.contains("@{") {
        return Err(eyre!("name cannot contain '..' or '@{{'"));
    }
    for c in name.chars() {
        let bad = c.is_control()
            || c == ' '
            || c == '~'
            || c == '^'
            || c == ':'
            || c == '?'
            || c == '*'
            || c == '['
            || c == '\\'
            || c == '\x7f';
        if bad {
            return Err(eyre!(
                "name cannot contain spaces or any of: ~ ^ : ? * [ \\ (got `{}`)",
                c
            ));
        }
    }
    Ok(())
}

/// Make workspace `name` in `space` (the trunk or a branch-space).
pub fn create<P: ProgressSink>(
    space: &Path,
    name: &str,
    repos: &[String],
    progress: &P,
) -> Result<ChorosInfo> {
    validate_name(space, name)?;
    let trunk = space::find_trunk(space).ok_or_else(|| eyre!("{} is not under a choros trunk", space.display()))?;
    let prefix = space::rel(&trunk, space)?;
    let branch = if prefix.is_empty() { name.to_string() } else { format!("{prefix}/{name}") };
    let target = space.join(name);
    progress.status(format!("mkdir {}", target.display()));
    // Non-recursive create on purpose: it fails with AlreadyExists if another
    // process claimed the name between validate_name and here, which is what
    // makes the cleanup below safe — we only ever remove a dir we created.
    std::fs::create_dir(&target)?;

    // Meta is written last, so a half-built workspace is invisible to `scan`
    // (unlisted by `choros ls`, unarchivable) yet still blocks the name;
    // remove it rather than strand it.
    create_in(&trunk, name, &branch, repos, progress, &target).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&target);
    })
}

fn create_in<P: ProgressSink>(
    trunk: &Path,
    name: &str,
    branch: &str,
    repos: &[String],
    progress: &P,
    target: &Path,
) -> Result<ChorosInfo> {
    for repo in repos {
        clone_into(trunk, target, branch, repo, progress)?;
    }

    cache::setup_workspace_caches(trunk, target, repos, progress)
        .wrap_err("setting up workspace caches")?;

    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".into());
    let meta = ChorosMeta {
        name: name.to_string(),
        created_at: now,
        repos: repos.to_vec(),
        branch: Some(branch.to_string()),
    };
    write_meta(target, &meta)?;
    progress.status("copying agent templates…".to_string());
    agent::provision(target)?;
    Ok(ChorosInfo {
        path: target.to_path_buf(),
        meta,
    })
}

/// Clone `repos` from the registry into an existing workspace, on its branch,
/// and record them in its meta. All or nothing, like [`create`]: a failure
/// removes the clones this call made and leaves the meta as it was.
pub fn add_repos<P: ProgressSink>(
    space: &Path,
    name: &str,
    repos: &[String],
    progress: &P,
) -> Result<ChorosMeta> {
    let target = space.join(name);
    let trunk = &space::find_trunk(space).ok_or_else(|| eyre!("{} is not under a choros trunk", space.display()))?;
    let mut meta = read_meta(&target)?;
    for (i, repo) in repos.iter().enumerate() {
        registry::ensure_repo_exists(trunk, repo)?;
        if meta.repos.contains(repo) || repos[..i].contains(repo) {
            return Err(eyre!("{repo} is already in workspace {name}"));
        }
        // Not in the meta but on disk: a hand-made clone or stray dir that a
        // rollback must never delete, so refuse before cloning anything.
        if target.join(repo).exists() {
            return Err(eyre!("{} already exists", target.join(repo).display()));
        }
    }

    let mut cloned = Vec::new();
    let result = (|| {
        for repo in repos {
            clone_into(trunk, &target, meta.branch(), repo, progress)?;
            cloned.push(repo);
        }
        cache::setup_workspace_caches(trunk, &target, repos, progress)
            .wrap_err("setting up workspace caches")
    })();
    if let Err(e) = result {
        for repo in cloned {
            let _ = std::fs::remove_dir_all(target.join(repo));
        }
        return Err(e);
    }

    meta.repos.extend(repos.iter().cloned());
    write_meta(&target, &meta)?;
    Ok(meta)
}

fn clone_into<P: ProgressSink>(
    trunk: &Path,
    workspace: &Path,
    branch: &str,
    repo: &str,
    progress: &P,
) -> Result<()> {
    progress.status(format!("cloning {repo}…"));
    let reg_path = registry::ensure_repo_exists(trunk, repo)?;
    let url = git::remote_get_url(&reg_path, "origin")?;
    if let Err(e) = git::fetch(&reg_path, "origin") {
        progress.status(format!("fetch warning ({repo}): {e}; continuing"));
    }
    let dest = workspace.join(repo);
    git::clone_with_reference(&reg_path, &url, &dest)?;
    // Git can't hold `a` and `a/b` at once, so a branch on origin at either
    // end of this one's path would make it unpushable; say so now.
    if let Some(taken) = git::remote_branch_clash(&dest, branch)? {
        return Err(eyre!(
            "{repo}'s origin has a branch {taken:?}, which leaves no room for {branch:?}"
        ));
    }
    git::checkout_new_branch(&dest, branch)
}

pub fn archive_dir(trunk: &Path) -> PathBuf {
    trunk.join(ARCHIVE_REL)
}

/// The (space, name) of the workspace a command is about.
///
/// - If `name` is provided, it is a workspace in the space `cwd` is in.
/// - If `name` is `None`, walk up from `cwd` looking for a directory that
///   contains `META_FILE`. Its basename is the workspace name; its parent is
///   its space.
pub fn resolve_target(cwd: &Path, name: Option<&str>) -> Result<(PathBuf, String)> {
    if let Some(name) = name {
        let space = space::find_space(cwd).unwrap_or_else(|| cwd.to_path_buf());
        return Ok((space, name.to_string()));
    }
    let mut cur = cwd;
    loop {
        if cur.join(META_FILE).exists() {
            let workspace_name = cur
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| eyre!("workspace folder {} has no usable name", cur.display()))?
                .to_string();
            let trunk = cur
                .parent()
                .ok_or_else(|| eyre!("workspace {} has no parent folder", cur.display()))?
                .to_path_buf();
            return Ok((trunk, workspace_name));
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => {
                return Err(eyre!(
                    "not inside a choros workspace (no {} found walking up from {}); pass a name explicitly",
                    META_FILE,
                    cwd.display()
                ));
            }
        }
    }
}

/// The workspace containing `cwd` and the trunk above it, however deep in
/// branch-spaces, or under `<trunk>/landing/<queue>/landing-N`.
pub fn find_workspace(cwd: &Path) -> Result<(PathBuf, PathBuf)> {
    let dir = cwd
        .ancestors()
        .find(|d| d.join(META_FILE).exists())
        .ok_or_else(|| eyre!("not inside a choros workspace (no {META_FILE} above {})", cwd.display()))?;
    let trunk = space::find_trunk(dir)
        .ok_or_else(|| eyre!("workspace {} is not under a choros trunk ({CONFIG_DIR})", dir.display()))?;
    Ok((trunk, dir.to_path_buf()))
}

/// Archive the workspace at `ws`, wherever it sits.
pub fn archive_at(ws: &Path) -> Result<PathBuf> {
    let (Some(space), Some(name)) = (ws.parent(), ws.file_name().and_then(|n| n.to_str())) else {
        return Err(eyre!("{} is not a workspace folder", ws.display()));
    };
    archive(space, name)
}

/// Move workspace `name` out of `space` to the same path under
/// `<trunk>/.choros-config/archive/`. Returns the destination path.
pub fn archive(space: &Path, name: &str) -> Result<PathBuf> {
    let src = space.join(name);
    if !src.join(META_FILE).exists() {
        return Err(eyre!(
            "refusing to archive: {} is not a choros workspace (no {})",
            src.display(),
            META_FILE
        ));
    }
    let trunk = space::find_trunk(space).ok_or_else(|| eyre!("{} is not under a choros trunk", space.display()))?;
    let dst = space::archive_path(&trunk, &src)?;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if dst.exists() {
        return Err(eyre!(
            "the archive already holds a {name}: {}",
            dst.display()
        ));
    }
    std::fs::rename(&src, &dst)?;
    Ok(dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trunk: every workspace lives under one.
    fn tempdir() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(CONFIG_DIR)).unwrap();
        tmp
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
            branch: None,
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
                branch: None,
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
        let trunk = tmp.path();

        let src_a = trunk.join("origins/alpha.git-src");
        let src_b = trunk.join("origins/beta.git-src");
        make_source_repo(&src_a);
        make_source_repo(&src_b);

        let registry = trunk.join(".choros-config/registry");
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
            trunk,
            "PROJ-1",
            &["alpha".to_string(), "beta".to_string()],
            &(),
        )
        .unwrap();

        assert_eq!(info.meta.name, "PROJ-1");
        assert!(trunk.join("PROJ-1/alpha/.git").exists());
        assert!(trunk.join("PROJ-1/beta/.git").exists());
        assert!(trunk.join("PROJ-1/.choros-meta.toml").exists());
        assert!(trunk.join("PROJ-1/alpha/hello.txt").exists());

        let origin = crate::git::remote_get_url(&trunk.join("PROJ-1/alpha"), "origin").unwrap();
        assert_eq!(origin, src_a.to_str().unwrap());

        let scanned = scan(trunk).unwrap();
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].meta.name, "PROJ-1");

        archive(trunk, &info.meta.name).unwrap();
        assert!(!trunk.join("PROJ-1").exists());
        assert!(scan(trunk).unwrap().is_empty());
    }

    /// A repo added to a live workspace lands like one it was created with:
    /// cloned from the registry onto the workspace's branch and listed in its
    /// meta. Asking for one it already has, or one the registry lacks, changes
    /// nothing.
    #[test]
    fn add_repos_joins_an_existing_workspace() {
        let tmp = tempdir();
        let trunk = tmp.path();
        let registry = trunk.join(".choros-config/registry");
        for name in ["alpha", "beta"] {
            let src = trunk.join(format!("origins/{name}.git-src"));
            make_source_repo(&src);
            git(&[
                "clone",
                "--quiet",
                src.to_str().unwrap(),
                registry.join(name).to_str().unwrap(),
            ]);
        }
        create(trunk, "PROJ-1", &["alpha".to_string()], &()).unwrap();

        let meta = add_repos(trunk, "PROJ-1", &["beta".to_string()], &()).unwrap();
        assert_eq!(meta.repos, ["alpha", "beta"]);
        assert_eq!(
            read_meta(&trunk.join("PROJ-1")).unwrap().repos,
            ["alpha", "beta"]
        );
        assert!(trunk.join("PROJ-1/beta/hello.txt").exists());
        let head = std::process::Command::new("git")
            .args(["-C", trunk.join("PROJ-1/beta").to_str().unwrap()])
            .args(["branch", "--show-current"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "PROJ-1");

        assert!(add_repos(trunk, "PROJ-1", &["alpha".to_string()], &()).is_err());
        assert!(add_repos(trunk, "PROJ-1", &["gamma".to_string()], &()).is_err());
        assert_eq!(
            read_meta(&trunk.join("PROJ-1")).unwrap().repos,
            ["alpha", "beta"]
        );
        assert!(trunk.join("PROJ-1/alpha/hello.txt").exists());
    }

    #[test]
    fn scan_surfaces_corrupt_meta_under_dir_name() {
        let tmp = tempdir();
        let dir = tmp.path().join("PROJ-BAD");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(META_FILE), b"not = [valid toml").unwrap();

        let found = scan(tmp.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].meta.name, "PROJ-BAD");
        assert!(found[0].meta.repos.is_empty());
        // Still archivable despite the unreadable meta.
        archive(tmp.path(), &found[0].meta.name).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn validate_rejects_bad_names() {
        let tmp = tempdir();
        // Original cases.
        assert!(validate_name(tmp.path(), "").is_err());
        assert!(validate_name(tmp.path(), "a/b").is_err());
        assert!(validate_name(tmp.path(), ".hidden").is_err());
        std::fs::create_dir(tmp.path().join("exists")).unwrap();
        assert!(validate_name(tmp.path(), "exists").is_err());
        assert!(validate_name(tmp.path(), "PROJ-1").is_ok());

        // Branch-name rules: must also be a legal git branch.
        for bad in [
            "has space",
            "tilde~name",
            "caret^name",
            "colon:name",
            "ques?",
            "star*",
            "bracket[",
            "back\\slash",
            "ends.",
            "ends.lock",
            "double..dot",
            "ref@{0}",
            "@",
            "-leading",
            "ctrl\u{0001}char",
        ] {
            assert!(
                validate_name(tmp.path(), bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }

        // Reasonable names still pass.
        for good in ["PROJ-1234", "feature_x", "fix.thing", "v1.2.3"] {
            assert!(
                validate_name(tmp.path(), good).is_ok(),
                "expected {good:?} to be accepted"
            );
        }
    }

    fn make_choros(trunk: &Path, name: &str) {
        let dir = trunk.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        write_meta(
            &dir,
            &ChorosMeta {
                name: name.into(),
                created_at: "2026-05-09T00:00:00Z".into(),
                repos: vec![],
                branch: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn archive_moves_to_archive_dir() {
        let tmp = tempdir();
        let trunk = tmp.path();
        make_choros(trunk, "PROJ-1");

        let dst = archive(trunk, "PROJ-1").unwrap();
        assert!(!trunk.join("PROJ-1").exists());
        assert_eq!(dst, trunk.join(".choros-config/archive/PROJ-1"));
        assert!(dst.join(META_FILE).exists());
        assert!(scan(trunk).unwrap().is_empty());
    }

    #[test]
    fn archive_rejects_non_choros() {
        let tmp = tempdir();
        let trunk = tmp.path();
        std::fs::create_dir_all(trunk.join("just-a-dir")).unwrap();
        assert!(archive(trunk, "just-a-dir").is_err());
        assert!(archive(trunk, "does-not-exist").is_err());
    }

    #[test]
    fn archive_rejects_collision() {
        let tmp = tempdir();
        let trunk = tmp.path();
        make_choros(trunk, "PROJ-1");
        std::fs::create_dir_all(trunk.join(".choros-config/archive/PROJ-1")).unwrap();

        let err = archive(trunk, "PROJ-1").unwrap_err();
        assert!(err.to_string().contains("the archive already holds a"));
        // Source must not have been moved.
        assert!(trunk.join("PROJ-1").exists());
    }

    #[test]
    fn resolve_target_walks_up_from_cwd() {
        let tmp = tempdir();
        let trunk = tmp.path();
        make_choros(trunk, "PROJ-7");
        let nested = trunk.join("PROJ-7/some/nested/path");
        std::fs::create_dir_all(&nested).unwrap();

        let (resolved_trunk, name) = resolve_target(&nested, None).unwrap();
        assert_eq!(name, "PROJ-7");
        // Compare canonical paths — tempdirs on macOS may include /private prefix.
        assert_eq!(
            resolved_trunk.canonicalize().unwrap(),
            trunk.canonicalize().unwrap()
        );
    }

    #[test]
    fn resolve_target_uses_explicit_name() {
        let tmp = tempdir();
        let (resolved_trunk, name) =
            resolve_target(tmp.path(), Some("PROJ-9")).unwrap();
        assert_eq!(name, "PROJ-9");
        assert_eq!(resolved_trunk, tmp.path());
    }

    #[test]
    fn resolve_target_errors_outside_workspace() {
        let tmp = tempdir();
        // No .choros-meta.toml anywhere up the chain.
        let err = resolve_target(tmp.path(), None).unwrap_err();
        assert!(err.to_string().contains("not inside a choros workspace"));
    }

    #[test]
    fn failed_create_removes_partial_workspace() {
        let tmp = tempdir();
        let trunk = tmp.path();

        // Two registry repos: alpha clones fine, broken's origin points at
        // nothing, so create fails after alpha was already cloned.
        stage_alpha_registry(trunk);
        let broken_src = trunk.join("origins/broken.git-src");
        make_source_repo(&broken_src);
        let registry = trunk.join(".choros-config/registry");
        git(&[
            "clone",
            "--quiet",
            broken_src.to_str().unwrap(),
            registry.join("broken").to_str().unwrap(),
        ]);
        git(&[
            "-C",
            registry.join("broken").to_str().unwrap(),
            "remote",
            "set-url",
            "origin",
            trunk.join("origins/does-not-exist").to_str().unwrap(),
        ]);

        let err = create(
            trunk,
            "PROJ-F",
            &["alpha".to_string(), "broken".to_string()],
            &(),
        );
        assert!(err.is_err());
        assert!(
            !trunk.join("PROJ-F").exists(),
            "partial workspace should be cleaned up"
        );
        // The name is immediately reusable.
        assert!(validate_name(trunk, "PROJ-F").is_ok());
        assert!(scan(trunk).unwrap().is_empty());
    }

    #[test]
    fn create_drops_choros_archive_skill() {
        let tmp = tempdir();
        let trunk = tmp.path();

        let src = trunk.join("origins/alpha.git-src");
        make_source_repo(&src);

        let registry = trunk.join(".choros-config/registry");
        std::fs::create_dir_all(&registry).unwrap();
        git(&[
            "clone",
            "--quiet",
            src.to_str().unwrap(),
            registry.join("alpha").to_str().unwrap(),
        ]);

        create(trunk, "PROJ-SK", &["alpha".to_string()], &()).unwrap();

        let skill = trunk.join("PROJ-SK/.claude/skills/choros-archive/SKILL.md");
        assert!(skill.exists(), "expected skill at {:?}", skill);
        let body = std::fs::read_to_string(&skill).unwrap();
        assert!(
            body.starts_with("---\nname: choros-archive"),
            "skill content unexpected: {:?}",
            &body[..body.len().min(80)]
        );
    }

    #[test]
    fn create_checks_out_branch_named_after_choros() {
        let tmp = tempdir();
        let trunk = tmp.path();

        let src = trunk.join("origins/alpha.git-src");
        make_source_repo(&src);

        let registry = trunk.join(".choros-config/registry");
        std::fs::create_dir_all(&registry).unwrap();
        git(&[
            "clone",
            "--quiet",
            src.to_str().unwrap(),
            registry.join("alpha").to_str().unwrap(),
        ]);

        create(trunk, "PROJ-42", &["alpha".to_string()], &()).unwrap();

        let head = std::process::Command::new("git")
            .arg("-C")
            .arg(trunk.join("PROJ-42/alpha"))
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert!(head.status.success());
        let branch = String::from_utf8_lossy(&head.stdout).trim().to_string();
        assert_eq!(branch, "PROJ-42");
    }

    fn stage_alpha_registry(trunk: &Path) {
        let src = trunk.join("origins/alpha.git-src");
        make_source_repo(&src);
        let registry = trunk.join(".choros-config/registry");
        std::fs::create_dir_all(&registry).unwrap();
        git(&[
            "clone",
            "--quiet",
            src.to_str().unwrap(),
            registry.join("alpha").to_str().unwrap(),
        ]);
    }

    #[test]
    fn create_copies_templates_into_workspace() {
        let tmp = tempdir();
        let trunk = tmp.path();
        stage_alpha_registry(trunk);

        let claude = trunk.join(".choros-config/templates/.claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            br#"{"permissions":{"allow":["Bash(ls:*)"]}}"#,
        )
        .unwrap();
        let cursor = trunk.join(".choros-config/templates/.cursor");
        std::fs::create_dir_all(&cursor).unwrap();
        std::fs::write(
            cursor.join("cli.json"),
            br#"{"permissions":{"allow":["Shell(git)"],"deny":[]}}"#,
        )
        .unwrap();
        // A file that isn't a known agent template should be ignored.
        let stray = trunk.join(".choros-config/templates/.cursor/rules");
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::write(stray.join("rules.md"), b"stray").unwrap();

        create(trunk, "PROJ-T", &["alpha".to_string()], &()).unwrap();

        let settings = trunk.join("PROJ-T/.claude/settings.json");
        assert!(settings.exists(), "expected settings at {:?}", settings);
        assert!(std::fs::read_to_string(&settings).unwrap().contains("Bash(ls:*)"));

        let cursor_dest = trunk.join("PROJ-T/.cursor/cli.json");
        assert!(cursor_dest.exists(), "expected {:?}", cursor_dest);
        assert!(std::fs::read_to_string(&cursor_dest).unwrap().contains("Shell(git)"));

        // Stray non-agent file is not copied.
        assert!(!trunk.join("PROJ-T/.cursor/rules/rules.md").exists());

        assert!(trunk
            .join("PROJ-T/.claude/skills/choros-archive/SKILL.md")
            .exists());
    }

    #[test]
    fn create_without_templates_is_ok() {
        let tmp = tempdir();
        let trunk = tmp.path();
        stage_alpha_registry(trunk);
        assert!(!trunk.join(".choros-config/templates").exists());

        let info = create(trunk, "PROJ-N", &["alpha".to_string()], &()).unwrap();
        assert_eq!(info.meta.name, "PROJ-N");
        assert!(trunk.join("PROJ-N/.choros-meta.toml").exists());
    }


    fn head_branch(repo: &Path) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A workspace in a branch-space gets the space's path as its branch
    /// prefix, records it, and keeps it for repos added later; templates come
    /// from the nearest space that has them. A trunk workspace is unchanged.
    #[test]
    fn workspaces_in_a_branch_space_branch_under_its_path() {
        let tmp = tempdir();
        let trunk = tmp.path();
        stage_alpha_registry(trunk);
        let src_b = trunk.join("origins/beta.git-src");
        make_source_repo(&src_b);
        git(&["clone", "--quiet", src_b.to_str().unwrap(),
              trunk.join(".choros-config/registry/beta").to_str().unwrap()]);
        let trunk_tpl = trunk.join(".choros-config/templates/.claude");
        std::fs::create_dir_all(&trunk_tpl).unwrap();
        std::fs::write(trunk_tpl.join("settings.json"), b"trunk").unwrap();

        let epic = space::create_branch(trunk, "epic-x").unwrap();
        let auth = space::create_branch(&epic, "auth").unwrap();
        let epic_tpl = epic.join(".choros-branch-config/templates/.claude");
        std::fs::create_dir_all(&epic_tpl).unwrap();
        std::fs::write(epic_tpl.join("settings.json"), b"epic").unwrap();

        let info = create(&auth, "login", &["alpha".to_string()], &()).unwrap();
        assert_eq!(info.meta.branch(), "epic-x/auth/login");
        assert_eq!(head_branch(&auth.join("login/alpha")), "epic-x/auth/login");
        assert_eq!(
            std::fs::read_to_string(auth.join("login/.claude/settings.json")).unwrap(),
            "epic"
        );

        add_repos(&auth, "login", &["beta".to_string()], &()).unwrap();
        assert_eq!(head_branch(&auth.join("login/beta")), "epic-x/auth/login");

        let plain = create(trunk, "typo", &["alpha".to_string()], &()).unwrap();
        assert_eq!(plain.meta.branch(), "typo");
        assert_eq!(
            std::fs::read_to_string(trunk.join("typo/.claude/settings.json")).unwrap(),
            "trunk"
        );
    }

    /// Names that would collide on origin are refused before anything is
    /// made: a queue's name at the trunk, and a branch on origin at either
    /// end of the new branch's path.
    #[test]
    fn names_that_would_clash_on_origin_are_refused() {
        let tmp = tempdir();
        let trunk = tmp.path();
        stage_alpha_registry(trunk);
        std::fs::create_dir_all(trunk.join(QUEUES_REL).join("web")).unwrap();
        assert!(validate_name(trunk, "web").is_err());
        assert!(space::create_branch(trunk, "web").is_err());

        let src = trunk.join("origins/alpha.git-src");
        git(&["-C", src.to_str().unwrap(), "branch", "epic-x"]);
        git(&["-C", trunk.join(".choros-config/registry/alpha").to_str().unwrap(), "fetch", "--quiet", "origin"]);
        let epic = space::create_branch(trunk, "epic-x").unwrap();
        let err = create(&epic, "login", &["alpha".to_string()], &()).unwrap_err();
        assert!(err.to_string().contains("\"epic-x\""), "{err}");
        assert!(!epic.join("login").exists());
    }
}
