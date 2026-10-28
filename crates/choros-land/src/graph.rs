//! Questions about the dependency graph that need no git: what is live, what
//! is a leaf, what is at the front.
//!
//! A set builds on its dependencies in the same repo: they are its base.
//! One in another repo can't be built on, only waited for before its PR
//! opens. "Ancestors" and "leaves" follow the first kind only.

use std::collections::BTreeSet;

use crate::state::{Dep, LandingSet, QueueState, SetStatus};

/// Every same-queue dependency of a set, whatever its repo.
pub fn set_deps(set: &LandingSet) -> impl Iterator<Item = u32> + '_ {
    set.deps.iter().filter_map(|d| match d {
        Dep::Set(n) => Some(*n),
        Dep::Cross { .. } => None,
    })
}

/// The dependencies a set builds on: those in its own repo.
pub fn base_deps<'a>(state: &'a QueueState, set: &'a LandingSet) -> impl Iterator<Item = u32> + 'a {
    set_deps(set).filter(|d| state.sets.get(d).is_some_and(|s| s.repo() == set.repo()))
}

/// A queued set is blocked when it, or anything it builds on, is parked or
/// was closed: none of it may move or enter the merge head until resolved.
pub fn blocked(state: &QueueState, n: u32) -> bool {
    let mut seen = BTreeSet::new();
    let mut stack = vec![n];
    while let Some(m) = stack.pop() {
        if !seen.insert(m) {
            continue;
        }
        let Some(set) = state.sets.get(&m) else {
            continue;
        };
        if set.status == SetStatus::Closed || set.park.is_some() {
            return true;
        }
        stack.extend(base_deps(state, set).filter(|d| {
            state
                .sets
                .get(d)
                .is_some_and(|s| matches!(s.status, SetStatus::Queued | SetStatus::Closed))
        }));
    }
    false
}

/// Queued, unblocked sets, in dependency order.
pub fn live(state: &QueueState) -> Vec<u32> {
    topo(state)
        .into_iter()
        .filter(|n| state.sets[n].is_open() && !blocked(state, *n))
        .collect()
}

/// Every queued set, dependencies before dependents.
pub fn topo(state: &QueueState) -> Vec<u32> {
    let mut out = Vec::new();
    let mut done = BTreeSet::new();
    fn visit(state: &QueueState, n: u32, done: &mut BTreeSet<u32>, out: &mut Vec<u32>) {
        if !done.insert(n) {
            return;
        }
        if let Some(set) = state.sets.get(&n) {
            for d in base_deps(state, set) {
                visit(state, d, done, out);
            }
            if set.is_open() {
                out.push(n);
            }
        }
    }
    for n in state.sets.keys() {
        visit(state, *n, &mut done, &mut out);
    }
    out
}

/// Of `members`, those no other member depends on, directly or not. Each
/// leaf's history contains its ancestors, so merging the leaves is merging
/// them all.
pub fn leaves(state: &QueueState, members: &[u32]) -> Vec<u32> {
    let member_set: BTreeSet<u32> = members.iter().copied().collect();
    let mut covered = BTreeSet::new();
    for n in members {
        covered.extend(ancestors(state, *n));
    }
    member_set
        .into_iter()
        .filter(|n| !covered.contains(n))
        .collect()
}

/// What a new set stores as its dependencies: `deps` without any that
/// another of them already builds on. A set names only what it sits directly
/// on, so editing a dependency's place carries its dependents along.
pub fn reduce(state: &QueueState, deps: &[Dep]) -> Vec<Dep> {
    let sets: Vec<u32> = deps
        .iter()
        .filter_map(|d| if let Dep::Set(n) = d { Some(*n) } else { None })
        .collect();
    let kept: BTreeSet<u32> = leaves(state, &sets).into_iter().collect();
    let mut out: Vec<Dep> = deps
        .iter()
        .filter(|d| match d {
            Dep::Set(n) => kept.contains(n),
            Dep::Cross { .. } => true,
        })
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Every set `n` builds on, transitively, excluding `n`.
pub fn ancestors(state: &QueueState, n: u32) -> BTreeSet<u32> {
    reachable(state, n, |set| base_deps(state, set).collect())
}

/// Every set `n` waits for, built on or not, transitively, excluding `n`.
fn ancestors_any_repo(state: &QueueState, n: u32) -> BTreeSet<u32> {
    reachable(state, n, |set| set_deps(set).collect())
}

fn reachable(state: &QueueState, n: u32, next: impl Fn(&LandingSet) -> Vec<u32>) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    let mut stack: Vec<u32> = state.sets.get(&n).map(&next).unwrap_or_default();
    while let Some(m) = stack.pop() {
        if out.insert(m)
            && let Some(set) = state.sets.get(&m)
        {
            stack.extend(next(set));
        }
    }
    out
}

/// `deps` and everything they build on.
pub fn ancestors_of_deps(state: &QueueState, deps: &[u32]) -> BTreeSet<u32> {
    let mut out: BTreeSet<u32> = deps.iter().copied().collect();
    for d in deps {
        out.extend(ancestors(state, *d));
    }
    out
}

/// Sets that build on `n`, transitively, excluding `n`.
pub fn dependents(state: &QueueState, n: u32) -> BTreeSet<u32> {
    state
        .sets
        .keys()
        .copied()
        .filter(|m| ancestors(state, *m).contains(&n))
        .collect()
}

/// A dependency cycle, across repos or not, would leave its sets waiting on
/// each other forever; edits must keep the graph acyclic.
pub fn has_cycle(state: &QueueState) -> bool {
    state.sets.keys().any(|n| ancestors_any_repo(state, *n).contains(n))
}

/// Queued, unblocked sets whose dependencies have all merged. `cross_merged`
/// answers for dependencies in other queues.
pub fn front(state: &QueueState, cross_merged: &dyn Fn(&str, u32) -> bool) -> Vec<u32> {
    live(state)
        .into_iter()
        .filter(|n| {
            state.sets[n].deps.iter().all(|d| match d {
                Dep::Set(m) => state.sets.get(m).is_none_or(|s| s.status == SetStatus::Merged),
                Dep::Cross { queue, n } => cross_merged(queue, *n),
            })
        })
        .collect()
}

/// Dependencies `n` builds on that are still queued.
pub fn unmerged_deps(state: &QueueState, n: u32) -> Vec<u32> {
    base_deps(state, &state.sets[&n])
        .filter(|d| state.sets.get(d).is_some_and(|s| s.is_open()))
        .collect()
}

pub fn merged_deps(state: &QueueState, n: u32) -> Vec<u32> {
    base_deps(state, &state.sets[&n])
        .filter(|d| state.sets.get(d).is_some_and(|s| s.status == SetStatus::Merged))
        .collect()
}

/// Remove `n` from the graph's edges: its dependents inherit its
/// dependencies, so they keep building on everything below it.
pub fn splice_out(state: &mut QueueState, n: u32) {
    let inherited: Vec<_> = state.sets[&n].deps.clone();
    let ids: Vec<u32> = state.sets.keys().copied().collect();
    for m in ids {
        let set = state.sets.get_mut(&m).unwrap();
        if set.deps.contains(&Dep::Set(n)) {
            set.deps.retain(|d| *d != Dep::Set(n));
            for d in &inherited {
                if !set.deps.contains(d) {
                    set.deps.push(d.clone());
                }
            }
            set.deps.sort();
        }
    }
}

/// The live sets of one repo, in dependency order.
pub fn live_in(state: &QueueState, repo: &str) -> Vec<u32> {
    live(state).into_iter().filter(|n| state.sets[n].repo() == repo).collect()
}
