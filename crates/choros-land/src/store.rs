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

use crate::mend::{By, Mended, Side, Sides};
use crate::state::{
    self, Change, DiffbaseState, LandingSet, Placement, QueueConfig, QueueState, RepoState,
    now_secs,
};

const DB: &str = "queue.db";
const JSON: &str = "state.json";
const JSON_KEPT: &str = "state.json.migrated";
const MOVED: &str = "This queue's state moved to queue.db. The choros that tried to read this file is older than the queue: reinstall it.\n";

/// The layout's steps, oldest first: a database's `PRAGMA user_version`
/// counts the ones it has had.
const LAYOUT: &[&str] = &[
    "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
     CREATE TABLE sets (n INTEGER PRIMARY KEY, body TEXT NOT NULL) STRICT;
     CREATE TABLE repos (name TEXT PRIMARY KEY, body TEXT NOT NULL) STRICT;",
    // Each change ever made to a set, and each place the queue put one. A
    // set's row holds only the latest of each.
    "CREATE TABLE changes (
         n INTEGER NOT NULL, rev INTEGER NOT NULL,
         base TEXT NOT NULL, tip TEXT NOT NULL,
         by TEXT NOT NULL, who TEXT NOT NULL, at INTEGER NOT NULL,
         PRIMARY KEY (n, rev)) STRICT;
     CREATE TABLE placements (
         id INTEGER PRIMARY KEY,
         n INTEGER NOT NULL, rev INTEGER NOT NULL,
         onto TEXT NOT NULL, base TEXT NOT NULL, tip TEXT NOT NULL,
         at INTEGER NOT NULL) STRICT;
     CREATE INDEX placements_of ON placements (n, id);",
    // `resolutions`: what a conflicted file came to, by its three versions
    // (blobs; '' where a side has no file), so the same conflict is settled
    // the same way again. `settled`: which files each commit the queue made
    // had settled, and by what.
    "CREATE TABLE resolutions (
         repo TEXT NOT NULL, path TEXT NOT NULL,
         base TEXT NOT NULL, ours TEXT NOT NULL, theirs TEXT NOT NULL,
         result TEXT NOT NULL, by TEXT NOT NULL, tool TEXT NOT NULL, at INTEGER NOT NULL,
         PRIMARY KEY (repo, path, base, ours, theirs)) STRICT;
     CREATE TABLE settled (
         repo TEXT NOT NULL, made TEXT NOT NULL, path TEXT NOT NULL,
         by TEXT NOT NULL, tool TEXT NOT NULL, remembered INTEGER NOT NULL, at INTEGER NOT NULL,
         PRIMARY KEY (repo, made, path)) STRICT;",
];

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

/// Bring the queue's record to what this build keeps: a queue kept in
/// `state.json` is moved into `queue.db`, with the file kept as
/// `state.json.migrated`, and a database from an older build gains what
/// was added to the layout since. The caller holds the queue's lock.
/// Wherever a crash cut an earlier call short, this one carries on.
pub fn adopt(dir: &Path, config: &QueueConfig) -> Result<()> {
    let (json, kept) = (dir.join(JSON), dir.join(JSON_KEPT));
    if !db_path(dir).exists() {
        refuse_lost_db(dir)?;
        if !json.exists() || is_note(&json) {
            return create(dir);
        }
        build_db(dir, Some(&state::read_state_json(&json, config)?))?;
    }
    Store::open(dir)?.upgrade()?;
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
        if layout_steps(&conn)? > LAYOUT.len() {
            return Err(eyre!(
                "{} was written by a newer choros than this one: reinstall choros",
                path.display()
            ));
        }
        Ok(Store { conn })
    }

    fn create(path: &Path) -> Result<Store> {
        let conn =
            Connection::open(path).wrap_err_with(|| format!("creating {}", path.display()))?;
        let mut store = Store { conn };
        store.upgrade()?;
        Ok(store)
    }

    fn upgrade(&mut self) -> Result<()> {
        if layout_steps(&self.conn)? == LAYOUT.len() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        for step in &LAYOUT[layout_steps(&tx)?..] {
            tx.execute_batch(step)?;
        }
        tx.execute_batch(&format!("PRAGMA user_version = {}", LAYOUT.len()))?;
        tx.commit()?;
        Ok(())
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
        for (n, set) in &st.sets {
            let Some(c) = &set.change else { continue };
            // A tip whose push is still in doubt is no placement yet: a
            // refused push puts the old one back.
            if set.pending.is_some() {
                continue;
            }
            tx.execute(
                "INSERT OR IGNORE INTO changes (n, rev, base, tip, by, who, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![n, c.rev, c.base, c.tip, c.by.as_str(), serde_json::to_string(&c.who)?, c.at as i64],
            )?;
            tx.execute(
                "INSERT INTO placements (n, rev, onto, base, tip, at) SELECT ?1, ?2, ?3, ?4, ?5, ?6
                 WHERE (?4, ?5) IS NOT (SELECT base, tip FROM placements WHERE n = ?1 ORDER BY id DESC LIMIT 1)",
                params![n, c.rev, set.base.key(), set.base_sha, set.tip, now_secs() as i64],
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

fn layout_steps(conn: &Connection) -> Result<usize> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))? as usize)
}

fn parse<T: serde::de::DeserializeOwned>(body: &str, what: &str) -> Result<T> {
    serde_json::from_str(body).wrap_err_with(|| format!("parsing {what} in queue.db"))
}

/// Everything that has happened to landing-`n`'s change, oldest first.
pub struct History {
    pub changes: Vec<Change>,
    pub placements: Vec<Placement>,
}

impl Store {
    pub fn history(&self, n: u32) -> Result<History> {
        // A database no one has locked since this build was installed.
        if layout_steps(&self.conn)? < LAYOUT.len() {
            return Ok(History {
                changes: Vec::new(),
                placements: Vec::new(),
            });
        }
        let mut changes = self
            .conn
            .prepare("SELECT rev, base, tip, by, who, at FROM changes WHERE n = ?1 ORDER BY rev")?;
        let changes = changes
            .query_map([n], |r| {
                Ok((
                    r.get::<_, u32>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            })?
            .map(|row| {
                let (rev, base, tip, by, who, at) = row?;
                Ok(Change {
                    rev,
                    base,
                    tip,
                    by: parse(&format!("\"{by}\""), "a change")?,
                    who: parse(&who, "a change")?,
                    at: at as u64,
                })
            })
            .collect::<Result<_>>()?;
        let mut placements = self
            .conn
            .prepare("SELECT rev, onto, base, tip, at FROM placements WHERE n = ?1 ORDER BY id")?;
        let placements = placements
            .query_map([n], |r| {
                Ok(Placement {
                    rev: r.get(0)?,
                    onto: r.get(1)?,
                    base: r.get(2)?,
                    tip: r.get(3)?,
                    at: r.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(History {
            changes,
            placements,
        })
    }
}

/// A file a commit of the queue's had settled.
pub struct Settled {
    pub path: String,
    pub by: By,
    /// The queue had the answer already.
    pub remembered: bool,
}

impl Store {
    /// What this queue settled `sides` as before, if it has.
    pub fn resolution(&self, repo: &str, sides: &Sides) -> Result<Option<Mended>> {
        let mut found = self.conn.prepare(
            "SELECT result, by, tool FROM resolutions WHERE repo = ?1 AND path = ?2 AND base = ?3 AND ours = ?4 AND theirs = ?5",
        )?;
        let mut rows = found.query(params![
            repo,
            sides.path,
            blob(&sides.base),
            blob(&sides.ours),
            blob(&sides.theirs)
        ])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let result: String = row.get(0)?;
        Ok(Some(Mended {
            path: sides.path.clone(),
            result: (!result.is_empty()).then_some(result),
            by: By::named(&row.get::<_, String>(1)?, row.get(2)?)?,
            remembered: true,
        }))
    }

    /// Note that the commit `made` settled these files, and keep each
    /// newly given answer for the next time its three versions meet.
    pub fn settle(&mut self, repo: &str, made: &str, files: &[(&Sides, &Mended)]) -> Result<()> {
        let at = now_secs() as i64;
        let tx = self.conn.transaction()?;
        for (sides, m) in files {
            tx.execute(
                "INSERT OR REPLACE INTO settled (repo, made, path, by, tool, remembered, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![repo, made, m.path, m.by.name(), m.by.tool(), m.remembered, at],
            )?;
            // What a command made of the whole tree says nothing about
            // these three versions in another.
            if m.remembered || matches!(m.by, By::Regenerate(_)) {
                continue;
            }
            tx.execute(
                "INSERT OR REPLACE INTO resolutions (repo, path, base, ours, theirs, result, by, tool, at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    repo,
                    m.path,
                    blob(&sides.base),
                    blob(&sides.ours),
                    blob(&sides.theirs),
                    m.result.as_deref().unwrap_or_default(),
                    m.by.name(),
                    m.by.tool(),
                    at
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The files settled in any of the commits `made`.
    pub fn settled_in(&self, repo: &str, made: &[String]) -> Result<Vec<Settled>> {
        if layout_steps(&self.conn)? < LAYOUT.len() {
            return Ok(Vec::new());
        }
        let mut found = self.conn.prepare(
            "SELECT path, by, tool, remembered FROM settled
             WHERE repo = ?1 AND made IN (SELECT value FROM json_each(?2)) ORDER BY path",
        )?;
        let rows = found.query_map(params![repo, serde_json::to_string(made)?], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, bool>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (path, by, tool, remembered) = row?;
            Ok(Settled {
                path,
                by: By::named(&by, tool)?,
                remembered,
            })
        })
        .collect()
    }
}

fn blob(side: &Option<Side>) -> &str {
    side.as_ref().map_or("", |s| &s.blob)
}
