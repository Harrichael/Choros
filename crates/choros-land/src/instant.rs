//! Instant mode: how it reaches main, and what it put there.
//!
//! How: `<trunk>/.choros-config/instant.toml`, overridable by a branch-space's
//! `landing.toml` and per workspace in its `.choros-landing.toml`, nearest
//! first.
//!
//! What: each land is a line in `<trunk>/.choros-config/instant.jsonl`. It
//! makes no landing-set, so this log is the only record of its labels and
//! sessions, which `choros status --json` reports for other tools. A PR that
//! merges after its land is logged by chorosd, through [`log_merged`].

use std::io::Write;
use std::path::{Path, PathBuf};

use choros_core::choros::{ARCHIVE_REL, CONFIG_DIR, META_FILE};
use color_eyre::eyre::Result;
use serde::{Deserialize, Serialize};

use crate::forge::{Forge, PrState};
use crate::workspace::{self, InstantPr};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    /// Push straight to main.
    #[default]
    Push,
    /// Open a PR and merge it at once: a PR record, and required checks run.
    Pr,
}

#[derive(Debug, Default, Deserialize)]
struct InstantFile {
    #[serde(default)]
    instant: Section,
}

#[derive(Debug, Default, Deserialize)]
struct Section {
    #[serde(default)]
    via: Via,
}

pub fn via(trunk: &Path, ws: &Path) -> Result<Via> {
    if let Some(v) = workspace::read(ws)?.instant {
        return Ok(v);
    }
    if let Some(space) = ws.parent().and_then(choros_core::space::find_space)
        && let Some(v) = crate::branch::default_instant(&space)?
    {
        return Ok(v);
    }
    let path = trunk.join(CONFIG_DIR).join("instant.toml");
    if !path.exists() {
        return Ok(Via::default());
    }
    let file: InstantFile = toml::from_str(&std::fs::read_to_string(path)?)?;
    Ok(file.instant.via)
}

/// A land that went straight to main.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Landed {
    pub repo: String,
    pub workspace: PathBuf,
    /// The last commit it put on main, as rebased in the workspace.
    pub commit: String,
    /// The PR it merged through, with `via = "pr"`.
    pub pr: Option<u64>,
    pub labels: Vec<String>,
    /// The Claude Code sessions the work came from.
    pub sessions: Vec<String>,
}

fn log_path(trunk: &Path) -> PathBuf {
    trunk.join(CONFIG_DIR).join("instant.jsonl")
}

/// One line per land, appended whole, so concurrent lands need no lock.
pub fn record(trunk: &Path, landed: &Landed) -> Result<()> {
    let mut line = serde_json::to_string(landed)?;
    line.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(trunk))?
        .write_all(line.as_bytes())?;
    Ok(())
}

pub fn read(trunk: &Path) -> Result<Vec<Landed>> {
    let path = log_path(trunk);
    if !path.exists() {
        return Ok(Vec::new());
    }
    std::fs::read_to_string(path)?
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| Ok(serde_json::from_str(l)?))
        .collect()
}

/// Log the merge of an instant PR, unless it is logged already: chorosd and
/// the workspace's next land may both see it merged. Both can also read the
/// log before either writes, and log it twice; readers shrug that off.
pub fn record_pr(trunk: &Path, ws: &Path, repo: &str, pr: &InstantPr) -> Result<()> {
    if is_logged(&read(trunk)?, repo, pr) {
        return Ok(());
    }
    record(trunk, &pr_landed(ws, repo, pr))
}

#[derive(Debug, Default)]
pub struct MergedReport {
    pub logged: Vec<Landed>,
    /// PRs or workspaces that couldn't be checked; the rest still were.
    pub errors: Vec<String>,
}

/// Log every instant PR in the trunk that merged after its land, rather than
/// wait for the workspace's next land, which may never come: archived
/// workspaces are checked too, and logged under the path they were landed
/// from. The workspace's own record is left alone: an instant land takes no
/// lock, so writing it here could undo a land's write. That next land, if
/// any, settles it.
pub fn log_merged(trunk: &Path, forge: &dyn Forge) -> Result<MergedReport> {
    let logged = read(trunk)?;
    let mut workspaces: Vec<(PathBuf, PathBuf)> = choros_core::space::tree(trunk)?
        .all_workspaces()
        .into_iter()
        .map(|i| (i.path.clone(), i.path.clone()))
        .collect();
    let archive = trunk.join(ARCHIVE_REL);
    for dir in archived_workspaces(&archive)? {
        let landed_from = trunk.join(dir.strip_prefix(&archive)?);
        workspaces.push((dir, landed_from));
    }
    let mut report = MergedReport::default();
    for (dir, landed_from) in workspaces {
        let repos = match workspace::read(&dir) {
            Ok(wl) => wl.repo,
            Err(e) => {
                report.errors.push(format!("{}: {e:#}", dir.display()));
                continue;
            }
        };
        for (repo, rl) in repos {
            let Some(pr) = rl.instant_pr else { continue };
            if is_logged(&logged, &repo, &pr) {
                continue;
            }
            match forge.pr_state(&dir.join(&repo), pr.number) {
                Ok(PrState::Merged) => {}
                Ok(_) => continue,
                Err(e) => {
                    report.errors.push(format!("{repo} PR #{} in {}: {e:#}", pr.number, dir.display()));
                    continue;
                }
            }
            let landed = pr_landed(&landed_from, &repo, &pr);
            record(trunk, &landed)?;
            report.logged.push(landed);
        }
    }
    Ok(report)
}

/// The workspaces under the archive, however deep their old branch-spaces.
fn archived_workspaces(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        if path.join(META_FILE).exists() {
            out.push(path);
        } else {
            out.extend(archived_workspaces(&path)?);
        }
    }
    Ok(out)
}

fn is_logged(log: &[Landed], repo: &str, pr: &InstantPr) -> bool {
    log.iter().any(|l| l.repo == repo && l.pr == Some(pr.number))
}

fn pr_landed(ws: &Path, repo: &str, pr: &InstantPr) -> Landed {
    Landed {
        repo: repo.to_string(),
        workspace: ws.to_path_buf(),
        commit: pr.head.clone(),
        pr: Some(pr.number),
        labels: pr.labels.clone(),
        sessions: pr.sessions.clone(),
    }
}
