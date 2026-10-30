//! What a queue is configured to do (`queue.toml`, yours to edit) and what it
//! currently holds (`state.json`, the engine's).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueConfig {
    /// Queues made when a queue held one repo name it here; their sets are
    /// in it, and `#N` in their diffbase means it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Such a queue's branch to land on. Others use each repo's default
    /// branch on origin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main: Option<String>,
    #[serde(default)]
    pub scope: Scope,
    #[serde(default)]
    pub push: PushConfig,
    #[serde(default)]
    pub pr: PrConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub stack: StackConfig,
    /// PRs the queue builds on before they reach main, as `repo#N`. Empty:
    /// the queue builds on main.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diffbase: Vec<String>,
}

/// A PR in a queue's diffbase.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PrSpec {
    pub repo: String,
    pub number: u64,
}

impl PrSpec {
    /// `repo#N`, or `#N` for `own` (a one-repo queue's repo).
    pub fn parse(s: &str, own: Option<&str>) -> Result<PrSpec> {
        let bad = || color_eyre::eyre::eyre!("`{s}` is not a PR; write repo#N");
        let (repo, n) = s.rsplit_once('#').ok_or_else(bad)?;
        let number = n.parse().map_err(|_| bad())?;
        let repo = match (repo, own) {
            ("", Some(own)) => own,
            ("", None) => return Err(bad()),
            (repo, _) => repo,
        };
        Ok(PrSpec {
            repo: repo.to_string(),
            number,
        })
    }
}

/// PRs written as a list, by commas or spaces: `app#40,41 lib#7`. A bare
/// number is in the repo named before it.
pub fn parse_pr_list(s: &str, own: Option<&str>) -> Result<Vec<PrSpec>> {
    let mut out: Vec<PrSpec> = Vec::new();
    for item in s.split([' ', ',']).filter(|i| !i.is_empty()) {
        let spec = match (item.parse::<u64>(), out.last()) {
            (Ok(number), Some(last)) => PrSpec {
                repo: last.repo.clone(),
                number,
            },
            _ => PrSpec::parse(item, own)?,
        };
        out.push(spec);
    }
    Ok(out)
}

impl std::fmt::Display for PrSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.repo, self.number)
    }
}

impl QueueConfig {
    /// A queue for `scope`, with every setting at its default.
    pub fn new(scope: Scope) -> QueueConfig {
        QueueConfig {
            repo: None,
            main: None,
            scope,
            push: Default::default(),
            pr: Default::default(),
            agent: Default::default(),
            stack: Default::default(),
            diffbase: Vec::new(),
        }
    }

    pub fn diffbase(&self) -> Result<Vec<PrSpec>> {
        self.diffbase
            .iter()
            .map(|s| PrSpec::parse(s, self.repo.as_deref()))
            .collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    #[default]
    #[serde(alias = "root")]
    Trunk,
    /// One workspace, by its path below the trunk.
    Workspace(String),
    /// Every workspace in a branch-space and below it, by the branch-space's
    /// path below the trunk.
    Branch(String),
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scope::Trunk => write!(f, "trunk"),
            Scope::Workspace(p) => write!(f, "workspace {p}"),
            Scope::Branch(p) => write!(f, "branch {p}"),
        }
    }
}

impl Scope {
    /// May the workspace at `ws_rel` (its path below the trunk) land here?
    pub fn admits(&self, ws_rel: &str) -> bool {
        match self {
            Scope::Trunk => true,
            Scope::Workspace(only) => only == ws_rel,
            Scope::Branch(b) => ws_rel.strip_prefix(b.as_str()).is_some_and(|r| r.starts_with('/')),
        }
    }

    /// May a workspace made in the space at `space_rel` land here?
    pub fn admits_space(&self, space_rel: &str) -> bool {
        match self {
            Scope::Trunk => true,
            Scope::Workspace(_) => false,
            Scope::Branch(b) => {
                space_rel == b || space_rel.strip_prefix(b.as_str()).is_some_and(|r| r.starts_with('/'))
            }
        }
    }
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

/// GitHub stacked PRs for sets behind the front (see the design doc's
/// *Stacked PRs*).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackConfig {
    pub enabled: bool,
    /// Stacking stops once a repo has this many open PRs in the queue;
    /// front PRs open regardless.
    pub max_open: u32,
}

impl Default for StackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_open: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub rotate_after: u32,
    /// Always passed to `claude`, so the resync agent never runs on
    /// whatever model `claude` happens to default to.
    #[serde(default = "default_model")]
    pub model: String,
    /// The daemon hands conflicts to the agent itself, within the limits
    /// below, rather than parking them for `choros queue resolve`.
    #[serde(default = "yes")]
    pub auto_resolve: bool,
    /// How long one run may take before it's stopped.
    #[serde(default = "default_timeout")]
    pub timeout_minutes: u32,
    /// How many runs the daemon may start in a day (UTC).
    #[serde(default = "default_daily_runs")]
    pub daily_runs: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            rotate_after: 10,
            model: default_model(),
            auto_resolve: true,
            timeout_minutes: default_timeout(),
            daily_runs: default_daily_runs(),
        }
    }
}

fn default_model() -> String {
    "opus".into()
}

fn yes() -> bool {
    true
}

fn default_timeout() -> u32 {
    20
}

fn default_daily_runs() -> u32 {
    20
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueueState {
    pub next_n: u32,
    pub sets: BTreeMap<u32, LandingSet>,
    /// Each repo something has landed into, by registry name.
    #[serde(default)]
    pub repos: BTreeMap<String, RepoState>,
    #[serde(default)]
    pub diffbase: DiffbaseState,
    /// From a one-repo queue's state.json; `read_state` moves them into
    /// `repos`.
    #[serde(default, skip_serializing)]
    merge_head: Option<MergeHead>,
    #[serde(default, skip_serializing)]
    resolutions: Vec<Resolution>,
    #[serde(default, skip_serializing)]
    agent: Option<AgentSession>,
}

/// What the queue keeps per repo: each has its own origin, sync clone and
/// merge head.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoState {
    /// The branch its work lands on.
    pub main: String,
    #[serde(default)]
    pub merge_head: Option<MergeHead>,
    #[serde(default)]
    pub resolutions: Vec<Resolution>,
    /// The resync agent works in this repo's sync clone, so it has a session
    /// of its own here.
    #[serde(default)]
    pub agent: Option<AgentSession>,
    /// Main with this repo's still-open listed PRs merged in, in list order:
    /// where its sets' bases start. `None`: main itself.
    #[serde(default)]
    pub diffbase: Option<BuiltDiffbase>,
}

/// What the engine knows of the queue's diffbase.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiffbaseState {
    /// Each listed PR as last seen, by `repo#N`.
    #[serde(default)]
    pub prs: BTreeMap<String, SeenPr>,
    /// Nothing in the queue moves while this is set.
    #[serde(default)]
    pub park: Option<DiffbasePark>,
    /// A one-repo queue's built diffbase; `read_state` moves it into `repos`.
    #[serde(default, skip_serializing)]
    built: Option<BuiltDiffbase>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeenPr {
    pub state: crate::forge::PrState,
    /// The head fetched, for a PR in a repo the queue has.
    #[serde(default)]
    pub head: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuiltDiffbase {
    pub main: String,
    /// `repo#N` and the head merged, in order.
    pub heads: Vec<(String, String)>,
    pub sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffbasePark {
    /// The PR at fault.
    pub pr: String,
    /// `None`: it was closed without merging. Otherwise it conflicts with
    /// main and the PRs listed before it (`onto`) in these files.
    pub conflict: Option<DiffbaseConflict>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffbaseConflict {
    pub onto: Vec<String>,
    pub files: Vec<String>,
    /// The inputs that conflicted; when they change, the build is retried.
    pub on: String,
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
    /// The PR's review, as of the last pass that saw it open.
    #[serde(default)]
    pub review: Option<crate::forge::PrReview>,
    /// The branch the PR is based on, when not main: the set below it in a
    /// stack. Cleared once the PR is on main.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_base: Option<String>,
    /// The GitHub stack its PR is in, as of the last pass that looked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<u64>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
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
    /// A set in this queue. In the same repo it is part of this set's base;
    /// in another it only gates when this set's PR opens.
    Set(u32),
    /// A landing-set in another queue: it gates when this set's PR opens but
    /// contributes nothing to its base.
    Cross { queue: String, n: u32 },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseSpec {
    /// The main commit merged into the base, when any dependency has merged
    /// or there are none.
    pub main: Option<String>,
    /// Unmerged same-repo dependencies and the tips used.
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

impl std::fmt::Display for SetStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SetStatus::Queued => "queued",
            SetStatus::Merged => "merged",
            SetStatus::Closed => "closed",
            SetStatus::Dropped => "dropped",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Park {
    pub op: ParkOp,
    pub files: Vec<String>,
    /// The inputs that conflicted. When they change (a dependency moved,
    /// main moved), the operation is simply tried again.
    pub on: String,
    /// Someone, or the agent, already had a go at it. One parked where no
    /// one could (a land, say) still gets the daemon's try.
    #[serde(default)]
    pub tried: bool,
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

    pub fn repo(&self) -> &str {
        &self.source.repo
    }

    pub fn is_open(&self) -> bool {
        self.status == SetStatus::Queued
    }
}

pub fn branch(queue: &str, n: u32) -> String {
    format!("{queue}/landing-{n}")
}

pub fn read_config(path: &Path) -> Result<QueueConfig> {
    let body = std::fs::read_to_string(path).wrap_err_with(|| format!("reading {}", path.display()))?;
    toml::from_str(&body).wrap_err_with(|| format!("parsing {}", path.display()))
}

pub fn write_config(path: &Path, config: &QueueConfig) -> Result<()> {
    write_atomic(path, toml::to_string_pretty(config)?.as_bytes())
}

/// `config` says, for a queue made when queues held one repo, which repo
/// its old per-queue fields belong to.
pub fn read_state(path: &Path, config: &QueueConfig) -> Result<QueueState> {
    if !path.exists() {
        return Ok(QueueState {
            next_n: 1,
            ..QueueState::default()
        });
    }
    let body = std::fs::read_to_string(path)?;
    let mut st: QueueState = serde_json::from_str(&body).wrap_err_with(|| format!("parsing {}", path.display()))?;
    if let (Some(repo), Some(main)) = (&config.repo, &config.main)
        && !st.repos.contains_key(repo)
    {
        st.repos.insert(
            repo.clone(),
            RepoState {
                main: main.clone(),
                merge_head: st.merge_head.take(),
                resolutions: std::mem::take(&mut st.resolutions),
                agent: st.agent.take(),
                diffbase: st.diffbase.built.take(),
            },
        );
    }
    Ok(st)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pr_list_carries_its_repo_to_bare_numbers() {
        let prs: Vec<String> = parse_pr_list("app#40,41 lib#7, 8", None)
            .unwrap()
            .iter()
            .map(PrSpec::to_string)
            .collect();
        assert_eq!(prs, ["app#40", "app#41", "lib#7", "lib#8"]);
        assert!(parse_pr_list("41", None).is_err(), "no repo to carry");
    }
}
