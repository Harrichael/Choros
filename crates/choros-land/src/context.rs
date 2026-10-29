//! What whoever resolves a conflict is told beyond the code: each set whose
//! changes meet in the conflicted files, with its PR text, its commits, and
//! what `<trunk>/.choros-config/context.toml` says about its labels:
//!
//! ```toml
//! label = "ergon show {} --intent"   # {} is the label; what it prints is shown
//! ```
//!
//! Choros still never reads a label itself. The command does, so labels can
//! name anything (tasks, tickets) and the trunk says how to look them up.

use std::path::Path;
use std::process::Command;

use choros_core::choros::CONFIG_DIR;
use serde::Deserialize;

use crate::queue::Queue;
use crate::state::QueueState;

pub struct Side {
    /// `appq/landing-3`.
    pub set: String,
    pub title: String,
    pub body: Option<String>,
    /// Subjects, oldest first.
    pub commits: Vec<String>,
    /// Each label the hook had something to say about, and what it said.
    pub labels: Vec<(String, String)>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    label: Option<String>,
}

/// The open sets in `repo` that change any of `files`. Context is a help,
/// not a gate: anything that can't be read is left out.
pub fn sides(q: &Queue, st: &QueueState, repo: &str, files: &[String]) -> Vec<Side> {
    let hook = read_config(&q.trunk).label;
    let git = q.git(repo);
    let mut out = Vec::new();
    for set in st.sets.values().filter(|s| s.is_open() && s.repo() == repo) {
        let range = format!("{}..{}", set.base_sha, set.tip);
        let Ok(changed) = git.run(&["diff", "--name-only", &set.base_sha, &set.tip]) else {
            continue;
        };
        if !changed.lines().any(|f| files.iter().any(|c| c == f)) {
            continue;
        }
        out.push(Side {
            set: set.branch(&q.name),
            title: q.pr_title(set).unwrap_or_default(),
            body: set.body.clone().filter(|b| !b.trim().is_empty()),
            commits: git
                .run(&["log", "--reverse", "--format=%s", &range])
                .map(|s| s.lines().map(str::to_string).collect())
                .unwrap_or_default(),
            labels: set
                .labels
                .iter()
                .filter_map(|l| Some((l.clone(), run_hook(&q.trunk, hook.as_deref()?, l)?)))
                .collect(),
        });
    }
    out
}

fn read_config(trunk: &Path) -> Config {
    std::fs::read_to_string(trunk.join(CONFIG_DIR).join("context.toml"))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

/// The label goes in as an argument, never into the command's text, so no
/// label can change what runs.
fn run_hook(trunk: &Path, command: &str, label: &str) -> Option<String> {
    let out = Command::new("sh")
        .args([
            "-c",
            &command.replace("{}", "\"$1\""),
            "choros-context",
            label,
        ])
        .current_dir(trunk)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}
