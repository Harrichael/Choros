//! What `choros status` reports, and the one-token form for shell prompts.

use std::path::{Path, PathBuf};

use choros_core::choros as core;
use choros_core::space;
use color_eyre::eyre::Result;
use serde::Serialize;

use crate::git::Git;
use crate::graph;
use crate::queue::{self, Queue};
use crate::forge::{PrReview, PrState};
use crate::state::{DiffbasePark, Park, PrRef, Scope, SessionFork, SetStatus};
use crate::workspace;

/// Serialized, this is `choros status --json`; the set fields are a contract
/// other tools read. `pr` and `sessions` serialize state.rs types, so renaming
/// their fields there renames them here too.
#[derive(Serialize)]
pub struct QueueStatus {
    pub trunk: PathBuf,
    pub name: String,
    /// The repos something has landed into.
    pub repos: Vec<String>,
    pub sets: Vec<SetLine>,
    /// Workspaces (and repos) whose base the merge head has moved past.
    pub needs_sync: Vec<NeedsSync>,
    /// Each PR in the diffbase, with what's known of it.
    pub diffbase: Vec<DiffbaseLine>,
    pub diffbase_park: Option<DiffbasePark>,
}

#[derive(Serialize)]
pub struct NeedsSync {
    pub workspace: PathBuf,
    pub repo: String,
}

#[derive(Serialize)]
pub struct DiffbaseLine {
    pub pr: String,
    /// `open`, `merged`, `closed`, or `unseen` before a pass has asked.
    pub state: &'static str,
}

#[derive(Serialize)]
pub struct SetLine {
    pub n: u32,
    pub repo: String,
    pub branch: String,
    pub state: &'static str,
    pub title: Option<String>,
    /// What the text view calls an open set; `title` stays what was given
    /// at land, as the JSON contract has it.
    #[serde(skip)]
    pub pr_title: Option<String>,
    pub pr: Option<PrRef>,
    /// The open PR's review, once a pass has seen it.
    pub review: Option<PrReview>,
    pub sessions: Vec<SessionFork>,
    pub labels: Vec<String>,
    pub deps: Vec<String>,
    pub park: Option<Park>,
    pub blocked: bool,
    pub conflicts_with: Vec<u32>,
    pub landing_dirty: bool,
    /// A stacked PR not yet linked into a GitHub stack.
    pub unlinked: bool,
}

/// The queues a command run at `cwd` is about. Inside a workspace: the ones
/// its repos land into. In a branch-space: those scoped to it or below it, and those
/// its workspaces land into. At the trunk, or with `all`: every queue there.
/// Outside any trunk: every queue of every registered trunk.
pub fn visible_queues(cwd: &Path, config_home: Option<&Path>, all: bool) -> Result<Vec<(PathBuf, String)>> {
    let mut targets: Vec<(PathBuf, String)> = Vec::new();
    let Some(space) = space::find_space(cwd) else {
        let trunks = config_home.map(choros_core::trunks::list).transpose()?.unwrap_or_default();
        for trunk in trunks {
            for name in queue::list(&trunk)? {
                targets.push((trunk.clone(), name));
            }
        }
        return Ok(targets);
    };
    let trunk = space::find_trunk(&space).unwrap_or_else(|| space.clone());
    match core::find_workspace(cwd) {
        Ok((_, ws)) if !all && ws.starts_with(&space) => {
            targets.extend(queues_of(&trunk, &ws)?.into_iter().map(|q| (trunk.clone(), q)));
            if let Some(l) = workspace::read(&ws)?.landing {
                targets.push((trunk.clone(), l.queue));
            }
        }
        _ if all || space == trunk => {
            targets.extend(queue::list(&trunk)?.into_iter().map(|q| (trunk.clone(), q)));
        }
        _ => {
            let here = space::rel(&trunk, &space)?;
            let below = |p: &str| p == here || p.strip_prefix(here.as_str()).is_some_and(|r| r.starts_with('/'));
            for name in queue::list(&trunk)? {
                let scoped_here = match &Queue::open(&trunk, &name)?.config.scope {
                    Scope::Trunk => false,
                    Scope::Workspace(p) | Scope::Branch(p) => below(p),
                };
                if scoped_here {
                    targets.push((trunk.clone(), name));
                }
            }
            for ws in space::tree(&space)?.all_workspaces() {
                targets.extend(queues_of(&trunk, &ws.path)?.into_iter().map(|q| (trunk.clone(), q)));
            }
        }
    }
    targets.sort();
    targets.dedup();
    Ok(targets)
}

/// The queues `ws`'s repos land into.
fn queues_of(trunk: &Path, ws: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for repo in core::read_meta(ws)?.repos {
        out.extend(workspace::queue_for(trunk, ws, &repo)?);
    }
    Ok(out)
}

/// The straight-to-main lands a command run at `cwd` is about, chosen as
/// [`visible_queues`] chooses queues: inside a workspace, its own; in a
/// branch-space, those of the workspaces under it; at the trunk, or with
/// `all`, every one there; outside any trunk, those of every trunk.
pub fn instant_lands(cwd: &Path, config_home: Option<&Path>, all: bool) -> Result<Vec<crate::instant::Landed>> {
    let Some(space) = space::find_space(cwd) else {
        let trunks = config_home.map(choros_core::trunks::list).transpose()?.unwrap_or_default();
        let mut out = Vec::new();
        for trunk in trunks {
            out.extend(crate::instant::read(&trunk)?);
        }
        return Ok(out);
    };
    let trunk = space::find_trunk(&space).unwrap_or_else(|| space.clone());
    let within = match core::find_workspace(cwd) {
        Ok((_, ws)) if !all && ws.starts_with(&space) => ws,
        _ if all => trunk.clone(),
        _ => space,
    };
    let mut lands = crate::instant::read(&trunk)?;
    lands.retain(|l| l.workspace.starts_with(&within));
    Ok(lands)
}

pub fn report(cwd: &Path, config_home: Option<&Path>, all: bool) -> Result<Vec<QueueStatus>> {
    visible_queues(cwd, config_home, all)?
        .into_iter()
        .map(|(trunk, name)| queue_status(&trunk, &name))
        .collect()
}

pub fn queue_status(trunk: &Path, name: &str) -> Result<QueueStatus> {
    let q = Queue::open(trunk, name)?;
    let st = q.read_state()?;
    let mut sets = Vec::new();
    for (n, set) in &st.sets {
        if matches!(set.status, SetStatus::Dropped) {
            continue;
        }
        let state = match set.status {
            SetStatus::Merged => "merged",
            SetStatus::Closed => "closed",
            SetStatus::Dropped => "dropped",
            SetStatus::Queued if graph::stacked(&st, *n) => "stacked",
            SetStatus::Queued if set.pr.is_some() => "front",
            SetStatus::Queued => "queued",
        };
        let landing = q.landing_dir(*n).join(set.repo());
        let landing_dirty = set.is_open()
            && landing.exists()
            && !Git::new(&landing).is_clean_tracked().unwrap_or(true);
        sets.push(SetLine {
            n: *n,
            repo: set.repo().to_string(),
            branch: set.branch(name),
            state,
            title: set.title.clone(),
            pr_title: set.is_open().then(|| q.pr_title(set).ok()).flatten(),
            pr: set.pr.clone(),
            review: set.review.clone().filter(|_| set.is_open()),
            sessions: set.sessions.clone(),
            labels: set.labels.clone(),
            deps: set
                .deps
                .iter()
                .map(|d| match d {
                    crate::state::Dep::Set(m) => format!("landing-{m}"),
                    crate::state::Dep::Cross { queue, n } => format!("{queue}/landing-{n}"),
                })
                .collect(),
            park: set.park.clone(),
            blocked: set.is_open() && set.park.is_none() && graph::blocked(&st, *n),
            conflicts_with: set.conflicts_with.clone(),
            landing_dirty,
            unlinked: graph::stacked(&st, *n) && set.stack.is_none(),
        });
    }
    let mut needs_sync = Vec::new();
    for ws in space::tree(trunk)?.all_workspaces() {
        let wl = workspace::read(&ws.path)?;
        for repo in core::read_meta(&ws.path)?.repos {
            if workspace::queue_for(trunk, &ws.path, &repo)?.as_deref() != Some(name) {
                continue;
            }
            let current = merge_head_key(&st, &repo);
            if wl.repo.get(&repo).and_then(|rl| rl.composition.clone()) != current {
                needs_sync.push(NeedsSync { workspace: ws.path.clone(), repo });
            }
        }
    }
    let diffbase = q
        .config
        .diffbase()?
        .into_iter()
        .map(|pr| DiffbaseLine {
            state: match st.diffbase.prs.get(&pr.to_string()).map(|s| &s.state) {
                Some(PrState::Open) => "open",
                Some(PrState::Merged) => "merged",
                Some(PrState::Closed) => "closed",
                None => "unseen",
            },
            pr: pr.to_string(),
        })
        .collect();
    Ok(QueueStatus {
        trunk: trunk.to_path_buf(),
        name: name.to_string(),
        repos: st.repos.keys().cloned().collect(),
        sets,
        needs_sync,
        diffbase,
        diffbase_park: st.diffbase.park.clone(),
    })
}

/// `!resolve` when a queue this workspace uses is parked, `⇣sync` when its
/// merge head has moved past the workspace, else nothing. Files only: this
/// runs on every prompt.
pub fn prompt_token(cwd: &Path) -> String {
    let Ok((trunk, ws)) = core::find_workspace(cwd) else {
        return String::new();
    };
    let (Ok(wl), Ok(meta)) = (workspace::read(&ws), core::read_meta(&ws)) else {
        return String::new();
    };
    let mut sync = false;
    for repo in &meta.repos {
        let Ok(Some(name)) = workspace::queue_for(&trunk, &ws, repo) else { continue };
        let Ok(st) = Queue::open(&trunk, &name).and_then(|q| q.read_state()) else {
            continue;
        };
        if st.diffbase.park.is_some()
            || st
                .sets
                .values()
                .any(|s| (s.is_open() && s.park.is_some()) || s.status == SetStatus::Closed)
        {
            return "!resolve".into();
        }
        if merge_head_key(&st, repo) != wl.repo.get(repo).and_then(|rl| rl.composition.clone()) {
            sync = true;
        }
    }
    if sync { "⇣sync".into() } else { String::new() }
}

/// Whether `repo` of workspace `ws` lands into a queue whose merge head has
/// moved since the workspace last synced onto it.
pub fn needs_sync(trunk: &Path, ws: &Path, repo: &str) -> Result<bool> {
    let Some(name) = workspace::queue_for(trunk, ws, repo)? else {
        return Ok(false);
    };
    let st = Queue::open(trunk, &name)?.read_state()?;
    let synced = workspace::read(ws)?.repo.get(repo).and_then(|rl| rl.composition.clone());
    Ok(merge_head_key(&st, repo) != synced)
}

/// What `repo`'s merge head holds, as workspaces record it.
fn merge_head_key(st: &crate::state::QueueState, repo: &str) -> Option<String> {
    st.repos.get(repo)?.merge_head.as_ref().map(|m| m.composition.key())
}
