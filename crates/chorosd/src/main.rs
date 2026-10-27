//! chorosd: runs every landing queue's pass on a timer — notices merged and
//! closed PRs, re-syncs what is behind them, rebuilds merge heads, opens the
//! next PRs — and logs instant PRs that merged after their land. Conflicts
//! park; it never runs Claude and never asks anything.

use std::path::Path;
use std::time::Duration;

use choros_core::choros::CONFIG_DIR;
use choros_core::trunks;
use choros_land::agent::Claude;
use choros_land::engine::{Env, PassReport, Work};
use choros_land::forge::Gh;
use choros_land::queue::{self, Queue};
use color_eyre::eyre::Result;
use time::OffsetDateTime;

fn main() -> Result<()> {
    color_eyre::install()?;
    let once = std::env::args().any(|a| a == "--once");
    let interval = std::env::var("CHOROSD_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    loop {
        if let Err(e) = tick() {
            log(&format!("pass failed: {e:#}"));
        }
        if once {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(interval));
    }
}

fn tick() -> Result<()> {
    let Some(config) = trunks::config_home() else {
        log("neither XDG_CONFIG_HOME nor HOME is set; nothing to watch");
        return Ok(());
    };
    for trunk in trunks::list(&config)? {
        if !trunk.exists() {
            continue;
        }
        for name in queue::list(&trunk)? {
            // One queue failing (a network blip, a bad state file) must not
            // stop the others.
            if let Err(e) = pass(&trunk, &name) {
                log(&format!("{name} @ {}: {e:#}", trunk.display()));
            }
        }
        if let Err(e) = log_instant_merges(&trunk) {
            log(&format!("instant PRs @ {}: {e:#}", trunk.display()));
        }
    }
    Ok(())
}

fn log_instant_merges(trunk: &Path) -> Result<()> {
    let gh_dir = trunk.join(CONFIG_DIR).join("gh");
    let gh = Gh {
        config_dir: gh_dir.exists().then_some(gh_dir),
    };
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

fn pass(trunk: &Path, name: &str) -> Result<()> {
    let q = Queue::open(trunk, name)?;
    let gh = Gh {
        config_dir: q.gh_config_dir(),
    };
    let claude = Claude;
    let env = Env {
        forge: &gh,
        agent: &claude,
        home: None,
    };
    // Someone is landing or resolving; they will finish the job, and the
    // next tick will see their result.
    let Some(mut work) = Work::begin(&q, env.agent, None, false)? else {
        return Ok(());
    };
    let report = work.pass(env.forge);
    work.save()?;
    describe(name, &report?);
    if queue::retire_if_orphaned(&q, &work.st)? {
        log(&format!("{name}: its workspace or branch-space is archived and nothing is open; archived it too"));
    }
    Ok(())
}

fn describe(name: &str, r: &PassReport) {
    let list = |v: &[u32]| v.iter().map(|n| format!("landing-{n}")).collect::<Vec<_>>().join(", ");
    for (what, v) in [
        ("merged", &r.merged),
        ("closed", &r.closed),
        ("re-synced", &r.resynced),
        ("took main in", &r.updated_public),
        ("opened PRs for", &r.opened),
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
