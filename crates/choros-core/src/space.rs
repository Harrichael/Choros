//! Trunks and branch-spaces. The trunk is the folder `choros init` ran in,
//! marked by `.choros-config/`; a branch-space is a folder under it, marked by
//! `.choros-branch-config/`, that groups workspaces and further branch-spaces.
//! Both are "spaces": every workspace lives directly in one, and anything a
//! space configures applies to everything under it unless a nearer space
//! says otherwise.

use std::path::{Component, Path, PathBuf};

use color_eyre::eyre::{eyre, Result};

use crate::choros::{self, ChorosInfo, ARCHIVE_REL, CONFIG_DIR, META_FILE};

pub const BRANCH_CONFIG_DIR: &str = ".choros-branch-config";

pub fn is_trunk(dir: &Path) -> bool {
    dir.join(CONFIG_DIR).is_dir()
}

pub fn is_branch_space(dir: &Path) -> bool {
    dir.join(BRANCH_CONFIG_DIR).is_dir()
}

/// The nearest ancestor of `cwd` (itself included) holding `.choros-config`.
pub fn find_trunk(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors().find(|d| is_trunk(d)).map(Path::to_path_buf)
}

/// The space `cwd` is in: the nearest trunk or branch-space at or above it.
/// From inside a workspace, that is the space holding the workspace. Inside
/// `.choros-config` (an archived branch-space, say) there is none: nothing
/// may be made there.
pub fn find_space(cwd: &Path) -> Option<PathBuf> {
    if cwd.components().any(|c| c.as_os_str() == CONFIG_DIR) {
        return None;
    }
    cwd.ancestors()
        .find(|d| is_trunk(d) || is_branch_space(d))
        .map(Path::to_path_buf)
}

/// `dir` below the trunk, `/`-separated; empty for the trunk itself. This is
/// also the git branch prefix of the workspaces in a space.
pub fn rel(trunk: &Path, dir: &Path) -> Result<String> {
    let rel = dir
        .strip_prefix(trunk)
        .map_err(|_| eyre!("{} is not under the trunk {}", dir.display(), trunk.display()))?;
    let parts: Vec<&str> = rel
        .components()
        .map(|c| match c {
            Component::Normal(s) => s.to_str().ok_or_else(|| eyre!("{} is not a UTF-8 path", dir.display())),
            _ => Err(eyre!("unexpected path component in {}", dir.display())),
        })
        .collect::<Result<_>>()?;
    Ok(parts.join("/"))
}

/// `space` and each space above it, nearest first, ending at the trunk.
pub fn chain(space: &Path) -> Result<Vec<PathBuf>> {
    let trunk = find_trunk(space).ok_or_else(|| eyre!("{} is not under a choros trunk", space.display()))?;
    Ok(space
        .ancestors()
        .take_while(|d| d.starts_with(&trunk))
        .filter(|d| is_trunk(d) || is_branch_space(d))
        .map(Path::to_path_buf)
        .collect())
}

/// A space's workspaces and branch-spaces, recursively.
#[derive(Debug)]
pub struct Tree {
    pub path: PathBuf,
    pub workspaces: Vec<ChorosInfo>,
    pub branches: Vec<Tree>,
}

impl Tree {
    /// Every workspace in this space and below it.
    pub fn all_workspaces(&self) -> Vec<&ChorosInfo> {
        let mut out: Vec<&ChorosInfo> = self.workspaces.iter().collect();
        for b in &self.branches {
            out.extend(b.all_workspaces());
        }
        out
    }
}

pub fn tree(space: &Path) -> Result<Tree> {
    let mut branches = Vec::new();
    for entry in std::fs::read_dir(space)?.flatten() {
        let path = entry.path();
        let hidden = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_none_or(|n| n.starts_with('.'));
        if !hidden && path.is_dir() && is_branch_space(&path) && !path.join(META_FILE).exists() {
            branches.push(tree(&path)?);
        }
    }
    branches.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Tree {
        path: space.to_path_buf(),
        workspaces: choros::scan(space)?,
        branches,
    })
}

/// Make the branch-space `name` inside `parent` (the trunk or another
/// branch-space). Its name follows the same rules as a workspace's, since it
/// becomes part of their git branch names.
pub fn create_branch(parent: &Path, name: &str) -> Result<PathBuf> {
    if !(is_trunk(parent) || is_branch_space(parent)) {
        return Err(eyre!("{} is not a trunk or branch-space", parent.display()));
    }
    choros::validate_name(parent, name)?;
    let dir = parent.join(name);
    std::fs::create_dir(&dir)?;
    std::fs::create_dir(dir.join(BRANCH_CONFIG_DIR))?;
    Ok(dir)
}

/// Where something at `dir` under the trunk goes when archived: the same
/// path below `.choros-config/archive/`, so the archive mirrors the tree.
pub fn archive_path(trunk: &Path, dir: &Path) -> Result<PathBuf> {
    Ok(trunk.join(ARCHIVE_REL).join(rel(trunk, dir)?))
}

/// Move the branch-space at `dir`, with everything in it, into the archive.
/// Workspaces archived from it earlier already sit at its archive path, so
/// the two are merged; a name taken on both sides refuses the whole move.
pub fn archive_branch(dir: &Path) -> Result<PathBuf> {
    if !is_branch_space(dir) {
        return Err(eyre!("refusing to archive: {} is not a branch-space", dir.display()));
    }
    let trunk = find_trunk(dir).ok_or_else(|| eyre!("{} is not under a choros trunk", dir.display()))?;
    let dst = archive_path(&trunk, dir)?;
    let mut clashes = Vec::new();
    find_clashes(dir, &dst, &mut clashes);
    if !clashes.is_empty() {
        return Err(eyre!(
            "the archive already holds {}",
            clashes.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        ));
    }
    merge_move(dir, &dst)?;
    Ok(dst)
}

/// A folder of the archive that only groups earlier archives can take more;
/// anything else at the same path is a clash.
fn find_clashes(src: &Path, dst: &Path, out: &mut Vec<PathBuf>) {
    if !dst.exists() {
        return;
    }
    let src_groups = src.is_dir() && !src.join(META_FILE).exists() && !is_config(src);
    let dst_groups = dst.is_dir() && !dst.join(META_FILE).exists() && !is_config(dst);
    if !(src_groups && dst_groups) {
        out.push(dst.to_path_buf());
        return;
    }
    for entry in std::fs::read_dir(src).into_iter().flatten().flatten() {
        if entry.file_name() == BRANCH_CONFIG_DIR {
            continue;
        }
        find_clashes(&entry.path(), &dst.join(entry.file_name()), out);
    }
}

fn is_config(dir: &Path) -> bool {
    dir.file_name().is_some_and(|n| n == BRANCH_CONFIG_DIR)
}

fn merge_move(src: &Path, dst: &Path) -> Result<()> {
    if !dst.exists() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(src, dst)?;
        return Ok(());
    }
    for entry in std::fs::read_dir(src)?.flatten() {
        let to = dst.join(entry.file_name());
        // A branch-space archived before under the same path: the settings
        // it had last are the ones worth keeping.
        if entry.file_name() == BRANCH_CONFIG_DIR && to.exists() {
            std::fs::remove_dir_all(&to)?;
        }
        merge_move(&entry.path(), &to)?;
    }
    std::fs::remove_dir(src)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::choros::{write_meta, ChorosMeta};

    fn trunk() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(CONFIG_DIR)).unwrap();
        tmp
    }

    fn workspace(space: &Path, name: &str) -> PathBuf {
        let dir = space.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        write_meta(
            &dir,
            &ChorosMeta {
                name: name.into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                repos: vec![],
                branch: None,
            },
        )
        .unwrap();
        dir
    }

    /// Branch-spaces nest; the tree walks them, a workspace's space is the
    /// nearest one above it, and settings resolve from there up to the trunk.
    #[test]
    fn spaces_nest_and_resolve_upwards() {
        let tmp = trunk();
        let t = tmp.path();
        let epic = create_branch(t, "epic-x").unwrap();
        let auth = create_branch(&epic, "auth").unwrap();
        workspace(t, "fix-typo");
        workspace(&epic, "fix-login");
        let deep = workspace(&auth, "token");

        assert_eq!(find_space(&deep.join("repo/src")), Some(auth.clone()));
        assert_eq!(find_trunk(&deep), Some(t.to_path_buf()));
        assert_eq!(rel(t, &auth).unwrap(), "epic-x/auth");
        assert_eq!(rel(t, t).unwrap(), "");
        assert_eq!(chain(&auth).unwrap(), [auth.clone(), epic.clone(), t.to_path_buf()]);

        let tree = tree(t).unwrap();
        let names: Vec<&str> = tree.all_workspaces().iter().map(|w| w.meta.name.as_str()).collect();
        assert_eq!(names, ["fix-typo", "fix-login", "token"]);
        assert_eq!(tree.branches.len(), 1);
        assert_eq!(tree.branches[0].branches[0].path, auth);
    }

    /// A workspace and a branch-space in one space can't share a name: both
    /// would claim the same folder and the same git branch prefix.
    #[test]
    fn a_branch_space_cannot_take_a_workspace_name() {
        let tmp = trunk();
        workspace(tmp.path(), "epic-x");
        assert!(create_branch(tmp.path(), "epic-x").is_err());
        assert!(create_branch(tmp.path(), "a/b").is_err());
    }

    /// Archiving a branch-space moves its whole subtree, merging with the
    /// workspaces archived from it earlier.
    #[test]
    fn archiving_a_branch_space_keeps_earlier_archives() {
        let tmp = trunk();
        let t = tmp.path();
        let epic = create_branch(t, "epic-x").unwrap();
        workspace(&epic, "done-early");
        workspace(&epic, "still-open");
        choros::archive(&epic, "done-early").unwrap();

        let dst = archive_branch(&epic).unwrap();
        assert!(!epic.exists());
        assert!(dst.join("done-early").join(META_FILE).exists());
        assert!(dst.join("still-open").join(META_FILE).exists());
        assert!(dst.join(BRANCH_CONFIG_DIR).is_dir());
    }

    #[test]
    fn archiving_refuses_a_clash_and_moves_nothing() {
        let tmp = trunk();
        let t = tmp.path();
        let epic = create_branch(t, "epic-x").unwrap();
        workspace(&epic, "dup");
        choros::archive(&epic, "dup").unwrap();
        workspace(&epic, "dup");
        workspace(&epic, "other");
        assert!(archive_branch(&epic).is_err());
        assert!(epic.join("other").exists());
    }

    /// A branch-space made again under an archived one's name and archived
    /// again keeps its newer settings; and inside the archive nothing counts
    /// as a space, so nothing can be made there.
    #[test]
    fn re_archiving_keeps_the_newer_settings_and_the_archive_is_no_space() {
        let tmp = trunk();
        let t = tmp.path();
        let epic = create_branch(t, "epic-x").unwrap();
        std::fs::write(epic.join(BRANCH_CONFIG_DIR).join("landing.toml"), "old").unwrap();
        let dst = archive_branch(&epic).unwrap();

        let epic = create_branch(t, "epic-x").unwrap();
        std::fs::write(epic.join(BRANCH_CONFIG_DIR).join("landing.toml"), "new").unwrap();
        archive_branch(&epic).unwrap();
        assert_eq!(std::fs::read_to_string(dst.join(BRANCH_CONFIG_DIR).join("landing.toml")).unwrap(), "new");

        assert_eq!(find_space(&dst), None);
        assert_eq!(find_trunk(&dst), Some(t.to_path_buf()));
    }
}
