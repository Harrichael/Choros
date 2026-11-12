//! Archiving a workspace a session is working in. Moving the folder then
//! would take it out from under the session, so the workspace is marked
//! instead, and the daemon archives it once the session's process is gone.

use std::path::{Path, PathBuf};

use choros_core::choros::{self, META_FILE};
use choros_core::process::Proc;
use choros_core::space;
use color_eyre::eyre::{Result, eyre};
use serde::{Deserialize, Serialize};

use crate::workspace;

/// Told to whoever asked for an archive that waits.
pub const WAITS: &str = "the workspace is archived once this session closes (chorosd does it); `choros archive --force` archives it now";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct After {
    pub session: Proc,
    /// Asked for by `land archive`, which leaves no commit unlanded: a
    /// workspace with commits made since stays.
    pub landed: bool,
}

/// Archives `ws`, or with a `session` at work in it, marks it to be
/// archived once that closes. Returns where it went, when it did.
pub fn archive(ws: &Path, session: Option<Proc>, landed: bool) -> Result<Option<PathBuf>> {
    if !ws.join(META_FILE).exists() {
        return Err(eyre!(
            "refusing to archive: {} is not a choros workspace (no {META_FILE})",
            ws.display()
        ));
    }
    let mut wl = workspace::read(ws)?;
    let after = session.map(|session| After { session, landed });
    let waits = after.is_some();
    if wl.archive_after != after {
        wl.archive_after = after;
        workspace::write(ws, &wl)?;
    }
    if waits {
        return Ok(None);
    }
    choros::archive_at(ws).map(Some)
}

#[derive(Debug, Default)]
pub struct Swept {
    pub archived: Vec<PathBuf>,
    /// Left in place, each with why.
    pub kept: Vec<(PathBuf, String)>,
    pub errors: Vec<String>,
}

/// Archives each workspace of `trunk` whose session has closed.
pub fn sweep(trunk: &Path) -> Result<Swept> {
    let mut swept = Swept::default();
    for info in space::tree(trunk)?.all_workspaces() {
        let ws = &info.path;
        match settle(ws, &info.meta.repos) {
            Ok(Some(Ok(_))) => swept.archived.push(ws.clone()),
            Ok(Some(Err(why))) => swept.kept.push((ws.clone(), why)),
            Ok(None) => {}
            Err(e) => swept.errors.push(format!("{}: {e:#}", ws.display())),
        }
    }
    Ok(swept)
}

/// `None` while `ws` isn't waiting or its session is still open; else where
/// it was archived to, or why it wasn't.
fn settle(ws: &Path, repos: &[String]) -> Result<Option<Result<PathBuf, String>>> {
    let mut wl = workspace::read(ws)?;
    let Some(after) = wl.archive_after.take() else {
        return Ok(None);
    };
    if after.session.alive() {
        return Ok(None);
    }
    workspace::write(ws, &wl)?;
    if after.landed {
        let left: Vec<&str> = repos
            .iter()
            .filter(|r| crate::land::has_unlanded(ws, r).unwrap_or(true))
            .map(String::as_str)
            .collect();
        if !left.is_empty() {
            return Ok(Some(Err(format!(
                "unlanded commits in {}",
                left.join(", ")
            ))));
        }
    }
    choros::archive_at(ws).map(|dst| Some(Ok(dst)))
}
