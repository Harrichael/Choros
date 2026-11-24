//! When a queued set's PR opens, and if it hasn't, why not. The pass opens
//! PRs by these rules and `choros status` explains a set by them, so the
//! two can't disagree. Only the queue's record is read: no git, no forge.

use std::fmt;

use serde::Serialize;

use crate::graph;
use crate::state::{Dep, QueueConfig, QueueState, SetStatus};

/// What a queued set without a PR is waiting for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "why", rename_all = "snake_case")]
pub enum Waiting {
    /// Sets in another repo or queue, which can only be waited for.
    Elsewhere { on: Vec<String> },
    /// The diffbase's PRs, which every front PR waits for.
    Diffbase,
    /// Its base still holds sets that merged; a pass moves it onto main.
    Resync,
    /// Stacking is off, so it opens only at the front.
    StackingOff { on: Vec<u32> },
    /// A PR has one base branch, so a set on several open sets can't stack.
    SitsOnMany { on: Vec<u32> },
    /// The set below has no PR to stack on yet.
    BelowHasNoPr { on: u32 },
    /// The set below's stacked PR isn't linked into a GitHub stack yet.
    BelowUnlinked { on: u32 },
    /// The set below already has a stacked PR on it.
    BelowTaken { on: u32, by: u32 },
    /// It isn't built on the set below's tip: it needs a main that set's
    /// branch lacks, or a re-sync onto it.
    NotOnBelowTip { on: u32 },
    /// Nothing of its own sits over the set below.
    NothingOwn { on: u32 },
    /// The repo has as many open PRs as stacking allows.
    Cap { max_open: u32 },
    /// Nothing: the next pass opens its PR.
    NextPass,
}

impl Waiting {
    /// A set waiting only on what the queue's graph already shows: the set
    /// below's PR, or sets to merge when nothing can stack.
    pub fn shown_by_the_graph(&self) -> bool {
        matches!(self, Waiting::BelowHasNoPr { .. } | Waiting::StackingOff { .. } | Waiting::Elsewhere { .. })
    }
}

fn sets(on: &[u32]) -> String {
    on.iter().map(|n| format!("landing-{n}")).collect::<Vec<_>>().join(", ")
}

impl fmt::Display for Waiting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Waiting::Elsewhere { on } => write!(f, "no PR: waits for {} to merge", on.join(", ")),
            Waiting::Diffbase => write!(f, "no PR: waits for the diffbase's PRs to merge"),
            Waiting::Resync => write!(f, "no PR yet: the next pass moves it onto main and opens one"),
            Waiting::StackingOff { on } => {
                write!(f, "no PR: waits for {} to merge, as stacking is off", sets(on))
            }
            Waiting::SitsOnMany { on } => write!(
                f,
                "no PR: it sits on {} open sets ({}), and a stacked PR has one base, so it waits until all but one merge",
                on.len(),
                sets(on)
            ),
            Waiting::BelowHasNoPr { on } => write!(f, "no PR: waits for landing-{on}'s PR"),
            Waiting::BelowUnlinked { on } => {
                write!(f, "no PR: waits for landing-{on}'s PR to be linked into a GitHub stack")
            }
            Waiting::BelowTaken { on, by } => write!(
                f,
                "no PR: landing-{by} is already stacked on landing-{on}, so it waits for landing-{on} to merge"
            ),
            Waiting::NotOnBelowTip { on } => write!(
                f,
                "no PR: it isn't built on landing-{on}'s tip, as it needs a newer main than landing-{on}'s branch \
                 has (or a re-sync); it stacks once landing-{on} takes main in, or opens once it merges"
            ),
            Waiting::NothingOwn { on } => write!(f, "no PR: it has nothing of its own over landing-{on}"),
            Waiting::Cap { max_open } => {
                write!(f, "no PR: the repo is at the stacking cap of {max_open} open PRs")
            }
            Waiting::NextPass => write!(f, "no PR yet: the next pass opens one"),
        }
    }
}

/// Why landing-`n` has no PR, or `None` when it needs no reason: it has a
/// PR, isn't queued, or is parked or blocked (which say so themselves).
/// `cross_merged` answers for sets in other queues; `diffbase_waiting` is
/// whether the diffbase still holds front PRs back.
pub fn waiting(
    st: &QueueState,
    config: &QueueConfig,
    queue: &str,
    n: u32,
    cross_merged: &dyn Fn(&str, u32) -> bool,
    diffbase_waiting: bool,
) -> Option<Waiting> {
    let set = st.sets.get(&n)?;
    if set.status != SetStatus::Queued || set.pr.is_some() || set.park.is_some() || graph::blocked(st, n) {
        return None;
    }
    let elsewhere = elsewhere(st, n, cross_merged);
    if !elsewhere.is_empty() {
        return Some(Waiting::Elsewhere { on: elsewhere });
    }
    // The pass opens no PR at all, front or stacked, while it waits.
    if diffbase_waiting {
        return Some(Waiting::Diffbase);
    }
    let on = graph::sits_on(st, n);
    if on.is_empty() {
        return Some(if !set.base.deps.is_empty() {
            Waiting::Resync
        } else {
            Waiting::NextPass
        });
    }
    if !config.stack.enabled {
        return Some(Waiting::StackingOff { on });
    }
    Some(match stack_spot(st, queue, n, cross_merged).and_then(|p| stack_ready(st, config, n, p)) {
        Ok(()) => Waiting::NextPass,
        Err(why) => why,
    })
}

/// The sets in another repo or queue that landing-`n` waits for.
fn elsewhere(st: &QueueState, n: u32, cross_merged: &dyn Fn(&str, u32) -> bool) -> Vec<String> {
    let set = &st.sets[&n];
    set.deps
        .iter()
        .filter_map(|d| match d {
            Dep::Set(m) => st
                .sets
                .get(m)
                .filter(|s| s.status != SetStatus::Merged && s.repo() != set.repo())
                .map(|_| format!("landing-{m}")),
            Dep::Cross { queue, n } => (!cross_merged(queue, *n)).then(|| format!("{queue}/landing-{n}")),
        })
        .collect()
}

/// The set landing-`n` would stack on, cap and base aside: the one set it
/// sits on, with a PR in a stack (or on main) and no other stacked child.
pub(crate) fn stack_spot(
    st: &QueueState,
    queue: &str,
    n: u32,
    cross_merged: &dyn Fn(&str, u32) -> bool,
) -> Result<u32, Waiting> {
    // Waiting on a set in another repo or queue: its PR opens at the
    // front, if at all, so there is nothing to stack on.
    let elsewhere = elsewhere(st, n, cross_merged);
    if !elsewhere.is_empty() {
        return Err(Waiting::Elsewhere { on: elsewhere });
    }
    let on = graph::sits_on(st, n);
    let [p] = on[..] else {
        return Err(Waiting::SitsOnMany { on });
    };
    let under = &st.sets[&p];
    if under.pr.is_none() {
        return Err(Waiting::BelowHasNoPr { on: p });
    }
    // Under an unlinked stacked PR, linking would start a second stack and
    // strand that PR's own link.
    if !graph::public(st, p) && under.stack.is_none() {
        return Err(Waiting::BelowUnlinked { on: p });
    }
    // A closed child is still in GitHub's stack until it is dropped.
    let below = under.branch(queue);
    if let Some(child) = st.sets.values().find(|s| {
        s.n != n
            && matches!(s.status, SetStatus::Queued | SetStatus::Closed)
            && s.pr_base.as_deref() == Some(below.as_str())
    }) {
        return Err(Waiting::BelowTaken { on: p, by: child.n });
    }
    Ok(p)
}

/// Whether landing-`n` may open its PR stacked on landing-`p` now: it is
/// built on that set's tip, has something of its own, and the cap allows.
pub(crate) fn stack_ready(st: &QueueState, config: &QueueConfig, n: u32, p: u32) -> Result<(), Waiting> {
    let (set, under) = (&st.sets[&n], &st.sets[&p]);
    if set.base.deps != [(p, under.tip.clone())] || set.base_sha != under.tip {
        return Err(Waiting::NotOnBelowTip { on: p });
    }
    if set.tip == set.base_sha {
        return Err(Waiting::NothingOwn { on: p });
    }
    let open = st.sets.values().filter(|s| s.is_open() && s.pr.is_some() && s.repo() == set.repo()).count();
    if open >= config.stack.max_open as usize {
        return Err(Waiting::Cap { max_open: config.stack.max_open });
    }
    Ok(())
}
