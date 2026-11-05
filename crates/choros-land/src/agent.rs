//! The engine's view of Claude Code: resolve a conflict left in progress in
//! the sync clone, and fork a session into a landing folder. [`Claude`]
//! shells out to `claude`.

use std::path::Path;
use std::process::Command;

use choros_core::process::output_within;
use color_eyre::eyre::{Result, eyre};

pub struct ConflictBrief<'a> {
    pub dir: &'a Path,
    /// What was being done, in a sentence: "replaying landing-4 onto main".
    pub operation: &'a str,
    pub files: &'a [String],
    /// `git show --remerge-diff` of earlier resolutions touching these files.
    pub recorded: &'a [String],
    pub sides: &'a [crate::context::Side],
    pub session: &'a AgentSlot,
    pub model: &'a str,
    /// Past this the run is stopped and counts as failed.
    pub timeout: std::time::Duration,
}

/// The session to use, and whether it has to be created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSlot {
    pub id: String,
    pub fresh: bool,
}

pub trait Agent {
    /// Resolve the conflict in progress in `brief.dir`, leaving every path
    /// resolved and staged. The caller verifies and concludes; the agent
    /// must not commit or push. The session it ran in: `brief.session`, or
    /// a fresh one when resuming that failed.
    fn resolve(&self, brief: &ConflictBrief) -> Result<AgentSlot>;
    /// From `dir`, fork `source` into a new session with id `fork`.
    fn fork_session(&self, dir: &Path, source: &str, fork: &str, name: &str, note: &str) -> Result<()>;
}

pub struct Claude {
    pub bin: std::path::PathBuf,
}

impl Default for Claude {
    fn default() -> Claude {
        Claude { bin: "claude".into() }
    }
}

/// What the resync agent may do, whether run by Choros or driven by you:
/// edit and stage, never commit, push or rewrite. The tool flags take
/// several values: written apart, they would swallow the prompt after them.
pub const TOOL_ARGS: [&str; 4] = [
    "--permission-mode",
    "acceptEdits",
    "--allowedTools=Read,Edit,Write,Grep,Glob,\
Bash(git status*),Bash(git diff*),Bash(git add*),Bash(git log*),Bash(git show*),\
Bash(git checkout --ours*),Bash(git checkout --theirs*)",
    "--disallowedTools=Bash(git push*),Bash(git commit*),Bash(git reset*),Bash(git rebase*),Bash(git merge*),Bash(git cherry-pick*)",
];

/// `claude` (as `bin`) in `dir` on `session`: created, or resumed.
pub fn claude_on(bin: &Path, dir: &Path, session: &AgentSlot) -> Command {
    let mut cmd = Command::new(bin);
    cmd.current_dir(dir);
    if session.fresh {
        cmd.args(["--session-id", &session.id]);
    } else {
        cmd.args(["--resume", &session.id]);
    }
    cmd
}

/// A session to start in place of `session`, whose resume failed.
fn fresh_instead(session: &AgentSlot) -> Option<AgentSlot> {
    (!session.fresh).then(|| AgentSlot {
        id: crate::uuid::v4(),
        fresh: true,
    })
}

impl Claude {
    fn run(&self, brief: &ConflictBrief, session: &AgentSlot) -> Result<std::process::Output> {
        let mut cmd = claude_on(&self.bin, brief.dir, session);
        cmd.args(["-p", "--output-format", "json", "--model", brief.model]);
        cmd.args(TOOL_ARGS);
        cmd.arg(conflict_prompt(brief));
        output_within(cmd, brief.timeout)?.ok_or_else(|| {
            eyre!("the resync agent ran past {} minutes and was stopped", brief.timeout.as_secs() / 60)
        })
    }
}

impl Agent for Claude {
    fn resolve(&self, brief: &ConflictBrief) -> Result<AgentSlot> {
        let mut session = brief.session.clone();
        let mut out = self.run(brief, &session)?;
        // A session can stop resuming (a Claude Code fault has done it); one
        // that did must not keep every conflict from the agent until it
        // rotates.
        if !out.status.success()
            && let Some(fresh) = fresh_instead(&session)
        {
            session = fresh;
            out = self.run(brief, &session)?;
        }
        if !out.status.success() {
            return Err(eyre!(
                "the resync agent failed:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(session)
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
    let mut p = format!(
        "You are the resync agent for a Choros landing queue, working in {}.\n\
         A conflict is in progress while {}.\n\
         Conflicted files: {}.\n\n\
         Resolve every conflict so both sides' intent survives, then `git add` each file. \
         Do not commit, continue, abort, reset or push: Choros checks your result and \
         finishes the operation itself. If the right resolution is genuinely unclear, \
         say so plainly in your final message instead of guessing.\n\
         CLAUDE.md here holds notes from earlier resync sessions on this queue (it is \
         not part of the repo); add to it anything the next session should know.\n",
        brief.dir.display(),
        brief.operation,
        files(brief),
    );
    push_context(&mut p, brief);
    p
}

/// For someone resolving by hand in the resync agent's session.
pub fn driven_conflict_prompt(brief: &ConflictBrief) -> String {
    let mut p = format!(
        "A conflict is in progress here while {}.\n\
         Conflicted files: {}.\n\n\
         Help me resolve it so both sides' intent survives, then `git add` each file. \
         Do not commit, continue, abort, reset or push: Choros finishes the operation.\n",
        brief.operation,
        files(brief),
    );
    push_context(&mut p, brief);
    p
}

fn files(brief: &ConflictBrief) -> String {
    if brief.files.is_empty() {
        "(none reported; check `git status`)".to_string()
    } else {
        brief.files.join(", ")
    }
}

fn push_context(p: &mut String, brief: &ConflictBrief) {
    if !brief.sides.is_empty() {
        p.push_str("\nThe queued sets whose changes meet in these files, and what each is for:\n");
        for s in brief.sides {
            p.push_str(&format!("\n## {}: {}\n", s.set, s.title));
            if let Some(body) = &s.body {
                p.push_str(&format!("\n{body}\n"));
            }
            if !s.commits.is_empty() {
                p.push_str("\nCommits:\n");
                for c in &s.commits {
                    p.push_str(&format!("- {c}\n"));
                }
            }
            for (label, text) in &s.labels {
                p.push_str(&format!("\nFrom its label {label}:\n{text}\n"));
            }
        }
    }
    if !brief.recorded.is_empty() {
        p.push_str("\nThese files conflicted before; this is how that was resolved:\n\n");
        for r in brief.recorded {
            p.push_str(r);
            p.push_str("\n\n");
        }
    }
    p.push_str(
        "\nThe goal: get the front of the queue up as (draft) PRs. Once this resolves, \
         Choros pushes the set and opens or updates its PR.\n\n\
         Glossary:\n\
         - queue: where landed work waits to merge, each set on top of the sets it builds on.\n\
         - landing-set (landing-N): one land's commits from one repo; it becomes one PR.\n\
         - front: the sets whose dependencies have all merged; they get PRs.\n",
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// `claude -p --resume` can fail on a session it made (a Claude Code
    /// fault, seen in the wild). The agent then starts a fresh session and
    /// says which, so that one is recorded to resume next time. A fresh
    /// session that fails is not tried again.
    #[test]
    fn a_session_that_wont_resume_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("claude");
        std::fs::write(
            &bin,
            "#!/bin/sh\ncase \"$*\" in\n\
             *--resume*) echo 'No deferred tool marker found in the resumed session.' >&2; exit 1 ;;\n\
             *--session-id\\ broken*) exit 1 ;;\n\
             esac\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let claude = Claude { bin };
        let brief = |session| ConflictBrief {
            dir: dir.path(),
            operation: "replaying landing-2 onto landing-1",
            files: &[],
            recorded: &[],
            sides: &[],
            session,
            model: "opus",
            timeout: Duration::from_secs(10),
        };

        let old = AgentSlot { id: "old".into(), fresh: false };
        let used = claude.resolve(&brief(&old)).unwrap();
        assert!(used.fresh);
        assert_ne!(used.id, "old");

        let broken = AgentSlot { id: "broken".into(), fresh: true };
        assert!(claude.resolve(&brief(&broken)).is_err());
    }
}
