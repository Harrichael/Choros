//! A workspace's landing bookkeeping, in `.choros-landing.toml` beside its
//! `.choros-meta.toml`: which queue each repo was bound to, what each repo's
//! branch builds on, and how far it has been landed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use choros_core::space;
use color_eyre::eyre::Result;
use serde::{Deserialize, Serialize};

use crate::queue::{self, Queue};
use crate::state::{Scope, write_atomic};

pub const LANDING_FILE: &str = ".choros-landing.toml";

/// A repo bound to this instead of a queue lands straight to main.
pub const NO_QUEUE: &str = "none";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceLanding {
    #[serde(default)]
    pub repo: BTreeMap<String, RepoLanding>,
    /// Set when this workspace is itself a landing folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landing: Option<LandingOf>,
    /// Instant mode's route to main for this workspace, overriding the trunk's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instant: Option<crate::instant::Via>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoLanding {
    /// The queue it was bound to, or [`NO_QUEUE`]. Absent: whichever queue
    /// is scoped to the workspace (see [`queue_for`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
    /// Everything up to here has been landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed_through: Option<String>,
    /// The merge head content `base` had (`Composition::key`), so the prompt
    /// can tell "needs sync" from files alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition: Option<String>,
    /// Set while the workspace is being rebased onto this commit (a sync, or
    /// a fork's base). Once HEAD contains it with no rebase in progress, it
    /// becomes `landed_through`; a conflict resolved by hand therefore
    /// finishes the sync rather than looking like rewritten history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_composition: Option<String>,
    /// Sets a fork land moved this repo onto: the commits now build on them,
    /// so the fork must keep depending on them when the land is re-run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fork_deps: Vec<u32>,
    /// Unix seconds of the last land, so a raw-terminal land forks only the
    /// sessions active since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_landed: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LandingOf {
    pub queue: String,
    pub n: u32,
    pub parent: PathBuf,
}

pub fn read(workspace: &Path) -> Result<WorkspaceLanding> {
    let path = workspace.join(LANDING_FILE);
    if !path.exists() {
        return Ok(WorkspaceLanding::default());
    }
    Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
}

pub fn write(workspace: &Path, landing: &WorkspaceLanding) -> Result<()> {
    write_atomic(
        &workspace.join(LANDING_FILE),
        toml::to_string_pretty(landing)?.as_bytes(),
    )
}

/// The queue `repo` of workspace `ws` lands into: the one it was bound to;
/// else the queue scoped to the workspace or to the nearest branch-space
/// holding it; else none, for straight to main.
pub fn queue_for(trunk: &Path, ws: &Path, repo: &str) -> Result<Option<String>> {
    if let Some(q) = read(ws)?.repo.get(repo).and_then(|r| r.queue.clone()) {
        return Ok((q != NO_QUEUE).then_some(q));
    }
    scoped_queue(trunk, ws)
}

/// The queue that takes `ws`'s landings unasked: one scoped to it, or to the
/// nearest branch-space above it.
pub fn scoped_queue(trunk: &Path, ws: &Path) -> Result<Option<String>> {
    let rel = space::rel(trunk, ws)?;
    for name in queue::list(trunk)? {
        if Queue::open(trunk, &name)?.config.scope == Scope::Workspace(rel.clone()) {
            return Ok(Some(name));
        }
    }
    let space = ws.parent().and_then(space::find_space).unwrap_or_else(|| trunk.to_path_buf());
    space_queue(trunk, &space::rel(trunk, &space)?)
}

/// The queue scoped to the branch-space at `space_rel`, or to the nearest
/// one above it, that its workspaces land into unasked.
pub fn space_queue(trunk: &Path, space_rel: &str) -> Result<Option<String>> {
    let mut best: Option<(usize, String)> = None;
    for name in queue::list(trunk)? {
        if let Scope::Branch(b) = Queue::open(trunk, &name)?.config.scope
            && Scope::Branch(b.clone()).admits_space(space_rel)
            && best.as_ref().is_none_or(|(n, _)| b.len() > *n)
        {
            best = Some((b.len(), name));
        }
    }
    Ok(best.map(|(_, name)| name))
}
