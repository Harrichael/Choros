//! A queue on disk: where its files live, its lock, and its sync clone.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use choros_core::choros::{ARCHIVE_REL, CONFIG_DIR, LANDING_DIR};
use choros_core::{git as core_git, registry};
use color_eyre::eyre::{Result, eyre};

use crate::git::Git;
use crate::state::{self, QueueConfig, QueueState, Scope};

pub struct Queue {
    pub root: PathBuf,
    pub name: String,
    pub config: QueueConfig,
}

/// Held for the duration of one short step; dropping it unlocks.
pub struct QueueLock(#[allow(dead_code)] File);

const RESERVED: &[&str] = &[LANDING_DIR, "_sync", "merge-head"];

pub fn queues_dir(root: &Path) -> PathBuf {
    root.join(CONFIG_DIR).join("queues")
}

pub fn list(root: &Path) -> Result<Vec<String>> {
    let dir = queues_dir(root);
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
    pub fn open(root: &Path, name: &str) -> Result<Queue> {
        let dir = queues_dir(root).join(name);
        if !dir.join("queue.toml").exists() {
            return Err(eyre!("no queue named {name:?} under {root:?}"));
        }
        Ok(Queue {
            root: root.to_path_buf(),
            name: name.to_string(),
            config: state::read_config(&dir.join("queue.toml"))?,
        })
    }

    /// Create a queue for `repo` (a registry name) and its sync clone.
    pub fn create(root: &Path, name: &str, repo: &str, scope: Scope) -> Result<Queue> {
        if name.is_empty()
            || RESERVED.contains(&name)
            || name.contains(['/', '\\', ' ', '~', '^', ':', '?', '*', '['])
            || name.starts_with(['.', '-'])
        {
            return Err(eyre!("{name:?} cannot be a queue name"));
        }
        let dir = queues_dir(root).join(name);
        if dir.exists() {
            return Err(eyre!("queue {name:?} already exists"));
        }
        let reg = registry::ensure_repo_exists(root, repo)?;
        let url = core_git::remote_get_url(&reg, "origin")?;
        // A branch named exactly like the queue would make `<queue>/landing-N`
        // impossible to create on origin (a ref cannot also be a directory).
        let clash = Git::new(&reg).run(&["ls-remote", "--heads", "origin", name])?;
        if !clash.is_empty() {
            return Err(eyre!("origin already has a branch named {name:?}; pick another queue name"));
        }

        let sync = sync_dir(root, name, repo);
        if !sync.exists() {
            std::fs::create_dir_all(sync.parent().unwrap())?;
            core_git::clone_with_reference(&reg, &url, &sync)?;
        }
        let git = Git::owned(&sync);
        for (k, v) in [("rerere.enabled", "true"), ("rerere.autoUpdate", "true")] {
            git.run(&["config", k, v])?;
        }
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
            root: root.to_path_buf(),
            name: name.to_string(),
            config,
        };
        queue.write_state(&state::read_state(&queue.state_path())?)?;
        Ok(queue)
    }

    pub fn dir(&self) -> PathBuf {
        queues_dir(&self.root).join(&self.name)
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
        sync_dir(&self.root, &self.name, &self.config.repo)
    }

    pub fn git(&self) -> Git {
        Git::owned(&self.sync_dir())
    }

    pub fn origin_main(&self) -> String {
        format!("refs/remotes/origin/{}", self.config.main)
    }

    pub fn landing_dir(&self, n: u32) -> PathBuf {
        self.root.join(LANDING_DIR).join(&self.name).join(format!("landing-{n}"))
    }

    pub fn archived_landing_dir(&self, n: u32) -> PathBuf {
        self.root
            .join(ARCHIVE_REL)
            .join(LANDING_DIR)
            .join(&self.name)
            .join(format!("landing-{n}"))
    }

    /// `gh` runs with the root's own account when it has one.
    pub fn gh_config_dir(&self) -> Option<PathBuf> {
        let dir = self.root.join(CONFIG_DIR).join("gh");
        dir.exists().then_some(dir)
    }

    /// Put the sync clone back in a known state: whatever a crash left in
    /// progress is abandoned, because every pass recomputes from state.json.
    pub fn recover(&self) -> Result<()> {
        let git = self.git();
        git.abort_in_progress()?;
        git.run(&["reset", "--quiet", "--hard"])?;
        git.run(&["clean", "--quiet", "-fd"])?;
        Ok(())
    }

    pub fn fetch(&self) -> Result<()> {
        self.git().fetch("origin", &[])
    }
}

pub fn sync_dir(root: &Path, queue: &str, repo: &str) -> PathBuf {
    root.join(LANDING_DIR).join(queue).join("_sync").join(repo)
}
