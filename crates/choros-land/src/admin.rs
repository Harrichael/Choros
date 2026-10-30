//! Operations you run on a queue by hand: settle its parked conflicts, drop
//! a set, reconcile it with origin, open a landing's session.

use std::path::PathBuf;

use color_eyre::eyre::{Result, eyre};

use crate::engine::{Handler, Env, PassReport, Work};
use crate::graph;
use crate::queue::Queue;
use crate::state::{Dep, ParkOp, SetStatus};
use crate::workspace::{self, Returned};

/// One pass now, in the foreground. With a handler, parked conflicts are
/// retried and handed to it.
pub fn sync(env: &Env, q: &Queue, handler: Handler<'_>) -> Result<PassReport> {
    let retry = handler.is_some();
    let mut work = Work::begin(q, env.agent, handler, true)?.unwrap();
    if retry {
        work.unpark_conflicts();
        let diverged: Vec<u32> = work
            .st
            .sets
            .iter()
            .filter(|(_, s)| s.park.as_ref().is_some_and(|p| p.op == ParkOp::Diverged))
            .map(|(n, _)| *n)
            .collect();
        for n in diverged {
            work.accept_diverged(n)?;
        }
    }
    let report = work.pass(env.forge);
    // Whatever was settled before a failure stays settled.
    work.save()?;
    report
}

/// Remove a queued set. Its dependents inherit its dependencies and re-sync
/// without it on the pass that follows. Its commits go back to the
/// workspace that landed them, to come back as unlanded on its next sync:
/// otherwise that sync would take them out, as no longer in the queue.
///
/// In a GitHub stack, the PR stacked on it moves down onto what it sat on.
/// GitHub won't change a stacked PR's base, nor link a stack without one of
/// its PRs, closed ones included: so the stack is dissolved first, and the
/// pass links what is left again.
pub fn drop_set(env: &Env, q: &Queue, n: u32) -> Result<Dropped> {
    let mut work = Work::begin(q, env.agent, None, true)?.unwrap();
    // As GitHub has it now: a drop that failed halfway may have unstacked.
    work.observe_stacks(env.forge)?;
    let set = work
        .st
        .sets
        .get(&n)
        .ok_or_else(|| eyre!("no landing-{n} in {}", q.name))?;
    if !matches!(set.status, SetStatus::Queued | SetStatus::Closed) {
        return Err(eyre!("landing-{n} is already {}", set.status));
    }
    // Choros stops watching a dropped set; an open PR would be orphaned.
    if set.status == SetStatus::Queued && set.pr.is_some() {
        return Err(eyre!("landing-{n} has an open PR; close it on GitHub, then drop the set"));
    }
    let branch = set.branch(&q.name);
    let dir = work.git(set.repo()).dir().to_path_buf();
    if let Some(stack) = set.stack {
        env.forge.unstack(&dir, stack)?;
        for s in work.st.sets.values_mut().filter(|s| s.stack == Some(stack)) {
            s.stack = None;
        }
    }
    let set = &work.st.sets[&n];
    let stacked_on: Vec<u32> = work
        .st
        .sets
        .values()
        .filter(|s| s.is_open() && s.pr_base.as_deref() == Some(branch.as_str()))
        .map(|s| s.n)
        .collect();
    let returned = Returned {
        queue: q.name.clone(),
        n,
        base: set.base_sha.clone(),
        tip: set.tip.clone(),
    };
    let source = set.source.clone();
    work.git(&source.repo)
        .run(&["update-ref", &format!("refs/choros/returned/{n}"), &returned.tip])?;
    graph::splice_out(&mut work.st, n);
    let set = work.st.sets.get_mut(&n).unwrap();
    set.status = SetStatus::Dropped;
    set.park = None;
    // Onto main it goes in the pass, once replayed: on main before that, it
    // would count as public and keep the dropped work.
    for m in stacked_on {
        let [p] = graph::unmerged_deps(&work.st, m)[..] else { continue };
        let below = work.st.sets[&p].branch(&q.name);
        let pr = work.st.sets[&m].pr.as_ref().unwrap().number;
        env.forge.set_pr_base(&dir, pr, &below)?;
        work.st.sets.get_mut(&m).unwrap().pr_base = Some(below);
    }
    // The pass deletes the dropped branch once no PR is based on it.
    let report = work.pass(env.forge);
    work.save()?;
    let returned_to = workspace::give_back(&source.workspace, &source.repo, returned)?
        .then(|| source.workspace.join(&source.repo));
    Ok(Dropped {
        report: report?,
        returned_to,
    })
}

pub struct Dropped {
    pub report: PassReport,
    /// The repo its commits went back to, unless that workspace is gone.
    pub returned_to: Option<PathBuf>,
}

/// Differences between state.json and origin that a pass would not fix on
/// its own, then a pass.
pub fn doctor(env: &Env, q: &Queue) -> Result<Vec<String>> {
    let mut work = Work::begin(q, env.agent, None, true)?.unwrap();
    let mut notes = Vec::new();
    for (n, set) in &work.st.sets {
        if !set.is_open() {
            continue;
        }
        let branch = set.branch(&q.name);
        let git = work.set_git(*n);
        let remote = git.rev_parse(&format!("refs/remotes/origin/{branch}"))?;
        match (&set.remote, &remote) {
            (Some(_), None) => notes.push(format!("landing-{n}: gone from origin; it will be pushed again")),
            (None, Some(r)) => notes.push(format!(
                "landing-{n}: origin has {branch} at {r:.12} but Choros never pushed it; leaving it alone"
            )),
            (Some(a), Some(b)) if a != b => notes.push(format!(
                "landing-{n}: origin moved from {a:.12} to {b:.12}"
            )),
            _ => {}
        }
        if git.rev_parse(&set.tip)?.is_none() {
            notes.push(format!("landing-{n}: its tip {:.12} is missing from the sync clone", set.tip));
        }
    }
    let report = work.pass(env.forge)?;
    work.save()?;
    if !report.parked.is_empty() {
        notes.push("conflicts are parked; run `choros queue resolve`".into());
    }
    Ok(notes)
}

/// The landing folder and the forked session to resume there.
pub fn landing_session(q: &Queue, n: u32, session: Option<&str>) -> Result<(PathBuf, Option<String>)> {
    let st = q.read_state()?;
    let set = st
        .sets
        .get(&n)
        .ok_or_else(|| eyre!("no landing-{n} in {}", q.name))?;
    let dir = [q.landing_dir(n), q.archived_landing_dir(n)]
        .into_iter()
        .find(|d| d.exists())
        .ok_or_else(|| eyre!("landing-{n} of {} has no folder", q.name))?;
    let fork = match session {
        Some(s) => Some(
            set.sessions
                .iter()
                .find(|f| f.fork == s || f.source == s)
                .map(|f| f.fork.clone())
                .ok_or_else(|| eyre!("landing-{n} has no session {s}"))?,
        ),
        None => set.sessions.first().map(|f| f.fork.clone()),
    };
    Ok((dir, fork))
}

/// Replace a private set's dependencies. Public sets cannot gain any: their
/// place was settled before their PR opened.
pub fn set_deps(env: &Env, q: &Queue, n: u32, deps: Vec<Dep>) -> Result<PassReport> {
    let mut work = Work::begin(q, env.agent, None, true)?.unwrap();
    let set = work
        .st
        .sets
        .get(&n)
        .ok_or_else(|| eyre!("no landing-{n} in {}", q.name))?;
    if !set.is_open() || set.pr.is_some() {
        return Err(eyre!("landing-{n} is public or finished; its dependencies are fixed"));
    }
    for d in &deps {
        match d {
            Dep::Set(m) if *m == n => return Err(eyre!("landing-{n} cannot depend on itself")),
            Dep::Set(m) => {
                if !work.st.sets.get(m).is_some_and(|s| s.is_open()) {
                    return Err(eyre!("landing-{m} is not queued"));
                }
            }
            Dep::Cross { queue, n: m } => {
                let other = Queue::open(&q.trunk, queue)?;
                if !other.read_state()?.sets.contains_key(m) {
                    return Err(eyre!("{queue} has no landing-{m}"));
                }
            }
        }
    }
    let previous = std::mem::replace(&mut work.st.sets.get_mut(&n).unwrap().deps, deps);
    if graph::has_cycle(&work.st) {
        work.st.sets.get_mut(&n).unwrap().deps = previous;
        return Err(eyre!("that would make a dependency cycle"));
    }
    let report = work.pass(env.forge);
    work.save()?;
    report
}
