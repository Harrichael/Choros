//! How the daemon settles a conflict without anyone to ask: it hands it to
//! the resync agent, within limits, and trusts the result only once the
//! repo's check passes on it. Anything short of that parks, as it would
//! with no handler, for `choros queue resolve`.
//!
//! The limits, from the queue's `[agent]` settings: `auto_resolve` turns it
//! on; one try per conflict (the same operation on the same two commits);
//! `daily_runs` agent runs a day; `timeout_minutes` a run. The agent and the
//! check run under the queue's lock, so lands into the queue wait meanwhile.

use std::path::PathBuf;

use color_eyre::eyre::Result;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::engine::{Conflict, ConflictHandler, Handled, OnConflict};
use crate::git::Git;
use crate::queue::Queue;
use crate::repos;

/// Owns what it needs, as handlers must: the queue is opened again when a
/// conflict comes.
pub struct AutoResolve {
    pub trunk: PathBuf,
    pub queue: String,
    /// What happened, a line each, for the daemon's log.
    pub notes: Vec<String>,
}

/// What the daemon has already spent, in `auto-resolve.json` beside the
/// queue's state.
#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    day: String,
    runs: u32,
    /// Conflicts already handed over, newest last.
    tried: Vec<String>,
}

/// Enough to stop a conflict that keeps coming back from being tried
/// forever, without the file growing for good.
const TRIED_KEPT: usize = 200;

impl AutoResolve {
    pub fn new(trunk: PathBuf, queue: String) -> Self {
        AutoResolve {
            trunk,
            queue,
            notes: Vec::new(),
        }
    }

    fn try_resolve(&mut self, q: &Queue, c: &Conflict) -> Result<Handled> {
        let git = Git::owned(c.dir);
        let what = format!("{} ({})", c.operation, c.files.join(", "));
        let Some(check) = repos::read(&q.trunk, c.repo)?.check else {
            self.notes.push(format!(
                "left {what} parked: {} has no check to vouch for a fix",
                c.repo
            ));
            return Ok(Handled::Declined);
        };
        let other = ["MERGE_HEAD", "CHERRY_PICK_HEAD"]
            .iter()
            .find_map(|r| git.rev_parse(r).ok().flatten())
            .unwrap_or_default();
        let key = format!("{}|{}|{other}", c.operation, git.head()?);

        let path = q.dir().join("auto-resolve.json");
        let mut ledger: Ledger = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let today = OffsetDateTime::now_utc().date().to_string();
        if ledger.day != today {
            ledger.day = today;
            ledger.runs = 0;
        }
        if ledger.tried.contains(&key) {
            return Ok(Handled::Declined);
        }
        if ledger.runs >= q.config.agent.daily_runs {
            self.notes.push(format!(
                "left {what} parked: the agent already ran {} times today",
                ledger.runs
            ));
            return Ok(Handled::Declined);
        }
        // Spent before the run, so a crash midway can't buy another try.
        ledger.runs += 1;
        ledger.tried.push(key);
        let drop = ledger.tried.len().saturating_sub(TRIED_KEPT);
        ledger.tried.drain(..drop);
        std::fs::write(&path, serde_json::to_string_pretty(&ledger)?)?;

        let log = q.dir().join(format!("auto-resolve-{}.log", c.repo));
        if let Err(e) = c.agent.resolve(&c.brief()) {
            std::fs::write(&log, format!("{what}\n\n{e:#}\n"))?;
            self.notes.push(format!(
                "left {what} parked: the agent didn't settle it; see {}",
                log.display()
            ));
            return Ok(Handled::Declined);
        }
        if let Some(problem) = git.resolution_problems()? {
            std::fs::write(&log, format!("{what}\n\n{problem}\n"))?;
            self.notes.push(format!(
                "left {what} parked: the agent's fix is incomplete; see {}",
                log.display()
            ));
            return Ok(Handled::Declined);
        }
        // The check needs a commit to stand on: the fix as it would be
        // concluded, made without touching the operation in progress.
        let tree = git.run(&["write-tree"])?;
        let fix = git.run(&[
            "commit-tree",
            &tree,
            "-p",
            "HEAD",
            "-m",
            "choros: the agent's fix, for the check",
        ])?;
        if let Err(output) = q.check(c.repo, &fix, &check)? {
            let diff = git.run(&["diff", "--cached"]).unwrap_or_default();
            std::fs::write(
                &log,
                format!("{what}\n\nThe agent's fix:\n{diff}\n\n`{check}` failed on it:\n{output}"),
            )?;
            self.notes.push(format!(
                "left {what} parked: `{check}` failed on the agent's fix; see {}",
                log.display()
            ));
            return Ok(Handled::Declined);
        }
        self.notes
            .push(format!("the agent settled {what}; `{check}` passed"));
        Ok(Handled::Resolved { used_session: true })
    }
}

impl ConflictHandler for AutoResolve {
    fn resolve(&mut self, c: &Conflict) -> Result<Handled> {
        let q = Queue::open(&self.trunk, &self.queue)?;
        if !q.config.agent.auto_resolve {
            return Ok(Handled::Declined);
        }
        self.try_resolve(&q, c)
    }

    /// The daemon never lands, so it is never asked.
    fn fork_conflict(&mut self, _fork: u32, _with: &[u32]) -> Result<Option<OnConflict>> {
        Ok(None)
    }

    /// A parked conflict already had its try; it waits for what's under it
    /// to change, or for `choros queue resolve`.
    fn retries_parked(&self) -> bool {
        false
    }
}
