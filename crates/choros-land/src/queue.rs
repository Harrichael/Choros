//! A queue on disk: where its files live, its lock, its sync clone, and the
//! worktree its checks run in.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use choros_core::choros::{ARCHIVE_REL, CONFIG_DIR, LANDING_DIR, QUEUES_REL};
use choros_core::{agent, git as core_git, registry};
use choros_core::process::Proc;
use color_eyre::eyre::{Result, eyre};
use serde::{Deserialize, Serialize};

use crate::git::Git;
use crate::state::{self, LandingSet, QueueConfig, QueueState, RepoState, Scope, SetStatus};
use crate::store::{self, Store};

pub struct Queue {
    pub trunk: PathBuf,
    pub name: String,
    pub config: QueueConfig,
    /// When this command last fetched each repo; see `prefetch`.
    fetched: RefCell<BTreeMap<String, Instant>>,
}

/// Held for the duration of one short step; dropping it unlocks.
pub struct QueueLock(File);

impl QueueLock {
    /// Write who holds it into the lock file, for whoever is kept waiting.
    fn held(file: File) -> QueueLock {
        if let Some(me) = Holder::me()
            && let Ok(body) = serde_json::to_vec(&me)
        {
            use std::os::unix::fs::FileExt;
            let _ = file.set_len(0).and_then(|()| file.write_all_at(&body, 0));
        }
        QueueLock(file)
    }
}

impl Drop for QueueLock {
    fn drop(&mut self) {
        let _ = self.0.set_len(0);
    }
}

/// Who holds a queue's lock, and since when. A holder that died leaves its
/// record behind, but the lock goes with it, so a record is believed only
/// while its process lives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Holder {
    pub process: Proc,
    pub command: String,
    pub since: u64,
}

impl Holder {
    fn me() -> Option<Holder> {
        let mut args = std::env::args();
        let program = args.next().unwrap_or_default();
        let program = program.rsplit('/').next().unwrap_or_default().to_string();
        let command: String = std::iter::once(program).chain(args).collect::<Vec<_>>().join(" ");
        let command = match command.char_indices().nth(100) {
            Some((i, _)) => format!("{}…", &command[..i]),
            None => command,
        };
        Some(Holder { process: Proc::of(std::process::id())?, command, since: state::now_secs() })
    }
}

impl std::fmt::Display for Holder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let held = state::now_secs().saturating_sub(self.since);
        write!(f, "`{}` (pid {}) for {}", self.command, self.process.pid, how_long(held))
    }
}

/// `2h 14m`, `5m`, `40s`.
pub fn how_long(secs: u64) -> String {
    match (secs / 3600, secs / 60 % 60) {
        (0, 0) => format!("{secs}s"),
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

const RESERVED: &[&str] = &[LANDING_DIR, "_sync", "_check", "_resolve", "merge-head", crate::workspace::NO_QUEUE];

pub fn queues_dir(trunk: &Path) -> PathBuf {
    trunk.join(QUEUES_REL)
}

pub fn list(trunk: &Path) -> Result<Vec<String>> {
    let dir = queues_dir(trunk);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names: Vec<String> = std::fs::read_dir(dir)?
        .flatten()
        .filter(|e| e.path().join("queue.toml").exists())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .collect();
    names.sort();
    Ok(names)
}

impl Queue {
    pub fn open(trunk: &Path, name: &str) -> Result<Queue> {
        let dir = queues_dir(trunk).join(name);
        if !dir.join("queue.toml").exists() {
            return Err(eyre!("no queue named {name} in {}; `choros queue ls` lists them", trunk.display()));
        }
        Ok(Queue {
            trunk: trunk.to_path_buf(),
            name: name.to_string(),
            config: state::read_config(&dir.join("queue.toml"))?,
            fetched: RefCell::default(),
        })
    }

    /// Refuse a name `create` would: one taken here, or not a branch name.
    /// Each repo's origin is checked when the repo first lands.
    pub fn check_name(trunk: &Path, name: &str) -> Result<()> {
        if name.is_empty()
            || RESERVED.contains(&name)
            || name.contains(['/', '\\', ' ', '~', '^', ':', '?', '*', '['])
            || name.starts_with(['.', '-'])
        {
            return Err(eyre!("`{name}` cannot be a queue name"));
        }
        if queues_dir(trunk).join(name).exists() {
            return Err(eyre!("queue {name} already exists"));
        }
        // `<name>/landing-N` on origin can't coexist with a workspace's
        // branch `<name>` or a branch-space's `<name>/…`.
        if trunk.join(name).exists() {
            return Err(eyre!("{name} is already a workspace or branch-space here"));
        }
        Ok(())
    }

    /// Refuse a second queue for a workspace or branch-space: its workspaces
    /// would not know which to land into.
    pub fn check_scope(trunk: &Path, scope: &Scope) -> Result<()> {
        if *scope == Scope::Trunk {
            return Ok(());
        }
        for other in list(trunk)? {
            if Queue::open(trunk, &other)?.config.scope == *scope {
                return Err(eyre!("queue {other} already takes the landings from there"));
            }
        }
        Ok(())
    }

    pub fn create(trunk: &Path, name: &str, config: QueueConfig) -> Result<Queue> {
        Self::check_name(trunk, name)?;
        Self::check_scope(trunk, &config.scope)?;
        let dir = queues_dir(trunk).join(name);
        std::fs::create_dir_all(&dir)?;
        state::write_config(&dir.join("queue.toml"), &config)?;
        let queue = Queue {
            trunk: trunk.to_path_buf(),
            name: name.to_string(),
            config,
            fetched: RefCell::default(),
        };
        store::create(&dir)?;
        queue.write_state(&queue.read_state()?)?;
        Ok(queue)
    }

    /// Ready the queue to take `repo`: its sync clone, and the branch its
    /// work lands on. The caller holds the lock and saves `st`.
    pub fn ensure_repo(&self, st: &mut QueueState, repo: &str) -> Result<()> {
        if st.repos.contains_key(repo) {
            return Ok(());
        }
        let reg = registry::ensure_repo_exists(&self.trunk, repo)?;
        // A branch named exactly like the queue would make `<queue>/landing-N`
        // impossible to create on origin (a ref cannot also be a directory).
        let clash = Git::new(&reg).remote(&["ls-remote", "origin", &format!("refs/heads/{}", self.name)])?;
        if !clash.is_empty() {
            return Err(eyre!(
                "{repo}'s origin has a branch named {0}, so it can't land into queue {0}",
                self.name
            ));
        }
        let main = default_branch(&self.trunk, repo)?;
        let sync = self.sync_dir(repo);
        if !sync.exists() {
            std::fs::create_dir_all(sync.parent().unwrap())?;
            core_git::clone_with_reference(&reg, &core_git::remote_get_url(&reg, "origin")?, &sync)?;
        }
        let git = Git::owned(&sync);
        no_rerere(&git)?;
        ensure_notes(&git)?;
        st.repos.insert(
            repo.to_string(),
            RepoState {
                main,
                ..RepoState::default()
            },
        );
        Ok(())
    }

    pub fn dir(&self) -> PathBuf {
        queues_dir(&self.trunk).join(&self.name)
    }

    pub fn read_state(&self) -> Result<QueueState> {
        store::read(&self.dir(), &self.config)
    }

    pub fn history(&self, n: u32) -> Result<store::History> {
        Store::open(&self.dir())?.history(n)
    }

    /// The files settled in any of the commits `made`.
    pub fn settled_in(&self, repo: &str, made: &[String]) -> Result<Vec<store::Settled>> {
        Store::open(&self.dir())?.settled_in(repo, made)
    }

    pub fn write_state(&self, state: &QueueState) -> Result<()> {
        let before = self.read_state().ok();
        // A log begun after this write would tell of it twice.
        crate::events::begin(&self.trunk);
        Store::open(&self.dir())?.write_state(state)?;
        crate::events::sets_changed(self, before.as_ref(), state);
        Ok(())
    }

    /// The queue's lock, waiting for it if need be, and saying who for.
    pub fn lock(&self) -> Result<QueueLock> {
        if let Some(lock) = self.try_lock()? {
            return Ok(lock);
        }
        let by = match self.holder() {
            Some(h) => format!("held by {h}"),
            None => "someone is landing or resolving there".to_string(),
        };
        eprintln!("waiting for queue {} ({by})…", self.name);
        let file = self.lock_file()?;
        file.lock()?;
        self.adopt_old_layout()?;
        store::adopt(&self.dir(), &self.config)?;
        Ok(QueueLock::held(file))
    }

    /// Who holds the queue's lock now, if anyone living does.
    pub fn holder(&self) -> Option<Holder> {
        let body = std::fs::read(self.dir().join("lock")).ok()?;
        let holder: Holder = serde_json::from_slice(&body).ok()?;
        holder.process.alive().then_some(holder)
    }

    pub fn try_lock(&self) -> Result<Option<QueueLock>> {
        let file = self.lock_file()?;
        match file.try_lock() {
            Ok(()) => {
                self.adopt_old_layout()?;
                store::adopt(&self.dir(), &self.config)?;
                Ok(Some(QueueLock::held(file)))
            }
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e.into()),
        }
    }

    /// Landing folders and sync clones once lived in `<trunk>/landing/<queue>/`.
    /// Move them into the queue's folder, Claude sessions and all, so
    /// `claude --resume` still finds the landings' forked sessions.
    fn adopt_old_layout(&self) -> Result<()> {
        let old = self.trunk.join(LANDING_DIR).join(&self.name);
        if !old.exists() {
            return Ok(());
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let moved = |from: &Path, to: &Path| -> Result<()> {
            // Sessions first: a retry finds the folder still to move, and
            // its sessions with it.
            if let Some(home) = &home {
                agent::move_claude_sessions(home, from, to)?;
            }
            std::fs::create_dir_all(to.parent().unwrap())?;
            std::fs::rename(from, to)?;
            Ok(())
        };
        if old.join("_sync").exists() {
            // A check running in the old worktree would lose its clone mid-run.
            let mut checks = Vec::new();
            for repo in std::fs::read_dir(old.join("_sync"))?.flatten() {
                let lock = self.check_lock(&repo.file_name().to_string_lossy())?;
                lock.lock()?;
                checks.push(lock);
            }
            if old.join("_check").exists() {
                std::fs::remove_dir_all(old.join("_check"))?;
            }
            moved(&old.join("_sync"), &self.dir().join("_sync"))?;
        }
        for entry in std::fs::read_dir(&old)?.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("landing-") {
                moved(&entry.path(), &self.dir().join(LANDING_DIR).join(&name))?;
            }
        }
        // Whatever else was left there (a .DS_Store, say) must not hold up the queue.
        let _ = std::fs::remove_dir(&old);
        let _ = std::fs::remove_dir(self.trunk.join(LANDING_DIR));
        Ok(())
    }

    fn check_lock(&self, repo: &str) -> Result<File> {
        Ok(OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir().join(format!("check-{repo}.lock")))?)
    }

    fn lock_file(&self) -> Result<File> {
        Ok(OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir().join("lock"))?)
    }

    pub fn sync_dir(&self, repo: &str) -> PathBuf {
        sync_dir(&self.trunk, &self.name, repo)
    }

    pub fn git(&self, repo: &str) -> Git {
        Git::owned(&self.sync_dir(repo))
    }

    /// Run `check` on `tip`: `Err` with its output when it fails. It runs in
    /// a worktree of the sync clone kept at one path, so build output carries
    /// over between runs, and under a lock of its own rather than the
    /// queue's: a slow check holds up only other checks of the repo.
    pub fn check(&self, repo: &str, tip: &str, check: &str) -> Result<std::result::Result<(), String>> {
        Ok(self.check_unless(repo, tip, check, || Ok(false))?.expect("never done"))
    }

    /// As `check`, unless `done` says, once the check's lock is held, that
    /// someone already has: `None` then.
    pub fn check_unless(
        &self,
        repo: &str,
        tip: &str,
        check: &str,
        done: impl FnOnce() -> Result<bool>,
    ) -> Result<Option<std::result::Result<(), String>>> {
        let lock = self.check_lock(repo)?;
        lock.lock()?;
        if done()? {
            return Ok(None);
        }
        let dir = self.worktree(repo, "_check", tip)?;
        let wt = Git::owned(&dir);
        let out = std::process::Command::new("sh")
            .args(["-c", check])
            .current_dir(&dir)
            .output()
            .map_err(|e| eyre!("running `{check}`: {e}"))?;
        wt.reset_hard(tip)?;
        if out.status.success() {
            return Ok(Some(Ok(())));
        }
        Ok(Some(Err(format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))))
    }

    /// A worktree of `repo`'s sync clone under `<queue>/<name>/`, kept at
    /// one path, checked out (detached) at `at`, keeping what it ignores
    /// (build output). Work in it leaves the sync clone, and so the
    /// queue's lock, alone.
    pub fn worktree(&self, repo: &str, name: &str, at: &str) -> Result<PathBuf> {
        let dir = self.dir().join(name).join(repo);
        let wt = Git::owned(&dir);
        // A sync clone made again, or a trunk moved, leaves the worktree
        // pointing nowhere; it holds only build output, so start over.
        let linked = || -> Result<bool> {
            let top = wt.run(&["rev-parse", "--show-toplevel"]).ok().map(PathBuf::from);
            Ok(top.is_some() && top == Some(dir.canonicalize()?))
        };
        if dir.exists() && !linked()? {
            std::fs::remove_dir_all(&dir)?;
        }
        if !dir.exists() {
            let sync = self.git(repo);
            sync.run(&["worktree", "prune"])?;
            sync.run(&["worktree", "add", "--quiet", "--detach", dir.to_str().unwrap(), at])?;
        }
        wt.abort_in_progress()?;
        wt.run(&["checkout", "--quiet", "--force", "--detach", at])?;
        Ok(dir)
    }

    /// The title `set`'s PR opens with: the one given at land, else its
    /// first commit's subject.
    pub fn pr_title(&self, set: &LandingSet) -> Result<String> {
        if let Some(t) = &set.title {
            return Ok(t.clone());
        }
        let git = self.git(set.repo());
        let first = git
            .commits(&set.base_sha, &set.tip)?
            .into_iter()
            .next()
            .ok_or_else(|| eyre!("{} has no commits of its own", set.branch(&self.name)))?;
        git.subject(&first)
    }

    pub fn landing_dir(&self, n: u32) -> PathBuf {
        self.dir().join(LANDING_DIR).join(format!("landing-{n}"))
    }

    pub fn archived_landing_dir(&self, n: u32) -> PathBuf {
        self.trunk
            .join(ARCHIVE_REL)
            .join(LANDING_DIR)
            .join(&self.name)
            .join(format!("landing-{n}"))
    }

    /// `gh` runs with the trunk's own account when it has one.
    pub fn gh_config_dir(&self) -> Option<PathBuf> {
        let dir = self.trunk.join(CONFIG_DIR).join("gh");
        dir.exists().then_some(dir)
    }

    /// Put a sync clone back in a known state: whatever a crash left in
    /// progress is abandoned, because every pass recomputes from the queue's state.
    pub fn recover(&self, repo: &str) -> Result<()> {
        let git = self.git(repo);
        git.abort_in_progress()?;
        git.run(&["reset", "--quiet", "--hard"])?;
        // Excluded files (the notes) survive: clean without -x keeps them.
        git.run(&["clean", "--quiet", "-fd"])?;
        no_rerere(&git)?;
        ensure_notes(&git)
    }

    pub fn fetch(&self, repo: &str) -> Result<()> {
        self.git(repo).fetch("origin", &[])
    }

    /// Fetch origin without the queue's lock, so the network wait holds no
    /// one up. What it sees goes to `refs/choros/fetched/`, not to
    /// `origin/*`: the queue's steps read `origin/*`, and must not see it
    /// move under them. `take_fetched`, under the lock, moves it over.
    ///
    /// Each command fetches at its start; its later steps reuse that fetch
    /// while it is recent. The queue's own changes reach `origin/*` through
    /// its own pushes, so a fetch only learns what others did, and a pass
    /// that hears of a merge fetches for itself. A fetch another process
    /// began while this one waited for it is as fresh as one of our own.
    pub fn prefetch(&self, repo: &str) -> Result<()> {
        let git = self.git(repo);
        if !git.dir().exists() {
            return Ok(());
        }
        if self.fetched.borrow().get(repo).is_some_and(|at| at.elapsed() < FETCH_FRESH) {
            return Ok(());
        }
        let asked = millis_now();
        let lock = self.fetch_lock(repo)?;
        lock.lock()?;
        let record = self.dir().join(format!("fetched-{repo}.json"));
        if read_fetched(&record).is_some_and(|f| f.began >= asked) {
            self.fetched.borrow_mut().insert(repo.to_string(), Instant::now());
            return Ok(());
        }
        let began = millis_now();
        let before = git.refs_under(ORIGIN)?;
        // Atomic: a fetch cut off halfway must not leave refs from two
        // fetches. No FETCH_HEAD: a locked step may be fetching here too.
        git.remote(&[
            "fetch",
            "--quiet",
            "--prune",
            "--atomic",
            "--no-write-fetch-head",
            "origin",
            &format!("+refs/heads/*:{FETCHED}*"),
        ])?;
        let fetched = Fetched {
            began,
            taken: false,
            before,
        };
        write_fetched(&record, &fetched)?;
        self.fetched.borrow_mut().insert(repo.to_string(), Instant::now());
        Ok(())
    }

    /// Move what the last `prefetch` saw into `origin/*`, ref by ref, unless
    /// the queue itself moved that ref after the fetch began. Its own push
    /// is newer than what the fetch saw, and taking the fetch's would undo
    /// it. The caller holds the queue's lock.
    ///
    /// A fetch still running is left for a later step to take, unless the
    /// caller `must_see_origin`: a push journalled but never confirmed is
    /// settled by what origin holds, and a stale answer would put back a
    /// tip origin already moved past. Then it fetches here, under the lock.
    pub fn take_fetched(&self, repo: &str, must_see_origin: bool) -> Result<()> {
        let git = self.git(repo);
        let lock = self.fetch_lock(repo)?;
        // A process another thread is starting holds a copy of the lock for
        // a moment; a fetch holds it for seconds. Only the fetch is waited out.
        let mut tries = 0;
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) if tries < 25 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(TryLockError::WouldBlock) if must_see_origin => return self.fetch(repo),
                Err(TryLockError::WouldBlock) => return Ok(()),
                Err(TryLockError::Error(e)) => return Err(e.into()),
            }
        }
        let record = self.dir().join(format!("fetched-{repo}.json"));
        let Some(mut fetched) = read_fetched(&record).filter(|f| !f.taken) else {
            return Ok(());
        };
        let staged = git.refs_under(FETCHED)?;
        let now = git.refs_under(ORIGIN)?;
        let branches: BTreeSet<&String> = staged.keys().chain(fetched.before.keys()).collect();
        let mut lines = String::new();
        for branch in branches {
            let (was, is, seen) = (fetched.before.get(branch), now.get(branch), staged.get(branch));
            if was != is || is == seen {
                continue;
            }
            let name = format!("{ORIGIN}{branch}");
            match (seen, is) {
                (Some(seen), Some(is)) => lines.push_str(&format!("update {name} {seen} {is}\n")),
                (Some(seen), None) => lines.push_str(&format!("create {name} {seen}\n")),
                (None, Some(is)) => lines.push_str(&format!("delete {name} {is}\n")),
                (None, None) => {}
            }
        }
        // Refs git won't take as given must not stop every step that follows:
        // a plain fetch gets origin/* right instead.
        let taken = git.update_refs_stdin(&lines);
        fetched.taken = true;
        write_fetched(&record, &fetched)?;
        if taken.is_err() {
            self.fetch(repo)?;
        }
        Ok(())
    }

    fn fetch_lock(&self, repo: &str) -> Result<File> {
        Ok(OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir().join(format!("fetch-{repo}.lock")))?)
    }
}

const ORIGIN: &str = "refs/remotes/origin/";
const FETCHED: &str = "refs/choros/fetched/";

/// How long a command's fetch stays good enough for its later steps.
const FETCH_FRESH: Duration = Duration::from_secs(30);

/// What `origin/*` was as a `prefetch` began, for `take_fetched` to tell
/// the queue's own later pushes from what the fetch brought.
#[derive(serde::Serialize, serde::Deserialize)]
struct Fetched {
    /// Milliseconds since the epoch.
    began: u64,
    taken: bool,
    before: BTreeMap<String, String>,
}

fn read_fetched(record: &Path) -> Option<Fetched> {
    serde_json::from_str(&std::fs::read_to_string(record).ok()?).ok()
}

fn write_fetched(record: &Path, fetched: &Fetched) -> Result<()> {
    let tmp = record.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(fetched)?)?;
    std::fs::rename(&tmp, record)?;
    Ok(())
}

fn millis_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// git's rerere keys a remembered answer on the clashing lines alone, so it
/// replays one set's answer onto another set whose clash merely looks the same,
/// and drops that set's work unseen. The queue's own record of past answers
/// (`mend`) keys on whole files instead. Sync clones made when this was on
/// are turned off here, and what they remembered goes.
fn no_rerere(git: &Git) -> Result<()> {
    let enabled = git.run(&["config", "--get", "rerere.enabled"]).unwrap_or_default();
    if enabled.trim() != "false" {
        git.run(&["config", "rerere.enabled", "false"])?;
        let _ = git.run(&["config", "--unset", "rerere.autoUpdate"]);
    }
    let cache = PathBuf::from(git.run(&["rev-parse", "--absolute-git-dir"])?).join("rr-cache");
    if cache.exists() {
        std::fs::remove_dir_all(&cache)?;
    }
    Ok(())
}

/// The resync agent's notes on this queue, for whichever session comes next.
/// Excluded from git so they can never be staged into a resolution; skipped
/// when the repo tracks a CLAUDE.md of its own, which must not be shadowed.
fn ensure_notes(git: &Git) -> Result<()> {
    let notes = git.dir().join("CLAUDE.md");
    if notes.exists() || git.succeeds(&["ls-files", "--error-unmatch", "CLAUDE.md"])? {
        return Ok(());
    }
    let exclude = PathBuf::from(git.run(&["rev-parse", "--absolute-git-dir"])?).join("info/exclude");
    std::fs::create_dir_all(exclude.parent().unwrap())?;
    let mut body = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !body.lines().any(|l| l.trim() == "/CLAUDE.md") {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str("/CLAUDE.md\n");
        std::fs::write(&exclude, body)?;
    }
    std::fs::write(
        notes,
        "# Resync notes\n\n\
         Choros's resync agent works here to resolve conflicts in this landing queue. \
         This file is not part of the repo. Keep short notes for the next session: \
         recurring conflicts, how they were settled, anything surprising about the code.\n",
    )?;
    Ok(())
}

/// A queue scoped to a workspace or branch-space that has since been
/// archived, with no set left open, goes to the archive too: its config to
/// `archive/landing/<name>/_queue`, beside its archived landing folders, and
/// its sync clone with it. Returns whether it went. The caller holds the
/// queue's lock and passes the state it read under it.
/// Whether landing-`n` of another queue has merged, by its state file.
pub fn merged_in(trunk: &Path, queue: &str, n: u32) -> bool {
    Queue::open(trunk, queue)
        .and_then(|q| q.read_state())
        .ok()
        .and_then(|s| s.sets.get(&n).map(|set| set.status == SetStatus::Merged))
        .unwrap_or(false)
}

pub fn retire_if_orphaned(q: &Queue, st: &QueueState) -> Result<bool> {
    let (trunk, name) = (&q.trunk, &q.name);
    let owner = match &q.config.scope {
        Scope::Trunk => return Ok(false),
        Scope::Workspace(p) | Scope::Branch(p) => trunk.join(p),
    };
    if owner.exists() {
        return Ok(false);
    }
    // A closed PR still waits on someone to reopen or drop it.
    if st
        .sets
        .values()
        .any(|s| !matches!(s.status, SetStatus::Merged | SetStatus::Dropped))
    {
        return Ok(false);
    }
    let dst = trunk.join(ARCHIVE_REL).join(LANDING_DIR).join(name);
    std::fs::create_dir_all(&dst)?;
    if dst.join("_queue").exists() || dst.join("_sync").exists() {
        return Err(eyre!("{} already holds an archived queue named {name}", dst.display()));
    }
    // Only build output and the agent's scratch, in worktrees of the sync
    // clone that moving it would orphan.
    for scratch in ["_check", "_resolve"] {
        if q.dir().join(scratch).exists() {
            std::fs::remove_dir_all(q.dir().join(scratch))?;
        }
    }
    if q.dir().join("_sync").exists() {
        std::fs::rename(q.dir().join("_sync"), dst.join("_sync"))?;
    }
    std::fs::rename(q.dir(), dst.join("_queue"))?;
    Ok(true)
}

/// The branch `repo`'s origin points HEAD at: where its work lands.
pub fn default_branch(trunk: &Path, repo: &str) -> Result<String> {
    let reg = registry::ensure_repo_exists(trunk, repo)?;
    let out = Git::new(&reg).remote(&["ls-remote", "--symref", "origin", "HEAD"])?;
    out.lines()
        .find_map(|l| l.strip_prefix("ref: refs/heads/")?.split_whitespace().next())
        .map(str::to_string)
        .ok_or_else(|| eyre!("origin of {repo} names no default branch"))
}

/// Whether `repo`'s default branch has workflows that run `on: push`, as
/// pushing every queued branch would set them off on each re-sync.
pub fn push_workflows(trunk: &Path, repo: &str) -> bool {
    let Ok(reg) = registry::ensure_repo_exists(trunk, repo) else { return false };
    let git = Git::new(&reg);
    let Ok(files) = git.run(&["ls-tree", "--name-only", "origin/HEAD", ".github/workflows/"]) else {
        return false;
    };
    files.lines().any(|f| {
        git.run(&["show", &format!("origin/HEAD:{f}")])
            .is_ok_and(|body| body.lines().any(|l| l.trim_start().starts_with("push")))
    })
}

pub fn sync_dir(trunk: &Path, queue: &str, repo: &str) -> PathBuf {
    queues_dir(trunk).join(queue).join("_sync").join(repo)
}
