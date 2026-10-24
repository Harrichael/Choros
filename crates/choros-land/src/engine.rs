//! Everything that changes a queue happens here, under its lock, in its sync
//! clone. The daemon's pass is level-triggered: it works out what every set
//! *should* look like from the graph, origin and GitHub, and fixes whatever
//! differs — so a crash halfway simply heals on the next pass.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, eyre};

use crate::agent::{Agent, AgentSlot};
use crate::forge::{Forge, PrRequest, PrState, RepoSettings};
use crate::git::{Git, Step};
use crate::graph;
use crate::queue::{Queue, QueueLock};
use crate::state::{
    AgentSession, BaseSpec, Composition, Dep, MergeHead, Park, ParkOp, PendingPush, QueueState, Resolution,
    SetStatus, branch,
};

pub struct Env<'a> {
    pub forge: &'a dyn Forge,
    pub agent: &'a dyn Agent,
    /// Where `~/.claude` lives, for finding sessions to fork.
    pub home: Option<PathBuf>,
}

/// Decides what happens when an operation conflicts. The daemon has none, so
/// conflicts park; `choros queue resolve` and an interactive land supply one.
pub trait ConflictHandler {
    /// A conflict is in progress in `conflict.dir`. Leave it resolved and
    /// staged and say so, or decline (the operation is then parked).
    fn resolve(&mut self, conflict: &Conflict) -> Result<Handled>;
    /// A fork being landed would conflict with queued sets `with` in the
    /// merge head. `None`: no decision can be made here.
    fn fork_conflict(&mut self, fork: u32, with: &[u32]) -> Result<Option<OnConflict>>;
}

/// Handlers own what they need; the bound keeps reborrowing one simple.
pub type Handler<'h> = Option<&'h mut (dyn ConflictHandler + 'static)>;

pub struct Conflict<'a> {
    pub queue: &'a str,
    pub dir: &'a Path,
    pub operation: String,
    pub files: Vec<String>,
    /// Earlier resolutions touching the same files, as remerge diffs.
    pub recorded: Vec<String>,
    pub slot: &'a AgentSlot,
    pub agent: &'a dyn Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    /// `used_session`: the resync agent's session was started or resumed
    /// (by the agent, or by you driving it), so it now exists to resume.
    Resolved { used_session: bool },
    Declined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnConflict {
    /// Keep the fork beside the others; resolve the clash in the merge head.
    MergeHead,
    /// Put the fork behind the sets it clashes with.
    Depend,
}

/// What one pass did, for the daemon's log and for tests.
#[derive(Debug, Default)]
pub struct PassReport {
    pub merged: Vec<u32>,
    pub closed: Vec<u32>,
    pub resynced: Vec<u32>,
    pub updated_public: Vec<u32>,
    pub opened: Vec<u32>,
    pub parked: Vec<u32>,
    /// Pushes origin refused; the old tip was kept and the next pass retries.
    pub push_failed: Vec<u32>,
    /// Other trouble the pass worked around, to be retried next pass.
    pub errors: Vec<String>,
}

/// One locked session of work on a queue.
pub struct Work<'q, 'h> {
    pub q: &'q Queue,
    pub st: QueueState,
    pub git: Git,
    pub agent: &'q dyn Agent,
    pub handler: Handler<'h>,
    _lock: QueueLock,
}

impl<'q, 'h> Work<'q, 'h> {
    /// Lock, recover the sync clone, fetch. `None` when `wait` is false and
    /// someone else holds the queue.
    pub fn begin(
        q: &'q Queue,
        agent: &'q dyn Agent,
        handler: Handler<'h>,
        wait: bool,
    ) -> Result<Option<Self>> {
        let lock = if wait {
            match q.try_lock()? {
                Some(l) => l,
                None => {
                    eprintln!("waiting for queue {} (someone is landing or resolving there)…", q.name);
                    q.lock()?
                }
            }
        } else {
            match q.try_lock()? {
                Some(l) => l,
                None => return Ok(None),
            }
        };
        q.recover()?;
        q.fetch()?;
        let mut work = Work {
            q,
            st: q.read_state()?,
            git: q.git(),
            agent,
            handler,
            _lock: lock,
        };
        work.settle_pending_pushes()?;
        Ok(Some(work))
    }

    /// A push journalled but never confirmed: keep its tip if origin has it,
    /// otherwise put the old one back.
    fn settle_pending_pushes(&mut self) -> Result<()> {
        let name = self.q.name.clone();
        let ids: Vec<u32> = self.st.sets.keys().copied().collect();
        for n in ids {
            let Some(pending) = self.st.sets[&n].pending.clone() else { continue };
            let remote = self.git.rev_parse(&format!("refs/remotes/origin/{}", branch(&name, n)))?;
            let set = self.st.sets.get_mut(&n).unwrap();
            if remote.as_deref() == Some(set.tip.as_str()) {
                set.remote = remote;
            } else {
                set.tip = pending.old_tip;
                set.base = pending.old_base;
                set.base_sha = pending.old_base_sha;
            }
            set.pending = None;
        }
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        self.q.write_state(&self.st)
    }

    pub fn main_sha(&self) -> Result<String> {
        self.git.sha(&self.q.origin_main())
    }

    /// Bring a conflicting step to a conclusion: resolved and committed
    /// (`None`), or aborted (`Some(files)`) for the caller to park.
    fn settle(&mut self, step: Step, operation: &str) -> Result<Option<Vec<String>>> {
        let Step::Conflict(files) = step else {
            return Ok(None);
        };
        // rerere replayed every hunk; it only needs checking.
        if files.is_empty() && self.git.resolution_problems()?.is_none() {
            self.git.conclude()?;
            self.record_resolution(&files)?;
            return Ok(None);
        }
        let Some(handler) = self.handler.as_deref_mut() else {
            self.git.abort_in_progress()?;
            return Ok(Some(files));
        };
        let slot = agent_slot(&self.st, self.q.config.agent.rotate_after);
        let recorded = recorded_for(&self.git, &self.st, &files);
        let handled = handler.resolve(&Conflict {
            queue: &self.q.name,
            dir: self.git.dir(),
            operation: operation.to_string(),
            files: files.clone(),
            recorded,
            slot: &slot,
            agent: self.agent,
        })?;
        if let Handled::Resolved { used_session } = handled {
            if used_session {
                record_session(&mut self.st, &slot);
            }
            if let Some(problem) = self.git.resolution_problems()? {
                self.git.abort_in_progress()?;
                return Err(eyre!("the resolution was not accepted: {problem}"));
            }
            self.git.conclude()?;
            self.record_resolution(&files)?;
            return Ok(None);
        }
        self.git.abort_in_progress()?;
        Ok(Some(files))
    }

    fn record_resolution(&mut self, files: &[String]) -> Result<()> {
        let commit = self.git.head()?;
        let files = if files.is_empty() {
            self.git.changed_files(&commit).unwrap_or_default()
        } else {
            files.to_vec()
        };
        self.st.resolutions.push(Resolution { commit, files });
        Ok(())
    }

    /// Start at `main` and merge in each tip. Tips already contained are
    /// skipped and a tip containing everything so far is fast-forwarded to,
    /// so the same inputs always give the same commit when no merge is needed.
    /// On a conflict that was not resolved: the index of the tip that clashed,
    /// and the files.
    pub fn build(&mut self, main: &str, tips: &[String], what: &str) -> Result<std::result::Result<String, (usize, Vec<String>)>> {
        self.git.checkout_detached(main)?;
        for (i, tip) in tips.iter().enumerate() {
            let head = self.git.head()?;
            if self.git.is_ancestor(tip, &head)? {
                continue;
            }
            if self.git.is_ancestor(&head, tip)? {
                self.git.checkout_detached(tip)?;
                continue;
            }
            let step = self.git.merge(tip, &format!("Merge {tip:.12} into {what}"))?;
            if let Some(files) = self.settle(step, &format!("building {what}"))? {
                return Ok(Err((i, files)));
            }
        }
        Ok(Ok(self.git.head()?))
    }

    /// Cherry-pick `commits` onto `base`, one at a time.
    pub fn replay(&mut self, base: &str, commits: &[String], what: &str) -> Result<std::result::Result<String, Vec<String>>> {
        self.git.checkout_detached(base)?;
        for c in commits {
            let step = self.git.cherry_pick(c)?;
            if let Some(files) = self.settle(step, &format!("replaying {what}"))? {
                return Ok(Err(files));
            }
        }
        Ok(Ok(self.git.head()?))
    }

    /// The base set `n` should sit on now.
    pub fn desired_base(&self, n: u32) -> Result<BaseSpec> {
        let set = &self.st.sets[&n];
        let unmerged = graph::unmerged_deps(&self.st, n);
        let tips = graph::leaves(&self.st, &unmerged)
            .into_iter()
            .map(|d| (d, self.st.sets[&d].tip.clone()))
            .collect();
        // Keep the main we already built on unless it lacks a dependency that
        // merged since; following main for its own sake would rewrite every
        // private set on every merge to main.
        let merged_points: Vec<String> = graph::merged_deps(&self.st, n)
            .into_iter()
            .filter_map(|d| self.st.sets[&d].merged_into.clone())
            .collect();
        let main = match &set.base.main {
            Some(m) if merged_points.iter().all(|p| self.git.is_ancestor(p, m).unwrap_or(false)) => {
                m.clone()
            }
            _ => self.main_sha()?,
        };
        Ok(BaseSpec {
            main: Some(main),
            deps: tips,
        })
    }

    /// Rebuild `n`'s base from `spec` and replay its own commits onto it.
    pub fn resync(&mut self, n: u32, spec: BaseSpec) -> Result<bool> {
        let set = self.st.sets[&n].clone();
        let own = self.git.commits(&set.base_sha, &set.tip)?;
        let main = spec.main.clone().ok_or_else(|| eyre!("base without main"))?;
        let tips: Vec<String> = spec.deps.iter().map(|(_, t)| t.clone()).collect();
        let what = set.branch(&self.q.name);
        let base = match self.build(&main, &tips, &format!("the base of {what}"))? {
            Ok(b) => b,
            Err((_, files)) => return self.park(n, ParkOp::BaseMerge, files, spec.key()),
        };
        let tip = match self.replay(&base, &own, &what)? {
            Ok(t) => t,
            Err(files) => return self.park(n, ParkOp::Resync, files, spec.key()),
        };
        let set = self.st.sets.get_mut(&n).unwrap();
        set.pending = Some(PendingPush {
            old_tip: std::mem::replace(&mut set.tip, tip),
            old_base: std::mem::replace(&mut set.base, spec),
            old_base_sha: std::mem::replace(&mut set.base_sha, base),
        });
        set.park = None;
        self.publish(n)
    }

    fn park(&mut self, n: u32, op: ParkOp, files: Vec<String>, on: String) -> Result<bool> {
        self.st.sets.get_mut(&n).unwrap().park = Some(Park { op, files, on });
        Ok(false)
    }

    /// Point the set's branch at its tip: always in the sync clone, and on
    /// origin when private sets are pushed or the set is public. A refused
    /// push puts back whatever the set had before (`pending`) and returns
    /// false; the rest of the pass carries on.
    pub fn publish(&mut self, n: u32) -> Result<bool> {
        let set = &self.st.sets[&n];
        let wanted = self.q.config.push.private_sets || set.pr.is_some() || set.remote.is_some();
        if wanted && set.remote.as_deref() != Some(set.tip.as_str()) {
            // The journal entry must reach disk before origin can change.
            self.save()?;
            let set = &self.st.sets[&n];
            let pushed = self
                .git
                .push_lease("origin", &set.tip, &set.branch(&self.q.name), set.remote.as_deref());
            let set = self.st.sets.get_mut(&n).unwrap();
            match pushed {
                Ok(()) => set.remote = Some(set.tip.clone()),
                Err(_) => {
                    if let Some(p) = set.pending.take() {
                        set.tip = p.old_tip;
                        set.base = p.old_base;
                        set.base_sha = p.old_base_sha;
                    }
                    self.set_local_ref(n)?;
                    return Ok(false);
                }
            }
        }
        self.st.sets.get_mut(&n).unwrap().pending = None;
        self.set_local_ref(n)?;
        Ok(true)
    }

    fn set_local_ref(&self, n: u32) -> Result<()> {
        let set = &self.st.sets[&n];
        self.git
            .run(&["update-ref", &format!("refs/heads/{}", set.branch(&self.q.name)), &set.tip])?;
        Ok(())
    }

    /// Note merges and closes on GitHub.
    pub fn observe_prs(&mut self, forge: &dyn Forge, report: &mut PassReport) -> Result<()> {
        let mut main = self.main_sha()?;
        let mut refetched = false;
        let ids: Vec<u32> = self.st.sets.keys().copied().collect();
        for n in ids {
            let set = &self.st.sets[&n];
            let Some(pr) = &set.pr else { continue };
            if !matches!(set.status, SetStatus::Queued | SetStatus::Closed) {
                continue;
            }
            let state = forge.pr_state(self.git.dir(), pr.number)?;
            // A merge that happened after this pass fetched would leave main
            // without it, and dependents would keep a base that lacks it.
            if state == PrState::Merged && !refetched {
                self.q.fetch()?;
                main = self.main_sha()?;
                refetched = true;
            }
            let set = self.st.sets.get_mut(&n).unwrap();
            match (state, &set.status) {
                (PrState::Merged, _) => {
                    set.status = SetStatus::Merged;
                    set.merged_into = Some(main.clone());
                    set.park = None;
                    report.merged.push(n);
                }
                (PrState::Closed, SetStatus::Queued) => {
                    set.status = SetStatus::Closed;
                    report.closed.push(n);
                }
                (PrState::Open, SetStatus::Closed) => set.status = SetStatus::Queued,
                _ => {}
            }
        }
        Ok(())
    }

    /// Commits pushed to a set's branch from elsewhere — review fixes from
    /// its landing folder — become part of the set. A rewrite does not.
    pub fn observe_pushes(&mut self) -> Result<()> {
        let name = self.q.name.clone();
        let ids: Vec<u32> = self.st.sets.keys().copied().collect();
        for n in ids {
            let set = &self.st.sets[&n];
            if !set.is_open() {
                continue;
            }
            let Some(seen) = set.remote.clone() else { continue };
            let remote = self
                .git
                .rev_parse(&format!("refs/remotes/origin/{}", branch(&name, n)))?;
            let set = self.st.sets.get_mut(&n).unwrap();
            match remote {
                None => set.remote = None,
                Some(r) if r == seen => {}
                // A merge commit cannot be replayed with its set; public sets
                // are never replayed, so only a private one has to stop.
                Some(r) if set.pr.is_none() && self.git.is_ancestor(&set.tip, &r)? && self.git.has_merges(&set.tip, &r)? => {
                    set.park = Some(Park {
                        op: ParkOp::PushedMerge,
                        files: Vec::new(),
                        on: r,
                    });
                }
                Some(r) => {
                    if self.git.is_ancestor(&set.tip, &r)? {
                        set.tip = r.clone();
                        set.remote = Some(r);
                        if set.park.as_ref().is_some_and(|p| p.op == ParkOp::Diverged) {
                            set.park = None;
                        }
                    } else {
                        set.park = Some(Park {
                            op: ParkOp::Diverged,
                            files: Vec::new(),
                            on: r,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// A public set is only ever added to: main is merged in when the set
    /// conflicts with it or the repo insists on up-to-date branches. `None`:
    /// nothing was needed; otherwise whether it went through (a park or a
    /// refused push is `Some(false)`).
    pub fn update_public(&mut self, n: u32, settings: &RepoSettings) -> Result<Option<bool>> {
        let main = self.main_sha()?;
        let tip = self.st.sets[&n].tip.clone();
        let behind = !self.git.is_ancestor(&main, &tip)?;
        let needed = behind
            && (settings.require_up_to_date || !self.git.merge_conflicts(&main, &tip)?.is_empty());
        if !needed {
            return Ok(None);
        }
        let what = branch(&self.q.name, n);
        self.git.checkout_detached(&tip)?;
        let step = self
            .git
            .merge(&main, &format!("Merge {} into {what}", self.q.config.main))?;
        if let Some(files) = self.settle(step, &format!("merging {} into {what}", self.q.config.main))? {
            return self.park(n, ParkOp::MainIntoPublic, files, main).map(Some);
        }
        let new_tip = self.git.head()?;
        let set = self.st.sets.get_mut(&n).unwrap();
        set.pending = Some(PendingPush {
            old_tip: std::mem::replace(&mut set.tip, new_tip),
            old_base: set.base.clone(),
            old_base_sha: set.base_sha.clone(),
        });
        // A set parked out of the merge head stays parked; that is another matter.
        if set.park.as_ref().is_some_and(|p| p.op == ParkOp::MainIntoPublic) {
            set.park = None;
        }
        self.publish(n).map(Some)
    }

    /// Rebuild the merge head if what it should contain changed. A leaf that
    /// clashes with the others and is not resolved here is parked out of it
    /// (with what it built on), so one clash never stops the rest of the
    /// queue — or anyone landing on it.
    pub fn rebuild_merge_head(&mut self) -> Result<()> {
        // What the merge head would hold with every set in: a set parked out
        // of it is retried only once that has changed.
        let parked_out: Vec<(u32, Park)> = self
            .st
            .sets
            .iter()
            .filter_map(|(n, s)| s.park.clone().filter(|p| p.op == ParkOp::MergeHead).map(|p| (*n, p)))
            .collect();
        for (n, _) in &parked_out {
            self.st.sets.get_mut(n).unwrap().park = None;
        }
        let main = self.main_sha()?;
        let full = self.composition(&main).key();
        for (n, park) in parked_out {
            if park.on == full && self.handler.is_none() {
                self.st.sets.get_mut(&n).unwrap().park = Some(park);
            }
        }

        let what = format!("{}/merge-head", self.q.name);
        loop {
            let composition = self.composition(&main);
            if self.st.merge_head.as_ref().is_some_and(|m| m.composition == composition) {
                return Ok(());
            }
            let tips: Vec<String> = composition.sets.iter().map(|(_, t)| t.clone()).collect();
            match self.build(&main, &tips, &what)? {
                Ok(sha) => {
                    self.git
                        .run(&["update-ref", &format!("refs/heads/{what}"), &sha])?;
                    if self.q.config.push.merge_head {
                        self.git.push_force("origin", &sha, &what)?;
                    }
                    self.st.merge_head = Some(MergeHead { sha, composition });
                    return Ok(());
                }
                Err((i, files)) => {
                    let n = self.clash_culprit(&composition, i)?;
                    self.st.sets.get_mut(&n).unwrap().park = Some(Park {
                        op: ParkOp::MergeHead,
                        files,
                        on: full.clone(),
                    });
                }
            }
        }
    }

    /// Leaf `i` would not merge onto the leaves before it. Of the pair that
    /// clash, park the one that was knowingly landed beside the other's work
    /// (it lists it, or something under it, in `conflicts_with`); failing
    /// that, the newer one, since the older was there first.
    fn clash_culprit(&self, composition: &Composition, i: usize) -> Result<u32> {
        let (n, tip) = &composition.sets[i];
        let Some((m, _)) = composition.sets[..i]
            .iter()
            .find(|(_, t)| !self.git.merge_conflicts(t, tip).unwrap_or_default().is_empty())
        else {
            return Ok(*n);
        };
        let knows = |a: u32, b: u32| {
            let mut under = graph::ancestors(&self.st, b);
            under.insert(b);
            self.st.sets[&a].conflicts_with.iter().any(|c| under.contains(c))
        };
        Ok(if knows(*m, *n) {
            *m
        } else if knows(*n, *m) {
            *n
        } else {
            (*n).max(*m)
        })
    }

    /// The merge head's contents: main and the live leaves.
    fn composition(&self, main: &str) -> Composition {
        let live = graph::live(&self.st);
        Composition {
            main: main.to_string(),
            sets: graph::leaves(&self.st, &live)
                .into_iter()
                .map(|n| (n, self.st.sets[&n].tip.clone()))
                .collect(),
        }
    }

    pub fn open_pr(&mut self, n: u32, forge: &dyn Forge) -> Result<()> {
        let set = self.st.sets[&n].clone();
        let branch = set.branch(&self.q.name);
        if set.remote.as_deref() != Some(set.tip.as_str()) {
            // A PR needs its branch on origin, whatever private_sets says.
            self.save()?;
            self.git.push_lease("origin", &set.tip, &branch, set.remote.as_deref())?;
            self.st.sets.get_mut(&n).unwrap().remote = Some(set.tip.clone());
        }
        let title = match &set.title {
            Some(t) => t.clone(),
            None => {
                let first = self
                    .git
                    .commits(&set.base_sha, &set.tip)?
                    .into_iter()
                    .next()
                    .ok_or_else(|| eyre!("{branch} has no commits of its own"))?;
                self.git.subject(&first)?
            }
        };
        // A PR opened by a pass that died before saving is still there; GitHub
        // would refuse a second one for the same branch.
        if let Some(pr) = forge.find_open_pr(self.git.dir(), &branch)? {
            self.st.sets.get_mut(&n).unwrap().pr = Some(pr);
            return Ok(());
        }
        let pr = forge.create_pr(
            self.git.dir(),
            &PrRequest {
                head: &branch,
                base: &self.q.config.main,
                title: &title,
                body: set.body.as_deref().unwrap_or(""),
                draft: self.q.config.pr.draft,
            },
        )?;
        self.st.sets.get_mut(&n).unwrap().pr = Some(pr);
        Ok(())
    }

    /// Whether a dependency in another queue has merged.
    pub fn cross_merged(&self) -> impl Fn(&str, u32) -> bool + '_ {
        |queue, n| {
            Queue::open(&self.q.trunk, queue)
                .and_then(|q| q.read_state())
                .ok()
                .and_then(|s| s.sets.get(&n).map(|set| set.status == SetStatus::Merged))
                .unwrap_or(false)
        }
    }

    /// The daemon's pass, minus the lock and fetch `begin` already did.
    pub fn pass(&mut self, forge: &dyn Forge) -> Result<PassReport> {
        let mut report = PassReport::default();
        self.observe_prs(forge, &mut report)?;
        self.observe_pushes()?;

        let mut settings: Option<RepoSettings> = None;
        for n in graph::topo(&self.st) {
            if graph::ancestors(&self.st, n)
                .into_iter()
                .any(|a| self.st.sets.get(&a).is_some_and(|s| s.is_open()) && graph::blocked(&self.st, a))
            {
                continue;
            }
            let set = &self.st.sets[&n];
            if set.status != SetStatus::Queued {
                continue;
            }
            if set.is_public() {
                if set.park.as_ref().is_some_and(|p| {
                    p.op == ParkOp::MainIntoPublic && p.on == self.main_sha().unwrap_or_default()
                }) {
                    continue;
                }
                if settings.is_none() {
                    settings = Some(forge.settings(self.git.dir(), &self.q.config.main)?);
                }
                match self.update_public(n, settings.as_ref().unwrap())? {
                    None => {}
                    Some(true) => report.updated_public.push(n),
                    Some(false) if self.st.sets[&n].park.is_some() => report.parked.push(n),
                    Some(false) => report.push_failed.push(n),
                }
                continue;
            }
            if set.park.as_ref().is_some_and(|p| matches!(p.op, ParkOp::Diverged | ParkOp::PushedMerge | ParkOp::MergeHead)) {
                continue;
            }
            let spec = self.desired_base(n)?;
            let set = &self.st.sets[&n];
            if set.park.as_ref().is_some_and(|p| p.on == spec.key()) && self.handler.is_none() {
                continue;
            }
            if spec == set.base && set.park.is_none() {
                continue;
            }
            if self.resync(n, spec)? {
                report.resynced.push(n);
            } else if self.st.sets[&n].park.is_some() {
                report.parked.push(n);
            } else {
                report.push_failed.push(n);
            }
        }

        self.rebuild_merge_head()?;
        let parked_out: Vec<u32> = self
            .st
            .sets
            .iter()
            .filter(|(n, s)| s.park.as_ref().is_some_and(|p| p.op == ParkOp::MergeHead) && !report.parked.contains(n))
            .map(|(n, _)| *n)
            .collect();
        report.parked.extend(parked_out);

        let cross = |queue: &str, n: u32| self.cross_merged()(queue, n);
        let front: Vec<u32> = graph::front(&self.st, &cross)
            .into_iter()
            .filter(|n| self.st.sets[n].pr.is_none())
            .collect();
        for n in front {
            // A set reaches the front on a base of main; one still built on
            // its merged dependencies waits for the re-sync above.
            if !self.st.sets[&n].base.deps.is_empty() {
                continue;
            }
            match self.open_pr(n, forge) {
                Ok(()) => report.opened.push(n),
                Err(e) => report.errors.push(format!("landing-{n}: opening its PR failed: {e}")),
            }
        }
        self.tend_landings(&report);
        Ok(report)
    }

    /// Keep landing folders in step: a clean one follows its set when the
    /// engine moves it; a merged set's folder goes to the archive. Dirty
    /// folders are left alone (status flags them). Best-effort: a landing
    /// folder is a convenience, never a reason to fail a pass.
    fn tend_landings(&self, report: &PassReport) {
        let repo = &self.q.config.repo;
        for n in report.resynced.iter().chain(&report.updated_public) {
            let dir = self.q.landing_dir(*n).join(repo);
            if !dir.exists() {
                continue;
            }
            let landing = Git::new(&dir);
            let branch = branch(&self.q.name, *n);
            let on_branch = landing
                .run(&["branch", "--show-current"])
                .is_ok_and(|b| b == branch);
            if !on_branch || !landing.is_clean_tracked().unwrap_or(false) {
                continue;
            }
            let refspec = format!("+refs/heads/{branch}:refs/choros/latest");
            if landing.fetch(self.git.dir().to_str().unwrap_or_default(), &[&refspec]).is_ok() {
                let _ = landing.reset_hard("refs/choros/latest");
            }
        }
        for n in &report.merged {
            let src = self.q.landing_dir(*n);
            let dst = self.q.archived_landing_dir(*n);
            if src.exists() && !dst.exists() {
                if let Some(parent) = dst.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::rename(&src, &dst);
            }
        }
    }

    /// Mark every park as worth retrying, for `choros queue resolve`.
    pub fn unpark_conflicts(&mut self) {
        for set in self.st.sets.values_mut() {
            if set.park.as_ref().is_some_and(|p| p.op != ParkOp::Diverged) {
                set.park = None;
            }
        }
    }

    /// Settle a set whose branch was rewritten on origin by taking origin's
    /// version, provided it still sits on the set's base.
    pub fn accept_diverged(&mut self, n: u32) -> Result<()> {
        let set = self.st.sets.get_mut(&n).unwrap();
        let Some(Park { op: ParkOp::Diverged, on, .. }) = set.park.clone() else {
            return Ok(());
        };
        if !self.git.is_ancestor(&set.base_sha, &on)? {
            return Err(eyre!(
                "origin's {} no longer sits on its base; drop the set or fix the branch",
                set.branch(&self.q.name)
            ));
        }
        set.tip = on.clone();
        set.remote = Some(on);
        set.park = None;
        Ok(())
    }

    pub fn next_dep_set(&self) -> Vec<Dep> {
        graph::live(&self.st).into_iter().map(Dep::Set).collect()
    }
}

fn agent_slot(st: &QueueState, rotate_after: u32) -> AgentSlot {
    match &st.agent {
        Some(s) if s.runs < rotate_after => AgentSlot {
            id: s.id.clone(),
            fresh: false,
        },
        _ => AgentSlot {
            id: crate::uuid::v4(),
            fresh: true,
        },
    }
}

/// Only a session that was actually started is worth resuming next time.
fn record_session(st: &mut QueueState, slot: &AgentSlot) {
    if !slot.fresh {
        if let Some(s) = &mut st.agent {
            s.runs += 1;
        }
        return;
    }
    let mut previous = Vec::new();
    if let Some(old) = st.agent.take() {
        previous = old.previous;
        previous.push(old.id);
    }
    st.agent = Some(AgentSession {
        id: slot.id.clone(),
        runs: 1,
        previous,
    });
}

/// Earlier resolutions that touched any of `files`, newest first.
fn recorded_for(git: &Git, st: &QueueState, files: &[String]) -> Vec<String> {
    let wanted: BTreeSet<&String> = files.iter().collect();
    st.resolutions
        .iter()
        .rev()
        .filter(|r| r.files.iter().any(|f| wanted.contains(f)))
        .take(3)
        .filter_map(|r| git.remerge_diff(&r.commit).ok())
        .collect()
}
