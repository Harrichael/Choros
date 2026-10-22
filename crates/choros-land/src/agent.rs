//! The engine's view of Claude Code: resolve a conflict left in progress in
//! the sync clone, and fork a session into a landing folder. [`Claude`]
//! shells out to `claude`.

use std::path::Path;
use std::process::Command;

use color_eyre::eyre::{Result, eyre};

pub struct ConflictBrief<'a> {
    pub dir: &'a Path,
    /// What was being done, in a sentence: "replaying landing-4 onto main".
    pub operation: &'a str,
    pub files: &'a [String],
    /// `git show --remerge-diff` of earlier resolutions touching these files.
    pub recorded: &'a [String],
    pub session: &'a AgentSlot,
}

/// The session to use, and whether it has to be created.
pub struct AgentSlot {
    pub id: String,
    pub fresh: bool,
}

pub trait Agent {
    /// Resolve the conflict in progress in `brief.dir`, leaving every path
    /// resolved and staged. The caller verifies and concludes; the agent
    /// must not commit or push.
    fn resolve(&self, brief: &ConflictBrief) -> Result<()>;
    /// From `dir`, fork `source` into a new session with id `fork`.
    fn fork_session(&self, dir: &Path, source: &str, fork: &str, name: &str, note: &str) -> Result<()>;
}

pub struct Claude;

const ALLOWED_TOOLS: &str = "Read,Edit,Write,Grep,Glob,\
Bash(git status*),Bash(git diff*),Bash(git add*),Bash(git log*),Bash(git show*),\
Bash(git checkout --ours*),Bash(git checkout --theirs*)";

impl Agent for Claude {
    fn resolve(&self, brief: &ConflictBrief) -> Result<()> {
        let prompt = conflict_prompt(brief);
        let mut cmd = Command::new("claude");
        cmd.current_dir(brief.dir).args(["-p", "--output-format", "json"]);
        if brief.session.fresh {
            cmd.args(["--session-id", &brief.session.id]);
        } else {
            cmd.args(["--resume", &brief.session.id]);
        }
        cmd.args([
            "--permission-mode",
            "acceptEdits",
            "--allowedTools",
            ALLOWED_TOOLS,
            "--disallowedTools",
            "Bash(git push*),Bash(git commit*),Bash(git reset*),Bash(git rebase*),Bash(git merge*),Bash(git cherry-pick*)",
        ]);
        cmd.arg(prompt);
        let out = cmd.output()?;
        if !out.status.success() {
            return Err(eyre!(
                "the resync agent failed:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }

    fn fork_session(&self, dir: &Path, source: &str, fork: &str, name: &str, note: &str) -> Result<()> {
        let out = Command::new("claude")
            .current_dir(dir)
            .args(["-p", "--resume", source, "--fork-session", "--session-id", fork, "-n", name])
            .arg(note)
            .output()?;
        if !out.status.success() {
            return Err(eyre!(
                "forking session {source} failed:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }
}

pub fn conflict_prompt(brief: &ConflictBrief) -> String {
    let mut p = String::new();
    p.push_str(&format!(
        "You are the resync agent for a Choros landing queue, working in {}.\n\
         A conflict is in progress while {}.\n\
         Conflicted files: {}.\n\n\
         Resolve every conflict so both sides' intent survives, then `git add` each file. \
         Do not commit, continue, abort, reset or push: Choros checks your result and \
         finishes the operation itself. If the right resolution is genuinely unclear, \
         say so plainly in your final message instead of guessing.\n",
        brief.dir.display(),
        brief.operation,
        if brief.files.is_empty() {
            "(none reported; check `git status`)".to_string()
        } else {
            brief.files.join(", ")
        },
    ));
    if !brief.recorded.is_empty() {
        p.push_str("\nThese files conflicted before; this is how that was resolved:\n\n");
        for r in brief.recorded {
            p.push_str(r);
            p.push_str("\n\n");
        }
    }
    p
}
