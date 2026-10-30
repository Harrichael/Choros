//! Everything that changes a queue happens here, under its lock, in its sync
//! clone. The daemon's pass is level-triggered: it works out what every set
//! *should* look like from the graph, origin and GitHub, and fixes whatever
//! differs — so a crash halfway simply heals on the next pass.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, eyre};

use crate::agent::{Agent, AgentSlot};
use crate::forge::{Forge, PrRequest, PrState, RepoSettings};
use crate::git::{Git, Step};
use crate::graph;
use crate::queue::{Queue, QueueLock};
use crate::state::{
    AgentSession, BaseSpec, LandingSet, Composition, Dep, MergeHead, Park, ParkOp, PendingPush, QueueState, RepoState,
    Resolution, SetStatus, branch,
};

/// How long after a merge GitHub is left to rebase the stack above it
/// before Choros replays what it left alone. A long stack takes GitHub
/// longer; its pushes are taken whenever they come.
pub const RESTACK_GRACE_SECS: u64 = 300;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

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
    /// Whether to try parked operations again though nothing under them
    /// changed. Someone asking at a terminal, yes; the daemon, which had
    /// its try when the conflict came up, no.
    fn retries_parked(&self) -> bool {
        true
    }
}

/// Handlers own what they need; the bound keeps reborrowing one simple.
pub type Handler<'h> = Option<&'h mut (dyn ConflictHandler + 'static)>;

pub struct Conflict<'a> {
    pub queue: &'a str,
    pub repo: &'a str,
    pub dir: &'a Path,
    pub operation: String,
    pub files: Vec<String>,
    /// Earlier resolutions touching the same files, as remerge diffs.
    pub recorded: Vec<String>,
    /// The sets whose changes meet in `files`, and what each is for.
    pub sides: Vec<crate::context::Side>,
    pub slot: &'a AgentSlot,
    pub agent: &'a dyn Agent,
    pub settings: &'a crate::state::AgentConfig,
}

impl Conflict<'_> {
    pub fn brief(&self) -> crate::agent::ConflictBrief<'_> {
        crate::agent::ConflictBrief {
            dir: self.dir,
            operation: &self.operation,
            files: &self.files,
            recorded: &self.recorded,
            sides: &self.sides,
            session: self.slot,
            model: &self.settings.model,
            timeout: std::time::Duration::from_secs(u64::from(self.settings.timeout_minutes) * 60),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    /// `used_session`: the resync agent's session was started or resumed
    /// (by the agent, or by you driving it), so it now exists to resume.
    Resolved { used_session: bool },
    /// It had its go and left the conflict as it was.
    Declined,
    /// It didn't look at it this time; the park waits for a later go.
    Deferred,
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
    /// Opened as stacked PRs, behind the front.
    pub stacked: Vec<u32>,
    /// Stacked sets whose branch GitHub rebased after a merge, taken as is.
    pub adopted: Vec<u32>,
    /// Stacked sets left empty by a replay, folded into the set below.
    pub folded: Vec<u32>,
    /// Merged without a PR: every change in them was already on main.
    pub already_on_main: Vec<u32>,
    pub parked: Vec<u32>,
    /// Pushes origin refused; the old tip was kept and the next pass retries.
    pub push_failed: Vec<u32>,
    /// The diffbase is parked, so nothing else moved.
    pub diffbase_parked: bool,
    /// Other trouble the pass worked around, to be retried next pass.
    pub errors: Vec<String>,
}

/// One locked session of work on a queue. Git work happens in the sync clone
/// of whichever repo the set or merge head at hand is in.
pub struct Work<'q, 'h> {
    pub q: &'q Queue,
    pub st: QueueState,
    pub agent: &'q dyn Agent,
    pub handler: Handler<'h>,
    /// Whether the handler had a go at the conflict the last build or
    /// replay stopped on: what a park made from it records.
    conflict_tried: bool,
    /// Stacked sets the last replay left with nothing of their own; see
    /// `fold_into_below`.
    emptied: BTreeSet<u32>,
    _lock: QueueLock,
}

impl<'q, 'h> Work<'q, 'h> {
    /// Lock, recover the sync clones, fetch. `None` when `wait` is false and
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
        let st = q.read_state()?;
        for repo in st.repos.keys() {
            q.recover(repo)?;
            q.fetch(repo)?;
        }
        let mut work = Work {
            q,
            st,
            agent,
            handler,
            conflict_tried: false,
            emptied: BTreeSet::new(),
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
            let remote = self
                .set_git(n)
                .rev_parse(&format!("refs/remotes/origin/{}", branch(&name, n)))?;
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

    /// The sync clone of `repo`.
    pub fn git(&self, repo: &str) -> Git {
        self.q.git(repo)
    }

    /// The sync clone of set `n`'s repo.
    pub fn set_git(&self, n: u32) -> Git {
        self.git(self.st.sets[&n].repo())
    }

    /// Ready the queue to take `repo`; see [`Queue::ensure_repo`].
    pub fn ensure_repo(&mut self, repo: &str) -> Result<()> {
        if !self.st.repos.contains_key(repo) {
            self.q.ensure_repo(&mut self.st, repo)?;
            self.q.fetch(repo)?;
            self.save()?;
        }
        Ok(())
    }

    fn repo_state(&self, repo: &str) -> Result<&RepoState> {
        self.st
            .repos
            .get(repo)
            .ok_or_else(|| eyre!("queue {} has nothing in {repo}", self.q.name))
    }

    /// The branch `repo`'s work lands on.
    pub fn main_branch(&self, repo: &str) -> Result<String> {
        Ok(self.repo_state(repo)?.main.clone())
    }

    pub fn main_sha(&self, repo: &str) -> Result<String> {
        self.git(repo)
            .sha(&format!("refs/remotes/origin/{}", self.main_branch(repo)?))
    }

    /// Bring a conflicting step in `repo`'s sync clone to a conclusion:
    /// resolved and committed (`None`), or aborted (`Some(files)`) for the
    /// caller to park.
    fn settle(&mut self, repo: &str, step: Step, operation: &str) -> Result<Option<Vec<String>>> {
        let Step::Conflict(files) = step else {
            return Ok(None);
        };
        let git = self.git(repo);
        // rerere replayed every hunk; it only needs checking.
        if files.is_empty() && git.resolution_problems()?.is_none() {
            git.conclude()?;
            self.record_resolution(repo, &files)?;
            return Ok(None);
        }
        self.conflict_tried = false;
        let Some(handler) = self.handler.as_deref_mut() else {
            git.abort_in_progress()?;
            return Ok(Some(files));
        };
        let rs = self.st.repos.get(repo).ok_or_else(|| eyre!("queue {} has nothing in {repo}", self.q.name))?;
        let slot = agent_slot(rs, self.q.config.agent.rotate_after);
        let recorded = recorded_for(&git, rs, &files);
        let sides = crate::context::sides(self.q, &self.st, repo, &files);
        let handled = handler.resolve(&Conflict {
            queue: &self.q.name,
            repo,
            dir: git.dir(),
            operation: operation.to_string(),
            files: files.clone(),
            recorded,
            sides,
            slot: &slot,
            agent: self.agent,
            settings: &self.q.config.agent,
        })?;
        if let Handled::Resolved { used_session } = handled {
            if used_session {
                record_session(self.st.repos.get_mut(repo).unwrap(), &slot);
            }
            if let Some(problem) = git.resolution_problems()? {
                git.abort_in_progress()?;
                return Err(eyre!("the resolution was not accepted: {problem}"));
            }
            git.conclude()?;
            self.record_resolution(repo, &files)?;
            return Ok(None);
        }
        self.conflict_tried = handled == Handled::Declined;
        git.abort_in_progress()?;
        Ok(Some(files))
    }

    fn record_resolution(&mut self, repo: &str, files: &[String]) -> Result<()> {
        let git = self.git(repo);
        let commit = git.head()?;
        let files = if files.is_empty() {
            git.changed_files(&commit).unwrap_or_default()
        } else {
            files.to_vec()
        };
        if let Some(rs) = self.st.repos.get_mut(repo) {
            rs.resolutions.push(Resolution { commit, files });
        }
        Ok(())
    }

    /// Start at `main` and merge in each tip. Tips already contained are
    /// skipped and a tip containing everything so far is fast-forwarded to,
    /// so the same inputs always give the same commit when no merge is needed.
    /// On a conflict that was not resolved: the index of the tip that clashed,
    /// and the files.
    pub fn build(
        &mut self,
        repo: &str,
        main: &str,
        tips: &[String],
        what: &str,
    ) -> Result<std::result::Result<String, (usize, Vec<String>)>> {
        let git = self.git(repo);
        git.checkout_detached(main)?;
        for (i, tip) in tips.iter().enumerate() {
            let head = git.head()?;
            if git.is_ancestor(tip, &head)? {
                continue;
            }
            if git.is_ancestor(&head, tip)? {
                git.checkout_detached(tip)?;
                continue;
            }
            let step = git.merge(tip, &format!("Merge {tip:.12} into {what}"))?;
            if let Some(files) = self.settle(repo, step, &format!("building {what}"))? {
                return Ok(Err((i, files)));
            }
        }
        Ok(Ok(git.head()?))
    }

    /// Cherry-pick `commits` onto `base`, one at a time, dropping any that
    /// change nothing there: their change already reached the base, and a
    /// PR listing them would carry their messages for nothing.
    pub fn replay(
        &mut self,
        repo: &str,
        base: &str,
        commits: &[String],
        what: &str,
    ) -> Result<std::result::Result<String, Vec<String>>> {
        let git = self.git(repo);
        git.checkout_detached(base)?;
        for c in commits {
            let step = git.cherry_pick(c)?;
            if let Some(files) = self.settle(repo, step, &format!("replaying {what} onto its new base"))? {
                return Ok(Err(files));
            }
            if git.is_empty_commit("HEAD")? {
                git.reset_hard("HEAD^")?;
            }
        }
        Ok(Ok(git.head()?))
    }

    /// The base set `n` should sit on now.
    pub fn desired_base(&self, n: u32) -> Result<BaseSpec> {
        let set = &self.st.sets[&n];
        let repo = set.repo();
        let git = self.git(repo);
        let unmerged = graph::unmerged_deps(&self.st, n);
        let tips = graph::leaves(&self.st, &unmerged)
            .into_iter()
            .map(|d| (d, self.st.sets[&d].tip.clone()))
            .collect();
        // Keep the main we already built on unless it lacks a dependency that
        // merged since; following main for its own sake would rewrite every
        // private set on every merge to main. With a diffbase there is nothing
        // to keep: the diffbase only moves when it has to.
        let merged_points: Vec<String> = graph::merged_deps(&self.st, n)
            .into_iter()
            .filter_map(|d| self.st.sets[&d].merged_into.clone())
            .collect();
        let main = match &set.base.main {
            Some(m)
                if self.repo_state(repo)?.diffbase.is_none()
                    && git.is_ancestor(m, &self.main_sha(repo)?)?
                    && merged_points.iter().all(|p| git.is_ancestor(p, m).unwrap_or(false)) =>
            {
                m.clone()
            }
            _ => self.root(repo)?,
        };
        Ok(BaseSpec {
            main: Some(main),
            deps: tips,
        })
    }

    /// Rebuild `n`'s base from `spec` and replay its own commits onto it.
    pub fn resync(&mut self, n: u32, spec: BaseSpec) -> Result<bool> {
        let set = self.st.sets[&n].clone();
        let repo = set.repo();
        let own = self.git(repo).commits(&set.base_sha, &set.tip)?;
        let main = spec.main.clone().ok_or_else(|| eyre!("base without main"))?;
        let tips: Vec<String> = spec.deps.iter().map(|(_, t)| t.clone()).collect();
        let what = set.branch(&self.q.name);
        let base = match self.build(repo, &main, &tips, &format!("the base of {what}"))? {
            Ok(b) => b,
            Err((_, files)) => return self.park(n, ParkOp::BaseMerge, files, spec.key()),
        };
        let tip = match self.replay(repo, &base, &own, &what)? {
            Ok(t) => t,
            Err(files) => return self.park(n, ParkOp::Resync, files, spec.key()),
        };
        // GitHub closes a PR left with nothing of its own, and won't merge a
        // stack through a closed PR; so the empty branch is never pushed.
        if tip == base && graph::stacked(&self.st, n) {
            self.emptied.insert(n);
            return Ok(false);
        }
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
        let tried = self.conflict_tried;
        self.st.sets.get_mut(&n).unwrap().park = Some(Park { op, files, on, tried });
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
                .set_git(n)
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
        self.set_git(n)
            .run(&["update-ref", &format!("refs/heads/{}", set.branch(&self.q.name)), &set.tip])?;
        Ok(())
    }

    /// Note merges, closes and reviews on GitHub.
    pub fn observe_prs(&mut self, forge: &dyn Forge, report: &mut PassReport) -> Result<()> {
        let mut refetched = BTreeSet::new();
        let ids: Vec<u32> = self.st.sets.keys().copied().collect();
        for n in ids {
            let set = &self.st.sets[&n];
            let Some(pr) = &set.pr else { continue };
            if !matches!(set.status, SetStatus::Queued | SetStatus::Closed) {
                continue;
            }
            let repo = set.repo().to_string();
            let state = forge.pr_state(self.git(&repo).dir(), pr.number)?;
            // A merge that happened after this pass fetched would leave main
            // without it, and dependents would keep a base that lacks it.
            if state == PrState::Merged && refetched.insert(repo.clone()) {
                self.q.fetch(&repo)?;
            }
            let main = self.main_sha(&repo)?;
            let set = self.st.sets.get_mut(&n).unwrap();
            match (state, &set.status) {
                (PrState::Merged, _) => {
                    set.status = SetStatus::Merged;
                    set.merged_into = Some(main);
                    set.merged_seen_at = Some(now_secs());
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
            if set.status == SetStatus::Queued {
                let number = set.pr.as_ref().unwrap().number;
                let review = forge.pr_review(self.git(&repo).dir(), number)?;
                self.st.sets.get_mut(&n).unwrap().review = Some(review);
            }
        }
        Ok(())
    }

    /// Commits pushed to a set's branch from elsewhere — review fixes from
    /// its landing folder — become part of the set. A rewrite does not,
    /// unless it is GitHub rebasing a stack after a merge. Dependencies
    /// first, so a set sees the tip adopted under it.
    pub fn observe_pushes(&mut self, report: &mut PassReport) -> Result<()> {
        let name = self.q.name.clone();
        for n in graph::topo(&self.st) {
            let set = &self.st.sets[&n];
            if !set.is_open() {
                continue;
            }
            let Some(seen) = set.remote.clone() else { continue };
            let git = self.set_git(n);
            let remote = git.rev_parse(&format!("refs/remotes/origin/{}", branch(&name, n)))?;
            let public = graph::public(&self.st, n);
            if let Some(r) = &remote
                && *r != seen
                && !git.is_ancestor(&self.st.sets[&n].tip, r)?
                && let Some((base, base_sha)) = self.rebased_by_github(n, r)?
            {
                let set = self.st.sets.get_mut(&n).unwrap();
                set.base = base;
                set.base_sha = base_sha;
                set.tip = r.clone();
                set.remote = Some(r.clone());
                if set.park.as_ref().is_some_and(|p| p.op == ParkOp::Diverged) {
                    set.park = None;
                }
                report.adopted.push(n);
                continue;
            }
            let set = self.st.sets.get_mut(&n).unwrap();
            match remote {
                None => set.remote = None,
                Some(r) if r == seen => {}
                // A merge commit cannot be replayed with its set; public sets
                // are never replayed, so only the others have to stop.
                Some(r) if !public && git.is_ancestor(&set.tip, &r)? && git.has_merges(&set.tip, &r)? => {
                    set.park = Some(Park {
                        op: ParkOp::PushedMerge,
                        files: Vec::new(),
                        on: r,
                        tried: false,
                    });
                }
                Some(r) => {
                    if git.is_ancestor(&set.tip, &r)? {
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
                            tried: false,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// When a set merges, GitHub rebases the linked branches above it onto
    /// what is left, dropping the merged commits. `r`, the branch's new tip,
    /// is taken as that rebase when it sits where the graph says the set
    /// now sits, and that is somewhere new. Once a set below has merged, a
    /// rewrite onto the right base is taken even if Choros got there first,
    /// since GitHub may push after Choros replayed the set. The base to
    /// record is what `desired_base` would give, so the next pass leaves the
    /// PR alone.
    fn rebased_by_github(&self, n: u32, r: &str) -> Result<Option<(BaseSpec, String)>> {
        let set = &self.st.sets[&n];
        if set.pr.is_none() || set.stack.is_none() {
            return Ok(None);
        }
        let git = self.set_git(n);
        let merged_below: Vec<&LandingSet> = graph::ancestors(&self.st, n)
            .iter()
            .map(|a| &self.st.sets[a])
            .filter(|a| a.status == SetStatus::Merged && a.merged_seen_at.is_some())
            .collect();
        match graph::sits_on(&self.st, n).as_slice() {
            // The set it sat on merged: GitHub moved it onto main.
            [] => {
                let main = self.main_sha(set.repo())?;
                let x = git.run(&["merge-base", r, &main])?;
                let moved_on = match &set.base.main {
                    Some(m) => *m != x && git.is_ancestor(m, &x)?,
                    None => x != set.base_sha,
                };
                // After Choros's own replay the base names no merged set, and
                // main may have moved on since GitHub's rebase: measure it
                // against the merged sets instead.
                let late = !moved_on && !merged_below.is_empty() && {
                    let mut past = true;
                    for a in &merged_below {
                        past &= a.base_sha != x && git.is_ancestor(&a.base_sha, &x)?;
                    }
                    past
                };
                // A merge commit leaves the merged set's commits on main, so
                // still holding them is fine; holding them off main is not.
                let mut dropped_merged = true;
                let held = set.base.deps.iter().map(|(_, t)| t).chain(merged_below.iter().map(|a| &a.tip));
                for t in held {
                    dropped_merged &= !git.is_ancestor(t, r)? || git.is_ancestor(t, &x)?;
                }
                Ok(((moved_on || late) && dropped_merged).then(|| {
                    let base = BaseSpec {
                        main: Some(x.clone()),
                        deps: Vec::new(),
                    };
                    (base, x)
                }))
            }
            // A set further down merged: GitHub moved the one under it too.
            [p] => {
                let under = self.st.sets[p].tip.clone();
                let was = set.base.deps.iter().find(|(d, _)| d == p).map(|(_, t)| t.as_str());
                let moved = (was != Some(under.as_str()) || !merged_below.is_empty()) && git.is_ancestor(&under, r)?;
                Ok(moved.then(|| {
                    let base = BaseSpec {
                        main: set.base.main.clone(),
                        deps: vec![(*p, under.clone())],
                    };
                    (base, under)
                }))
            }
            _ => Ok(None),
        }
    }

    /// Whether GitHub may still be rebasing linked `n` after a merge below.
    fn restacking(&self, n: u32) -> bool {
        let now = now_secs();
        self.st.sets[&n].stack.is_some()
            && graph::ancestors(&self.st, n).iter().any(|a| {
                let a = &self.st.sets[a];
                a.status == SetStatus::Merged
                    && a.merged_seen_at.is_some_and(|t| now.saturating_sub(t) < RESTACK_GRACE_SECS)
            })
    }

    /// Which GitHub stack each PR is in, and what its PR is based on. A
    /// linked PR is pointed at main by GitHub when the set below merges.
    pub fn observe_stacks(&mut self, forge: &dyn Forge) -> Result<()> {
        let repos: BTreeSet<String> = self
            .st
            .sets
            .values()
            .filter(|s| s.is_open() && s.pr.is_some())
            .filter(|s| self.q.config.stack.enabled || s.pr_base.is_some() || s.stack.is_some())
            .map(|s| s.repo().to_string())
            .collect();
        for repo in repos {
            let stacks = forge.stacks(self.git(&repo).dir())?;
            let main = self.main_branch(&repo)?;
            for set in self.st.sets.values_mut().filter(|s| s.is_open() && s.repo() == repo) {
                let Some(pr) = &set.pr else { continue };
                let found = stacks
                    .iter()
                    .find_map(|s| s.prs.iter().find(|(p, _)| *p == pr.number).map(|(_, base)| (s.number, base)));
                set.stack = found.map(|(s, _)| s);
                if let Some((_, base)) = found {
                    set.pr_base = (*base != main).then(|| base.clone());
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
        let repo = self.st.sets[&n].repo().to_string();
        let git = self.git(&repo);
        let main = self.main_sha(&repo)?;
        let tip = self.st.sets[&n].tip.clone();
        let behind = !git.is_ancestor(&main, &tip)?;
        let needed = behind && (settings.require_up_to_date || !git.merge_conflicts(&main, &tip)?.is_empty());
        if !needed {
            return Ok(None);
        }
        let what = branch(&self.q.name, n);
        let main_branch = self.main_branch(&repo)?;
        git.checkout_detached(&tip)?;
        let step = git.merge(&main, &format!("Merge {main_branch} into {what}"))?;
        if let Some(files) = self.settle(&repo, step, &format!("merging {main_branch} into {what}"))? {
            return self.park(n, ParkOp::MainIntoPublic, files, main).map(Some);
        }
        let new_tip = git.head()?;
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

    /// Rebuild every repo's merge head whose contents changed.
    pub fn rebuild_merge_heads(&mut self) -> Result<()> {
        let repos: Vec<String> = self.st.repos.keys().cloned().collect();
        for repo in repos {
            self.rebuild_merge_head(&repo)?;
        }
        Ok(())
    }

    /// Rebuild `repo`'s merge head if what it should contain changed. A leaf
    /// that clashes with the others and is not resolved here is parked out
    /// of it (with what it built on), so one clash never stops the rest of
    /// the queue — or anyone landing on it.
    pub fn rebuild_merge_head(&mut self, repo: &str) -> Result<()> {
        // What the merge head would hold with every set in: a set parked out
        // of it is retried only once that has changed.
        let parked_out: Vec<(u32, Park)> = self
            .st
            .sets
            .iter()
            .filter(|(_, s)| s.repo() == repo)
            .filter_map(|(n, s)| s.park.clone().filter(|p| p.op == ParkOp::MergeHead).map(|p| (*n, p)))
            .collect();
        for (n, _) in &parked_out {
            self.st.sets.get_mut(n).unwrap().park = None;
        }
        let main = self.root(repo)?;
        let full = self.composition(repo, &main).key();
        for (n, park) in parked_out {
            if park.on == full && !self.retries(&park) {
                self.st.sets.get_mut(&n).unwrap().park = Some(park);
            }
        }

        let what = format!("{}/merge-head", self.q.name);
        let git = self.git(repo);
        loop {
            let composition = self.composition(repo, &main);
            if self.repo_state(repo)?.merge_head.as_ref().is_some_and(|m| m.composition == composition) {
                return Ok(());
            }
            let tips: Vec<String> = composition.sets.iter().map(|(_, t)| t.clone()).collect();
            match self.build(repo, &main, &tips, &what)? {
                Ok(sha) => {
                    git.run(&["update-ref", &format!("refs/heads/{what}"), &sha])?;
                    if self.q.config.push.merge_head {
                        git.push_force("origin", &sha, &what)?;
                    }
                    self.st.repos.get_mut(repo).unwrap().merge_head = Some(MergeHead { sha, composition });
                    return Ok(());
                }
                Err((i, files)) => {
                    let n = self.clash_culprit(repo, &composition, i)?;
                    self.st.sets.get_mut(&n).unwrap().park = Some(Park {
                        op: ParkOp::MergeHead,
                        files,
                        on: full.clone(),
                        tried: self.conflict_tried,
                    });
                }
            }
        }
    }

    /// Leaf `i` would not merge onto the leaves before it. Of the pair that
    /// clash, park the one that was knowingly landed beside the other's work
    /// (it lists it, or something under it, in `conflicts_with`); failing
    /// that, the newer one, since the older was there first.
    fn clash_culprit(&self, repo: &str, composition: &Composition, i: usize) -> Result<u32> {
        let git = self.git(repo);
        let (n, tip) = &composition.sets[i];
        let Some((m, _)) = composition.sets[..i]
            .iter()
            .find(|(_, t)| !git.merge_conflicts(t, tip).unwrap_or_default().is_empty())
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

    /// A merge head's contents: main and the repo's live leaves.
    fn composition(&self, repo: &str, main: &str) -> Composition {
        let live = graph::live_in(&self.st, repo);
        Composition {
            main: main.to_string(),
            sets: graph::leaves(&self.st, &live)
                .into_iter()
                .map(|n| (n, self.st.sets[&n].tip.clone()))
                .collect(),
        }
    }

    /// Open `n`'s PR on main, or, for a stacked PR, on the branch of the
    /// set `on` it sits on.
    pub fn open_pr(&mut self, n: u32, on: Option<u32>, forge: &dyn Forge) -> Result<()> {
        let set = self.st.sets[&n].clone();
        let git = self.git(set.repo());
        let branch = set.branch(&self.q.name);
        if set.remote.as_deref() != Some(set.tip.as_str()) {
            // A PR needs its branch on origin, whatever private_sets says.
            self.save()?;
            git.push_lease("origin", &set.tip, &branch, set.remote.as_deref())?;
            self.st.sets.get_mut(&n).unwrap().remote = Some(set.tip.clone());
        }
        let title = self.q.pr_title(&set)?;
        // A PR opened by a pass that died before saving is still there; GitHub
        // would refuse a second one for the same branch.
        let pr_base = on.map(|p| self.st.sets[&p].branch(&self.q.name));
        if let Some(pr) = forge.find_open_pr(git.dir(), &branch)? {
            let set = self.st.sets.get_mut(&n).unwrap();
            set.pr = Some(pr);
            set.pr_base = pr_base;
            return Ok(());
        }
        let main = self.main_branch(set.repo())?;
        let body = match &set.body {
            Some(b) => b.clone(),
            // A nicety: if GitHub can't say, the PR still opens.
            None => forge.pr_template(git.dir()).ok().flatten().unwrap_or_default(),
        };
        let pr = forge.create_pr(
            git.dir(),
            &PrRequest {
                head: &branch,
                base: pr_base.as_deref().unwrap_or(&main),
                title: &title,
                body: &body,
                draft: self.q.config.pr.draft,
            },
        )?;
        let set = self.st.sets.get_mut(&n).unwrap();
        set.pr = Some(pr);
        set.pr_base = pr_base;
        Ok(())
    }

    /// Open stacked PRs while the cap allows, the longest line first, and
    /// link each into its stack. Errors are reported, never fatal: a PR
    /// left unlinked is linked by a later pass.
    fn open_stacked(&mut self, forge: &dyn Forge, report: &mut PassReport) {
        let mut tried = BTreeSet::new();
        let mut ready: BTreeMap<String, bool> = BTreeMap::new();
        loop {
            let pick = self
                .stack_candidates()
                .into_iter()
                .filter(|(n, _, _)| !tried.contains(n))
                .max_by_key(|(n, _, line)| (*line, std::cmp::Reverse(*n)));
            let Some((n, p, _)) = pick else { break };
            tried.insert(n);
            let repo = self.st.sets[&n].repo().to_string();
            let dir = self.git(&repo).dir().to_path_buf();
            let ok = *ready.entry(repo.clone()).or_insert_with(|| match forge.stacks_ready(&dir) {
                Ok(()) => true,
                Err(e) => {
                    report.errors.push(format!("{repo}: no stacked PRs: {e}"));
                    false
                }
            });
            if !ok {
                continue;
            }
            if let Err(e) = self.open_pr(n, Some(p), forge) {
                report.errors.push(format!("landing-{n}: opening its stacked PR failed: {e}"));
                continue;
            }
            report.stacked.push(n);
            if let Err(e) = self.link(n, p, forge) {
                report.errors.push(format!("landing-{n}: linking its PR into a stack failed: {e}"));
            }
        }
    }

    /// Put `n`'s PR on top of the stack `p`'s is in, or start one with them.
    fn link(&mut self, n: u32, p: u32, forge: &dyn Forge) -> Result<()> {
        let (under, top) = (&self.st.sets[&p], &self.st.sets[&n]);
        let (Some(under_pr), Some(pr)) = (&under.pr, &top.pr) else {
            return Ok(());
        };
        let dir = self.git(top.repo()).dir().to_path_buf();
        let stack = match under.stack {
            Some(s) => forge.link_stack(&dir, Some(s), &[pr.number])?,
            None => forge.link_stack(&dir, None, &[under_pr.number, pr.number])?,
        };
        self.st.sets.get_mut(&p).unwrap().stack = Some(stack);
        self.st.sets.get_mut(&n).unwrap().stack = Some(stack);
        Ok(())
    }

    /// A stacked PR that never got linked (a pass died, or `gh stack`
    /// failed) is linked now, if the set it sits on still has its PR.
    fn link_unlinked(&mut self, forge: &dyn Forge, report: &mut PassReport) {
        let name = self.q.name.clone();
        for n in graph::topo(&self.st) {
            let set = &self.st.sets[&n];
            if !graph::stacked(&self.st, n) || set.stack.is_some() {
                continue;
            }
            let under = self
                .st
                .sets
                .values()
                .find(|s| s.is_open() && s.pr.is_some() && set.pr_base.as_deref() == Some(s.branch(&name).as_str()))
                .map(|s| s.n);
            // As for a new stacked PR: under an unlinked one, linking would
            // start a second stack.
            let under = under.filter(|p| graph::public(&self.st, *p) || self.st.sets[p].stack.is_some());
            if let Some(p) = under
                && let Err(e) = self.link(n, p, forge)
            {
                report.errors.push(format!("landing-{n}: linking its PR into a stack failed: {e}"));
            }
        }
    }

    /// Sets that may get a stacked PR now, each with the set it would sit
    /// on and the length of its line: the stack under it, itself, and the
    /// longest chain of sets built on it.
    fn stack_candidates(&self) -> Vec<(u32, u32, usize)> {
        let name = &self.q.name;
        let cross = |queue: &str, m: u32| self.cross_merged()(queue, m);
        let open_prs = |repo: &str| {
            self.st
                .sets
                .values()
                .filter(|s| s.is_open() && s.pr.is_some() && s.repo() == repo)
                .count()
        };
        let mut out = Vec::new();
        for n in graph::live(&self.st) {
            let set = &self.st.sets[&n];
            if set.pr.is_some() || set.tip == set.base_sha {
                continue;
            }
            // Waiting on a set in another repo or queue: its PR opens at the
            // front, if at all, so there is nothing to stack on.
            let elsewhere = set.deps.iter().any(|d| match d {
                Dep::Set(m) => self
                    .st
                    .sets
                    .get(m)
                    .is_some_and(|s| s.status != SetStatus::Merged && s.repo() != set.repo()),
                Dep::Cross { queue, n } => !cross(queue, *n),
            });
            let [p] = graph::sits_on(&self.st, n)[..] else { continue };
            if elsewhere {
                continue;
            }
            let under = &self.st.sets[&p];
            // A closed child is still in GitHub's stack until it is dropped.
            let has_child = self.st.sets.values().any(|s| {
                matches!(s.status, SetStatus::Queued | SetStatus::Closed)
                    && s.pr_base.as_deref() == Some(under.branch(name).as_str())
            });
            // Under an unlinked stacked PR, linking would start a second
            // stack and strand that PR's own link.
            if under.repo() != set.repo()
                || under.pr.is_none()
                || !(graph::public(&self.st, p) || under.stack.is_some())
                || has_child
                || set.base.deps != [(p, under.tip.clone())]
                || open_prs(set.repo()) >= self.q.config.stack.max_open as usize
            {
                continue;
            }
            let below = graph::ancestors(&self.st, n)
                .into_iter()
                .filter(|a| self.st.sets[a].is_open())
                .count();
            out.push((n, p, below + 1 + self.longest_chain_on(n)));
        }
        out
    }

    /// How many open sets, each sitting on the one before, build on `n`
    /// at most.
    fn longest_chain_on(&self, n: u32) -> usize {
        self.st
            .sets
            .values()
            .filter(|s| s.is_open() && graph::sits_on(&self.st, s.n) == [n])
            .map(|s| 1 + self.longest_chain_on(s.n))
            .max()
            .unwrap_or(0)
    }

    /// GitHub won't move an unlinked PR when the set below merges; once its
    /// set has been replayed onto main, point the PR there.
    fn retarget_unlinked(&mut self, forge: &dyn Forge, report: &mut PassReport) {
        for n in graph::topo(&self.st) {
            let set = &self.st.sets[&n];
            if !graph::stacked(&self.st, n)
                || set.stack.is_some()
                || set.park.is_some()
                || !graph::unmerged_deps(&self.st, n).is_empty()
                || !set.base.deps.is_empty()
            {
                continue;
            }
            let repo = set.repo().to_string();
            let number = set.pr.as_ref().unwrap().number;
            let main = match self.main_branch(&repo) {
                Ok(m) => m,
                Err(e) => {
                    report.errors.push(format!("landing-{n}: {e}"));
                    continue;
                }
            };
            match forge.set_pr_base(self.git(&repo).dir(), number, &main) {
                Ok(()) => self.st.sets.get_mut(&n).unwrap().pr_base = None,
                Err(e) => report.errors.push(format!("landing-{n}: pointing its PR at {main} failed: {e}")),
            }
        }
    }

    /// A stacked set whose replay left it with nothing of its own. Its
    /// change is all in the set it sits on (a fix there did the same), so
    /// it leaves the queue and its tasks go with that set: its labels and
    /// sessions, which are how other tools find a set's tasks, are added
    /// there. With nothing under it, its change is already on main, and it
    /// is merged. Either way its PR is closed, and the PRs on it move down;
    /// GitHub won't change a stacked PR's base, so the stack is dissolved
    /// first and linked again later in the pass.
    fn fold_into_below(&mut self, n: u32, forge: &dyn Forge, report: &mut PassReport) {
        let name = self.q.name.clone();
        let p = match graph::sits_on(&self.st, n)[..] {
            [p] => Some(p),
            _ => None,
        };
        let set = &self.st.sets[&n];
        let repo = set.repo().to_string();
        let dir = self.git(&repo).dir().to_path_buf();
        let fail = |report: &mut PassReport, e: color_eyre::Report| {
            report.errors.push(format!("landing-{n}: closing its emptied PR failed: {e}"));
        };
        if let Some(stack) = set.stack {
            if let Err(e) = forge.unstack(&dir, stack) {
                return fail(report, e);
            }
            for s in self.st.sets.values_mut().filter(|s| s.stack == Some(stack)) {
                s.stack = None;
            }
        }
        let note = match p {
            Some(p) => {
                // Onto main instead, the pass moves them once replayed.
                let below = self.st.sets[&p].branch(&name);
                let mine = self.st.sets[&n].branch(&name);
                let on_it: Vec<u32> = self
                    .st
                    .sets
                    .values()
                    .filter(|s| s.is_open() && s.pr_base.as_deref() == Some(mine.as_str()))
                    .map(|s| s.n)
                    .collect();
                for m in on_it {
                    let pr = self.st.sets[&m].pr.as_ref().unwrap().number;
                    if let Err(e) = forge.set_pr_base(&dir, pr, &below) {
                        return fail(report, e);
                    }
                    self.st.sets.get_mut(&m).unwrap().pr_base = Some(below.clone());
                }
                match &self.st.sets[&p].pr {
                    Some(b) => format!("Everything here is now in #{}, which this sat on.", b.number),
                    None => format!("Everything here is now in {below}, which this sat on."),
                }
            }
            None => "Everything here is already on main.".to_string(),
        };
        let pr = self.st.sets[&n].pr.as_ref().unwrap().number;
        if let Err(e) = forge.close_pr(&dir, pr, &note) {
            return fail(report, e);
        }
        let Some(p) = p else {
            let main = match self.main_sha(&repo) {
                Ok(m) => m,
                Err(e) => return fail(report, e),
            };
            let set = self.st.sets.get_mut(&n).unwrap();
            set.status = SetStatus::Merged;
            set.merged_into = Some(main);
            report.merged.push(n);
            report.already_on_main.push(n);
            return;
        };
        let (labels, sessions) = (self.st.sets[&n].labels.clone(), self.st.sets[&n].sessions.clone());
        let under = self.st.sets.get_mut(&p).unwrap();
        for l in labels {
            if !under.labels.contains(&l) {
                under.labels.push(l);
            }
        }
        for s in sessions {
            if !under.sessions.iter().any(|u| u.source == s.source) {
                under.sessions.push(s);
            }
        }
        graph::splice_out(&mut self.st, n);
        let set = self.st.sets.get_mut(&n).unwrap();
        set.status = SetStatus::Dropped;
        set.park = None;
        report.folded.push(n);
    }

    /// A dropped set's branch goes once no open PR is based on it: GitHub
    /// closes a PR whose base branch is deleted.
    fn prune_dropped(&mut self, report: &mut PassReport) {
        let name = self.q.name.clone();
        let gone: Vec<u32> = self
            .st
            .sets
            .values()
            .filter(|s| s.status == SetStatus::Dropped && s.remote.is_some())
            .filter(|d| {
                let b = d.branch(&name);
                !self.st.sets.values().any(|s| s.is_open() && s.pr_base.as_deref() == Some(b.as_str()))
            })
            .map(|s| s.n)
            .collect();
        for n in gone {
            let git = self.set_git(n);
            let b = branch(&name, n);
            let on_origin = git.rev_parse(&format!("refs/remotes/origin/{b}")).ok().flatten().is_some();
            if !on_origin {
                self.st.sets.get_mut(&n).unwrap().remote = None;
                continue;
            }
            match git.delete_remote_branch("origin", &b) {
                Ok(()) => self.st.sets.get_mut(&n).unwrap().remote = None,
                Err(e) => report.errors.push(format!("landing-{n}: deleting its dropped branch failed: {e}")),
            }
        }
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
        if let Err(e) = self.observe_stacks(forge) {
            report.errors.push(format!("reading GitHub's stacks failed: {e}"));
        }
        self.observe_pushes(&mut report)?;
        // GitHub unreachable: build on what was last seen.
        if let Err(e) = self.observe_diffbase(forge) {
            report.errors.push(format!("checking the diffbase's PRs failed: {e}"));
        }
        self.rebuild_diffbase()?;
        if self.st.diffbase.park.is_some() {
            report.diffbase_parked = true;
            self.tend_landings(&report);
            return Ok(report);
        }

        let mut settings: BTreeMap<String, RepoSettings> = BTreeMap::new();
        for n in graph::topo(&self.st) {
            // A closed PR holds what builds on it until it is reopened or
            // dropped: replaying them now would take its work, and what it
            // sat on, out from under them.
            if graph::ancestors(&self.st, n)
                .iter()
                .any(|a| self.st.sets.get(a).is_some_and(|s| s.status == SetStatus::Closed))
            {
                continue;
            }
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
            // GitHub is rebasing the linked line above a merge; give it
            // time. That covers a PR GitHub has already put on main but not
            // yet rebased, which would otherwise count as public.
            if self.restacking(n) {
                continue;
            }
            if graph::public(&self.st, n) {
                let repo = set.repo().to_string();
                if set.park.as_ref().is_some_and(|p| {
                    p.op == ParkOp::MainIntoPublic
                        && p.on == self.main_sha(&repo).unwrap_or_default()
                        && !self.retries(p)
                }) {
                    continue;
                }
                if !settings.contains_key(&repo) {
                    let got = forge.settings(self.git(&repo).dir(), &self.main_branch(&repo)?)?;
                    settings.insert(repo.clone(), got);
                }
                match self.update_public(n, &settings[&repo])? {
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
            if set.park.as_ref().is_some_and(|p| p.on == spec.key() && !self.retries(p)) {
                continue;
            }
            if spec == set.base && set.park.is_none() {
                continue;
            }
            if self.resync(n, spec)? {
                report.resynced.push(n);
            } else if self.emptied.remove(&n) {
                self.fold_into_below(n, forge, &mut report);
            } else if self.st.sets[&n].park.is_some() {
                report.parked.push(n);
            } else {
                report.push_failed.push(n);
            }
        }

        self.rebuild_merge_heads()?;
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
        let waiting = self.diffbase_waiting()?;
        for n in front {
            // A set reaches the front on a base of main; one still built on
            // its merged dependencies, or on the diffbase, waits for the
            // re-sync above.
            let set = &self.st.sets[&n];
            let main = self.main_sha(set.repo())?;
            let git = self.set_git(n);
            let base = &set.base;
            if waiting
                || !base.deps.is_empty()
                || !base.main.as_deref().is_some_and(|m| git.is_ancestor(m, &main).unwrap_or(false))
            {
                continue;
            }
            // Everything it had reached main some other way: nothing is left
            // to review, so it is done as if its PR had merged.
            if set.tip == set.base_sha {
                let set = self.st.sets.get_mut(&n).unwrap();
                set.status = SetStatus::Merged;
                set.merged_into = Some(main);
                report.merged.push(n);
                report.already_on_main.push(n);
                continue;
            }
            match self.open_pr(n, None, forge) {
                Ok(()) => report.opened.push(n),
                Err(e) => report.errors.push(format!("landing-{n}: opening its PR failed: {e}")),
            }
        }
        self.retarget_unlinked(forge, &mut report);
        if self.q.config.stack.enabled && !waiting {
            self.open_stacked(forge, &mut report);
        }
        if self.q.config.stack.enabled {
            self.link_unlinked(forge, &mut report);
        }
        self.prune_dropped(&mut report);
        self.tend_landings(&report);
        Ok(report)
    }

    /// Keep landing folders in step: a clean one follows its set when the
    /// engine moves it; a merged set's folder goes to the archive. Dirty
    /// folders are left alone (status flags them). Best-effort: a landing
    /// folder is a convenience, never a reason to fail a pass.
    fn tend_landings(&self, report: &PassReport) {
        for n in report.resynced.iter().chain(&report.updated_public).chain(&report.adopted) {
            let repo = self.st.sets[n].repo();
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
            if landing.fetch(self.q.sync_dir(repo).to_str().unwrap_or_default(), &[&refspec]).is_ok() {
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
    pub fn retries_parked(&self) -> bool {
        self.handler.as_deref().is_some_and(|h| h.retries_parked())
    }

    /// Whether to try `park`'s operation again though its inputs are the
    /// same: when asked to, or when no one has had a go at it yet.
    fn retries(&self, park: &Park) -> bool {
        self.retries_parked() || (self.handler.is_some() && !park.tried)
    }

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
        let git = self.set_git(n);
        let set = self.st.sets.get_mut(&n).unwrap();
        let Some(Park { op: ParkOp::Diverged, on, .. }) = set.park.clone() else {
            return Ok(());
        };
        if !git.is_ancestor(&set.base_sha, &on)? {
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

    /// What a new set in `repo` depends on: everything live there.
    pub fn next_dep_set(&self, repo: &str) -> Vec<Dep> {
        graph::live_in(&self.st, repo).into_iter().map(Dep::Set).collect()
    }
}

fn agent_slot(st: &RepoState, rotate_after: u32) -> AgentSlot {
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
fn record_session(st: &mut RepoState, slot: &AgentSlot) {
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
fn recorded_for(git: &Git, st: &RepoState, files: &[String]) -> Vec<String> {
    let wanted: BTreeSet<&String> = files.iter().collect();
    st.resolutions
        .iter()
        .rev()
        .filter(|r| r.files.iter().any(|f| wanted.contains(f)))
        .take(3)
        .filter_map(|r| git.remerge_diff(&r.commit).ok())
        .collect()
}
