//! A queue on disk: where its files live, its lock, and its sync clone.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use choros_core::choros::{ARCHIVE_REL, CONFIG_DIR, LANDING_DIR, QUEUES_REL};
use choros_core::{git as core_git, registry};
use color_eyre::eyre::{Result, eyre};

use crate::git::Git;
use crate::state::{self, QueueConfig, QueueState, Scope};

pub struct Queue {
    pub trunk: PathBuf,
    pub name: String,
    pub config: QueueConfig,
}

/// Held for the duration of one short step; dropping it unlocks.
pub struct QueueLock(#[allow(dead_code)] File);

const RESERVED: &[&str] = &[LANDING_DIR, "_sync", "merge-head"];

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
            return Err(eyre!("no queue named {name:?} under {trunk:?}"));
        }
        Ok(Queue {
            trunk: trunk.to_path_buf(),
            name: name.to_string(),
            config: state::read_config(&dir.join("queue.toml"))?,
        })
    }

    /// Create a queue for `repo` (a registry name) and its sync clone.
    pub fn create(trunk: &Path, name: &str, repo: &str, scope: Scope) -> Result<Queue> {
        if name.is_empty()
            || RESERVED.contains(&name)
            || name.contains(['/', '\\', ' ', '~', '^', ':', '?', '*', '['])
            || name.starts_with(['.', '-'])
        {
            return Err(eyre!("{name:?} cannot be a queue name"));
        }
        let dir = queues_dir(trunk).join(name);
        if dir.exists() {
            return Err(eyre!("queue {name:?} already exists"));
        }
        // `<name>/landing-N` on origin can't coexist with a workspace's
        // branch `<name>` or a branch-space's `<name>/…`.
        if trunk.join(name).exists() {
            return Err(eyre!("{name:?} is already a workspace or branch-space here"));
        }
        let reg = registry::ensure_repo_exists(trunk, repo)?;
        let url = core_git::remote_get_url(&reg, "origin")?;
        // A branch named exactly like the queue would make `<queue>/landing-N`
        // impossible to create on origin (a ref cannot also be a directory).
        let clash = Git::new(&reg).run(&["ls-remote", "origin", &format!("refs/heads/{name}")])?;
        if !clash.is_empty() {
            return Err(eyre!("origin already has a branch named {name:?}; pick another queue name"));
        }

        let sync = sync_dir(trunk, name, repo);
        if !sync.exists() {
            std::fs::create_dir_all(sync.parent().unwrap())?;
            core_git::clone_with_reference(&reg, &url, &sync)?;
        }
        let git = Git::owned(&sync);
        for (k, v) in [("rerere.enabled", "true"), ("rerere.autoUpdate", "true")] {
            git.run(&["config", k, v])?;
        }
        ensure_notes(&git)?;
        let main = git
            .run(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])?
            .trim_start_matches("origin/")
            .to_string();

        std::fs::create_dir_all(&dir)?;
        let config = QueueConfig {
            repo: repo.to_string(),
            main,
            scope,
            push: Default::default(),
            pr: Default::default(),
            agent: Default::default(),
        };
        state::write_config(&dir.join("queue.toml"), &config)?;
        let queue = Queue {
            trunk: trunk.to_path_buf(),
            name: name.to_string(),
            config,
        };
        queue.write_state(&state::read_state(&queue.state_path())?)?;
        Ok(queue)
    }

    pub fn dir(&self) -> PathBuf {
        queues_dir(&self.trunk).join(&self.name)
    }

    fn state_path(&self) -> PathBuf {
        self.dir().join("state.json")
    }

    pub fn read_state(&self) -> Result<QueueState> {
        state::read_state(&self.state_path())
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

    pub fn sync_dir(&self) -> PathBuf {
        sync_dir(&self.trunk, &self.name, &self.config.repo)
    }

    pub fn git(&self) -> Git {
        Git::owned(&self.sync_dir())
    }

    pub fn origin_main(&self) -> String {
        format!("refs/remotes/origin/{}", self.config.main)
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

    /// Put the sync clone back in a known state: whatever a crash left in
    /// progress is abandoned, because every pass recomputes from state.json.
    pub fn recover(&self) -> Result<()> {
        let git = self.git();
        git.abort_in_progress()?;
        git.run(&["reset", "--quiet", "--hard"])?;
        // Excluded files (the notes) survive: clean without -x keeps them.
        git.run(&["clean", "--quiet", "-fd"])?;
        ensure_notes(&git)
    }

    pub fn fetch(&self) -> Result<()> {
        self.git().fetch("origin", &[])
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
/// its sync clone with it. Returns whether it went.
pub fn retire_if_orphaned(trunk: &Path, name: &str) -> Result<bool> {
    let q = Queue::open(trunk, name)?;
    let owner = match &q.config.scope {
        Scope::Trunk => return Ok(false),
        Scope::Workspace(p) | Scope::Branch(p) => trunk.join(p),
    };
    if owner.exists() {
        return Ok(false);
    }
    let Some(_lock) = q.try_lock()? else {
        return Ok(false);
    };
    if q.read_state()?.sets.values().any(|s| s.is_open()) {
        return Ok(false);
    }
    let dst = trunk.join(ARCHIVE_REL).join(LANDING_DIR).join(name);
    std::fs::create_dir_all(&dst)?;
    if dst.join("_queue").exists() || dst.join("_sync").exists() {
        return Err(eyre!("{dst:?} already holds an archived queue named {name}"));
    }
    let landing = trunk.join(LANDING_DIR).join(name);
    if landing.join("_sync").exists() {
        std::fs::rename(landing.join("_sync"), dst.join("_sync"))?;
    }
    std::fs::rename(q.dir(), dst.join("_queue"))?;
    let _ = std::fs::remove_dir(&landing);
    Ok(true)
}

pub fn sync_dir(trunk: &Path, queue: &str, repo: &str) -> PathBuf {
    trunk.join(LANDING_DIR).join(queue).join("_sync").join(repo)
}
