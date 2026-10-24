//! What `choros status` reports, and the one-token form for shell prompts.

use std::path::{Path, PathBuf};

use choros_core::choros as core;
use choros_core::space;
use color_eyre::eyre::Result;

use crate::git::Git;
use crate::graph;
use crate::queue::{self, Queue};
use crate::state::{Park, Scope, SetStatus};
use crate::workspace;

pub struct QueueStatus {
    pub trunk: PathBuf,
    pub name: String,
    pub repo: String,
    pub sets: Vec<SetLine>,
    /// Workspaces (and repos) whose base the merge head has moved past.
    pub needs_sync: Vec<(PathBuf, String)>,
}

pub struct SetLine {
    pub n: u32,
    pub branch: String,
    pub state: &'static str,
    pub deps: Vec<String>,
    pub pr: Option<String>,
    pub park: Option<Park>,
    pub blocked: bool,
    pub conflicts_with: Vec<u32>,
    pub landing_dirty: bool,
}

/// The queues a command run at `cwd` is about. Inside a workspace: the ones
/// it lands into. In a branch-space: those scoped to it or below it, and those
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
            let wl = workspace::read(&ws)?;
            targets.extend(wl.repo.values().filter_map(|rl| rl.queue.clone()).map(|q| (trunk.clone(), q)));
            if let Some(l) = wl.landing {
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
                for rl in workspace::read(&ws.path)?.repo.values() {
                    if let Some(q) = &rl.queue {
                        targets.push((trunk.clone(), q.clone()));
                    }
                }
            }
        }
    }
    targets.sort();
    targets.dedup();
    Ok(targets)
}

pub fn report(cwd: &Path, config_home: Option<&Path>, all: bool) -> Result<Vec<QueueStatus>> {
    visible_queues(cwd, config_home, all)?
        .into_iter()
        .map(|(trunk, name)| queue_status(&trunk, &name))
        .collect()
}

fn queue_status(trunk: &Path, name: &str) -> Result<QueueStatus> {
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
            SetStatus::Queued if set.pr.is_some() => "front",
            SetStatus::Queued => "queued",
        };
        let landing = q.landing_dir(*n).join(&q.config.repo);
        let landing_dirty = set.is_open()
            && landing.exists()
            && !Git::new(&landing).is_clean_tracked().unwrap_or(true);
        sets.push(SetLine {
            n: *n,
            branch: set.branch(name),
            state,
            deps: set
                .deps
                .iter()
                .map(|d| match d {
                    crate::state::Dep::Set(m) => format!("landing-{m}"),
                    crate::state::Dep::Cross { queue, n } => format!("{queue}/landing-{n}"),
                })
                .collect(),
            pr: set.pr.as_ref().map(|p| p.url.clone()),
            park: set.park.clone(),
            blocked: set.is_open() && set.park.is_none() && graph::blocked(&st, *n),
            conflicts_with: set.conflicts_with.clone(),
            landing_dirty,
        });
    }
    let current = st.merge_head.as_ref().map(|m| m.composition.key());
    let mut needs_sync = Vec::new();
    for ws in space::tree(trunk)?.all_workspaces() {
        for (repo, rl) in workspace::read(&ws.path)?.repo {
            if rl.queue.as_deref() == Some(name) && rl.composition != current {
                needs_sync.push((ws.path.clone(), repo));
            }
        }
    }
    Ok(QueueStatus {
        trunk: trunk.to_path_buf(),
        name: name.to_string(),
        repo: q.config.repo.clone(),
        sets,
        needs_sync,
    })
}

/// `!resolve` when a queue this workspace uses is parked, `⇣sync` when its
/// merge head has moved past the workspace, else nothing. Files only: this
/// runs on every prompt.
pub fn prompt_token(cwd: &Path) -> String {
    let Ok((trunk, ws)) = core::find_workspace(cwd) else {
        return String::new();
    };
    let Ok(wl) = workspace::read(&ws) else {
        return String::new();
    };
    let mut sync = false;
    for rl in wl.repo.values() {
        let Some(name) = &rl.queue else { continue };
        let Ok(st) = crate::state::read_state(&queue::queues_dir(&trunk).join(name).join("state.json")) else {
            continue;
        };
        if st
                .sets
                .values()
                .any(|s| (s.is_open() && s.park.is_some()) || s.status == SetStatus::Closed)
        {
            return "!resolve".into();
        }
        if st.merge_head.as_ref().map(|m| m.composition.key()) != rl.composition {
            sync = true;
        }
    }
    if sync { "⇣sync".into() } else { String::new() }
}
