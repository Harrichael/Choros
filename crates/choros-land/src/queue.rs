//! A queue on disk: where its files live, its lock, and its sync clone.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use choros_core::choros::{ARCHIVE_REL, CONFIG_DIR, LANDING_DIR, QUEUES_REL};
use choros_core::{git as core_git, registry};
use color_eyre::eyre::{Result, eyre};

use crate::git::Git;
use crate::state::{self, QueueConfig, QueueState, RepoState, Scope, SetStatus};

pub struct Queue {
    pub trunk: PathBuf,
    pub name: String,
    pub config: QueueConfig,
}

/// Held for the duration of one short step; dropping it unlocks.
pub struct QueueLock(#[allow(dead_code)] File);

const RESERVED: &[&str] = &[LANDING_DIR, "_sync", "merge-head", crate::workspace::NO_QUEUE];

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
        };
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
        let clash = Git::new(&reg).run(&["ls-remote", "origin", &format!("refs/heads/{}", self.name)])?;
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
        for (k, v) in [("rerere.enabled", "true"), ("rerere.autoUpdate", "true")] {
            git.run(&["config", k, v])?;
        }
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

    fn state_path(&self) -> PathBuf {
        self.dir().join("state.json")
    }

    pub fn read_state(&self) -> Result<QueueState> {
        state::read_state(&self.state_path(), &self.config)
    }

    pub fn write_state(&self, state: &QueueState) -> Result<()> {
        state::write_state(&self.state_path(), state)
    }

    pub fn lock(&self) -> Result<QueueLock> {
        let file = self.lock_file()?;
        file.lock()?;
        Ok(QueueLock(file))
    }

    pub fn try_lock(&self) -> Result<Option<QueueLock>> {
        let file = self.lock_file()?;
        match file.try_lock() {
            Ok(()) => Ok(Some(QueueLock(file))),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e.into()),
        }
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

    pub fn landing_dir(&self, n: u32) -> PathBuf {
        self.trunk.join(LANDING_DIR).join(&self.name).join(format!("landing-{n}"))
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
    /// progress is abandoned, because every pass recomputes from state.json.
    pub fn recover(&self, repo: &str) -> Result<()> {
        let git = self.git(repo);
        git.abort_in_progress()?;
        git.run(&["reset", "--quiet", "--hard"])?;
        // Excluded files (the notes) survive: clean without -x keeps them.
        git.run(&["clean", "--quiet", "-fd"])?;
        ensure_notes(&git)
    }

    pub fn fetch(&self, repo: &str) -> Result<()> {
        self.git(repo).fetch("origin", &[])
    }
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
    let landing = trunk.join(LANDING_DIR).join(name);
    if landing.join("_sync").exists() {
        std::fs::rename(landing.join("_sync"), dst.join("_sync"))?;
    }
    std::fs::rename(q.dir(), dst.join("_queue"))?;
    let _ = std::fs::remove_dir(&landing);
    Ok(true)
}

/// The branch `repo`'s origin points HEAD at: where its work lands.
pub fn default_branch(trunk: &Path, repo: &str) -> Result<String> {
    let reg = registry::ensure_repo_exists(trunk, repo)?;
    let out = Git::new(&reg).run(&["ls-remote", "--symref", "origin", "HEAD"])?;
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
    trunk.join(LANDING_DIR).join(queue).join("_sync").join(repo)
}
