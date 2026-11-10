//! The land hook: a command the trunk names, told of each land before it
//! is made and after, so that whatever tracks the work a land carries can
//! follow along. `<trunk>/.choros-config/hooks.toml`:
//!
//! ```toml
//! land = "tracker land-hook"   # run as `tracker land-hook before|after`
//! ```
//!
//! Before, it reads the land as JSON on stdin and may print JSON naming
//! what to land with instead; failing, it refuses the land, and what it
//! wrote to stderr says why:
//!
//! ```json
//! {"mode": "continue", "labels": ["tasks/a"], "priority": null, "title": null,
//!  "body": null, "session": "0b5e…", "workspace": "/trunk/space/ws"}
//! ```
//! ```json
//! {"labels": ["tasks/a", "tasks/b"], "priority": "p1", "body": "…"}
//! ```
//!
//! After, it reads the same with what it changed and `landed`, each set
//! made (`queue` and `n` are null straight to main). The land is made by
//! then, so a failure is only reported.
//!
//! Choros gives the labels no meaning here either: the hook does.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use choros_core::choros::CONFIG_DIR;
use color_eyre::eyre::{Result, WrapErr, eyre};
use serde::{Deserialize, Serialize};

use crate::land::{LandRequest, Landed};
use crate::state::LandMode;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    land: Option<String>,
}

#[derive(Serialize)]
struct Told<'a> {
    mode: LandMode,
    labels: &'a [String],
    priority: Option<String>,
    title: Option<&'a str>,
    body: Option<&'a str>,
    session: Option<&'a str>,
    workspace: &'a Path,
    #[serde(skip_serializing_if = "Option::is_none")]
    landed: Option<Vec<Set<'a>>>,
}

#[derive(Serialize)]
struct Set<'a> {
    repo: &'a str,
    queue: Option<&'a str>,
    n: Option<u32>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    labels: Option<Vec<String>>,
    priority: Option<String>,
    title: Option<String>,
    body: Option<String>,
}

/// The trunk's land hook, if it names one. A `hooks.toml` that can't be
/// read is an error: a hook that was meant to gate lands mustn't be
/// skipped in silence.
fn command(trunk: &Path) -> Result<Option<String>> {
    let path = trunk.join(CONFIG_DIR).join("hooks.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).wrap_err_with(|| format!("reading {}", path.display())),
    };
    let config: Config =
        toml::from_str(&text).wrap_err_with(|| format!("reading {}", path.display()))?;
    Ok(config.land.filter(|c| !c.trim().is_empty()))
}

fn told<'a>(req: &'a LandRequest, ws: &'a Path, landed: Option<&'a [Landed]>) -> Told<'a> {
    Told {
        mode: req.mode,
        labels: &req.labels,
        priority: req.priority.map(|p| format!("p{p}")),
        title: req.title.as_deref(),
        body: req.body.as_deref(),
        session: req.env_session.as_deref().filter(|s| !s.is_empty()),
        workspace: ws,
        landed: landed.map(|sets| {
            sets.iter()
                .map(|l| Set {
                    repo: &l.repo,
                    queue: l.queue.as_deref(),
                    n: l.n,
                })
                .collect()
        }),
    }
}

/// Runs the hook at `stage`, its stderr on ours. `Ok` holds what it printed.
fn run(command: &str, stage: &str, cwd: &Path, told: &Told) -> Result<String> {
    let mut child = Command::new("sh")
        .args([
            "-c",
            &format!("{command} \"$1\""),
            "choros-land-hook",
            stage,
        ])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .wrap_err_with(|| format!("running the land hook `{command}`"))?;
    // A hook that reads nothing closes the pipe; that isn't a failure.
    let _ = child
        .stdin
        .take()
        .expect("piped")
        .write_all(&serde_json::to_vec(told)?);
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(eyre!("the land hook `{command} {stage}` failed"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Tells the hook of a land about to be made, from `ws`. Returns the
/// request to land with, when the hook changed it.
pub fn before(trunk: &Path, ws: &Path, req: &LandRequest) -> Result<Option<LandRequest>> {
    let Some(command) = command(trunk)? else {
        return Ok(None);
    };
    let printed =
        run(&command, "before", &req.cwd, &told(req, ws, None)).wrap_err("nothing landed")?;
    if printed.is_empty() {
        return Ok(None);
    }
    let reply: Reply = serde_json::from_str(&printed).wrap_err_with(|| {
        format!("nothing landed: the land hook `{command} before` printed what isn't its reply")
    })?;
    let mut req = req.clone();
    if let Some(labels) = reply.labels {
        req.labels = labels;
    }
    if let Some(p) = reply.priority {
        req.priority = Some(match p.as_str() {
            "p0" | "p1" | "p2" | "p3" | "p4" => p.as_bytes()[1] - b'0',
            _ => {
                return Err(eyre!(
                    "nothing landed: the land hook gave priority `{p}`; it takes p0 (most urgent) to p4"
                ));
            }
        });
    }
    if reply.title.is_some() {
        req.title = reply.title;
    }
    if reply.body.is_some() {
        req.body = reply.body;
    }
    Ok(Some(req))
}

/// Tells the hook what landed. The land stands whatever it does, so its
/// failure comes back as a warning to show.
pub fn after(trunk: &Path, ws: &Path, req: &LandRequest, landed: &[Landed]) -> Option<String> {
    let command = command(trunk).ok().flatten()?;
    run(&command, "after", &req.cwd, &told(req, ws, Some(landed)))
        .err()
        .map(|e| format!("{e}: the work landed, but what the hook keeps may not have followed"))
}
