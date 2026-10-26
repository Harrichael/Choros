//! A branch-space's landing defaults, in `.choros-branch-config/landing.toml`:
//! the queue a new workspace lands into, per repo if need be, and instant
//! mode's route to main. The nearest branch-space that sets something wins.
//! A queue scoped to a branch-space needs none of this: its workspaces land
//! into it anyway. This is for queues shared from further up.
//!
//! ```toml
//! queue = "web"     # new workspaces land into the web queue
//!
//! [bind]
//! docs = "none"     # but docs straight to main, whatever a space above says
//!
//! [instant]
//! via = "pr"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use choros_core::space::{self, BRANCH_CONFIG_DIR};
use color_eyre::eyre::{eyre, Result};
use serde::{Deserialize, Serialize};

use crate::instant::Via;
use crate::queue::Queue;
use crate::state::write_atomic;

pub const LANDING_FILE: &str = "landing.toml";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BranchLanding {
    /// The queue every repo lands into, or `none` for straight to main.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
    /// Repo → queue, or `none`, overriding `queue`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bind: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instant: Option<InstantSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstantSection {
    pub via: Via,
}

fn path(branch: &Path) -> PathBuf {
    branch.join(BRANCH_CONFIG_DIR).join(LANDING_FILE)
}

pub fn read(branch: &Path) -> Result<BranchLanding> {
    let path = path(branch);
    if !path.exists() {
        return Ok(BranchLanding::default());
    }
    Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
}

fn write(branch: &Path, landing: &BranchLanding) -> Result<()> {
    write_atomic(&path(branch), toml::to_string_pretty(landing)?.as_bytes())
}

/// The branch-spaces from `space` up to (not including) the trunk.
fn branches_up(space: &Path) -> Result<Vec<PathBuf>> {
    Ok(space::chain(space)?
        .into_iter()
        .filter(|s| space::is_branch_space(s))
        .collect())
}

/// Where a new workspace in `space` should land `repo`: `Some(Some(queue))`,
/// `Some(None)` for straight to main, `None` when no branch-space says.
pub fn default_binding(space: &Path, repo: &str) -> Result<Option<Option<String>>> {
    for b in branches_up(space)? {
        let landing = read(&b)?;
        if let Some(q) = landing.bind.get(repo).or(landing.queue.as_ref()) {
            return Ok(Some((q != "none").then(|| q.clone())));
        }
    }
    Ok(None)
}

pub fn default_instant(space: &Path) -> Result<Option<Via>> {
    for b in branches_up(space)? {
        if let Some(i) = read(&b)?.instant {
            return Ok(Some(i.via));
        }
    }
    Ok(None)
}

/// Make new workspaces in `branch` land `repo` (every repo, without one)
/// into `queue` (`None`: straight to main). Existing workspaces keep their
/// bindings.
pub fn set_binding(branch: &Path, repo: Option<&str>, queue: Option<&str>) -> Result<()> {
    if !space::is_branch_space(branch) {
        return Err(eyre!("{branch:?} is not a branch-space"));
    }
    let trunk = space::find_trunk(branch).ok_or_else(|| eyre!("{branch:?} is not under a choros trunk"))?;
    if let Some(name) = queue {
        let q = Queue::open(&trunk, name)?;
        if !q.config.scope.admits_space(&space::rel(&trunk, branch)?) {
            return Err(eyre!("queue {name} doesn't take landings from here ({:?})", q.config.scope));
        }
    }
    let mut landing = read(branch)?;
    let queue = queue.unwrap_or("none").to_string();
    match repo {
        Some(repo) => {
            landing.bind.insert(repo.to_string(), queue);
        }
        None => landing.queue = Some(queue),
    }
    write(branch, &landing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use choros_core::choros::CONFIG_DIR;

    /// The nearest branch-space with an opinion decides, including an
    /// explicit `none` that overrides a binding further up.
    #[test]
    fn nearest_branch_space_decides() {
        let tmp = tempfile::tempdir().unwrap();
        let trunk = tmp.path();
        std::fs::create_dir(trunk.join(CONFIG_DIR)).unwrap();
        let epic = space::create_branch(trunk, "epic-x").unwrap();
        let auth = space::create_branch(&epic, "auth").unwrap();
        std::fs::write(
            epic.join(BRANCH_CONFIG_DIR).join(LANDING_FILE),
            "queue = \"web\"\n[bind]\ndocs = \"web-docs\"\n[instant]\nvia = \"pr\"\n",
        )
        .unwrap();
        std::fs::write(auth.join(BRANCH_CONFIG_DIR).join(LANDING_FILE), "[bind]\ndocs = \"none\"\n").unwrap();

        assert_eq!(default_binding(&auth, "ergon").unwrap(), Some(Some("web".into())));
        assert_eq!(default_binding(&auth, "docs").unwrap(), Some(None));
        assert_eq!(default_binding(&epic, "docs").unwrap(), Some(Some("web-docs".into())));
        assert_eq!(default_binding(trunk, "ergon").unwrap(), None);
        assert_eq!(default_instant(&auth).unwrap(), Some(Via::Pr));
    }
}
