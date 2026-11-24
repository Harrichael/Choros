//! chorosd: runs every landing queue's pass on a timer — notices merged and
//! closed PRs, re-syncs what is behind them, rebuilds merge heads, opens the
//! next PRs — logs instant PRs that merged after their land, and archives
//! workspaces left for when their session closed. A conflict
//! goes to the resync agent, within the queue's limits (see
//! `choros_land::auto_resolve`), and parks when it can't be settled; it never
//! asks anything.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::Duration;

use choros_core::choros::CONFIG_DIR;
use choros_core::trunks;
use choros_land::agent::Claude;
use choros_land::auto_resolve::AutoResolve;
use choros_land::engine::{ConflictHandler, Env, PassReport, Work};
use choros_land::forge::Gh;
use choros_land::queue::{self, Queue};
use color_eyre::eyre::Result;
use time::OffsetDateTime;

/// Passes in a row that may find a queue locked before the daemon waits
/// for it. Commands wait for the lock and the daemon only tries it, so a
/// queue that is never free between commands would otherwise never get a
/// pass of the daemon's own.
const WAIT_AFTER_SKIPS: u32 = 3;

/// Each queue's thread, by trunk and name.
type Watching = HashMap<(PathBuf, String), JoinHandle<()>>;

fn main() -> Result<()> {
    color_eyre::install()?;
    let once = std::env::args().any(|a| a == "--once");
    let interval = Duration::from_secs(
        std::env::var("CHOROSD_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(120),
    );
    let mut watching = Watching::new();
    loop {
        let watching = if once { None } else { Some(&mut watching) };
        if let Err(e) = tick(watching, interval) {
            log(&format!("pass failed: {e:#}"));
        }
        if once {
            return Ok(());
        }
        std::thread::sleep(interval);
    }
}

/// Look over every trunk: start a thread for each queue that has none
/// (or, `--once`, pass each queue here), then log instant merges and
/// archive closed sessions' workspaces.
fn tick(watching: Option<&mut Watching>, interval: Duration) -> Result<()> {
    let Some(config) = trunks::config_home() else {
        log("neither XDG_CONFIG_HOME nor HOME is set; nothing to watch");
        return Ok(());
    };
    let mut watching = watching;
    for trunk in trunks::list(&config)? {
        if !trunk.exists() {
            continue;
        }
        for name in queue::list(&trunk)? {
            match watching.as_deref_mut() {
                // Each queue on its own thread, so one waiting for its lock
                // holds up no other.
                Some(w) => {
                    let key = (trunk.clone(), name.clone());
                    if w.get(&key).is_some_and(|t| !t.is_finished()) {
                        continue;
                    }
                    let (trunk, name) = key.clone();
                    // Named, so a panic's message says which queue.
                    let thread = std::thread::Builder::new()
                        .name(name.clone())
                        .spawn(move || watch(&trunk, &name, interval))?;
                    w.insert(key, thread);
                }
                // One queue failing (a network blip, a bad state file) must
                // not stop the others.
                None => {
                    if let Err(e) = pass(&trunk, &name, false) {
                        log(&format!("{name} @ {}: {e:#}", trunk.display()));
                    }
                }
            }
        }
        if let Err(e) = log_instant_merges(&trunk) {
            log(&format!("instant PRs @ {}: {e:#}", trunk.display()));
        }
        match choros_land::archive::sweep(&trunk) {
            Ok(swept) => {
                for ws in &swept.archived {
                    log(&format!("{}: its session closed; archived it", ws.display()));
                }
                for (ws, why) in &swept.kept {
                    log(&format!("{}: its session closed, but not archiving: {why}", ws.display()));
                }
                for e in &swept.errors {
                    log(&format!("archiving: {e}"));
                }
            }
            Err(e) => log(&format!("archiving @ {}: {e:#}", trunk.display())),
        }
    }
    Ok(())
}

fn log_instant_merges(trunk: &Path) -> Result<()> {
    let gh_dir = trunk.join(CONFIG_DIR).join("gh");
    let gh = Gh::new(gh_dir.exists().then_some(gh_dir));
    let report = choros_land::instant::log_merged(trunk, &gh)?;
    for l in &report.logged {
        let pr = l.pr.map(|n| format!("PR #{n}")).unwrap_or_default();
        log(&format!("{}: {pr} merged; logged its instant land from {}", l.repo, l.workspace.display()));
    }
    for e in &report.errors {
        log(&format!("instant PRs: {e}"));
    }
    Ok(())
}

/// Pass one queue every `interval` for as long as it exists.
fn watch(trunk: &Path, name: &str, interval: Duration) {
    let mut skipped = 0;
    loop {
        if !queue::list(trunk).is_ok_and(|names| names.iter().any(|n| n == name)) {
            return;
        }
        let wait = skipped >= WAIT_AFTER_SKIPS;
        if wait {
            let by = Queue::open(trunk, name).ok().and_then(|q| q.holder());
            let by = by.map(|h| format!(", held by {h}")).unwrap_or_default();
            log(&format!("{name}: locked for {skipped} passes in a row{by}; waiting for it"));
        }
        match pass(trunk, name, wait) {
            Ok(true) => skipped = 0,
            Ok(false) => skipped += 1,
            Err(e) => log(&format!("{name} @ {}: {e:#}", trunk.display())),
        }
        std::thread::sleep(interval);
    }
}

/// One pass of the queue, then the jobs it asked for, with the queue let
/// go (see `choros_land::pending`), and another pass to settle what they
/// found, for as long as they find something. `false` when the queue was
/// locked and `wait` was not.
fn pass(trunk: &Path, name: &str, wait: bool) -> Result<bool> {
    // A pass that saved and then failed (GitHub unreachable, say) still
    // asked for its jobs, and they don't need GitHub.
    let passed = one_pass(trunk, name, wait);
    if matches!(passed, Ok(false)) {
        return Ok(false);
    }
    if let Err(e) = jobs(trunk, name) {
        log(&format!("{name} @ {}: {e:#}", trunk.display()));
    }
    passed
}

/// The jobs the last pass asked for, then a pass to take up what they
/// found, for as long as they find something.
fn jobs(trunk: &Path, name: &str) -> Result<()> {
    for _ in 0..choros_land::pending::ROUNDS {
        // The pass may have retired the queue.
        if !queue::list(trunk)?.iter().any(|n| n == name) {
            break;
        }
        let q = Queue::open(trunk, name)?;
        let claude = Claude::default();
        let mut auto = AutoResolve::new(trunk.to_path_buf(), name.to_string());
        let mut notes = Vec::new();
        let agent = q.config.agent.resolver.auto_resolve.then_some(&mut auto);
        let ran = choros_land::pending::run(&q, &claude, agent, &mut notes);
        for note in notes.iter().chain(&auto.notes) {
            log(&format!("{name}: {note}"));
        }
        // Whoever holds the queue now will pass it, or the next tick will.
        if ran? == 0 || !one_pass(trunk, name, false)? {
            break;
        }
    }
    Ok(())
}

/// One pass of the queue; `false` when it was locked and `wait` was not.
fn one_pass(trunk: &Path, name: &str, wait: bool) -> Result<bool> {
    // queue.toml is read when the queue is opened, and whoever held the
    // lock may have changed it (a diffbase added, say). So after a wait the
    // queue is opened afresh, and only a lock taken with no wait is used.
    let (q, lock) = loop {
        let q = Queue::open(trunk, name)?;
        if let Some(lock) = q.try_lock()? {
            break (q, lock);
        }
        // Someone is landing or resolving; they will finish the job, and
        // the next tick will see their result.
        if !wait {
            return Ok(false);
        }
        drop(q.lock()?);
    };
    let gh = Gh::new(q.gh_config_dir());
    let claude = Claude::default();
    let env = Env {
        forge: &gh,
        agent: &claude,
        home: None,
    };
    let mut auto = AutoResolve::new(trunk.to_path_buf(), name.to_string());
    let handler: Option<&mut dyn ConflictHandler> = q.config.agent.resolver.auto_resolve.then_some(&mut auto as _);
    let mut work = Work::locked(&q, env.agent, handler, lock)?;
    let report = work.pass(env.forge);
    work.save()?;
    let retired = queue::retire_if_orphaned(&q, &work.st)?;
    drop(work);
    for note in &auto.notes {
        log(&format!("{name}: {note}"));
    }
    describe(name, &report?);
    if retired {
        log(&format!("{name}: its workspace or branch-space is archived and nothing is open; archived it too"));
    }
    Ok(true)
}

fn describe(name: &str, r: &PassReport) {
    let list = |v: &[u32]| v.iter().map(|n| format!("landing-{n}")).collect::<Vec<_>>().join(", ");
    for (what, v) in [
        ("merged", &r.merged),
        ("closed", &r.closed),
        ("re-synced", &r.resynced),
        ("took main in", &r.updated_public),
        ("opened PRs for", &r.opened),
        ("opened stacked PRs for", &r.stacked),
        ("took GitHub's rebase of", &r.adopted),
        ("folded into the set below, as they were left empty,", &r.folded),
        ("left for GitHub's Rebase stack, as the set below moved,", &r.behind_in_stack),
        ("found everything already on main, so no PR, for", &r.already_on_main),
        ("parked", &r.parked),
    ] {
        if !v.is_empty() {
            log(&format!("{name}: {what} {}", list(v)));
        }
    }
    for e in &r.errors {
        log(&format!("{name}: {e}"));
    }
}

fn log(msg: &str) {
    let now = OffsetDateTime::now_utc();
    eprintln!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z {msg}",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );
}
