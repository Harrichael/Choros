//! The engine's view of GitHub: open, read and merge PRs, and read the repo
//! settings that decide how merging behaves. [`Gh`] shells out to `gh`.

use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::eyre::{Result, eyre};
use serde_json::Value;

use crate::state::PrRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

#[derive(Debug, Clone)]
pub struct RepoSettings {
    /// Allowed methods, in the order Choros prefers them.
    pub methods: Vec<MergeMethod>,
    pub require_up_to_date: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

pub struct PrRequest<'a> {
    pub head: &'a str,
    pub base: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub draft: bool,
}

/// `repo` is a local clone whose `origin` names the GitHub repository.
pub trait Forge {
    fn settings(&self, repo: &Path, branch: &str) -> Result<RepoSettings>;
    fn create_pr(&self, repo: &Path, req: &PrRequest) -> Result<PrRef>;
    fn pr_state(&self, repo: &Path, number: u64) -> Result<PrState>;
    fn merge_pr(&self, repo: &Path, number: u64, method: MergeMethod) -> Result<()>;
}

pub struct Gh {
    /// `<root>/.choros-config/gh` when the root has its own account.
    pub config_dir: Option<PathBuf>,
}

impl Gh {
    fn run(&self, repo: &Path, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("gh");
        cmd.current_dir(repo).args(args);
        if let Some(dir) = &self.config_dir {
            cmd.env("GH_CONFIG_DIR", dir);
        }
        let out = cmd.output()?;
        if !out.status.success() {
            return Err(eyre!(
                "gh {} failed:\n{}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

impl Forge for Gh {
    fn settings(&self, repo: &Path, branch: &str) -> Result<RepoSettings> {
        let v: Value = serde_json::from_str(&self.run(repo, &["api", "repos/{owner}/{repo}"])?)?;
        let allowed = |k: &str| v[k].as_bool().unwrap_or(false);
        let mut methods = Vec::new();
        for (key, method) in [
            ("allow_merge_commit", MergeMethod::Merge),
            ("allow_squash_merge", MergeMethod::Squash),
            ("allow_rebase_merge", MergeMethod::Rebase),
        ] {
            if allowed(key) {
                methods.push(method);
            }
        }
        // No protection (a 404) means no up-to-date requirement.
        let protection = self
            .run(repo, &["api", &format!("repos/{{owner}}/{{repo}}/branches/{branch}/protection")])
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok());
        let require_up_to_date = protection
            .is_some_and(|p| p["required_status_checks"]["strict"].as_bool().unwrap_or(false));
        Ok(RepoSettings {
            methods,
            require_up_to_date,
        })
    }

    fn create_pr(&self, repo: &Path, req: &PrRequest) -> Result<PrRef> {
        let mut args = vec![
            "pr", "create", "--head", req.head, "--base", req.base, "--title", req.title, "--body",
            req.body,
        ];
        if req.draft {
            args.push("--draft");
        }
        let url = self.run(repo, &args)?;
        let url = url.lines().last().unwrap_or_default().to_string();
        let number = url
            .rsplit('/')
            .next()
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| eyre!("could not read a PR number from {url:?}"))?;
        Ok(PrRef { number, url })
    }

    fn pr_state(&self, repo: &Path, number: u64) -> Result<PrState> {
        let v: Value = serde_json::from_str(&self.run(
            repo,
            &["pr", "view", &number.to_string(), "--json", "state"],
        )?)?;
        Ok(match v["state"].as_str() {
            Some("MERGED") => PrState::Merged,
            Some("CLOSED") => PrState::Closed,
            _ => PrState::Open,
        })
    }

    fn merge_pr(&self, repo: &Path, number: u64, method: MergeMethod) -> Result<()> {
        let flag = match method {
            MergeMethod::Merge => "--merge",
            MergeMethod::Squash => "--squash",
            MergeMethod::Rebase => "--rebase",
        };
        self.run(repo, &["pr", "merge", &number.to_string(), flag])?;
        Ok(())
    }
}
