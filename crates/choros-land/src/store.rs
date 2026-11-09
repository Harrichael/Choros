//! A queue's record on disk: one SQLite file, `queue.db`, in the queue's
//! folder. Git holds the content (commits, trees); this holds what the
//! queue knows about it.
//!
//! The engine still works on a whole [`QueueState`] read at the start of a
//! step and written at the end of it, under the queue's lock. A set and a
//! repo are each a row, so what the queue comes to know per set has a row to
//! hang from.
//!
//! Queues once kept all of this in `state.json`. [`adopt_json`] moves one
//! over, and leaves that file saying where its contents went: a `choros`
//! from before the move stops on it rather than starting the queue afresh.

use std::path::{Path, PathBuf};

use color_eyre::eyre::{Context, Result, eyre};
use rusqlite::{Connection, OpenFlags, params};

use crate::state::{self, DiffbaseState, LandingSet, QueueConfig, QueueState, RepoState};

const DB: &str = "queue.db";
const JSON: &str = "state.json";
const JSON_KEPT: &str = "state.json.migrated";
const MOVED: &str = "This queue's state moved to queue.db. The choros that tried to read this file is older than the queue: reinstall it.\n";

/// The newest layout this build reads and writes, in `PRAGMA user_version`.
const VERSION: u32 = 1;

const SCHEMA: &str = "
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
    CREATE TABLE sets (n INTEGER PRIMARY KEY, body TEXT NOT NULL) STRICT;
    CREATE TABLE repos (name TEXT PRIMARY KEY, body TEXT NOT NULL) STRICT;
";

pub struct Store {
    conn: Connection,
}

pub fn db_path(dir: &Path) -> PathBuf {
    dir.join(DB)
}

/// What the queue in `dir` holds, wherever it is kept: a queue no one has
/// locked since the move is still read from its `state.json`.
pub fn read(dir: &Path, config: &QueueConfig) -> Result<QueueState> {
    if db_path(dir).exists() {
        return Store::open(dir)?.read_state();
    }
    refuse_lost_db(dir)?;
    state::read_state_json(&dir.join(JSON), config)
}

/// A queue that was moved and has no database has lost its state. Reading
/// it as a queue with nothing in it would number its next set landing-1.
fn refuse_lost_db(dir: &Path) -> Result<()> {
    if dir.join(JSON_KEPT).exists() {
        return Err(eyre!(
            "{} is gone, though this queue's state was moved into it; {} is the state as it was before the move",
            db_path(dir).display(),
            dir.join(JSON_KEPT).display()
        ));
    }
    Ok(())
}

fn is_note(json: &Path) -> bool {
    std::fs::read(json).is_ok_and(|body| body == MOVED.as_bytes())
}

/// By rename: `state.json` is never missing or half-written, either of
/// which an older `choros` reads as a queue with nothing in it.
fn leave_note(dir: &Path) -> Result<()> {
    let tmp = dir.join("state.json.tmp");
    std::fs::write(&tmp, MOVED)?;
    std::fs::rename(&tmp, dir.join(JSON))?;
    Ok(())
}

fn build_db(dir: &Path, st: Option<&QueueState>) -> Result<()> {
    let tmp = dir.join("queue.db.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut store = Store::create(&tmp)?;
    if let Some(st) = st {
        store.write_state(st)?;
    }
    drop(store);
    std::fs::rename(&tmp, db_path(dir))?;
    Ok(())
}

/// Start a new queue's database. The note goes down first: a crash before
/// the database is in place leaves a folder [`adopt_json`] finishes.
pub fn create(dir: &Path) -> Result<()> {
    leave_note(dir)?;
    build_db(dir, None)
}

/// Move a queue kept in `state.json` into `queue.db`, keeping the file as
/// `state.json.migrated`. The caller holds the queue's lock. Wherever a
/// crash cut an earlier call short, this one carries on from there.
pub fn adopt_json(dir: &Path, config: &QueueConfig) -> Result<()> {
    let (json, kept) = (dir.join(JSON), dir.join(JSON_KEPT));
    if !db_path(dir).exists() {
        refuse_lost_db(dir)?;
        if !json.exists() || is_note(&json) {
            return create(dir);
        }
        build_db(dir, Some(&state::read_state_json(&json, config)?))?;
    }
    if is_note(&json) {
        return Ok(());
    }
    if json.exists() && !kept.exists() {
        std::fs::hard_link(&json, &kept)?;
    }
    leave_note(dir)
}

impl Store {
    pub fn open(dir: &Path) -> Result<Store> {
        let path = db_path(dir);
        if !path.exists() {
            return Err(eyre!("{} is missing", path.display()));
        }
        // Read-write even to read: a write that died leaves a journal only
        // a connection that can write may roll back.
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .wrap_err_with(|| format!("opening {}", path.display()))?;
        // Readers don't take the queue's lock, so one can meet a write.
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        let found: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if found != VERSION {
            return Err(eyre!(
                "{} is in layout {found} and this choros knows layout {VERSION}: reinstall choros",
                path.display()
            ));
        }
        Ok(Store { conn })
    }

    fn create(path: &Path) -> Result<Store> {
        let conn =
            Connection::open(path).wrap_err_with(|| format!("creating {}", path.display()))?;
        conn.execute_batch(&format!(
            "BEGIN; {SCHEMA} PRAGMA user_version = {VERSION}; COMMIT;"
        ))?;
        Ok(Store { conn })
    }

    pub fn read_state(&self) -> Result<QueueState> {
        // One transaction, so a write between the queries can't be seen in half.
        let tx = self.conn.unchecked_transaction()?;
        let mut st = QueueState::default();
        st.next_n = 1;
        let mut meta = tx.prepare("SELECT key, value FROM meta")?;
        for row in meta.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (key, value) = row?;
            match key.as_str() {
                "next_n" => st.next_n = value.parse().wrap_err("next_n in queue.db")?,
                "diffbase" => st.diffbase = parse::<DiffbaseState>(&value, "the diffbase")?,
                _ => {}
            }
        }
        let mut sets = tx.prepare("SELECT n, body FROM sets")?;
        for row in sets.query_map([], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?)))? {
            let (n, body) = row?;
            st.sets
                .insert(n, parse::<LandingSet>(&body, &format!("landing-{n}"))?);
        }
        let mut repos = tx.prepare("SELECT name, body FROM repos")?;
        for row in repos.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (name, body) = row?;
            let repo = parse::<RepoState>(&body, &name)?;
            st.repos.insert(name, repo);
        }
        Ok(st)
    }

    pub fn write_state(&mut self, st: &QueueState) -> Result<()> {
        let tx = self.conn.transaction()?;
        let put = "ON CONFLICT DO UPDATE SET";
        for (key, value) in [
            ("next_n", st.next_n.to_string()),
            ("diffbase", serde_json::to_string(&st.diffbase)?),
        ] {
            tx.execute(
                &format!("INSERT INTO meta (key, value) VALUES (?1, ?2) {put} value = ?2 WHERE value <> ?2"),
                params![key, value],
            )?;
        }
        for (n, set) in &st.sets {
            tx.execute(
                &format!(
                    "INSERT INTO sets (n, body) VALUES (?1, ?2) {put} body = ?2 WHERE body <> ?2"
                ),
                params![n, serde_json::to_string(set)?],
            )?;
        }
        for (name, repo) in &st.repos {
            tx.execute(
                &format!("INSERT INTO repos (name, body) VALUES (?1, ?2) {put} body = ?2 WHERE body <> ?2"),
                params![name, serde_json::to_string(repo)?],
            )?;
        }
        let kept = serde_json::to_string(&st.sets.keys().collect::<Vec<_>>())?;
        tx.execute(
            "DELETE FROM sets WHERE n NOT IN (SELECT value FROM json_each(?1))",
            [kept],
        )?;
        let kept = serde_json::to_string(&st.repos.keys().collect::<Vec<_>>())?;
        tx.execute(
            "DELETE FROM repos WHERE name NOT IN (SELECT value FROM json_each(?1))",
            [kept],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn parse<T: serde::de::DeserializeOwned>(body: &str, what: &str) -> Result<T> {
    serde_json::from_str(body).wrap_err_with(|| format!("parsing {what} in queue.db"))
}
