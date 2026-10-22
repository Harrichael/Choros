//! What a queue is configured to do (`queue.toml`, yours to edit) and what it
//! currently holds (`state.json`, the engine's).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueConfig {
    pub repo: String,
    /// The branch landed work ends up on; read from origin when the queue is
    /// created.
    pub main: String,
    #[serde(default)]
    pub scope: Scope,
    #[serde(default)]
    pub push: PushConfig,
    #[serde(default)]
    pub pr: PrConfig,
    #[serde(default)]
    pub agent: AgentConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    #[default]
    Root,
    Workspace(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushConfig {
    pub merge_head: bool,
    pub private_sets: bool,
}

impl Default for PushConfig {
    fn default() -> Self {
        Self {
            merge_head: true,
            private_sets: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrConfig {
    pub draft: bool,
}

impl Default for PrConfig {
    fn default() -> Self {
        Self { draft: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub rotate_after: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self { rotate_after: 10 }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueueState {
    pub next_n: u32,
    pub sets: BTreeMap<u32, LandingSet>,
    #[serde(default)]
    pub merge_head: Option<MergeHead>,
    /// Set when the merge head could not be rebuilt; the previous merge head
    /// stays in place until it is resolved.
    #[serde(default)]
    pub merge_head_park: Option<Park>,
    #[serde(default)]
    pub resolutions: Vec<Resolution>,
    #[serde(default)]
    pub agent: Option<AgentSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LandingSet {
    pub n: u32,
    pub deps: Vec<Dep>,
    /// What `base_sha` was built from. Comparing specs, not SHAs, keeps the
    /// daemon from rebuilding a merge commit (new SHA every time) on every pass.
    pub base: BaseSpec,
    pub base_sha: String,
    pub tip: String,
    pub status: SetStatus,
    #[serde(default)]
    pub park: Option<Park>,
    #[serde(default)]
    pub pr: Option<PrRef>,
    /// Where `<queue>/landing-N` was on origin when we last pushed or
    /// adopted it; the lease for the next push. `None`: not on origin.
    #[serde(default)]
    pub remote: Option<String>,
    /// origin's main when the merge was observed: a base built on main from
    /// here on contains this set.
    #[serde(default)]
    pub merged_into: Option<String>,
    pub source: Source,
    pub mode: LandMode,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub sessions: Vec<SessionFork>,
    /// Forks this one was found to conflict with in the merge head and was
    /// left beside rather than behind.
    #[serde(default)]
    pub conflicts_with: Vec<u32>,
    /// Written before a rewritten tip is pushed, cleared after: if the
    /// process dies in between, the next pass sees whether the push landed
    /// and either keeps the new tip or puts the old one back.
    #[serde(default)]
    pub pending: Option<PendingPush>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingPush {
    pub old_tip: String,
    pub old_base: BaseSpec,
    pub old_base_sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dep {
    Set(u32),
    /// A landing-set in another queue, so another repo: it gates when this
    /// set's PR opens but contributes nothing to its base.
    Cross { queue: String, n: u32 },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseSpec {
    /// The main commit merged into the base, when any dependency has merged
    /// or there are none.
    pub main: Option<String>,
    /// Unmerged same-queue dependencies and the tips used.
    pub deps: Vec<(u32, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "state")]
pub enum SetStatus {
    Queued,
    Merged,
    /// Its PR was closed without merging; it and everything behind it wait.
    Closed,
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Park {
    pub op: ParkOp,
    pub files: Vec<String>,
    /// The inputs that conflicted. When they change (a dependency moved,
    /// main moved), the operation is simply tried again.
    pub on: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParkOp {
    /// Replaying the set's commits onto a new base.
    Resync,
    /// Merging several dependency tips into the set's base.
    BaseMerge,
    /// Merging main into a public set.
    MainIntoPublic,
    /// Merging the queue's leaves into the merge head.
    MergeHead,
    /// Someone rewrote the set's branch on origin rather than adding to it.
    Diverged,
    /// A merge commit was pushed to a private set's branch; its commits can
    /// no longer be replayed one by one.
    PushedMerge,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrRef {
    pub number: u64,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub workspace: PathBuf,
    pub repo: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LandMode {
    Continue,
    Archive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFork {
    pub source: String,
    pub fork: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeHead {
    pub sha: String,
    pub composition: Composition,
}

/// What a merge head contains, independent of the merge commits that
/// assemble it — so "has the merge head moved?" asks about content.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Composition {
    pub main: String,
    pub sets: Vec<(u32, String)>,
}

impl BaseSpec {
    pub fn key(&self) -> String {
        let deps: Vec<String> = self.deps.iter().map(|(n, t)| format!("{n}@{t}")).collect();
        format!("{};{}", self.main.as_deref().unwrap_or("-"), deps.join(","))
    }
}

impl Composition {
    pub fn key(&self) -> String {
        let sets: Vec<String> = self.sets.iter().map(|(n, t)| format!("{n}@{t}")).collect();
        format!("{};{}", self.main, sets.join(","))
    }
}

/// A conflict once resolved by hand or by the agent, kept so the same
/// conflict later can be shown how it was fixed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resolution {
    pub commit: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: String,
    pub runs: u32,
    #[serde(default)]
    pub previous: Vec<String>,
}

impl LandingSet {
    pub fn branch(&self, queue: &str) -> String {
        branch(queue, self.n)
    }

    pub fn is_open(&self) -> bool {
        self.status == SetStatus::Queued
    }

    pub fn is_public(&self) -> bool {
        self.pr.is_some() && self.is_open()
    }
}

pub fn branch(queue: &str, n: u32) -> String {
    format!("{queue}/landing-{n}")
}

pub fn read_config(path: &Path) -> Result<QueueConfig> {
    let body = std::fs::read_to_string(path).wrap_err_with(|| format!("reading {path:?}"))?;
    toml::from_str(&body).wrap_err_with(|| format!("parsing {path:?}"))
}

pub fn write_config(path: &Path, config: &QueueConfig) -> Result<()> {
    write_atomic(path, toml::to_string_pretty(config)?.as_bytes())
}

pub fn read_state(path: &Path) -> Result<QueueState> {
    if !path.exists() {
        return Ok(QueueState {
            next_n: 1,
            ..QueueState::default()
        });
    }
    let body = std::fs::read_to_string(path)?;
    serde_json::from_str(&body).wrap_err_with(|| format!("parsing {path:?}"))
}

pub fn write_state(path: &Path, state: &QueueState) -> Result<()> {
    write_atomic(path, serde_json::to_string_pretty(state)?.as_bytes())
}

/// Readers (the prompt token in particular) must never see half a file.
pub fn write_atomic(path: &Path, body: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
