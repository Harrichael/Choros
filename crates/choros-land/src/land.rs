//! Landing a workspace's work: into its queue as a new landing-set per repo,
//! or straight onto main in instant mode. Also binding a repo to a queue, and
//! syncing a workspace onto its queue's merge head for that repo.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use choros_core::choros::{self as core, ChorosMeta};
use choros_core::{agent as core_agent, git as core_git, registry, space};
use color_eyre::eyre::{Result, eyre};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::engine::{Handler, Env, OnConflict, Work};
use crate::forge::{PrRequest, PrState};
use crate::git::Git;
use crate::graph;
use crate::queue::Queue;
use crate::repos;
use crate::state::{BaseSpec, Dep, LandMode, LandingSet, MergeHead, SessionFork, SetStatus, Source};
use crate::workspace::{self, InstantPr, LandingOf, RepoLanding};

pub struct LandRequest {
    /// Where `land` was run: a repo (land that repo) or the workspace root
    /// (land every repo with something to land).
    pub cwd: PathBuf,
    pub mode: LandMode,
    pub deps: Deps,
    /// Land without running the repo's check.
    pub force: bool,
    pub on_conflict: Option<OnConflict>,
    pub title: Option<String>,
    pub body: Option<String>,
    /// Kept on each set made, for other tools to read; choros never looks
    /// inside them.
    pub labels: Vec<String>,
    /// Sessions to fork into the landing; empty means "work it out".
    pub sessions: Vec<String>,
    /// The Claude Code session `land` is running inside, if any (the CLI
    /// reads `CLAUDE_CODE_SESSION_ID`).
    pub env_session: Option<String>,
}

/// What a land builds on. Whichever it is, the set must pass the repo's
/// check on its own tip unless the land is forced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Deps {
    /// As few queued sets as the commits need: see [`land_least`].
    #[default]
    Least,
    /// These sets (plus the workspace's own earlier ones); empty is main.
    Picked(Vec<Dep>),
    /// Everything queued: the merge head.
    Head,
}

#[derive(Debug)]
pub struct Landed {
    pub repo: String,
    pub queue: Option<String>,
    pub n: Option<u32>,
    pub landing: Option<PathBuf>,
    pub forked_sessions: Vec<String>,
    /// What the land depends on and why, when it chose for itself.
    pub notes: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct LandOutcome {
    pub landed: Vec<Landed>,
    /// Where the workspace went, for `land archive`.
    pub archived_to: Option<PathBuf>,
}

pub fn land(env: &Env, req: &LandRequest, mut handler: Handler<'_>) -> Result<LandOutcome> {
    let (trunk, ws) = core::find_workspace(&req.cwd)?;
    refuse_landing_folder(&ws)?;
    let meta = core::read_meta(&ws)?;
    let mut scope: Vec<String> = match repo_of(&ws, &req.cwd, &meta) {
        Some(repo) => vec![repo],
        None => meta.repos.clone(),
    };
    for repo in &scope {
        settle_pending(&ws, repo)?;
    }
    // One queue lock at a time, taken in queue-name order.
    let mut queues = std::collections::BTreeMap::new();
    for repo in &scope {
        queues.insert(repo.clone(), workspace::queue_for(&trunk, &ws, repo)?);
    }
    scope.sort_by_key(|r| queues[r].clone());
    // Settled for every repo before any lands, so a missing check refuses
    // the whole land rather than half of it.
    let mut checks = BTreeMap::new();
    for repo in &scope {
        if queues[repo].is_none() || req.force {
            continue;
        }
        let check = repos::read(&trunk, repo)?.check.ok_or_else(|| {
            eyre!(
                "{repo} has no check to land with: set one under [{repo}] in .choros-config/repos.toml, \
                 or pass --force to land unchecked"
            )
        })?;
        checks.insert(repo.clone(), check);
    }

    if req.body.is_none() && req.env_session.as_ref().is_some_and(|s| !s.is_empty()) {
        refuse_without_body(env, &trunk, &ws, &scope)?;
    }

    let mut landed = Vec::new();
    for repo in &scope {
        if !has_unlanded(&ws, repo)? {
            continue;
        }
        let out = match &queues[repo] {
            None => {
                land_instant(env, &trunk, &ws, repo, req)?
            }
            Some(queue) => {
                let q = Queue::open(&trunk, queue)?;
                land_queued(env, &q, &ws, repo, req, checks.get(repo), handler.as_deref_mut())?
            }
        };
        landed.push(out);
    }
    if landed.is_empty() {
        return Err(eyre!("nothing to land: no repo in scope has commits past its last landing"));
    }

    let archived_to = if req.mode == LandMode::Archive {
        let left: Vec<&String> = meta
            .repos
            .iter()
            .filter(|r| has_unlanded(&ws, r).unwrap_or(true))
            .collect();
        if !left.is_empty() {
            return Err(eyre!(
                "landed, but not archiving: {} still {} unlanded commits",
                left.iter().map(|r| r.as_str()).collect::<Vec<_>>().join(", "),
                if left.len() == 1 { "has" } else { "have" }
            ));
        }
        Some(core::archive_at(&ws)?)
    } else {
        None
    };
    Ok(LandOutcome { landed, archived_to })
}

/// A landing folder's branch is already a landing-set: landing it again would
/// put the PR's commits somewhere else (with no queue, straight onto main).
fn refuse_landing_folder(ws: &Path) -> Result<()> {
    if let Some(l) = workspace::read(ws)?.landing {
        return Err(eyre!(
            "this is landing-{} of queue {}: push review fixes to {} instead",
            l.n,
            l.queue,
            crate::state::branch(&l.queue, l.n)
        ));
    }
    Ok(())
}

/// The repo `cwd` is inside, if any.
/// An agent that lands without a PR body has most likely not thought about
/// one, so it is told what one is for, with the template to fill, before
/// anything lands. A person landing without one gets the template as is.
/// "Agent" is a guess from running inside a Claude Code session, which also
/// catches a person's `!` command there; the message says what to do.
fn refuse_without_body(env: &Env, trunk: &Path, ws: &Path, scope: &[String]) -> Result<()> {
    let mut templates = Vec::new();
    for repo in scope {
        if has_unlanded(ws, repo)? && opens_pr(trunk, ws, repo)? {
            templates.push((repo, env.forge.pr_template(&ws.join(repo))));
        }
    }
    if templates.is_empty() {
        return Ok(());
    }
    let mut msg = "nothing landed: a PR body tells the reviewer what changed and why. Fill in the \
                   template below, save it to a file, and land again with --body-file FILE. Leave \
                   out landing-sets and dependencies; choros tracks those."
        .to_string();
    for (repo, template) in templates {
        match template {
            Ok(Some(t)) => msg.push_str(&format!("\n\n--- {repo}'s PR template ---\n{}", t.trim_end())),
            Ok(None) => msg.push_str(&format!("\n\n{repo} has no PR template; say what changed and why.")),
            Err(_) => msg.push_str(&format!("\n\ncouldn't fetch {repo}'s PR template; say what changed and why.")),
        }
    }
    Err(eyre!(msg))
}

/// Whether landing `repo` now would open a PR: always into a queue; straight
/// to main only by `via = "pr"`, and not when the PR already open covers it.
fn opens_pr(trunk: &Path, ws: &Path, repo: &str) -> Result<bool> {
    if workspace::queue_for(trunk, ws, repo)?.is_some() {
        return Ok(true);
    }
    if crate::instant::via(trunk, ws)? != crate::instant::Via::Pr {
        return Ok(false);
    }
    let waiting = workspace::read(ws)?.repo.get(repo).and_then(|r| r.instant_pr.clone());
    Ok(waiting.is_none_or(|pr| Git::new(&ws.join(repo)).head().is_ok_and(|h| h != pr.head)))
}

fn repo_of(ws: &Path, cwd: &Path, meta: &ChorosMeta) -> Option<String> {
    let rel = cwd.strip_prefix(ws).ok()?;
    let first = rel.components().next()?.as_os_str().to_str()?;
    meta.repos.iter().find(|r| r.as_str() == first).cloned()
}

fn has_unlanded(ws: &Path, repo: &str) -> Result<bool> {
    let git = Git::new(&ws.join(repo));
    let through = landed_through(ws, repo)?;
    Ok(through != git.head()? || !returned(ws, repo)?.is_empty())
}

fn returned(ws: &Path, repo: &str) -> Result<Vec<workspace::Returned>> {
    Ok(workspace::read(ws)?.repo.get(repo).map(|r| r.returned.clone()).unwrap_or_default())
}

/// Where this repo's landing history starts. A workspace made before
/// landing existed has no record; its branch point from main stands in.
/// Repos of `ws` holding commits that haven't been landed, and how many.
pub fn unlanded(ws: &Path) -> Result<Vec<(String, usize)>> {
    let mut out = Vec::new();
    for repo in core::read_meta(ws)?.repos {
        let n = unlanded_count(ws, &repo)?;
        if n > 0 {
            out.push((repo, n));
        }
    }
    Ok(out)
}

/// How many commits in `repo` of `ws` haven't been landed.
pub fn unlanded_count(ws: &Path, repo: &str) -> Result<usize> {
    let through = landed_through(ws, repo)?;
    Ok(Git::new(&ws.join(repo)).commits(&through, "HEAD")?.len())
}

pub(crate) fn landed_through(ws: &Path, repo: &str) -> Result<String> {
    let rl = workspace::read(ws)?.repo.get(repo).cloned().unwrap_or_default();
    if let Some(sha) = rl.landed_through {
        return Ok(sha);
    }
    let git = Git::new(&ws.join(repo));
    let main = main_branch(&git)?;
    git.run(&["merge-base", "HEAD", &format!("refs/remotes/origin/{main}")])
}

fn main_branch(git: &Git) -> Result<String> {
    Ok(git
        .run(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])?
        .trim_start_matches("origin/")
        .to_string())
}

fn update_repo(ws: &Path, repo: &str, f: impl FnOnce(&mut RepoLanding)) -> Result<()> {
    let mut wl = workspace::read(ws)?;
    f(wl.repo.entry(repo.to_string()).or_default());
    workspace::write(ws, &wl)
}

/// Finish the bookkeeping of a rebase this repo was sent into: once HEAD
/// contains the commit it was rebasing onto, that commit is where landing
/// resumes — however the conflict was resolved, and by whom.
fn settle_pending(ws: &Path, repo: &str) -> Result<()> {
    let git = Git::new(&ws.join(repo));
    if git.in_progress()?.is_some() {
        return Err(eyre!(
            "{} is in the middle of a rebase or merge; finish it (`git rebase --continue`) \
             or abort it, then try again",
            git.dir().display()
        ));
    }
    let Some(pending) = workspace::read(ws)?.repo.get(repo).and_then(|r| r.pending.clone()) else {
        return Ok(());
    };
    let reached = git.is_ancestor(&pending, "HEAD")?;
    update_repo(ws, repo, |rl| {
        let composition = rl.pending_composition.take();
        let returned = std::mem::take(&mut rl.pending_returned);
        if reached {
            rl.landed_through = Some(pending);
            rl.composition = composition;
        } else {
            rl.returned.extend(returned);
        }
        rl.pending = None;
    })
}

fn check_clean(git: &Git, through: &str) -> Result<()> {
    if !git.is_clean()? {
        return Err(eyre!("{} has uncommitted changes; commit or stash them first", git.dir().display()));
    }
    if !git.is_ancestor(through, "HEAD")? {
        return Err(eyre!(
            "commits that were already landed have been rewritten in {}; \
             the landed history must stay as it was",
            git.dir().display()
        ));
    }
    Ok(())
}

fn check_landable(git: &Git, through: &str) -> Result<()> {
    check_clean(git, through)?;
    if git.has_merges(through, "HEAD")? {
        return Err(eyre!(
            "the commits to land in {} include a merge; rebase them onto the \
             merge head instead (`choros repo sync`)",
            git.dir().display()
        ));
    }
    Ok(())
}

/// Rebase the repo's unlanded commits onto `onto_ref` of the queue's sync
/// clone, in place. A conflict is left in progress for whoever is in the
/// workspace; `pending` remembers where it was headed.
#[allow(clippy::too_many_arguments)]
fn rebase_workspace(
    q: &Queue,
    ws: &Path,
    repo: &str,
    git: &Git,
    onto_ref: &str,
    onto_sha: &str,
    composition: Option<String>,
    what: &str,
) -> Result<()> {
    let through = landed_through(ws, repo)?;
    update_repo(ws, repo, |rl| {
        rl.pending = Some(onto_sha.to_string());
        rl.pending_composition = composition;
    })?;
    git.fetch(q.sync_dir(repo).to_str().unwrap(), &[&format!("+{onto_ref}:refs/choros/onto")])?;
    if !git.succeeds(&["rebase", "--onto", "refs/choros/onto", &through])? {
        return Err(eyre!(
            "rebasing {repo} onto {what} conflicts in: {}\n\
             resolve them, `git rebase --continue`, then run the same command again",
            git.unmerged()?.join(", ")
        ));
    }
    settle_pending(ws, repo)
}

/// The queue's merge head for `repo`, rebuilt if it is out of date.
fn current_merge_head(env: &Env, q: &Queue, repo: &str) -> Result<MergeHead> {
    let mut work = Work::begin(q, env.agent, None, true)?.unwrap();
    work.ensure_repo(repo)?;
    work.rebuild_merge_head(repo)?;
    work.save()?;
    Ok(work.st.repos[repo].merge_head.clone().unwrap())
}

/// Put the repo's unlanded commits on the queue's current merge head. Whether
/// that is needed is a question of content, not position: the merge head can
/// also move *back* (a set closed or dropped), and then the rebase is what
/// takes its commits out of this workspace.
fn sync_to_merge_head(env: &Env, q: &Queue, ws: &Path, repo: &str) -> Result<MergeHead> {
    let git = Git::new(&ws.join(repo));
    let through = landed_through(ws, repo)?;
    check_clean(&git, &through)?;
    let mh = current_merge_head(env, q, repo)?;
    let key = mh.composition.key();
    let known = workspace::read(ws)?.repo.get(repo).and_then(|r| r.composition.clone());
    // A merge in the range (main merged in by hand) is straightened out too:
    // the rebase drops it, and main's commits already in the merge head.
    let returned = returned(ws, repo)?;
    let needed = known.as_deref() != Some(key.as_str())
        || !git.is_ancestor(&mh.sha, "HEAD")?
        || git.has_merges(&through, "HEAD")?
        || !returned.is_empty();
    if !needed {
        return Ok(mh);
    }
    let what = format!("{}'s merge head", q.name);
    if returned.is_empty() {
        let mh_ref = format!("refs/heads/{}/merge-head", q.name);
        rebase_workspace(q, ws, repo, &git, &mh_ref, &mh.sha, Some(key), &what)?;
        return Ok(mh);
    }
    // Dropped sets' commits go between the merge head and the workspace's
    // own unlanded ones, which may build on them; landing counts only up to
    // the merge head, so they come back unlanded.
    let onto = return_onto(env, q, repo, &mh.sha, &returned)?;
    update_repo(ws, repo, |rl| rl.pending_returned = std::mem::take(&mut rl.returned))?;
    let onto_ref = "refs/choros/onto-returned";
    let sync = Git::new(&q.sync_dir(repo));
    sync.run(&["update-ref", onto_ref, &onto])?;
    rebase_workspace(q, ws, repo, &git, onto_ref, &mh.sha, Some(key), &what)?;
    Ok(mh)
}

/// The merge head with the dropped sets' commits replayed on top, in the
/// sync clone.
fn return_onto(env: &Env, q: &Queue, repo: &str, mh: &str, returned: &[workspace::Returned]) -> Result<String> {
    let mut work = Work::begin(q, env.agent, None, true)?.unwrap();
    let sync = work.git(repo);
    let mut commits = Vec::new();
    for r in returned {
        commits.extend(sync.commits(&r.base, &r.tip)?);
    }
    let names: Vec<String> = returned.iter().map(|r| crate::state::branch(&r.queue, r.n)).collect();
    let what = format!("the commits of dropped {}", names.join(", "));
    let tip = work.replay(repo, mh, &commits, &what)?.map_err(|files| {
        eyre!(
            "{what} no longer apply onto {}'s merge head (conflicts in {}); they are kept in {} \
             under refs/choros/returned/",
            q.name,
            files.join(", "),
            q.sync_dir(repo).display()
        )
    })?;
    work.save()?;
    Ok(tip)
}

/// `check` is the repo's, unless the land was forced: every set must pass
/// it on its own tip, its dependencies plus itself.
fn land_queued(
    env: &Env,
    q: &Queue,
    ws: &Path,
    repo: &str,
    req: &LandRequest,
    check: Option<&String>,
    handler: Handler<'_>,
) -> Result<Landed> {
    let mut warnings = Vec::new();
    let mut notes = Vec::new();

    // A dropped set's commits come back before anything lands beside the
    // queue, which works from the workspace as it stands.
    if !returned(ws, repo)?.is_empty() {
        sync_to_merge_head(env, q, ws, repo)?;
    }
    let mut checker = Checker::new(q, repo, check);
    let n = match &req.deps {
        Deps::Head => land_normal(env, q, ws, repo, req, &mut checker)?,
        Deps::Picked(deps) => land_fork(env, q, ws, repo, deps, req, &mut checker, handler)?,
        Deps::Least => match land_least(env, q, ws, repo, req, &mut checker)? {
            Least::Landed(n, why) => {
                notes = why;
                n
            }
            Least::Fallback(why) => {
                warnings.extend(why);
                land_normal(env, q, ws, repo, req, &mut checker)?
            }
        },
    };

    // Settle the rest of the queue now — the new set may already be at the
    // front — rather than leave it to the daemon's next poll. The land itself
    // is done by here, so trouble (GitHub unreachable, say) is a warning: the
    // daemon's next pass picks up where this one stopped. No handler: other
    // sets' conflicts are not this land's to ask about.
    if let Some(mut work) = Work::begin(q, env.agent, None, true)? {
        if let Err(e) = work.pass(env.forge) {
            warnings.push(format!("landed, but settling the queue failed ({e}); the daemon will retry"));
        }
        work.save()?;
    }

    // Landing again makes a new set; changes to an open one go through its
    // landing folder, and the obvious move doesn't say so.
    for set in q.read_state()?.sets.values() {
        if set.n != n && set.is_open() && set.source.workspace == ws && set.source.repo == repo {
            notes.push(format!(
                "{} is still open; to change it, work in its landing folder (`choros landing open {} --queue {}`)",
                set.branch(&q.name),
                set.n,
                q.name
            ));
        }
    }

    let landing = make_landing(q, n, ws)?;
    let forked = fork_sessions(env, q, n, ws, repo, &landing, req, &mut warnings)?;
    Ok(Landed {
        repo: repo.to_string(),
        queue: Some(q.name.clone()),
        n: Some(n),
        landing: Some(landing),
        forked_sessions: forked,
        notes,
        warnings,
    })
}

/// The repo's check, run on a would-be set's tip with the queue let go, so
/// a slow check holds up no one else's land. Verdicts are kept by tree: the
/// land takes the queue again and builds the tip again, and when nothing
/// moved meanwhile it's the same tree, already judged. When something did,
/// the new tree is checked in turn.
struct Checker<'a> {
    q: &'a Queue,
    repo: &'a str,
    check: Option<&'a String>,
    verdicts: BTreeMap<String, std::result::Result<(), String>>,
}

enum Verdict {
    Pass,
    Fail(String),
    /// Not run on this tree yet.
    Unknown,
}

impl<'a> Checker<'a> {
    fn new(q: &'a Queue, repo: &'a str, check: Option<&'a String>) -> Self {
        Checker { q, repo, check, verdicts: BTreeMap::new() }
    }

    fn verdict(&self, sync: &Git, tip: &str) -> Result<Verdict> {
        if self.check.is_none() {
            return Ok(Verdict::Pass);
        }
        Ok(match self.verdicts.get(&tree(sync, tip)?) {
            None => Verdict::Unknown,
            Some(Ok(())) => Verdict::Pass,
            Some(Err(out)) => Verdict::Fail(out.clone()),
        })
    }

    /// Call with the queue let go.
    fn run(&mut self, tip: &str) -> Result<()> {
        let check = self.check.expect("a verdict is unknown only with a check");
        let tree = tree(&self.q.git(self.repo), tip)?;
        let verdict = self.q.check(self.repo, tip, check)?;
        self.verdicts.insert(tree, verdict);
        Ok(())
    }

    /// The refusal for a set whose tip failed, its full output kept in the
    /// queue's folder.
    fn refusal(&self, on: &str, output: &str) -> Result<color_eyre::Report> {
        let log = self.q.dir().join(format!("check-{}.log", self.repo));
        std::fs::write(&log, output)?;
        Ok(eyre!(
            "`{}` failed on {}'s commits {on}, so they didn't land; full output: {}. \
             Fix it, or pass --force to land without the check",
            self.check.map_or("", |c| c.as_str()),
            self.repo,
            log.display()
        ))
    }
}

fn tree(git: &Git, commit: &str) -> Result<String> {
    git.run(&["rev-parse", &format!("{commit}^{{tree}}")])
}

/// Land on the merge head: the workspace is first synced onto it (so any
/// conflict is resolved here, by whoever wrote the code), after which the
/// landing-set is simply the workspace's tip.
fn land_normal(env: &Env, q: &Queue, ws: &Path, repo: &str, req: &LandRequest, checker: &mut Checker) -> Result<u32> {
    let repo_dir = ws.join(repo);
    let git = Git::new(&repo_dir);
    // Each round lands, checks, or moves the workspace onto a newer merge
    // head; the merge head only moves when someone else lands meanwhile.
    let mut moves = 0;
    loop {
        let mh = sync_to_merge_head(env, q, ws, repo)?;
        let head = git.head()?;
        if head == mh.sha {
            return Err(eyre!("nothing left to land in {repo}: its commits are already in the queue"));
        }

        // No handler: a clash elsewhere in the queue is not this land's to
        // resolve; it parks that set and the merge head goes on without it.
        let mut work = Work::begin(q, env.agent, None, true)?.unwrap();
        work.rebuild_merge_head(repo)?;
        let now = work.st.repos[repo].merge_head.clone().unwrap();
        if now.composition != mh.composition {
            moves += 1;
            if moves == 5 {
                return Err(eyre!("the merge head kept moving while landing; try again"));
            }
            continue;
        }
        let sync = work.git(repo);
        sync.fetch(repo_dir.to_str().unwrap(), &["+HEAD:refs/choros/incoming"])?;
        match checker.verdict(&sync, &head)? {
            Verdict::Pass => {}
            Verdict::Fail(out) => return Err(checker.refusal("on the merge head", &out)?),
            Verdict::Unknown => {
                drop(work);
                checker.run(&head)?;
                continue;
            }
        }

        let n = work.st.next_n.max(1);
        work.st.next_n = n + 1;
        let deps = graph::reduce(&work.st, &work.next_dep_set(repo));
        let base = BaseSpec {
            main: Some(now.composition.main.clone()),
            deps: now.composition.sets.clone(),
        };
        work.st.sets.insert(
            n,
            new_set(n, deps, base, now.sha.clone(), head.clone(), ws, repo, req),
        );
        // A refused push is retried by the pass that follows.
        work.publish(n)?;
        work.rebuild_merge_head(repo)?;
        work.save()?;
        let key = work.st.repos[repo].merge_head.as_ref().map(|m| m.composition.key());
        update_repo(ws, repo, |rl| {
            rl.landed_through = Some(head.clone());
            rl.composition = key;
        })?;
        return Ok(n);
    }
}

/// Land beside the queue rather than on top of it: on main, or on the
/// dependencies picked. The workspace's own earlier sets always come along.
/// Picked sets in other repos are only waited for.
#[allow(clippy::too_many_arguments)]
fn land_fork(
    env: &Env,
    q: &Queue,
    ws: &Path,
    repo: &str,
    picked: &[Dep],
    req: &LandRequest,
    checker: &mut Checker,
    mut handler: Handler<'_>,
) -> Result<u32> {
    let repo_dir = ws.join(repo);
    let git = Git::new(&repo_dir);
    let through = landed_through(ws, repo)?;
    check_landable(&git, &through)?;
    let head = git.head()?;
    let mut deps: Vec<Dep> = picked.to_vec();
    // Asked once: a round that let the queue go for the check comes back to
    // the same clash.
    let mut decision = req.on_conflict;
    let mut rounds = 0;
    'round: loop {
        let mut work = Work::begin(q, env.agent, handler.as_deref_mut(), true)?.unwrap();
        work.ensure_repo(repo)?;
        // A set picked may have merged while the check ran: it's in main now.
        if rounds > 0 {
            deps.retain(|d| !matches!(d, Dep::Set(m) if work.st.sets.get(m).is_some_and(|s| s.status == SetStatus::Merged)));
        }
        rounds += 1;
        let moved_onto = workspace::read(ws)?.repo.get(repo).map(|r| r.fork_deps.clone()).unwrap_or_default();
        deps.extend(moved_onto.into_iter().map(Dep::Set));
        for (m, set) in &work.st.sets {
            if set.is_open() && set.source.workspace == ws && set.source.repo == repo && !deps.contains(&Dep::Set(*m)) {
                deps.push(Dep::Set(*m));
            }
        }
        for d in &deps {
            if let Dep::Set(m) = d
                && !work.st.sets.get(m).is_some_and(|s| s.is_open())
            {
                return Err(eyre!("{} is not a queued landing-set", crate::state::branch(&q.name, *m)));
            }
        }
        let sync = work.git(repo);
        sync.fetch(repo_dir.to_str().unwrap(), &["+HEAD:refs/choros/incoming"])?;
        let own = sync.commits(&through, &head)?;
        let n = work.st.next_n.max(1);
        work.st.next_n = n + 1;

        loop {
            deps.sort();
            deps.dedup();
            // What the fork builds on: its dependencies in this repo.
            let set_deps: Vec<u32> = deps
                .iter()
                .filter_map(|d| if let Dep::Set(m) = d { Some(*m) } else { None })
                .filter(|m| work.st.sets[m].repo() == repo)
                .collect();
            let leaves = graph::leaves(&work.st, &set_deps);
            let main = work.root(repo)?;
            let spec = BaseSpec {
                main: Some(main.clone()),
                deps: leaves.iter().map(|d| (*d, work.st.sets[d].tip.clone())).collect(),
            };
            let tips: Vec<String> = spec.deps.iter().map(|(_, t)| t.clone()).collect();
            let what = crate::state::branch(&q.name, n);
            let base = work
                .build(repo, &main, &tips, &format!("the base of {what}"))?
                .map_err(|(_, files)| eyre!("the dependencies picked for {what} conflict with each other in: {}", files.join(", ")))?;
            let tip = match work.replay(repo, &base, &own, &what)? {
                Ok(t) => t,
                // No one here to resolve in the sync clone (a land from inside a
                // session): move the workspace onto the base instead, so the
                // session that wrote the commits resolves them where they live.
                Err(_) if work.handler.is_none() => {
                    sync.run(&["update-ref", "refs/choros/fork-base", &base])?;
                    drop(work);
                    update_repo(ws, repo, |rl| rl.fork_deps = set_deps.clone())?;
                    let onto = format!("{what}'s dependencies");
                    rebase_workspace(q, ws, repo, &git, "refs/choros/fork-base", &base, None, &onto)?;
                    return land_fork(env, q, ws, repo, &deps, req, checker, None);
                }
                Err(files) => {
                    return Err(eyre!("{what} does not apply onto its dependencies; conflicts in: {}", files.join(", ")));
                }
            };
            let mine = graph::ancestors_of_deps(&work.st, &set_deps);
            let live = graph::live_in(&work.st, repo);
            // As in a plain land, a clean replay can still lose what the commits
            // change; here the dependencies were picked, so say which are missing.
            let lost = lost_changes(&sync, (&through, &head), (&base, &tip))?;
            let missing = owners(&work.st, &sync, &live, &mine, &mut BTreeMap::new(), &lost)?;
            if !missing.is_empty() {
                let names: Vec<String> = missing.iter().map(|m| crate::state::branch(&q.name, *m)).collect();
                return Err(eyre!(
                    "{what} would lose part of what its commits change in {}: they change what {} wrote; \
                     add {} to --deps, or land without --deps",
                    lost.join(", "),
                    names.join(", "),
                    if missing.len() == 1 { "it" } else { "them" }
                ));
            }

            // Settle the fork's place before anything about it is public: check it
            // against every other set of the merge head it would sit beside.
            let clashes: Vec<u32> = clashes(&work.st, &sync, &live, &mine, &tip)?.into_iter().map(|(m, _)| m).collect();
            if !clashes.is_empty() {
                if decision.is_none()
                    && let Some(h) = work.handler.as_deref_mut()
                {
                    decision = h.fork_conflict(n, &clashes)?;
                }
                match decision {
                    Some(OnConflict::Depend) => {
                        deps.extend(clashes.into_iter().map(Dep::Set));
                        continue;
                    }
                    Some(OnConflict::MergeHead) => {}
                    None => {
                        return Err(eyre!(
                            "{what} conflicts with {} in the merge head; pass --on-conflict=depend \
                             (land behind them) or --on-conflict=merge-head (resolve beside them)",
                            clashes
                                .iter()
                                .map(|c| crate::state::branch(&q.name, *c))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                }
            }

            match checker.verdict(&sync, &tip)? {
                Verdict::Pass => {}
                Verdict::Fail(out) => {
                    let on = if set_deps.is_empty() {
                        "on main".to_string()
                    } else {
                        format!("on {}", names(q, &set_deps))
                    };
                    return Err(checker.refusal(&on, &out)?);
                }
                Verdict::Unknown => {
                    drop(work);
                    checker.run(&tip)?;
                    continue 'round;
                }
            }

            let mut set = new_set(n, graph::reduce(&work.st, &deps), spec, base, tip, ws, repo, req);
            set.conflicts_with = clashes;
            work.st.sets.insert(n, set);
            work.publish(n)?;
            // A clash kept beside may park the merge head; `choros queue resolve`
            // settles it, and status says so.
            work.rebuild_merge_head(repo)?;
            work.save()?;
            update_repo(ws, repo, |rl| {
                rl.landed_through = Some(head.clone());
                rl.fork_deps.clear();
            })?;
            return Ok(n);
        }
    }
}

fn names(q: &Queue, sets: &[u32]) -> String {
    sets.iter().map(|m| crate::state::branch(&q.name, *m)).collect::<Vec<_>>().join(", ")
}

/// The leaves beside `mine` that `tip` conflicts with, merged side by
/// side, and in which files.
fn clashes(
    st: &crate::state::QueueState,
    sync: &Git,
    live: &[u32],
    mine: &BTreeSet<u32>,
    tip: &str,
) -> Result<Vec<(u32, Vec<String>)>> {
    let mut out = Vec::new();
    for leaf in graph::leaves(st, live) {
        if mine.contains(&leaf) {
            continue;
        }
        let files = sync.merge_conflicts(&st.sets[&leaf].tip, tip)?;
        if !files.is_empty() {
            out.push((leaf, files));
        }
    }
    Ok(out)
}

enum Least {
    /// The set, and what it depends on and why.
    Landed(u32, Vec<String>),
    /// It lands on everything queued after all; the line saying why, when
    /// the queue holds anything to skip.
    Fallback(Option<String>),
}

/// Where the commits sit on a given start: the sets they depend on, and
/// why each, grown until they replay whole; and the sets beside them they
/// clash with, which they land beside all the same.
struct Placed {
    why: BTreeMap<u32, String>,
    spec: BaseSpec,
    base: String,
    tip: String,
    clashes: Vec<(u32, Vec<String>)>,
}

/// What `place` needs that doesn't change within a round.
struct Commits<'a> {
    repo: &'a str,
    sync: &'a Git,
    live: &'a [u32],
    through: &'a str,
    head: &'a str,
    own: &'a [String],
}

/// Land on as little of the queue as the commits need. Start on main and the
/// workspace's own earlier sets; take in a queued set only when the commits
/// don't replay whole without it. One they merely clash with side by side
/// isn't needed: either could merge first, so the land goes beside it, and
/// the merge head holds the pair's resolution (the resync agent's). Not conflicting
/// isn't the same as not needing (the commits may call what a set adds), so
/// the result must pass the repo's check too. When it doesn't, look for what
/// it was missing: each line of skipped work added alone, newest first, and
/// the line that passes narrowed to where it starts passing. Anything short
/// of that falls back to everything queued, where the check runs again.
fn land_least(env: &Env, q: &Queue, ws: &Path, repo: &str, req: &LandRequest, checker: &mut Checker) -> Result<Least> {
    let repo_dir = ws.join(repo);
    let git = Git::new(&repo_dir);
    let through = landed_through(ws, repo)?;
    check_landable(&git, &through)?;
    let head = git.head()?;
    loop {
        // No handler: a conflict here says what the commits need; it isn't
        // there to be resolved.
        let mut work = Work::begin(q, env.agent, None, true)?.unwrap();
        work.ensure_repo(repo)?;
        let live = graph::live_in(&work.st, repo);
        if live.is_empty() {
            return Ok(Least::Fallback(None));
        }
        let mut start: BTreeMap<u32, String> = BTreeMap::new();
        for m in &live {
            let source = &work.st.sets[m].source;
            if source.workspace == ws && source.repo == repo {
                start.insert(*m, "this workspace's earlier set".into());
            }
        }
        let sync = work.git(repo);
        sync.fetch(repo_dir.to_str().unwrap(), &["+HEAD:refs/choros/incoming"])?;
        let own = sync.commits(&through, &head)?;
        let c = Commits { repo, sync: &sync, live: &live, through: &through, head: &head, own: &own };
        let mut touched = BTreeMap::new();

        let first = match place(&mut work, &c, start.clone(), &mut touched)? {
            Ok(p) => p,
            Err(why) => return Ok(Least::Fallback(why)),
        };
        let chosen = match search(&mut work, &c, checker, &first, &mut touched)? {
            Search::Found(p) => p,
            Search::Run(tip) => {
                drop(work);
                checker.run(&tip)?;
                continue;
            }
            Search::Exhausted => {
                return Ok(Least::Fallback(Some(format!(
                    "`{}` failed on the fewest sets its commits need, and with each line of the \
                     rest added alone; landing on everything queued",
                    checker.check.map_or("", |s| s.as_str())
                ))));
            }
        };

        let Placed { why, spec, base, tip, clashes } = chosen;
        let deps: Vec<u32> = why.keys().copied().collect();
        let mine = graph::ancestors_of_deps(&work.st, &deps);
        let n = work.st.next_n.max(1);
        work.st.next_n = n + 1;
        let deps_of_set = graph::leaves(&work.st, &deps).into_iter().map(Dep::Set).collect();
        let mut set = new_set(n, deps_of_set, spec, base, tip, ws, repo, req);
        set.conflicts_with = clashes.iter().map(|(m, _)| *m).collect();
        work.st.sets.insert(n, set);
        work.publish(n)?;
        work.rebuild_merge_head(repo)?;
        work.save()?;
        update_repo(ws, repo, |rl| {
            rl.landed_through = Some(head.clone());
            rl.fork_deps.clear();
        })?;

        let mut notes: Vec<String> =
            why.iter().map(|(m, w)| format!("on {}: {w}", crate::state::branch(&q.name, *m))).collect();
        for (m, files) in &clashes {
            notes.push(format!(
                "beside {}: they clash in {}, but either can merge first; the resync agent settles the pair \
                 in the merge head",
                crate::state::branch(&q.name, *m),
                files.join(", ")
            ));
        }
        let ahead: Vec<u32> = live.iter().copied().filter(|m| !mine.contains(m)).collect();
        if !ahead.is_empty() {
            notes.push(match checker.check {
                Some(check) => format!("ahead of {}; `{check}` passed without them", names(q, &ahead)),
                None => format!("ahead of {}; not checked (--force)", names(q, &ahead)),
            });
        }
        return Ok(Least::Landed(n, notes));
    }
}

enum Search {
    Found(Placed),
    /// This tip must be checked before the search can go on.
    Run(String),
    Exhausted,
}

/// What the check needs beyond `first`. Every probe adds whole lines of
/// work — a set with what it builds on — since a set can't land without
/// its own base. More sets can only help a check pass, so a line that fails
/// rules out everything below it too, and along the line that passes the
/// first passing prefix ends at the set the commits were missing.
fn search(
    work: &mut Work,
    c: &Commits,
    checker: &Checker,
    first: &Placed,
    touched: &mut BTreeMap<u32, BTreeSet<String>>,
) -> Result<Search> {
    let mut probe = |work: &mut Work, extra: &[u32]| -> Result<Probe> {
        let mut start = first.why.clone();
        for m in graph::leaves(&work.st, extra) {
            start.entry(m).or_insert_with(|| "the check fails without it".into());
        }
        let Ok(p) = place(work, c, start, touched)? else {
            return Ok(Probe::Fail);
        };
        Ok(match checker.verdict(c.sync, &p.tip)? {
            Verdict::Pass => Probe::Pass(p),
            Verdict::Fail(_) => Probe::Fail,
            Verdict::Unknown => Probe::Run(p.tip),
        })
    };
    macro_rules! probe {
        ($extra:expr) => {
            match probe(work, $extra)? {
                Probe::Run(tip) => return Ok(Search::Run(tip)),
                Probe::Pass(p) => Some(p),
                Probe::Fail => None,
            }
        };
    }
    if let Some(p) = probe!(&[]) {
        return Ok(Search::Found(p));
    }

    let mine = graph::ancestors_of_deps(&work.st, &first.why.keys().copied().collect::<Vec<_>>());
    let skipped: Vec<u32> = c.live.iter().copied().filter(|m| !mine.contains(m)).collect();
    let line = |st: &crate::state::QueueState, top: u32| -> Vec<u32> {
        let below = graph::ancestors(st, top);
        graph::topo(st).into_iter().filter(|m| *m == top || (below.contains(m) && skipped.contains(m))).collect()
    };
    let mut leaves = graph::leaves(&work.st, &skipped);
    leaves.reverse();
    for leaf in leaves {
        let whole = line(&work.st, leaf);
        // A line holding all that was skipped is everything queued, which
        // the fallback lands on and checks; only its prefixes are worth a
        // probe here.
        let mut best = None;
        if whole.len() < skipped.len() {
            match probe!(&whole) {
                Some(p) => best = Some(p),
                None => continue,
            }
        }
        // The shortest prefix of the line that passes; every prefix of a
        // line in dependency order carries its own bases.
        let (mut lo, mut hi) = (0, whole.len());
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            match probe!(&whole[..mid]) {
                Some(p) => (hi, best) = (mid, Some(p)),
                None => lo = mid,
            }
        }
        let Some(mut best) = best else { break };
        // The prefix may carry sets beside the one that made it pass.
        let needed = line(&work.st, whole[hi - 1]);
        if needed.len() < hi
            && let Some(p) = probe!(&needed)
        {
            best = p;
        }
        return Ok(Search::Found(best));
    }
    Ok(Search::Exhausted)
}

enum Probe {
    Pass(Placed),
    Fail,
    Run(String),
}

/// Grow `why` from conflicts until the commits replay onto it whole, and
/// note what they clash with beside it. `Err` when only everything queued will do,
/// with why when that's news: needing every set is just where it belongs.
fn place(
    work: &mut Work,
    c: &Commits,
    mut why: BTreeMap<u32, String>,
    touched: &mut BTreeMap<u32, BTreeSet<String>>,
) -> Result<std::result::Result<Placed, Option<String>>> {
    let what = format!("{}'s land", c.repo);
    loop {
        let deps: Vec<u32> = why.keys().copied().collect();
        let mine = graph::ancestors_of_deps(&work.st, &deps);
        if c.live.iter().all(|m| mine.contains(m)) {
            return Ok(Err(None));
        }
        let main = work.root(c.repo)?;
        let spec = BaseSpec {
            main: Some(main.clone()),
            deps: graph::leaves(&work.st, &deps)
                .into_iter()
                .map(|d| (d, work.st.sets[&d].tip.clone()))
                .collect(),
        };
        let tips: Vec<String> = spec.deps.iter().map(|(_, t)| t.clone()).collect();
        let Ok(base) = work.build(c.repo, &main, &tips, &what)? else {
            return Ok(Err(Some(
                "landed on everything queued: the sets its commits need conflict with each other".into(),
            )));
        };
        let tip = match work.replay(c.repo, &base, c.own, &what)? {
            Ok(tip) => tip,
            Err(files) => {
                let owners = owners(&work.st, c.sync, c.live, &mine, touched, &files)?;
                if owners.is_empty() {
                    return Ok(Err(Some(format!(
                        "landed on everything queued: its commits conflict with main itself in {}",
                        files.join(", ")
                    ))));
                }
                for m in owners {
                    why.insert(m, format!("its commits build on it ({})", files.join(", ")));
                }
                continue;
            }
        };
        // A replay can go through clean and still lose changes: deleting or
        // re-deleting lines a skipped set added is "already done" without
        // that set, so the hunk vanishes. Such commits need the set.
        let lost = lost_changes(c.sync, (c.through, c.head), (&base, &tip))?;
        let owners = owners(&work.st, c.sync, c.live, &mine, touched, &lost)?;
        if !owners.is_empty() {
            for m in owners {
                why.insert(m, format!("its commits change what it wrote ({})", lost.join(", ")));
            }
            continue;
        }
        let clashes = clashes(&work.st, c.sync, c.live, &mine, &tip)?;
        return Ok(Ok(Placed { why, spec, base, tip, clashes }));
    }
}

/// The live sets outside `mine` that change any of `files`.
fn owners(
    st: &crate::state::QueueState,
    sync: &Git,
    live: &[u32],
    mine: &BTreeSet<u32>,
    touched: &mut BTreeMap<u32, BTreeSet<String>>,
    files: &[String],
) -> Result<Vec<u32>> {
    let mut owners = Vec::new();
    for m in live.iter().filter(|m| !mine.contains(m)) {
        if !touched.contains_key(m) {
            let set = &st.sets[m];
            let changed = sync.run(&["diff", "--name-only", &set.base_sha, &set.tip])?;
            touched.insert(*m, changed.lines().map(str::to_string).collect());
        }
        if files.iter().any(|f| touched[m].contains(f)) {
            owners.push(*m);
        }
    }
    Ok(owners)
}

/// Files whose added and removed lines differ between the commits as written
/// and as replayed.
fn lost_changes(git: &Git, written: (&str, &str), replayed: (&str, &str)) -> Result<Vec<String>> {
    let a = changed_lines(git, written)?;
    let b = changed_lines(git, replayed)?;
    let files: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    Ok(files
        .into_iter()
        .filter(|f| a.get(*f) != b.get(*f))
        .cloned()
        .collect())
}

/// Each file's `+`/`-` lines from `from` to `to`, sorted, so that where a
/// hunk lands doesn't matter, only what it changes.
pub(crate) fn changed_lines(git: &Git, (from, to): (&str, &str)) -> Result<BTreeMap<String, Vec<String>>> {
    let diff = git.run(&["diff", "--no-color", "--no-renames", "--no-ext-diff", "-U0", from, to])?;
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut file = None;
    let mut in_hunk = false;
    for line in diff.lines() {
        if let Some(name) = line.strip_prefix("diff --git a/") {
            file = name.split(" b/").next().map(str::to_string);
            in_hunk = false;
        } else if line.starts_with("@@") {
            in_hunk = true;
        } else if in_hunk
            && (line.starts_with('+') || line.starts_with('-'))
            && let Some(f) = &file
        {
            out.entry(f.clone()).or_default().push(line.to_string());
        }
    }
    for lines in out.values_mut() {
        lines.sort();
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn new_set(
    n: u32,
    deps: Vec<Dep>,
    base: BaseSpec,
    base_sha: String,
    tip: String,
    ws: &Path,
    repo: &str,
    req: &LandRequest,
) -> LandingSet {
    LandingSet {
        n,
        deps,
        base,
        base_sha,
        tip,
        status: SetStatus::Queued,
        park: None,
        pr: None,
        review: None,
        remote: None,
        merged_into: None,
        source: Source {
            workspace: ws.to_path_buf(),
            repo: repo.to_string(),
        },
        mode: req.mode,
        title: req.title.clone(),
        body: req.body.clone(),
        labels: req.labels.clone(),
        sessions: Vec::new(),
        conflicts_with: Vec::new(),
        pending: None,
    }
}

/// `choros repo sync`: move a repo's unlanded commits onto its queue's current
/// merge head.
pub fn sync_workspace(env: &Env, cwd: &Path) -> Result<Vec<String>> {
    let (trunk, ws) = core::find_workspace(cwd)?;
    refuse_landing_folder(&ws)?;
    let meta = core::read_meta(&ws)?;
    let repos = match repo_of(&ws, cwd, &meta) {
        Some(r) => vec![r],
        None => meta.repos.clone(),
    };
    let mut synced = Vec::new();
    for repo in repos {
        settle_pending(&ws, &repo)?;
        let Some(queue) = workspace::queue_for(&trunk, &ws, &repo)? else {
            continue;
        };
        let q = Queue::open(&trunk, &queue)?;
        let before = Git::new(&ws.join(&repo)).head()?;
        sync_to_merge_head(env, &q, &ws, &repo)?;
        if Git::new(&ws.join(&repo)).head()? != before {
            synced.push(repo);
        }
    }
    Ok(synced)
}

/// Bind a workspace's repo to a queue (or to none, for instant mode). A repo
/// with no work of its own yet is moved onto the merge head, so new work
/// starts there.
pub fn bind(env: &Env, ws: &Path, repo: &str, queue: Option<&str>) -> Result<()> {
    let trunk = space::find_trunk(ws).ok_or_else(|| eyre!("{} is not under a choros trunk", ws.display()))?;
    let git = Git::new(&ws.join(repo));
    let Some(queue) = queue else {
        return update_repo(ws, repo, |rl| rl.queue = Some(workspace::NO_QUEUE.to_string()));
    };
    let q = Queue::open(&trunk, queue)?;
    if !q.config.scope.admits(&space::rel(&trunk, ws)?) {
        return Err(eyre!("queue {queue} only takes landings from {}", q.config.scope));
    }
    let mh = current_merge_head(env, &q, repo)?;

    git.fetch("origin", &[])?;
    let main = format!("refs/remotes/origin/{}", q.read_state()?.repos[repo].main);
    let fresh = git.commits(&main, "HEAD")?.is_empty() && git.is_clean()?;
    if fresh {
        let mh_ref = format!("refs/heads/{}/merge-head", q.name);
        git.fetch(q.sync_dir(repo).to_str().unwrap(), &[&format!("+{mh_ref}:refs/choros/merge-head")])?;
        git.reset_hard("refs/choros/merge-head")?;
        update_repo(ws, repo, |rl| {
            rl.queue = Some(queue.to_string());
            rl.landed_through = Some(mh.sha.clone());
            rl.composition = Some(mh.composition.key());
        })
    } else {
        let point = git.run(&["merge-base", "HEAD", &main])?;
        update_repo(ws, repo, |rl| {
            rl.queue = Some(queue.to_string());
            rl.landed_through.get_or_insert(point);
            rl.composition = None;
        })
    }
}

/// No queue: rebase onto main and put it there, by push or by a PR merged on
/// the spot. A PR that couldn't merge then (a review it needs, checks still
/// running) is remembered, and finished here first: retried while open,
/// counted once merged, forgotten if closed, which leaves its commits to
/// land again.
fn land_instant(env: &Env, trunk: &Path, ws: &Path, repo: &str, req: &LandRequest) -> Result<Landed> {
    let git = Git::new(&ws.join(repo));
    let landed = Landed {
        repo: repo.to_string(),
        queue: None,
        n: None,
        landing: None,
        forked_sessions: Vec::new(),
        notes: Vec::new(),
        warnings: Vec::new(),
    };
    let earlier = workspace::read(ws)?.repo.get(repo).and_then(|r| r.instant_pr.clone());
    if let Some(pr) = earlier {
        match env.forge.pr_state(git.dir(), pr.number)? {
            PrState::Open => merge_instant_pr(env, &git, trunk, ws, repo, &pr)?,
            PrState::Merged => finish_instant_pr(trunk, ws, repo, &pr)?,
            PrState::Closed => update_repo(ws, repo, |rl| rl.instant_pr = None)?,
        }
        if !has_unlanded(ws, repo)? {
            return Ok(landed);
        }
    }

    let through = landed_through(ws, repo)?;
    check_landable(&git, &through)?;
    git.fetch("origin", &[])?;
    let main = main_branch(&git)?;
    let origin_main = format!("refs/remotes/origin/{main}");
    let main_sha = git.sha(&origin_main)?;
    update_repo(ws, repo, |rl| rl.pending = Some(main_sha))?;
    let sessions = pick_sessions(env, ws, &session_scope(ws, repo, req)?, req);
    if !git.succeeds(&["rebase", "--onto", &origin_main, &through])? {
        return Err(eyre!(
            "rebasing onto {main} conflicts in: {}\nresolve them, `git rebase --continue`, then land again",
            git.unmerged()?.join(", ")
        ));
    }
    let head = git.head()?;
    match crate::instant::via(trunk, ws)? {
        crate::instant::Via::Push => {
            git.run(&["push", "--quiet", "origin", &format!("HEAD:refs/heads/{main}")])?;
        }
        crate::instant::Via::Pr => {
            let ws_name = ws.file_name().and_then(|n| n.to_str()).unwrap_or("choros");
            let branch = format!("{ws_name}/instant-{}", &head[..12]);
            git.run(&["push", "--quiet", "origin", &format!("HEAD:refs/heads/{branch}")])?;
            let title = match &req.title {
                Some(t) => t.clone(),
                None => git.subject(&head)?,
            };
            let body = match &req.body {
                Some(b) => b.clone(),
                None => env.forge.pr_template(git.dir()).ok().flatten().unwrap_or_default(),
            };
            let pr = env.forge.create_pr(
                git.dir(),
                &PrRequest {
                    head: &branch,
                    base: &main,
                    title: &title,
                    body: &body,
                    draft: false,
                },
            )?;
            let pr = InstantPr {
                number: pr.number,
                head: head.clone(),
                labels: req.labels.clone(),
                sessions,
            };
            // `pending` stays: if the PR closes unmerged, the work is unlanded
            // again from the main it was rebased onto.
            update_repo(ws, repo, |rl| rl.instant_pr = Some(pr.clone()))?;
            return merge_instant_pr(env, &git, trunk, ws, repo, &pr).map(|()| landed);
        }
    }
    // Logged before the workspace counts it landed: a land that fails between
    // the two runs again and logs twice, which readers shrug off, rather
    // than never logging at all.
    crate::instant::record(
        trunk,
        &crate::instant::Landed {
            repo: repo.to_string(),
            workspace: ws.to_path_buf(),
            commit: head.clone(),
            pr: None,
            labels: req.labels.clone(),
            sessions,
        },
    )?;
    update_repo(ws, repo, |rl| {
        rl.landed_through = Some(head);
        rl.pending = None;
        rl.last_landed = Some(now_secs());
    })?;
    Ok(landed)
}

fn merge_instant_pr(env: &Env, git: &Git, trunk: &Path, ws: &Path, repo: &str, pr: &InstantPr) -> Result<()> {
    let main = main_branch(git)?;
    let settings = env.forge.settings(git.dir(), &main)?;
    let method = *settings
        .methods
        .first()
        .ok_or_else(|| eyre!("the repo allows no merge method"))?;
    env.forge.merge_pr(git.dir(), pr.number, method).map_err(|e| {
        eyre!(
            "PR #{} for {repo} is open but couldn't be merged: {e}
             land again once it can merge; it won't be opened a second time",
            pr.number
        )
    })?;
    finish_instant_pr(trunk, ws, repo, pr)
}

/// Logged first, as on the push path.
fn finish_instant_pr(trunk: &Path, ws: &Path, repo: &str, pr: &InstantPr) -> Result<()> {
    crate::instant::record_pr(trunk, ws, repo, pr)?;
    update_repo(ws, repo, |rl| {
        rl.landed_through = Some(pr.head.clone());
        rl.instant_pr = None;
        rl.pending = None;
        rl.last_landed = Some(now_secs());
    })
}

/// `<trunk>/landing/<queue>/landing-N/`: a workspace of its own on the set's
/// branch, where review fixes are made and pushed.
fn make_landing(q: &Queue, n: u32, parent: &Path) -> Result<PathBuf> {
    let dir = q.landing_dir(n);
    let st = q.read_state()?;
    let set = &st.sets[&n];
    let repo = &set.repo().to_string();
    let branch = set.branch(&q.name);
    if dir.exists() {
        return Ok(dir);
    }
    std::fs::create_dir_all(&dir)?;
    let reg = registry::ensure_repo_exists(&q.trunk, repo)?;
    let url = core_git::remote_get_url(&reg, "origin")?;
    let clone = dir.join(repo);
    core_git::clone_with_reference(&reg, &url, &clone)?;
    let git = Git::new(&clone);
    git.fetch(
        q.sync_dir(repo).to_str().unwrap(),
        &[&format!("+refs/heads/{branch}:refs/heads/{branch}")],
    )?;
    git.run(&["checkout", "--quiet", &branch])?;
    if set.remote.is_some() {
        git.run(&["branch", "--quiet", "--set-upstream-to", &format!("origin/{branch}")])?;
    }

    let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default();
    core::write_meta(
        &dir,
        &ChorosMeta {
            name: format!("landing-{n}"),
            created_at: now,
            repos: vec![repo.clone()],
            branch: Some(branch.clone()),
        },
    )?;
    let mut wl = workspace::read(&dir)?;
    wl.landing = Some(LandingOf {
        queue: q.name.clone(),
        n,
        parent: parent.to_path_buf(),
    });
    workspace::write(&dir, &wl)?;
    core_agent::provision(&dir)?;
    std::fs::write(
        dir.join("CLAUDE.md"),
        format!(
            "This is a Choros landing: {repo} on `{branch}`, which becomes a PR once \
             everything it depends on has merged.\n\n\
             Choros may move this branch (re-syncing it onto new dependencies, or \
             merging main into it once the PR is open). Run `git pull` before working.\n\
             Answer review feedback with new commits pushed to `{branch}`; never \
             rewrite or force-push it.{}\n",
            if q.config.push.private_sets {
                ""
            } else {
                " Until its PR opens the branch is not on origin, so wait for that first."
            }
        ),
    )?;
    Ok(dir)
}

#[allow(clippy::too_many_arguments)]
fn fork_sessions(
    env: &Env,
    q: &Queue,
    n: u32,
    ws: &Path,
    repo: &str,
    landing: &Path,
    req: &LandRequest,
    warnings: &mut Vec<String>,
) -> Result<Vec<String>> {
    let sources = pick_sessions(env, ws, &session_scope(ws, repo, req)?, req);
    let branch = crate::state::branch(&q.name, n);
    let mut forks = Vec::new();
    for source in sources {
        let fork = crate::uuid::v4();
        let note = format!(
            "Choros landed this work as `{branch}` and forked this session into the \
             landing folder {}. The land is complete. Reply only with: ready. \
             Later turns here will be about review feedback on that PR.",
            landing.display()
        );
        match env.agent.fork_session(landing, &source, &fork, &format!("{} landing-{n}", q.name), &note) {
            Ok(()) => forks.push(SessionFork { source, fork }),
            Err(e) => warnings.push(format!("could not fork session {source}: {e}")),
        }
    }
    let ids: Vec<String> = forks.iter().map(|f| f.fork.clone()).collect();
    let lock = q.lock()?;
    let mut st = q.read_state()?;
    if let Some(set) = st.sets.get_mut(&n) {
        set.sessions = forks;
    }
    q.write_state(&st)?;
    drop(lock);
    update_repo(ws, repo, |rl| rl.last_landed = Some(now_secs()))?;
    Ok(ids)
}

/// Where to look for sessions that touched the work: the repo, when `land`
/// ran inside it, else the whole workspace.
fn session_scope(ws: &Path, repo: &str, req: &LandRequest) -> Result<PathBuf> {
    Ok(if repo_of(ws, &req.cwd, &core::read_meta(ws)?).is_some() {
        ws.join(repo)
    } else {
        ws.to_path_buf()
    })
}

/// The sessions to fork: those named; else the one `land` runs inside; else
/// every Claude Code session in scope touched since the last land.
fn pick_sessions(env: &Env, ws: &Path, scope: &Path, req: &LandRequest) -> Vec<String> {
    if !req.sessions.is_empty() {
        return req.sessions.clone();
    }
    if let Some(id) = req.env_session.as_ref().filter(|id| !id.is_empty()) {
        return vec![id.clone()];
    }
    let Some(home) = &env.home else {
        return Vec::new();
    };
    let since = workspace::read(ws)
        .ok()
        .and_then(|wl| wl.repo.values().filter_map(|r| r.last_landed).max())
        .map(|s| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(s))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut sessions = core_agent::ls::claude_sessions(home, scope).unwrap_or_default();
    sessions.retain(|s| s.modified > since);
    sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
    sessions.into_iter().map(|s| s.id).collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
