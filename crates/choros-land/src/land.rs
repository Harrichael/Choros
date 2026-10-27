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
use crate::repos::{self, DepsDefault};
use crate::state::{BaseSpec, Dep, LandMode, LandingSet, MergeHead, SessionFork, SetStatus, Source};
use crate::workspace::{self, InstantPr, LandingOf, RepoLanding};

pub struct LandRequest {
    /// Where `land` was run: a repo (land that repo) or the workspace root
    /// (land every repo with something to land).
    pub cwd: PathBuf,
    pub mode: LandMode,
    /// What to land on instead of everything queued; `None` follows the
    /// repo's `deps` setting.
    pub fork: Option<ForkDeps>,
    pub on_conflict: Option<OnConflict>,
    pub title: Option<String>,
    pub body: Option<String>,
    /// Sessions to fork into the landing; empty means "work it out".
    pub sessions: Vec<String>,
    /// The Claude Code session `land` is running inside, if any (the CLI
    /// reads `CLAUDE_CODE_SESSION_ID`).
    pub env_session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkDeps {
    /// These sets (plus the workspace's own earlier ones); empty is main.
    Picked(Vec<Dep>),
    /// As few as the commits need: see [`land_auto`].
    Auto,
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
        if queues[repo].is_none() {
            continue;
        }
        let config = repos::read(&trunk, repo)?;
        let auto = match &req.fork {
            Some(ForkDeps::Auto) => true,
            Some(ForkDeps::Picked(_)) => false,
            None => config.deps == DepsDefault::Auto,
        };
        if auto {
            let check = config.check.ok_or_else(|| {
                eyre!("--deps auto needs a check for {repo}: set one under [{repo}] in .choros-config/repos.toml")
            })?;
            checks.insert(repo.clone(), check);
        }
    }

    let mut landed = Vec::new();
    for repo in &scope {
        if !has_unlanded(&ws, repo)? {
            continue;
        }
        let out = match &queues[repo] {
            None => land_instant(env, &trunk, &ws, repo)?,
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
fn repo_of(ws: &Path, cwd: &Path, meta: &ChorosMeta) -> Option<String> {
    let rel = cwd.strip_prefix(ws).ok()?;
    let first = rel.components().next()?.as_os_str().to_str()?;
    meta.repos.iter().find(|r| r.as_str() == first).cloned()
}

fn has_unlanded(ws: &Path, repo: &str) -> Result<bool> {
    let git = Git::new(&ws.join(repo));
    let through = landed_through(ws, repo)?;
    Ok(through != git.head()?)
}

/// Where this repo's landing history starts. A workspace made before
/// landing existed has no record; its branch point from main stands in.
/// Repos of `ws` holding commits that haven't been landed, and how many.
pub fn unlanded(ws: &Path) -> Result<Vec<(String, usize)>> {
    let mut out = Vec::new();
    for repo in core::read_meta(ws)?.repos {
        let through = landed_through(ws, &repo)?;
        let n = Git::new(&ws.join(&repo)).commits(&through, "HEAD")?.len();
        if n > 0 {
            out.push((repo, n));
        }
    }
    Ok(out)
}

fn landed_through(ws: &Path, repo: &str) -> Result<String> {
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
            "{:?} is in the middle of a rebase or merge; finish it (`git rebase --continue`) \
             or abort it, then try again",
            git.dir()
        ));
    }
    let Some(pending) = workspace::read(ws)?.repo.get(repo).and_then(|r| r.pending.clone()) else {
        return Ok(());
    };
    let reached = git.is_ancestor(&pending, "HEAD")?;
    update_repo(ws, repo, |rl| {
        let composition = rl.pending_composition.take();
        if reached {
            rl.landed_through = Some(pending);
            rl.composition = composition;
        }
        rl.pending = None;
    })
}

fn check_clean(git: &Git, through: &str) -> Result<()> {
    if !git.is_clean()? {
        return Err(eyre!("{:?} has uncommitted changes; commit or stash them first", git.dir()));
    }
    if !git.is_ancestor(through, "HEAD")? {
        return Err(eyre!(
            "commits that were already landed have been rewritten in {:?}; \
             the landed history must stay as it was",
            git.dir()
        ));
    }
    Ok(())
}

fn check_landable(git: &Git, through: &str) -> Result<()> {
    check_clean(git, through)?;
    if git.has_merges(through, "HEAD")? {
        return Err(eyre!(
            "the commits to land in {:?} include a merge; rebase them onto the \
             merge head instead (`choros repo sync`)",
            git.dir()
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
    let needed = known.as_deref() != Some(key.as_str())
        || !git.is_ancestor(&mh.sha, "HEAD")?
        || git.has_merges(&through, "HEAD")?;
    if needed {
        let mh_ref = format!("refs/heads/{}/merge-head", q.name);
        let what = format!("{}'s merge head", q.name);
        rebase_workspace(q, ws, repo, &git, &mh_ref, &mh.sha, Some(key), &what)?;
    }
    Ok(mh)
}

/// `auto_check` is set when the land picks its own dependencies: the repo's
/// check, which the result must pass.
fn land_queued(
    env: &Env,
    q: &Queue,
    ws: &Path,
    repo: &str,
    req: &LandRequest,
    auto_check: Option<&String>,
    handler: Handler<'_>,
) -> Result<Landed> {
    let mut warnings = Vec::new();
    let mut notes = Vec::new();

    let n = match (&req.fork, auto_check) {
        (_, Some(check)) => match land_auto(env, q, ws, repo, check, req)? {
            Auto::Landed(n, why) => {
                notes = why;
                n
            }
            Auto::Fallback(why) => {
                warnings.push(format!("landed on everything queued: {why}"));
                land_normal(env, q, ws, repo, req)?
            }
        },
        (Some(ForkDeps::Picked(deps)), None) => land_fork(env, q, ws, repo, deps, req, handler)?,
        (_, None) => land_normal(env, q, ws, repo, req)?,
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

/// Land on the merge head: the workspace is first synced onto it (so any
/// conflict is resolved here, by whoever wrote the code), after which the
/// landing-set is simply the workspace's tip.
fn land_normal(
    env: &Env,
    q: &Queue,
    ws: &Path,
    repo: &str,
    req: &LandRequest,
) -> Result<u32> {
    let repo_dir = ws.join(repo);
    let git = Git::new(&repo_dir);
    // Each round either lands or moves the workspace onto a newer merge head;
    // the merge head only moves when someone else lands meanwhile.
    for _ in 0..5 {
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
            continue;
        }

        let n = work.st.next_n.max(1);
        work.st.next_n = n + 1;
        let deps = work.next_dep_set(repo);
        let base = BaseSpec {
            main: Some(now.composition.main.clone()),
            deps: now.composition.sets.clone(),
        };
        work.git(repo)
            .fetch(repo_dir.to_str().unwrap(), &["+HEAD:refs/choros/incoming"])?;
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
            rl.landed(&q.name, n, &head);
            rl.composition = key;
        })?;
        return Ok(n);
    }
    Err(eyre!("the merge head kept moving while landing; try again"))
}

/// Land beside the queue rather than on top of it: on main, or on the
/// dependencies picked. The workspace's own earlier sets always come along.
/// Picked sets in other repos are only waited for.
fn land_fork(
    env: &Env,
    q: &Queue,
    ws: &Path,
    repo: &str,
    picked: &[Dep],
    req: &LandRequest,
    handler: Handler<'_>,
) -> Result<u32> {
    let repo_dir = ws.join(repo);
    let git = Git::new(&repo_dir);
    let through = landed_through(ws, repo)?;
    check_landable(&git, &through)?;
    let head = git.head()?;
    let mut work = Work::begin(q, env.agent, handler, true)?.unwrap();
    work.ensure_repo(repo)?;
    let mut deps: Vec<Dep> = picked.to_vec();
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
                return land_fork(env, q, ws, repo, &deps, req, None);
            }
            Err(files) => {
                return Err(eyre!("{what} does not apply onto its dependencies; conflicts in: {}", files.join(", ")));
            }
        };
        let mine = graph::ancestors_of_deps(&work.st, &set_deps);
        let live = graph::live_in(&work.st, repo);
        // As in an auto land, a clean replay can still lose what the commits
        // change; here the dependencies were picked, so say which are missing.
        let lost = lost_changes(&sync, (&through, &head), (&base, &tip))?;
        let missing = owners(&work.st, &sync, &live, &mine, &mut BTreeMap::new(), &lost)?;
        if !missing.is_empty() {
            let names: Vec<String> = missing.iter().map(|m| crate::state::branch(&q.name, *m)).collect();
            return Err(eyre!(
                "{what} would lose part of what its commits change in {}: they change what {} wrote; \
                 add {} to --deps, or use --deps auto",
                lost.join(", "),
                names.join(", "),
                if missing.len() == 1 { "it" } else { "them" }
            ));
        }

        // Settle the fork's place before anything about it is public: check it
        // against every other leaf of the merge head it would sit beside.
        let mut clashes = Vec::new();
        for leaf in graph::leaves(&work.st, &live) {
            if mine.contains(&leaf) {
                continue;
            }
            if !sync.merge_conflicts(&work.st.sets[&leaf].tip, &tip)?.is_empty() {
                clashes.push(leaf);
            }
        }
        if !clashes.is_empty() {
            let decision = match req.on_conflict {
                Some(d) => Some(d),
                None => match work.handler.as_deref_mut() {
                    Some(h) => h.fork_conflict(n, &clashes)?,
                    None => None,
                },
            };
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

        let mut set = new_set(n, deps.clone(), spec, base, tip, ws, repo, req);
        set.conflicts_with = clashes;
        work.st.sets.insert(n, set);
        work.publish(n)?;
        // A clash kept beside may park the merge head; `choros queue resolve`
        // settles it, and status says so.
        work.rebuild_merge_head(repo)?;
        work.save()?;
        update_repo(ws, repo, |rl| {
            rl.landed(&q.name, n, &head);
            rl.fork_deps.clear();
        })?;
        return Ok(n);
    }
}

enum Auto {
    /// The set, and what it depends on and why.
    Landed(u32, Vec<String>),
    /// Why it should land on everything queued after all.
    Fallback(String),
}

/// Land on as little of the queue as the commits need. Start on main and the
/// workspace's own earlier sets; take in a queued set only when the commits
/// conflict with it, replaying onto it or merged beside it. Not conflicting
/// isn't the same as not needing (the commits may call what a set adds), so
/// the result must also pass the repo's `check` without the rest. Anything
/// short of that falls back to landing on everything queued.
fn land_auto(env: &Env, q: &Queue, ws: &Path, repo: &str, check: &str, req: &LandRequest) -> Result<Auto> {
    let repo_dir = ws.join(repo);
    let git = Git::new(&repo_dir);
    let through = landed_through(ws, repo)?;
    check_landable(&git, &through)?;
    let head = git.head()?;
    // No handler: a conflict here says what the commits need; it isn't
    // there to be resolved.
    let mut work = Work::begin(q, env.agent, None, true)?.unwrap();
    work.ensure_repo(repo)?;
    let live = graph::live_in(&work.st, repo);
    let mut why: BTreeMap<u32, String> = BTreeMap::new();
    for m in &live {
        let source = &work.st.sets[m].source;
        if source.workspace == ws && source.repo == repo {
            why.insert(*m, "this workspace's earlier set".into());
        }
    }
    let sync = work.git(repo);
    sync.fetch(repo_dir.to_str().unwrap(), &["+HEAD:refs/choros/incoming"])?;
    let own = sync.commits(&through, &head)?;
    let what = format!("{repo}'s auto land");
    let mut touched: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();

    loop {
        let deps: Vec<u32> = why.keys().copied().collect();
        let mine = graph::ancestors_of_deps(&work.st, &deps);
        if live.iter().all(|m| mine.contains(m)) {
            return Ok(Auto::Fallback("its commits need every queued set".into()));
        }
        let main = work.root(repo)?;
        let spec = BaseSpec {
            main: Some(main.clone()),
            deps: graph::leaves(&work.st, &deps)
                .into_iter()
                .map(|d| (d, work.st.sets[&d].tip.clone()))
                .collect(),
        };
        let tips: Vec<String> = spec.deps.iter().map(|(_, t)| t.clone()).collect();
        let Ok(base) = work.build(repo, &main, &tips, &what)? else {
            return Ok(Auto::Fallback("the sets its commits need conflict with each other".into()));
        };
        let tip = match work.replay(repo, &base, &own, &what)? {
            Ok(tip) => tip,
            Err(files) => {
                let owners = owners(&work.st, &sync, &live, &mine, &mut touched, &files)?;
                if owners.is_empty() {
                    return Ok(Auto::Fallback(format!(
                        "its commits conflict with main itself in {}",
                        files.join(", ")
                    )));
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
        let lost = lost_changes(&sync, (&through, &head), (&base, &tip))?;
        let owners = owners(&work.st, &sync, &live, &mine, &mut touched, &lost)?;
        if !owners.is_empty() {
            for m in owners {
                why.insert(m, format!("its commits change what it wrote ({})", lost.join(", ")));
            }
            continue;
        }
        let mut clashed = false;
        for leaf in graph::leaves(&work.st, &live) {
            if mine.contains(&leaf) {
                continue;
            }
            let files = sync.merge_conflicts(&work.st.sets[&leaf].tip, &tip)?;
            if !files.is_empty() {
                why.insert(leaf, format!("they conflict side by side ({})", files.join(", ")));
                clashed = true;
            }
        }
        if clashed {
            continue;
        }
        if let Err(output) = run_check(&sync, &tip, check)? {
            return Ok(Auto::Fallback(format!("`{check}` failed without the rest of the queue:\n{output}")));
        }

        let n = work.st.next_n.max(1);
        work.st.next_n = n + 1;
        let deps_of_set = deps.iter().map(|m| Dep::Set(*m)).collect();
        work.st.sets.insert(n, new_set(n, deps_of_set, spec, base, tip, ws, repo, req));
        work.publish(n)?;
        work.rebuild_merge_head(repo)?;
        work.save()?;
        update_repo(ws, repo, |rl| {
            rl.landed(&q.name, n, &head);
            rl.fork_deps.clear();
        })?;

        let name = |m: u32| crate::state::branch(&q.name, m);
        let mut notes: Vec<String> = why.iter().map(|(m, w)| format!("on {}: {w}", name(*m))).collect();
        let ahead: Vec<String> = live.iter().filter(|m| !mine.contains(m)).map(|m| name(*m)).collect();
        if !ahead.is_empty() {
            notes.push(format!("ahead of {}; `{check}` passed without them", ahead.join(", ")));
        }
        return Ok(Auto::Landed(n, notes));
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
fn changed_lines(git: &Git, (from, to): (&str, &str)) -> Result<BTreeMap<String, Vec<String>>> {
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

/// Run `check` in the sync clone at `tip`: `Err` with the end of its output
/// when it fails. Tracked files are put back after, so the next step starts
/// clean; build output is left for the next run to reuse.
fn run_check(sync: &Git, tip: &str, check: &str) -> Result<std::result::Result<(), String>> {
    sync.checkout_detached(tip)?;
    let out = std::process::Command::new("sh")
        .args(["-c", check])
        .current_dir(sync.dir())
        .output()
        .map_err(|e| eyre!("running `{check}`: {e}"))?;
    sync.reset_hard(tip)?;
    if out.status.success() {
        return Ok(Ok(()));
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<&str> = text.lines().collect();
    Ok(Err(lines[lines.len().saturating_sub(20)..].join("\n")))
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
        remote: None,
        merged_into: None,
        source: Source {
            workspace: ws.to_path_buf(),
            repo: repo.to_string(),
        },
        mode: req.mode,
        title: req.title.clone(),
        body: req.body.clone(),
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
    let trunk = space::find_trunk(ws).ok_or_else(|| eyre!("{ws:?} is not under a choros trunk"))?;
    let git = Git::new(&ws.join(repo));
    let Some(queue) = queue else {
        return update_repo(ws, repo, |rl| rl.queue = Some(workspace::NO_QUEUE.to_string()));
    };
    let q = Queue::open(&trunk, queue)?;
    if !q.config.scope.admits(&space::rel(&trunk, ws)?) {
        return Err(eyre!("queue {queue} only takes landings from {:?}", q.config.scope));
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
fn land_instant(env: &Env, trunk: &Path, ws: &Path, repo: &str) -> Result<Landed> {
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
            PrState::Open => merge_instant_pr(env, &git, ws, repo, &pr)?,
            PrState::Merged => finish_instant_pr(ws, repo, &pr)?,
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
            let title = git.subject(&head)?;
            let pr = env.forge.create_pr(
                git.dir(),
                &PrRequest {
                    head: &branch,
                    base: &main,
                    title: &title,
                    body: "",
                    draft: false,
                },
            )?;
            let pr = InstantPr {
                number: pr.number,
                head: head.clone(),
            };
            // `pending` stays: if the PR closes unmerged, the work is unlanded
            // again from the main it was rebased onto.
            update_repo(ws, repo, |rl| rl.instant_pr = Some(pr.clone()))?;
            return merge_instant_pr(env, &git, ws, repo, &pr).map(|()| landed);
        }
    }
    update_repo(ws, repo, |rl| {
        rl.landed_through = Some(head.clone());
        rl.pending = None;
    })?;
    Ok(landed)
}

fn merge_instant_pr(env: &Env, git: &Git, ws: &Path, repo: &str, pr: &InstantPr) -> Result<()> {
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
    finish_instant_pr(ws, repo, pr)
}

fn finish_instant_pr(ws: &Path, repo: &str, pr: &InstantPr) -> Result<()> {
    update_repo(ws, repo, |rl| {
        rl.landed_through = Some(pr.head.clone());
        rl.instant_pr = None;
        rl.pending = None;
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
    let repo_dir = ws.join(repo);
    let scope = if repo_of(ws, &req.cwd, &core::read_meta(ws)?).is_some() {
        &repo_dir
    } else {
        ws
    };
    let sources = pick_sessions(env, ws, scope, req);
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
