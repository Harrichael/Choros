//! Git as the engine needs it: detached-HEAD replays and merges that report
//! conflicts instead of failing, and pushes guarded by an explicit lease.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use color_eyre::eyre::{Result, eyre};

pub struct Git {
    dir: PathBuf,
    /// The sync clone is Choros's own: the user's hooks and signing setup
    /// must not run (or prompt) in the middle of an unattended replay.
    owned: bool,
}

/// The result of a step that may stop on a conflict, leaving it in progress.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Clean,
    Conflict(Vec<String>),
}

impl Git {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            owned: false,
        }
    }

    pub fn owned(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            owned: true,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.dir);
        if self.owned {
            cmd.args(["-c", "core.hooksPath=/dev/null", "-c", "commit.gpgSign=false"]);
        }
        // Never block on an editor: every message is given on the command line.
        cmd.env("GIT_EDITOR", "true");
        cmd
    }

    fn output(&self, args: &[&str]) -> Result<Output> {
        Ok(self.command().args(args).output()?)
    }

    pub fn run(&self, args: &[&str]) -> Result<String> {
        let out = self.output(args)?;
        if !out.status.success() {
            return Err(eyre!(
                "git {} failed in {:?}:\n{}",
                args.join(" "),
                self.dir,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    pub fn succeeds(&self, args: &[&str]) -> Result<bool> {
        Ok(self.output(args)?.status.success())
    }

    pub fn rev_parse(&self, rev: &str) -> Result<Option<String>> {
        let out = self.output(&["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")])?;
        Ok(out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()))
    }

    pub fn sha(&self, rev: &str) -> Result<String> {
        self.rev_parse(rev)?
            .ok_or_else(|| eyre!("no commit {rev:?} in {:?}", self.dir))
    }

    pub fn is_ancestor(&self, ancestor: &str, of: &str) -> Result<bool> {
        self.succeeds(&["merge-base", "--is-ancestor", ancestor, of])
    }

    /// Commits in `base..tip`, oldest first.
    pub fn commits(&self, base: &str, tip: &str) -> Result<Vec<String>> {
        let out = self.run(&["rev-list", "--reverse", &format!("{base}..{tip}")])?;
        Ok(out.lines().map(str::to_string).collect())
    }

    pub fn has_merges(&self, base: &str, tip: &str) -> Result<bool> {
        let out = self.run(&["rev-list", "--merges", &format!("{base}..{tip}")])?;
        Ok(!out.is_empty())
    }

    pub fn subject(&self, sha: &str) -> Result<String> {
        self.run(&["log", "-1", "--format=%s", sha])
    }

    pub fn is_clean(&self) -> Result<bool> {
        Ok(self.run(&["status", "--porcelain"])?.is_empty())
    }

    pub fn fetch(&self, remote: &str, refspecs: &[&str]) -> Result<()> {
        let mut args = vec!["fetch", "--quiet", "--prune", remote];
        args.extend_from_slice(refspecs);
        self.run(&args)?;
        Ok(())
    }

    /// Point `refs/heads/<branch>` on `remote` at `sha`, but only if it is
    /// still where we last saw it (`expect`; `None` = must not exist). A bare
    /// `--force-with-lease` right after a fetch would compare against what the
    /// fetch just saw, guarding nothing.
    pub fn push_lease(&self, remote: &str, sha: &str, branch: &str, expect: Option<&str>) -> Result<()> {
        let lease = format!("--force-with-lease=refs/heads/{branch}:{}", expect.unwrap_or(""));
        self.run(&["push", "--quiet", &lease, remote, &format!("{sha}:refs/heads/{branch}")])?;
        Ok(())
    }

    pub fn push_force(&self, remote: &str, sha: &str, branch: &str) -> Result<()> {
        self.run(&["push", "--quiet", "--force", remote, &format!("{sha}:refs/heads/{branch}")])?;
        Ok(())
    }

    pub fn delete_remote_branch(&self, remote: &str, branch: &str) -> Result<()> {
        self.run(&["push", "--quiet", remote, &format!(":refs/heads/{branch}")])?;
        Ok(())
    }

    /// Would merging `a` and `b` conflict? Answered without a worktree.
    pub fn merge_conflicts(&self, a: &str, b: &str) -> Result<Vec<String>> {
        let out = self.output(&["merge-tree", "--write-tree", "--name-only", "--no-messages", a, b])?;
        match out.status.code() {
            Some(0) => Ok(Vec::new()),
            Some(1) => Ok(String::from_utf8_lossy(&out.stdout)
                .lines()
                .skip(1)
                .take_while(|l| !l.is_empty())
                .map(str::to_string)
                .collect()),
            _ => Err(eyre!(
                "git merge-tree failed:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            )),
        }
    }

    pub fn checkout_detached(&self, rev: &str) -> Result<()> {
        self.run(&["checkout", "--quiet", "--detach", rev])?;
        Ok(())
    }

    pub fn head(&self) -> Result<String> {
        self.sha("HEAD")
    }

    pub fn cherry_pick(&self, sha: &str) -> Result<Step> {
        // --keep-redundant-commits: a commit that became empty on its new
        // base still belongs to the set's history; dropping it would make
        // the set's own-commit list lie about what was landed.
        self.step(&["cherry-pick", "--allow-empty", "--keep-redundant-commits", sha])
    }

    pub fn merge(&self, rev: &str, message: &str) -> Result<Step> {
        self.step(&["merge", "--no-ff", "-m", message, rev])
    }

    fn step(&self, args: &[&str]) -> Result<Step> {
        let out = self.output(args)?;
        if out.status.success() {
            return Ok(Step::Clean);
        }
        let unmerged = self.unmerged()?;
        if unmerged.is_empty() {
            // rerere.autoUpdate may have resolved and staged every path.
            if self.in_progress()?.is_some() {
                return Ok(Step::Conflict(Vec::new()));
            }
            return Err(eyre!(
                "git {} failed in {:?}:\n{}",
                args.join(" "),
                self.dir,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(Step::Conflict(unmerged))
    }

    pub fn unmerged(&self) -> Result<Vec<String>> {
        let out = self.run(&["diff", "--name-only", "--diff-filter=U"])?;
        Ok(out.lines().map(str::to_string).collect())
    }

    /// Is the conflict in progress fully and plausibly resolved: nothing
    /// unmerged, no conflict markers left in what is staged.
    pub fn resolution_problems(&self) -> Result<Option<String>> {
        let unmerged = self.unmerged()?;
        if !unmerged.is_empty() {
            return Ok(Some(format!("still unmerged: {}", unmerged.join(", "))));
        }
        self.run(&["add", "--update"])?;
        let out = self.output(&["diff", "--cached", "--check"])?;
        if !out.status.success() {
            return Ok(Some(format!(
                "conflict markers or whitespace errors left:\n{}",
                String::from_utf8_lossy(&out.stdout).trim()
            )));
        }
        Ok(None)
    }

    /// Finish the cherry-pick or merge whose conflict was just resolved.
    pub fn conclude(&self) -> Result<()> {
        match self.in_progress()? {
            Some(InProgress::CherryPick) => {
                self.run(&["cherry-pick", "--continue"])?;
            }
            Some(InProgress::Merge) => {
                self.run(&["commit", "--no-edit"])?;
            }
            Some(InProgress::Rebase) => {
                self.run(&["rebase", "--continue"])?;
            }
            None => return Err(eyre!("nothing in progress to conclude in {:?}", self.dir)),
        }
        Ok(())
    }

    pub fn in_progress(&self) -> Result<Option<InProgress>> {
        let git_dir = PathBuf::from(self.run(&["rev-parse", "--absolute-git-dir"])?);
        Ok(if git_dir.join("CHERRY_PICK_HEAD").exists() {
            Some(InProgress::CherryPick)
        } else if git_dir.join("MERGE_HEAD").exists() {
            Some(InProgress::Merge)
        } else if git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists() {
            Some(InProgress::Rebase)
        } else {
            None
        })
    }

    pub fn abort_in_progress(&self) -> Result<()> {
        match self.in_progress()? {
            Some(InProgress::CherryPick) => self.run(&["cherry-pick", "--abort"])?,
            Some(InProgress::Merge) => self.run(&["merge", "--abort"])?,
            Some(InProgress::Rebase) => self.run(&["rebase", "--abort"])?,
            None => return Ok(()),
        };
        Ok(())
    }

    pub fn reset_hard(&self, rev: &str) -> Result<()> {
        self.run(&["reset", "--quiet", "--hard", rev])?;
        Ok(())
    }

    pub fn remerge_diff(&self, merge: &str) -> Result<String> {
        self.run(&["show", "--remerge-diff", "--format=%H %s", merge])
    }

    pub fn changed_files(&self, merge: &str) -> Result<Vec<String>> {
        let out = self.run(&["show", "--remerge-diff", "--name-only", "--format=", merge])?;
        Ok(out.lines().filter(|l| !l.is_empty()).map(str::to_string).collect())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InProgress {
    CherryPick,
    Merge,
    Rebase,
}
