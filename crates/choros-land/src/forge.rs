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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

/// Where an open PR's review stands.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrReview {
    pub draft: bool,
    /// Someone besides the PR's author commented or reviewed after the
    /// author last did, or last wrote a commit. An approval with nothing
    /// written in it needs no answer, so it doesn't count.
    pub unanswered: bool,
    pub approved: bool,
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
    fn find_open_pr(&self, repo: &Path, head: &str) -> Result<Option<PrRef>>;
    fn pr_state(&self, repo: &Path, number: u64) -> Result<PrState>;
    fn pr_review(&self, repo: &Path, number: u64) -> Result<PrReview>;
    fn merge_pr(&self, repo: &Path, number: u64, method: MergeMethod) -> Result<()>;
    /// The repo's PR template, the org's default included.
    fn pr_template(&self, repo: &Path) -> Result<Option<String>>;
}

pub struct Gh {
    /// `<trunk>/.choros-config/gh` when the trunk has its own account.
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
    fn pr_template(&self, repo: &Path) -> Result<Option<String>> {
        let v: Value = serde_json::from_str(&self.run(repo, &["repo", "view", "--json", "pullRequestTemplates"])?)?;
        Ok(v["pullRequestTemplates"]
            .as_array()
            .and_then(|t| t.first())
            .and_then(|t| t["body"].as_str())
            .map(str::to_string))
    }

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
            .ok_or_else(|| eyre!("could not read a PR number from `{url}`"))?;
        Ok(PrRef { number, url })
    }

    fn find_open_pr(&self, repo: &Path, head: &str) -> Result<Option<PrRef>> {
        let v: Value = serde_json::from_str(&self.run(
            repo,
            &["pr", "list", "--head", head, "--state", "open", "--json", "number,url"],
        )?)?;
        Ok(v.as_array().and_then(|a| a.first()).and_then(|pr| {
            Some(PrRef {
                number: pr["number"].as_u64()?,
                url: pr["url"].as_str()?.to_string(),
            })
        }))
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

    fn pr_review(&self, repo: &Path, number: u64) -> Result<PrReview> {
        let v: Value = serde_json::from_str(&self.run(
            repo,
            &["pr", "view", &number.to_string(), "--json", "isDraft,author,comments,reviews,commits,reviewDecision"],
        )?)?;
        Ok(PrReview {
            draft: v["isDraft"].as_bool().unwrap_or(false),
            unanswered: unanswered(&v),
            approved: approved(&v),
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

/// Commits count by when they were written, not committed: a rebase, like a
/// re-sync, rewrites committer dates but answers no one. GitHub's timestamps
/// are all UTC in one format, so they compare as strings.
fn unanswered(pr: &Value) -> bool {
    let author = pr["author"]["login"].as_str();
    let items = |k: &str| pr[k].as_array().cloned().unwrap_or_default();
    let mut theirs: Option<String> = None;
    let mut mine: Option<String> = None;
    let mut saw = |by_author: bool, at: Option<&str>| {
        let Some(at) = at else { return };
        let last = if by_author { &mut mine } else { &mut theirs };
        if last.as_deref().is_none_or(|l| l < at) {
            *last = Some(at.to_string());
        }
    };
    let by_author = |v: &Value| author.is_some() && v["author"]["login"].as_str() == author;
    for c in items("comments") {
        if c["isMinimized"].as_bool() != Some(true) {
            saw(by_author(&c), c["createdAt"].as_str());
        }
    }
    for r in items("reviews") {
        let bare_approval = r["state"] == "APPROVED" && r["body"].as_str().unwrap_or("").trim().is_empty();
        if !bare_approval {
            saw(by_author(&r), r["submittedAt"].as_str());
        }
    }
    for c in items("commits") {
        saw(true, c["authoredDate"].as_str());
    }
    match (theirs, mine) {
        (Some(t), Some(m)) => t > m,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// GitHub decides only where the repo requires reviews; elsewhere a PR is
/// approved when some reviewer's latest verdict approves and none asks for
/// changes.
fn approved(pr: &Value) -> bool {
    match pr["reviewDecision"].as_str().unwrap_or("") {
        "" => {}
        decision => return decision == "APPROVED",
    }
    let mut verdicts: std::collections::BTreeMap<&str, &str> = Default::default();
    for r in pr["reviews"].as_array().into_iter().flatten() {
        let (Some(who), Some(state)) = (r["author"]["login"].as_str(), r["state"].as_str()) else {
            continue;
        };
        if matches!(state, "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED") {
            verdicts.insert(who, state);
        }
    }
    verdicts.values().any(|&v| v == "APPROVED") && !verdicts.values().any(|&v| v == "CHANGES_REQUESTED")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A review comes in, the author pushes a fix, the reviewer replies, the
    /// author answers in a comment. Each turn flips whose move it is. A
    /// re-sync's rebase keeps the fix's written date, so it answers no one.
    #[test]
    fn the_last_word_decides_whether_a_pr_is_unanswered() {
        let pr = |comments: Vec<Value>, reviews: Vec<Value>, commits: Vec<&str>| {
            let commits: Vec<Value> = commits
                .into_iter()
                .map(|at| json!({"authoredDate": at, "committedDate": "2026-10-09T00:00:00Z"}))
                .collect();
            json!({"author": {"login": "me"}, "comments": comments, "reviews": reviews, "commits": commits})
        };
        let by = |who: &str, at: &str| json!({"author": {"login": who}, "createdAt": at});
        let review = |who: &str, at: &str| json!({"author": {"login": who}, "submittedAt": at});

        assert!(!unanswered(&pr(vec![], vec![], vec!["2026-10-01T00:00:00Z"])), "no one has spoken");

        let reviewed = pr(vec![], vec![review("ann", "2026-10-02T00:00:00Z")], vec!["2026-10-01T00:00:00Z"]);
        assert!(unanswered(&reviewed));

        let fixed = pr(
            vec![],
            vec![review("ann", "2026-10-02T00:00:00Z")],
            vec!["2026-10-01T00:00:00Z", "2026-10-03T00:00:00Z"],
        );
        assert!(!unanswered(&fixed));

        let replied = pr(
            vec![by("ann", "2026-10-04T00:00:00Z")],
            vec![review("ann", "2026-10-02T00:00:00Z")],
            vec!["2026-10-01T00:00:00Z", "2026-10-03T00:00:00Z"],
        );
        assert!(unanswered(&replied));

        let answered = pr(
            vec![by("ann", "2026-10-04T00:00:00Z"), by("me", "2026-10-05T00:00:00Z")],
            vec![review("ann", "2026-10-02T00:00:00Z")],
            vec!["2026-10-01T00:00:00Z", "2026-10-03T00:00:00Z"],
        );
        assert!(!unanswered(&answered));

        let approved = pr(
            vec![],
            vec![json!({"author": {"login": "ann"}, "submittedAt": "2026-10-04T00:00:00Z", "state": "APPROVED", "body": ""})],
            vec!["2026-10-01T00:00:00Z"],
        );
        assert!(!unanswered(&approved), "a bare approval needs no answer");
    }

    /// Without required reviews GitHub gives no decision, so each
    /// reviewer's latest verdict stands: one asking for changes holds back
    /// another's approval until they approve too.
    #[test]
    fn a_pr_is_approved_when_no_reviewer_still_wants_changes() {
        let verdict = |who: &str, state: &str| json!({"author": {"login": who}, "state": state});
        let pr = |reviews: Vec<Value>| json!({"reviewDecision": "", "reviews": reviews});

        assert!(!approved(&pr(vec![])));
        assert!(approved(&pr(vec![verdict("ann", "APPROVED")])));
        assert!(!approved(&pr(vec![verdict("ann", "APPROVED"), verdict("bo", "CHANGES_REQUESTED")])));
        assert!(approved(&pr(vec![
            verdict("bo", "CHANGES_REQUESTED"),
            verdict("bo", "COMMENTED"),
            verdict("bo", "APPROVED"),
        ])));
        assert!(!approved(&json!({"reviewDecision": "REVIEW_REQUIRED", "reviews": [verdict("ann", "APPROVED")]})));
    }
}
