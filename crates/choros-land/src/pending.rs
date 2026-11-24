//! Slow work a pass asks for, done with the queue's lock let go.
//!
//! A pass holds the queue's lock, so everything in it must be quick: a
//! repo's check or a resync agent can take most of an hour, and every land,
//! sync and daemon pass of the queue would wait on it. So when a conflict
//! needs one, the pass parks the set as awaiting it, notes the job, and
//! moves on. Whoever ran the pass then runs the job here, unlocked, and
//! keeps what came of it under the conflict's content (the trees of its two
//! sides and their base, which the pass's own commits don't change): the
//! tree that was checked, and whether the check passed. The next pass meets
//! the same conflict, takes that very tree, and settles at once. An answer
//! whose inputs have moved on is simply never met again, so nothing has to
//! check that the queue stood still meanwhile.

use serde::{Deserialize, Serialize};

use crate::agent::Agent;
use crate::auto_resolve::AutoResolve;
use crate::queue::Queue;
use crate::store::Store;

/// Rounds of jobs and passes a sync runs at most: each pass may meet new
/// conflicts, so it ends only when one asks for nothing new, or here.
pub const ROUNDS: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub repo: String,
    /// The step that met the conflict, as the log names it.
    pub what: String,
    pub task: Task,
}

/// Each is for one conflict, as `Git::conflict_key` names it, and what
/// comes of it is kept under that name and `by` (see `Store::keep_fix`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Task {
    /// The repo's check, on `tree`, the conflict settled without judgment.
    Check { conflict: String, by: String, tree: String, commit: String, check: String },
    /// The resync agent, on the conflict `other` meets on `head`.
    Agent { conflict: String, step: Redo, head: String, other: String },
}

/// Keep what came of a job for `conflict`. A tree that passed is held by a
/// ref, or git would in time clean it up as unreachable, while the queue
/// still means to take it.
pub fn keep(
    q: &Queue,
    repo: &str,
    conflict: &str,
    by: &str,
    tree: &str,
    verdict: &Result<(), String>,
) -> color_eyre::Result<()> {
    if verdict.is_ok() {
        q.git(repo).run(&["update-ref", &format!("refs/choros/fixes/{tree}"), tree])?;
    }
    Store::open(&q.dir())?.keep_fix(repo, conflict, by, tree, verdict)
}

/// Who made a fix, as kept: the merge rules as they stand (with
/// Mergiraf's version), with the check that vouched for it. Either
/// changing makes an earlier verdict another fix's.
pub fn by_rules(config: &crate::state::MergeConfig, check: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    serde_json::to_string(config).unwrap_or_default().hash(&mut h);
    crate::mend::mergiraf().hash(&mut h);
    check.hash(&mut h);
    format!("rules:{:016x}", h.finish())
}

/// As `by_rules`, for the resync agent's fix.
pub fn by_agent(check: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    check.hash(&mut h);
    format!("agent:{:016x}", h.finish())
}

/// How to meet the conflict again: as the sync clone met it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Redo {
    Merge,
    CherryPick,
}

impl Job {
    /// What it waits for, for `choros status`.
    pub fn waits_for(&self) -> String {
        match &self.task {
            Task::Check { check, .. } => format!("`{check}` on a fix made without judgment"),
            Task::Agent { .. } => "the resync agent".to_string(),
        }
    }
}

/// Run the queue's pending jobs, the lock let go: checks always, agent
/// runs only with `auto` (the daemon's limits on them). The number that
/// ran, so the caller knows a pass would now find more settled.
pub fn run(q: &Queue, agent: &dyn Agent, auto: Option<&mut AutoResolve>, notes: &mut Vec<String>) -> color_eyre::Result<usize> {
    let st = q.read_state()?;
    run_jobs(q, &st, &st.pending, agent, auto, notes)
}

/// As `run`, for `jobs` a step has just asked for: a land runs those it
/// needs itself, then tries the step again.
pub fn run_jobs(
    q: &Queue,
    st: &crate::state::QueueState,
    jobs: &[Job],
    agent: &dyn Agent,
    mut auto: Option<&mut AutoResolve>,
    notes: &mut Vec<String>,
) -> color_eyre::Result<usize> {
    let mut ran = 0;
    for job in jobs {
        match &job.task {
            Task::Check { conflict, by, tree, commit, check } => {
                // Asked again once the check's own lock is held: another
                // runner may have just run it.
                let done = || Ok(Store::open(&q.dir())?.fix(&job.repo, conflict, by)?.is_some_and(|(t, _)| t == *tree));
                let Some(verdict) = q.check_unless(&job.repo, commit, check, done)? else {
                    continue;
                };
                keep(q, &job.repo, conflict, by, tree, &verdict)?;
                let said = if verdict.is_ok() { "passed" } else { "failed" };
                notes.push(format!("`{check}` {said} on the fix made without judgment for {}", job.what));
                ran += 1;
            }
            Task::Agent { .. } => {
                if let Some(auto) = auto.as_deref_mut()
                    && auto.run(q, st, job, agent)?
                {
                    ran += 1;
                }
            }
        }
    }
    Ok(ran)
}
