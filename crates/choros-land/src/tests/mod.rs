//! Stories about the landing queue, told against real git: a bare repo as
//! origin, a choros trunk with a registry, workspaces made the way `choros
//! work` makes them. GitHub is a fake that performs real merges on origin;
//! conflicts go to a scripted handler.

mod branches;
mod diffbase;
mod regressions;
mod stories;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, Once};

use color_eyre::eyre::{Result, eyre};

use crate::agent::{Agent, ConflictBrief};
use crate::engine::{Conflict, ConflictHandler, Env, Handled, OnConflict, PassReport};
use crate::forge::{Forge, MergeMethod, PrRequest, PrState, RepoSettings};
use crate::land::{LandOutcome, LandRequest};
use crate::queue::Queue;
use crate::state::{Dep, LandMode, PrRef, QueueState, Scope};

static GIT_ENV: Once = Once::new();

/// Tests must not see the machine's git config (signing, hooks, a default
/// branch other than main) and need an identity to commit with.
fn isolate_git() {
    GIT_ENV.call_once(|| {
        let cfg = std::env::temp_dir().join(format!("choros-land-tests-{}.gitconfig", std::process::id()));
        std::fs::write(
            &cfg,
            "[init]\n\tdefaultBranch = main\n[user]\n\tname = Test\n\temail = test@example.test\n[advice]\n\tdetachedHead = false\n",
        )
        .unwrap();
        // SAFETY: set once, before any test spawns git, behind the Once.
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", &cfg);
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        }
    });
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {} in {dir:?}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[derive(Default)]
struct FakePr {
    head: String,
    title: String,
    body: String,
    draft: bool,
    state: Option<PrState>,
}

/// GitHub, minus the network: PRs live in memory, and merging one really
/// merges its branch into origin's main.
pub struct FakeForge {
    origin: PathBuf,
    prs: Mutex<BTreeMap<u64, FakePr>>,
    pub require_up_to_date: Mutex<bool>,
}

impl FakeForge {
    pub fn pr_for(&self, branch: &str) -> Option<u64> {
        self.prs
            .lock()
            .unwrap()
            .iter()
            .find(|(_, p)| p.head == branch)
            .map(|(n, _)| *n)
    }

    pub fn pr_text(&self, number: u64) -> (String, String, bool) {
        let prs = self.prs.lock().unwrap();
        let p = &prs[&number];
        (p.title.clone(), p.body.clone(), p.draft)
    }

    /// Press the merge button.
    pub fn merge(&self, number: u64, method: MergeMethod) {
        let head = self.prs.lock().unwrap()[&number].head.clone();
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("w");
        git(tmp.path(), &["clone", "--quiet", self.origin.to_str().unwrap(), "w"]);
        match method {
            MergeMethod::Squash => {
                git(&work, &["merge", "--squash", &format!("origin/{head}")]);
                git(&work, &["commit", "--quiet", "-m", &format!("Squashed #{number}")]);
            }
            MergeMethod::Merge => {
                git(&work, &["merge", "--no-ff", "-m", &format!("Merge #{number}"), &format!("origin/{head}")]);
            }
            MergeMethod::Rebase => panic!("not used"),
        }
        git(&work, &["push", "--quiet", "origin", "main"]);
        self.prs.lock().unwrap().get_mut(&number).unwrap().state = Some(PrState::Merged);
    }

    pub fn reopen(&self, number: u64) {
        self.prs.lock().unwrap().get_mut(&number).unwrap().state = Some(PrState::Open);
    }

    pub fn close(&self, number: u64) {
        self.prs.lock().unwrap().get_mut(&number).unwrap().state = Some(PrState::Closed);
    }
}

impl Forge for FakeForge {
    fn settings(&self, _repo: &Path, _branch: &str) -> Result<RepoSettings> {
        Ok(RepoSettings {
            methods: vec![MergeMethod::Merge, MergeMethod::Squash],
            require_up_to_date: *self.require_up_to_date.lock().unwrap(),
        })
    }

    fn create_pr(&self, repo: &Path, req: &PrRequest) -> Result<PrRef> {
        let exists = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["ls-remote", "--exit-code", "--heads", "origin", req.head])
            .output()?
            .status
            .success();
        if !exists {
            return Err(eyre!("GitHub refuses a PR for a branch it does not have: {}", req.head));
        }
        let mut prs = self.prs.lock().unwrap();
        // As GitHub does: one open PR per head branch.
        if prs.values().any(|p| p.head == req.head && p.state == Some(PrState::Open)) {
            return Err(eyre!("a pull request for branch {:?} already exists", req.head));
        }
        let number = prs.len() as u64 + 1;
        prs.insert(
            number,
            FakePr {
                head: req.head.to_string(),
                title: req.title.to_string(),
                body: req.body.to_string(),
                draft: req.draft,
                state: Some(PrState::Open),
            },
        );
        Ok(PrRef {
            number,
            url: format!("https://github.test/pull/{number}"),
        })
    }

    fn find_open_pr(&self, _repo: &Path, head: &str) -> Result<Option<PrRef>> {
        Ok(self
            .prs
            .lock()
            .unwrap()
            .iter()
            .find(|(_, p)| p.head == head && p.state == Some(PrState::Open))
            .map(|(n, _)| PrRef {
                number: *n,
                url: format!("https://github.test/pull/{n}"),
            }))
    }

    fn pr_state(&self, _repo: &Path, number: u64) -> Result<PrState> {
        let prs = self.prs.lock().unwrap();
        let pr = prs.get(&number).ok_or_else(|| eyre!("no PR #{number}"))?;
        let state = pr.state.clone().unwrap();
        // As GitHub does: an open PR's head is kept at refs/pull/N/head.
        if state == PrState::Open {
            git(
                &self.origin,
                &["update-ref", &format!("refs/pull/{number}/head"), &format!("refs/heads/{}", pr.head)],
            );
        }
        Ok(state)
    }

    fn merge_pr(&self, _repo: &Path, number: u64, method: MergeMethod) -> Result<()> {
        self.merge(number, method);
        Ok(())
    }
}

/// Claude Code, minus Claude: forks are recorded, never run.
#[derive(Default)]
pub struct FakeAgent {
    pub forks: Mutex<Vec<(PathBuf, String, String)>>,
}

impl Agent for FakeAgent {
    fn resolve(&self, _brief: &ConflictBrief) -> Result<()> {
        Err(eyre!("tests resolve through the handler"))
    }

    fn fork_session(&self, dir: &Path, source: &str, fork: &str, _name: &str, _note: &str) -> Result<()> {
        self.forks
            .lock()
            .unwrap()
            .push((dir.to_path_buf(), source.to_string(), fork.to_string()));
        Ok(())
    }
}

/// Resolves every conflicted file by writing the scripted content, and
/// remembers what it was shown.
#[derive(Default)]
pub struct Scripted {
    pub content: BTreeMap<String, String>,
    pub fork: Option<OnConflict>,
    pub seen: Vec<SeenConflict>,
}

pub struct SeenConflict {
    pub files: Vec<String>,
    pub recorded: usize,
}

impl ConflictHandler for Scripted {
    fn resolve(&mut self, c: &Conflict) -> Result<Handled> {
        for f in &c.files {
            let body = self.content.get(f).cloned().unwrap_or_else(|| "resolved\n".into());
            std::fs::write(c.dir.join(f), body)?;
            git(c.dir, &["add", f]);
        }
        self.seen.push(SeenConflict {
            files: c.files.clone(),
            recorded: c.recorded.len(),
        });
        Ok(Handled::Resolved { used_session: false })
    }

    fn fork_conflict(&mut self, _fork: u32, _with: &[u32]) -> Result<Option<OnConflict>> {
        Ok(self.fork)
    }
}

pub struct World {
    _tmp: tempfile::TempDir,
    pub trunk: PathBuf,
    pub origin: PathBuf,
    pub forge: FakeForge,
    pub agent: FakeAgent,
}

pub const REPO: &str = "app";
pub const QUEUE: &str = "appq";

impl World {
    /// A trunk whose registry holds `app` (one commit on main), with a queue
    /// `appq` for it.
    pub fn new() -> World {
        isolate_git();
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let origin = base.join("origin.git");
        git(&base, &["init", "--quiet", "--bare", "-b", "main", "origin.git"]);
        let seed = base.join("seed");
        git(&base, &["clone", "--quiet", origin.to_str().unwrap(), "seed"]);
        std::fs::write(seed.join("README"), "app\n").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "--quiet", "-m", "init"]);
        git(&seed, &["push", "--quiet", "origin", "main"]);

        let trunk = base.join("trunk");
        let registry = trunk.join(".choros-config/registry");
        std::fs::create_dir_all(&registry).unwrap();
        git(&registry, &["clone", "--quiet", origin.to_str().unwrap(), REPO]);
        Queue::create(&trunk, QUEUE, REPO, Scope::Trunk).unwrap();
        World {
            _tmp: tmp,
            forge: FakeForge {
                origin: origin.clone(),
                prs: Mutex::new(BTreeMap::new()),
                require_up_to_date: Mutex::new(false),
            },
            agent: FakeAgent::default(),
            trunk,
            origin,
        }
    }

    pub fn env(&self) -> Env<'_> {
        Env {
            forge: &self.forge,
            agent: &self.agent,
            home: None,
        }
    }

    pub fn queue(&self) -> Queue {
        Queue::open(&self.trunk, QUEUE).unwrap()
    }

    pub fn state(&self) -> QueueState {
        self.queue().read_state().unwrap()
    }

    /// A workspace with `app` bound to the queue: its branch starts on the
    /// merge head.
    pub fn workspace(&self, name: &str) -> PathBuf {
        choros_core::choros::create(&self.trunk, name, &[REPO.to_string()], &()).unwrap();
        let ws = self.trunk.join(name);
        crate::land::bind(&self.env(), &ws, REPO, Some(QUEUE)).unwrap();
        ws
    }

    pub fn commit(&self, ws: &Path, file: &str, content: &str) -> String {
        let repo = ws.join(REPO);
        std::fs::write(repo.join(file), content).unwrap();
        git(&repo, &["add", file]);
        git(&repo, &["commit", "--quiet", "-m", &format!("write {file}")]);
        git(&repo, &["rev-parse", "HEAD"])
    }

    pub fn land(&self, ws: &Path) -> Result<LandOutcome> {
        self.land_with(ws, None, None, None)
    }

    pub fn land_with(
        &self,
        ws: &Path,
        fork: Option<Vec<Dep>>,
        on_conflict: Option<OnConflict>,
        handler: Option<&mut Scripted>,
    ) -> Result<LandOutcome> {
        let req = LandRequest {
            cwd: ws.join(REPO),
            mode: LandMode::Continue,
            fork,
            on_conflict,
            title: None,
            body: None,
            sessions: vec!["source-session".into()],
            env_session: None,
        };
        crate::land::land(
            &self.env(),
            &req,
            handler.map(|h| h as &mut (dyn ConflictHandler + 'static)),
        )
    }

    pub fn pass(&self) -> PassReport {
        crate::admin::sync(&self.env(), &self.queue(), None).unwrap()
    }

    pub fn resolve(&self, handler: &mut Scripted) -> PassReport {
        crate::admin::sync(&self.env(), &self.queue(), Some(handler)).unwrap()
    }

    /// A file's content at a commit, read from the sync clone.
    pub fn file_at(&self, rev: &str, file: &str) -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(self.queue().sync_dir())
            .args(["show", &format!("{rev}:{file}")])
            .output()
            .unwrap();
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).to_string())
    }

    pub fn origin_main(&self) -> String {
        git(&self.origin, &["rev-parse", "main"])
    }

    /// Someone else's commit, straight onto main.
    pub fn push_to_main(&self, file: &str, content: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("w");
        git(tmp.path(), &["clone", "--quiet", self.origin.to_str().unwrap(), "w"]);
        std::fs::write(work.join(file), content).unwrap();
        git(&work, &["add", file]);
        git(&work, &["commit", "--quiet", "-m", &format!("main writes {file}")]);
        git(&work, &["push", "--quiet", "origin", "main"]);
    }

    /// Commits a set has of its own, on top of its base.
    pub fn own_commits(&self, n: u32) -> usize {
        let st = self.state();
        let set = &st.sets[&n];
        git(&self.queue().sync_dir(), &["rev-list", "--count", &format!("{}..{}", set.base_sha, set.tip)])
            .parse()
            .unwrap()
    }
}
