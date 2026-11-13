//! A trunk's event log: what became of each landing-set, in order, for
//! tools that keep their own picture of the queues rather than ask for
//! `choros status` each time. `choros events` reads it; choros keeps no
//! word of who does.
//!
//! One event a line, numbered from 1 by `seq`. A set's event is its whole
//! line as `choros status --json` gives it, less `landing_dirty`, which the
//! queue's record doesn't hold; a set that leaves the queue comes once more,
//! as `dropped`. A land straight to main makes no set, and comes as it is
//! logged:
//!
//! ```json
//! {"seq": 7, "trunk": "/src", "queue": "appq", "set": {"n": 3, "state": "merged", …}}
//! {"seq": 8, "trunk": "/src", "instant": {"repo": "app", "commit": "9c1e…", …}}
//! ```
//!
//! A log begins with every set and land its trunk already had, so reading
//! from 0 gives the whole picture.
//!
//! It is `<trunk>/.choros-config/events.db`. Writing to it never fails the
//! change it tells of: that is made by then.

use std::path::{Path, PathBuf};

use choros_core::choros::CONFIG_DIR;
use color_eyre::eyre::{Result, WrapErr, eyre};
use rusqlite::{Connection, TransactionBehavior, params};
use serde_json::{Map, Value, json};

use crate::queue::{self, Queue};
use crate::state::QueueState;
use crate::status;

fn db_path(trunk: &Path) -> PathBuf {
    trunk.join(CONFIG_DIR).join("events.db")
}

fn open(trunk: &Path) -> Result<Connection> {
    let path = db_path(trunk);
    let mut conn =
        Connection::open(&path).wrap_err_with(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    let begun: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if begun == 0 {
        // Under the write lock, so two that open a new log at once don't
        // both begin it.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))? == 0 {
            tx.execute_batch(
                "CREATE TABLE events (seq INTEGER PRIMARY KEY AUTOINCREMENT, body TEXT NOT NULL) STRICT;
                 PRAGMA user_version = 1;",
            )?;
            for body in standing(trunk)? {
                tx.execute("INSERT INTO events (body) VALUES (?1)", [body.to_string()])?;
            }
        }
        tx.commit()?;
    }
    Ok(conn)
}

/// Every set and instant land the trunk has, as events.
fn standing(trunk: &Path) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for name in queue::list(trunk)? {
        let st = Queue::open(trunk, &name)?.read_state()?;
        out.extend(st.sets.keys().map(|&n| set_event(&name, &st, n)));
    }
    for landed in crate::instant::read(trunk)? {
        out.push(json!({ "instant": landed }));
    }
    Ok(out)
}

fn set_event(queue: &str, st: &QueueState, n: u32) -> Value {
    let mut set =
        serde_json::to_value(status::set_line(queue, st, n)).expect("a set's line is plain data");
    set.as_object_mut()
        .expect("a struct")
        .remove("landing_dirty");
    json!({ "queue": queue, "set": set })
}

fn append(trunk: &Path, events: &[Value]) -> Result<()> {
    let mut conn = open(trunk)?;
    let tx = conn.transaction()?;
    for body in events {
        tx.execute("INSERT INTO events (body) VALUES (?1)", [body.to_string()])?;
    }
    tx.commit()?;
    Ok(())
}

fn tell(trunk: &Path, events: Vec<Value>) {
    if let Err(e) = append(trunk, &events) {
        eprintln!(
            "warning: couldn't write {}: {e:#}",
            db_path(trunk).display()
        );
    }
}

/// Begins the trunk's log, if it has none, with what the trunk holds now.
pub fn begin(trunk: &Path) {
    tell(trunk, Vec::new());
}

/// Logs each set whose line differs between `before` and `after`.
pub fn sets_changed(q: &Queue, before: Option<&QueueState>, after: &QueueState) {
    let line =
        |st: &QueueState, n: u32| st.sets.contains_key(&n).then(|| set_event(&q.name, st, n));
    let mut events = Vec::new();
    for &n in after.sets.keys() {
        let now = line(after, n);
        if before.and_then(|b| line(b, n)) != now {
            events.extend(now);
        }
    }
    for &n in before.map(|b| b.sets.keys()).into_iter().flatten() {
        if after.sets.contains_key(&n) {
            continue;
        }
        let mut gone = line(before.expect("it gave n"), n).expect("n is its own");
        gone["set"]["state"] = json!("dropped");
        events.push(gone);
    }
    tell(&q.trunk, events);
}

pub fn landed_instantly(trunk: &Path, landed: &crate::instant::Landed) {
    tell(trunk, vec![json!({ "instant": landed })]);
}

/// The events after `seq`, each a line of JSON. Fails when the log has
/// never held `seq`: whoever kept that number has another log's.
pub fn since(trunk: &Path, seq: i64) -> Result<Vec<(i64, String)>> {
    let conn = open(trunk)?;
    let last: i64 = conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?;
    if seq > last {
        return Err(eyre!(
            "{} holds no event {seq}: its last is {last}. Read it again from 0",
            db_path(trunk).display()
        ));
    }
    let mut rows = conn.prepare("SELECT seq, body FROM events WHERE seq > ?1 ORDER BY seq")?;
    rows.query_map(params![seq], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?
    .map(|row| {
        let (seq, body) = row?;
        let mut event = Map::new();
        event.insert("seq".into(), json!(seq));
        event.insert("trunk".into(), json!(trunk));
        event.extend(serde_json::from_str::<Map<String, Value>>(&body)?);
        Ok((seq, Value::Object(event).to_string()))
    })
    .collect()
}
