use std::path::{Path, PathBuf};

use color_eyre::eyre::{eyre, Context, Result};
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
pub const ARCHIVE_REL: &str = ".choros-config/archive";
pub const TEMPLATES_REL: &str = ".choros-config/templates";

pub const CHOROS_ARCHIVE_SKILL: &str = include_str!("skills/choros-archive.md");
pub const CHOROS_DEFAULT_CLAUDE_SETTINGS: &str =
    include_str!("templates/claude-settings.json");

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
    validate_branch_name(name)?;
    let target = root.join(name);
    if target.exists() {
        return Err(eyre!("'{}' already exists", name));
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
    // single directory under the project root, so we don't allow nested forms.
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
                "name cannot contain spaces or any of: ~ ^ : ? * [ \\ (got {:?})",
                c
            ));
        }
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
        git::checkout_new_branch(&dest, name)?;
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
    write_skill(&target)?;
    progress.status("copying templates…".to_string());
    copy_templates(&templates_dir(root), &target)
        .wrap_err("copying .choros-config/templates into workspace")?;
    Ok(ChorosInfo {
        path: target,
        meta,
    })
}

fn write_skill(choros_dir: &Path) -> Result<()> {
    let skill_dir = choros_dir.join(".claude/skills/choros-archive");
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(skill_dir.join("SKILL.md"), CHOROS_ARCHIVE_SKILL)?;
    Ok(())
}

pub fn archive_dir(root: &Path) -> PathBuf {
    root.join(ARCHIVE_REL)
}

pub fn templates_dir(root: &Path) -> PathBuf {
    root.join(TEMPLATES_REL)
}

pub fn init_templates(root: &Path) -> Result<PathBuf> {
    let dir = templates_dir(root);
    let claude_dir = dir.join(".claude");
    std::fs::create_dir_all(&claude_dir)?;
    let settings = claude_dir.join("settings.json");
    if !settings.exists() {
        std::fs::write(&settings, CHOROS_DEFAULT_CLAUDE_SETTINGS)?;
    }
    Ok(dir)
}

fn copy_templates(src: &Path, dest: &Path) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    copy_tree_inner(src, dest)
}

fn copy_tree_inner(src: &Path, dest: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_tree_inner(&from, &to)?;
        } else {
            if to.exists() {
                return Err(eyre!(
                    "template collision: {:?} already exists in workspace; refusing to overwrite",
                    to
                ));
            }
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Resolve the (root, name) pair for an archive call.
///
/// - If `name` is provided, treat `cwd` as the choros root.
/// - If `name` is `None`, walk up from `cwd` looking for a directory that
///   contains `META_FILE`. Its basename is the workspace name; its parent is
///   the root.
pub fn resolve_target(cwd: &Path, name: Option<&str>) -> Result<(PathBuf, String)> {
    if let Some(name) = name {
        return Ok((cwd.to_path_buf(), name.to_string()));
    }
    let mut cur = cwd;
    loop {
        if cur.join(META_FILE).exists() {
            let workspace_name = cur
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| eyre!("workspace dir has no usable name: {:?}", cur))?
                .to_string();
            let root = cur
                .parent()
                .ok_or_else(|| eyre!("workspace has no parent dir: {:?}", cur))?
                .to_path_buf();
            return Ok((root, workspace_name));
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => {
                return Err(eyre!(
                    "not inside a choros workspace (no {} found walking up from {:?}); pass a name explicitly",
                    META_FILE,
                    cwd
                ));
            }
        }
    }
}

/// Move `<root>/<name>/` into `<root>/.choros-config/archive/<name>/`.
/// Returns the destination path.
pub fn archive(root: &Path, name: &str) -> Result<PathBuf> {
    let src = root.join(name);
    if !src.join(META_FILE).exists() {
        return Err(eyre!(
            "refusing to archive: {:?} is not a choros (no {})",
            src,
            META_FILE
        ));
    }
    let archive_root = archive_dir(root);
    std::fs::create_dir_all(&archive_root)?;
    let dst = archive_root.join(name);
    if dst.exists() {
        return Err(eyre!(
            "archive already contains '{}': {:?}",
            name,
            dst
        ));
    }
    std::fs::rename(&src, &dst)?;
    Ok(dst)
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

    fn make_choros(root: &Path, name: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        write_meta(
            &dir,
            &ChorosMeta {
                name: name.into(),
                created_at: "2026-05-09T00:00:00Z".into(),
                repos: vec![],
            },
        )
        .unwrap();
    }

    #[test]
    fn archive_moves_to_archive_dir() {
        let tmp = tempdir();
        let root = tmp.path();
        make_choros(root, "PROJ-1");

        let dst = archive(root, "PROJ-1").unwrap();
        assert!(!root.join("PROJ-1").exists());
        assert_eq!(dst, root.join(".choros-config/archive/PROJ-1"));
        assert!(dst.join(META_FILE).exists());
        assert!(scan(root).unwrap().is_empty());
    }

    #[test]
    fn archive_rejects_non_choros() {
        let tmp = tempdir();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("just-a-dir")).unwrap();
        assert!(archive(root, "just-a-dir").is_err());
        assert!(archive(root, "does-not-exist").is_err());
    }

    #[test]
    fn archive_rejects_collision() {
        let tmp = tempdir();
        let root = tmp.path();
        make_choros(root, "PROJ-1");
        std::fs::create_dir_all(root.join(".choros-config/archive/PROJ-1")).unwrap();

        let err = archive(root, "PROJ-1").unwrap_err();
        assert!(err.to_string().contains("archive already contains"));
        // Source must not have been moved.
        assert!(root.join("PROJ-1").exists());
    }

    #[test]
    fn resolve_target_walks_up_from_cwd() {
        let tmp = tempdir();
        let root = tmp.path();
        make_choros(root, "PROJ-7");
        let nested = root.join("PROJ-7/some/nested/path");
        std::fs::create_dir_all(&nested).unwrap();

        let (resolved_root, name) = resolve_target(&nested, None).unwrap();
        assert_eq!(name, "PROJ-7");
        // Compare canonical paths — tempdirs on macOS may include /private prefix.
        assert_eq!(
            resolved_root.canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[test]
    fn resolve_target_uses_explicit_name() {
        let tmp = tempdir();
        let (resolved_root, name) =
            resolve_target(tmp.path(), Some("PROJ-9")).unwrap();
        assert_eq!(name, "PROJ-9");
        assert_eq!(resolved_root, tmp.path());
    }

    #[test]
    fn resolve_target_errors_outside_workspace() {
        let tmp = tempdir();
        // No .choros-meta.toml anywhere up the chain.
        let err = resolve_target(tmp.path(), None).unwrap_err();
        assert!(err.to_string().contains("not inside a choros workspace"));
    }

    #[test]
    fn create_drops_choros_archive_skill() {
        let tmp = tempdir();
        let root = tmp.path();

        let src = root.join("origins/alpha.git-src");
        make_source_repo(&src);

        let registry = root.join(".choros-config/registry");
        std::fs::create_dir_all(&registry).unwrap();
        git(&[
            "clone",
            "--quiet",
            src.to_str().unwrap(),
            registry.join("alpha").to_str().unwrap(),
        ]);

        create(root, "PROJ-SK", &["alpha".to_string()], &()).unwrap();

        let skill = root.join("PROJ-SK/.claude/skills/choros-archive/SKILL.md");
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
        let root = tmp.path();

        let src = root.join("origins/alpha.git-src");
        make_source_repo(&src);

        let registry = root.join(".choros-config/registry");
        std::fs::create_dir_all(&registry).unwrap();
        git(&[
            "clone",
            "--quiet",
            src.to_str().unwrap(),
            registry.join("alpha").to_str().unwrap(),
        ]);

        create(root, "PROJ-42", &["alpha".to_string()], &()).unwrap();

        let head = std::process::Command::new("git")
            .arg("-C")
            .arg(root.join("PROJ-42/alpha"))
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert!(head.status.success());
        let branch = String::from_utf8_lossy(&head.stdout).trim().to_string();
        assert_eq!(branch, "PROJ-42");
    }

    fn stage_alpha_registry(root: &Path) {
        let src = root.join("origins/alpha.git-src");
        make_source_repo(&src);
        let registry = root.join(".choros-config/registry");
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
        let root = tmp.path();
        stage_alpha_registry(root);

        let tdir = root.join(".choros-config/templates/.claude");
        std::fs::create_dir_all(&tdir).unwrap();
        std::fs::write(
            tdir.join("settings.json"),
            br#"{"permissions":{"allow":["Bash(ls:*)"]}}"#,
        )
        .unwrap();
        let nested = root.join(".choros-config/templates/.cursor/rules");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("rules.md"), b"my cursor rules").unwrap();

        create(root, "PROJ-T", &["alpha".to_string()], &()).unwrap();

        let settings = root.join("PROJ-T/.claude/settings.json");
        assert!(settings.exists(), "expected settings at {:?}", settings);
        let body = std::fs::read_to_string(&settings).unwrap();
        assert!(body.contains("Bash(ls:*)"), "got: {body}");

        let cursor = root.join("PROJ-T/.cursor/rules/rules.md");
        assert!(cursor.exists(), "expected cursor file at {:?}", cursor);
        assert_eq!(std::fs::read_to_string(&cursor).unwrap(), "my cursor rules");

        assert!(root
            .join("PROJ-T/.claude/skills/choros-archive/SKILL.md")
            .exists());
    }

    #[test]
    fn create_without_templates_is_ok() {
        let tmp = tempdir();
        let root = tmp.path();
        stage_alpha_registry(root);
        assert!(!root.join(".choros-config/templates").exists());

        let info = create(root, "PROJ-N", &["alpha".to_string()], &()).unwrap();
        assert_eq!(info.meta.name, "PROJ-N");
        assert!(root.join("PROJ-N/.choros-meta.toml").exists());
    }

    #[test]
    fn create_errors_on_template_collision() {
        let tmp = tempdir();
        let root = tmp.path();
        stage_alpha_registry(root);

        let tskill = root.join(".choros-config/templates/.claude/skills/choros-archive");
        std::fs::create_dir_all(&tskill).unwrap();
        std::fs::write(tskill.join("SKILL.md"), b"hijacked").unwrap();

        let err = create(root, "PROJ-C", &["alpha".to_string()], &()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("template collision"), "got: {msg}");

        let skill = root.join("PROJ-C/.claude/skills/choros-archive/SKILL.md");
        let body = std::fs::read_to_string(&skill).unwrap();
        assert!(
            body.starts_with("---\nname: choros-archive"),
            "skill content unexpected: {:?}",
            &body[..body.len().min(80)]
        );
    }

    #[test]
    fn init_templates_preserves_existing_settings() {
        let tmp = tempdir();
        let root = tmp.path();
        init_templates(root).unwrap();
        let settings = root.join(".choros-config/templates/.claude/settings.json");
        std::fs::write(&settings, b"user-edited").unwrap();
        init_templates(root).unwrap();
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "user-edited");
    }
}
