//! How the daemon settles a conflict without anyone to ask: it hands it to
//! the resync agent, within limits, and trusts the result only once the
//! repo's check passes on it. Anything short of that parks, as it would
//! with no handler, for `choros queue resolve`.
//!
//! The limits, from the queue's `[agent]` settings: `auto_resolve` turns it
//! on (the daemon attaches no handler otherwise); one try per conflict (the
//! same operation on the same content); `daily_runs` agent runs a day;
//! `timeout_minutes` a run. A conflict met past the day's runs is deferred,
//! not declined, so it gets its try once the day turns.
//!
//! The pass only decides, under the queue's lock, that the agent may have
//! its go; the go itself is a pending job (see `pending`), run with the
//! lock let go in a worktree of its own, where the conflict is met again.
//! Its fix is kept whole, with the check's verdict, under the conflict's
//! content, for the next pass to take as is. The one try is counted by that
//! content too, so a pass that makes its commits afresh can't buy another.

use std::path::PathBuf;

use color_eyre::eyre::Result;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::agent::{Agent, AgentSlot};
use crate::engine::{Conflict, ConflictHandler, Handled};
use crate::git::{Git, Step};
use crate::pending::{Job, Redo, Task};
use crate::queue::Queue;
use crate::repos;
use crate::state::QueueState;

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
    /// The day's runs ran out and the log said so; once is enough.
    #[serde(default)]
    spent_noted: bool,
}

/// Enough to stop a conflict that keeps coming back from being tried
/// forever, without the file growing for good.
const TRIED_KEPT: usize = 200;

/// Whether the agent may have its go at a conflict now.
enum Go {
    Yes,
    /// Not today: the day's runs are spent.
    Later,
    /// Never: it already had its go.
    No,
}

impl AutoResolve {
    pub fn new(trunk: PathBuf, queue: String) -> Self {
        AutoResolve { trunk, queue, notes: Vec::new() }
    }

    fn ledger(q: &Queue) -> (PathBuf, Ledger) {
        let path = q.dir().join("auto-resolve.json");
        let mut ledger: Ledger =
            std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let today = OffsetDateTime::now_utc().date().to_string();
        if ledger.day != today {
            ledger.day = today;
            ledger.runs = 0;
            ledger.spent_noted = false;
        }
        (path, ledger)
    }

    fn go(&mut self, q: &Queue, key: &str, what: &str) -> Result<Go> {
        let (path, mut ledger) = Self::ledger(q);
        if ledger.tried.iter().any(|t| t == key) {
            self.notes.push(format!("left {what} parked: the agent already had its try at it"));
            return Ok(Go::No);
        }
        if ledger.runs >= q.config.agent.resolver.daily_runs {
            if !ledger.spent_noted {
                self.notes.push(format!(
                    "left {what} parked until tomorrow (UTC): the agent already ran {} times today",
                    ledger.runs
                ));
                ledger.spent_noted = true;
                save(&path, &ledger)?;
            }
            return Ok(Go::Later);
        }
        Ok(Go::Yes)
    }

    /// Under the queue's lock: whether the agent may have its go at `c`,
    /// the go itself left for later.
    fn decide(&mut self, q: &Queue, c: &Conflict) -> Result<Handled> {
        let what = format!("{} ({})", c.operation, c.files.join(", "));
        if repos::read(&q.trunk, c.repo)?.check.is_none() {
            self.notes.push(format!("left {what} parked: {} has no check to vouch for a fix", c.repo));
            return Ok(Handled::Declined);
        }
        Ok(match self.go(q, &key(&c.operation, c.key), &what)? {
            Go::Yes => Handled::Later,
            Go::Later => Handled::Deferred,
            Go::No => Handled::Declined,
        })
    }

    /// With the queue's lock let go: the agent's go at `job`, its conflict
    /// met again in the `_resolve` worktree. Whether it went.
    pub fn run(&mut self, q: &Queue, st: &QueueState, job: &Job, agent: &dyn Agent) -> Result<bool> {
        let Task::Agent { conflict, step, head, other } = &job.task else {
            return Ok(false);
        };
        let Some(check) = repos::read(&q.trunk, &job.repo)?.check else {
            return Ok(false);
        };
        let key = key(&job.what, conflict);
        if !matches!(self.go(q, &key, &job.what)?, Go::Yes) {
            return Ok(false);
        }
        // Spent once the agent is about to run, and before it does, so a
        // crash midway can't buy another try, while trouble getting there
        // (git, a missing state) costs none.
        let spend = |q: &Queue| -> Result<()> {
            let (path, mut ledger) = Self::ledger(q);
            ledger.runs += 1;
            ledger.tried.push(key.clone());
            let drop = ledger.tried.len().saturating_sub(TRIED_KEPT);
            ledger.tried.drain(..drop);
            save(&path, &ledger)
        };

        let dir = q.worktree(&job.repo, "_resolve", head)?;
        let git = Git::owned(&dir);
        git.run(&["clean", "--quiet", "-fd"])?;
        let met = match step {
            Redo::Merge => git.merge(other, "choros: the conflict met again, for the agent")?,
            Redo::CherryPick => git.cherry_pick(other)?,
        };
        let Step::Conflict(files) = met else {
            // Counted as its try all the same, or each pass would ask again.
            spend(q)?;
            self.notes.push(format!("left {} parked: it met no conflict when tried again", job.what));
            return Ok(true);
        };
        let what = format!("{} ({})", job.what, files.join(", "));
        let Some(rs) = st.repos.get(&job.repo) else {
            git.abort_in_progress()?;
            return Ok(true);
        };
        // A session is kept per folder, and the sync clone's is not this
        // one's: each go starts afresh, told how the files were settled before.
        let slot = AgentSlot { id: crate::uuid::v4(), fresh: true };
        let brief = Conflict {
            queue: &q.name,
            repo: &job.repo,
            key: conflict,
            dir: &dir,
            operation: job.what.clone(),
            files: files.clone(),
            recorded: crate::engine::recorded_for(&q.git(&job.repo), rs, &files),
            sides: crate::context::sides(q, st, &job.repo, &files),
            slot: &slot,
            agent,
            settings: &q.config.agent.resolver,
        };
        let log = q.dir().join(format!("auto-resolve-{}.log", job.repo));
        let gave_up = |notes: &mut Vec<String>, why: &str, body: String| -> Result<bool> {
            std::fs::write(&log, body)?;
            notes.push(format!("left {what} parked: {why}; see {}", log.display()));
            git.abort_in_progress()?;
            Ok(true)
        };
        spend(q)?;
        if let Err(e) = agent.resolve(&brief.brief()) {
            return gave_up(&mut self.notes, "the agent didn't settle it", format!("{what}\n\n{e:#}\n"));
        }
        if let Some(problem) = git.resolution_problems()? {
            return gave_up(&mut self.notes, "the agent's fix is incomplete", format!("{what}\n\n{problem}\n"));
        }
        // The check needs a commit to stand on: the fix as it would be
        // concluded, made without touching the operation in progress.
        let tree = git.run(&["write-tree"])?;
        let fix = git.run(&["commit-tree", &tree, "-p", "HEAD", "-m", "choros: the agent's fix, for the check"])?;
        // Kept whole, by the conflict: the next pass to meet it takes this
        // very tree, whatever else the agent touched.
        let verdict = q.check(&job.repo, &fix, &check)?;
        crate::pending::keep(q, &job.repo, conflict, &crate::pending::by_agent(&check), &tree, &verdict)?;
        if let Err(output) = verdict {
            let diff = git.run(&["diff", "--cached"]).unwrap_or_default();
            let why = format!("`{check}` failed on the agent's fix");
            let body = format!("{what}\n\nThe agent's fix:\n{diff}\n\n`{check}` failed on it:\n{output}");
            return gave_up(&mut self.notes, &why, body);
        }
        git.abort_in_progress()?;
        self.notes.push(format!("the agent settled {what}; `{check}` passed"));
        Ok(true)
    }
}

/// A conflict, as the ledger knows it: the same operation on the same
/// content (see `Git::conflict_key`).
fn key(operation: &str, conflict: &str) -> String {
    format!("{operation}|{conflict}")
}

fn save(path: &std::path::Path, ledger: &Ledger) -> Result<()> {
    crate::state::write_atomic(path, serde_json::to_string_pretty(ledger)?.as_bytes())
}

impl ConflictHandler for AutoResolve {
    fn resolve(&mut self, c: &Conflict) -> Result<Handled> {
        let q = Queue::open(&self.trunk, &self.queue)?;
        self.decide(&q, c)
    }

    /// A parked conflict already had its try; it waits for what's under it
    /// to change, or for `choros queue resolve`.
    fn retries_parked(&self) -> bool {
        false
    }
}
