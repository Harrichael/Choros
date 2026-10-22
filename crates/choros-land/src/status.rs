//! What `choros status` reports, and the one-token form for shell prompts.

use std::path::{Path, PathBuf};

use choros_core::choros as core;
use color_eyre::eyre::Result;

use crate::git::Git;
use crate::graph;
use crate::queue::{self, Queue};
use crate::state::{Park, SetStatus};
use crate::workspace;

pub struct QueueStatus {
    pub root: PathBuf,
    pub name: String,
    pub repo: String,
    pub sets: Vec<SetLine>,
    pub merge_head_park: Option<Park>,
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

/// Inside a workspace: the queues it lands into. Elsewhere in a root: every
/// queue there. Outside any root: every queue of every registered root.
pub fn report(cwd: &Path, config_home: Option<&Path>) -> Result<Vec<QueueStatus>> {
    let mut targets: Vec<(PathBuf, String)> = Vec::new();
    if let Ok((root, ws)) = core::find_workspace(cwd) {
        for rl in workspace::read(&ws)?.repo.values() {
            if let Some(q) = &rl.queue {
                targets.push((root.clone(), q.clone()));
            }
        }
        if let Some(l) = workspace::read(&ws)?.landing {
            targets.push((root.clone(), l.queue));
        }
    } else {
        let roots = match core::find_root(cwd) {
            Some(r) => vec![r],
            None => config_home
                .map(choros_core::roots::list)
                .transpose()?
                .unwrap_or_default(),
        };
        for root in roots {
            for name in queue::list(&root)? {
                targets.push((root.clone(), name));
            }
        }
    }
    targets.sort();
    targets.dedup();
    targets
        .into_iter()
        .map(|(root, name)| queue_status(&root, &name))
        .collect()
}

fn queue_status(root: &Path, name: &str) -> Result<QueueStatus> {
    let q = Queue::open(root, name)?;
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
            && !Git::new(&landing).is_clean().unwrap_or(true);
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
    for ws in core::scan(root)? {
        for (repo, rl) in workspace::read(&ws.path)?.repo {
            if rl.queue.as_deref() == Some(name) && rl.composition != current {
                needs_sync.push((ws.path.clone(), repo));
            }
        }
    }
    Ok(QueueStatus {
        root: root.to_path_buf(),
        name: name.to_string(),
        repo: q.config.repo.clone(),
        sets,
        merge_head_park: st.merge_head_park.clone(),
        needs_sync,
    })
}

/// `!resolve` when a queue this workspace uses is parked, `⇣sync` when its
/// merge head has moved past the workspace, else nothing. Files only: this
/// runs on every prompt.
pub fn prompt_token(cwd: &Path) -> String {
    let Ok((root, ws)) = core::find_workspace(cwd) else {
        return String::new();
    };
    let Ok(wl) = workspace::read(&ws) else {
        return String::new();
    };
    let mut sync = false;
    for rl in wl.repo.values() {
        let Some(name) = &rl.queue else { continue };
        let Ok(st) = crate::state::read_state(&queue::queues_dir(&root).join(name).join("state.json")) else {
            continue;
        };
        if st.merge_head_park.is_some() || st.sets.values().any(|s| s.is_open() && (s.park.is_some() || s.status == SetStatus::Closed)) {
            return "!resolve".into();
        }
        if st.merge_head.as_ref().map(|m| m.composition.key()) != rl.composition {
            sync = true;
        }
    }
    if sync { "⇣sync".into() } else { String::new() }
}
