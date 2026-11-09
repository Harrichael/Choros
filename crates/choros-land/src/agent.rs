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
    /// Read your user-level Claude settings too (`agent.resolver.user_claude_md`).
    pub user_claude_md: bool,
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
Bash(git rev-parse*),Bash(git ls-files*),Bash(git blame*),\
Bash(git checkout --ours*),Bash(git checkout --theirs*)",
    "--disallowedTools=Bash(git push*),Bash(git commit*),Bash(git reset*),Bash(git rebase*),Bash(git merge*),Bash(git cherry-pick*)",
];

/// `claude` (as `bin`) in the brief's folder on `session`: created, or
/// resumed.
pub fn claude_on(bin: &Path, brief: &ConflictBrief, session: &AgentSlot) -> Command {
    let mut cmd = Command::new(bin);
    cmd.current_dir(brief.dir);
    if session.fresh {
        cmd.args(["--session-id", &session.id]);
    } else {
        cmd.args(["--resume", &session.id]);
    }
    if !brief.user_claude_md {
        cmd.arg("--setting-sources=project,local");
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
        let mut cmd = claude_on(&self.bin, brief, session);
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

/// The allow rules match a command by its start, so a `cd`, `git -C` or a
/// chain is refused even where each part alone would pass.
const GIT_USE: &str = "You may and should use git: `git add` is how you hand back each \
resolved file. Run each git command on its own, from this folder: no `cd`, no `git -C`, no \
`&&`, `;` or pipes, or it is refused. Look with `git status`, `git diff`, `git log` and \
`git show` (`git show :1:FILE`, `:2:FILE`, `:3:FILE` for the base and each side); settle a \
file with your edit tools, or with `git checkout --ours FILE` / `--theirs FILE` once \
`:2:`/`:3:` show you which side is which; then `git add FILE`.";

pub fn conflict_prompt(brief: &ConflictBrief) -> String {
    let mut p = format!(
        "You are the resync agent for a Choros landing queue, working in {}.\n\
         A conflict is in progress while {}.\n\
         Conflicted files: {}.\n\n\
         Resolve every conflict so both sides' intent survives, then `git add` each file. \
         Do not commit, continue, abort, reset or push: Choros checks your result and \
         finishes the operation itself. If the right resolution is genuinely unclear, \
         say so plainly in your final message instead of guessing.\n\n{GIT_USE}\n\
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
         Do not commit, continue, abort, reset or push: Choros finishes the operation.\n\n{GIT_USE}\n",
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
            user_claude_md: true,
        };

        let old = AgentSlot { id: "old".into(), fresh: false };
        let used = claude.resolve(&brief(&old)).unwrap();
        assert!(used.fresh);
        assert_ne!(used.id, "old");

        let broken = AgentSlot { id: "broken".into(), fresh: true };
        assert!(claude.resolve(&brief(&broken)).is_err());
    }

    /// With `agent.user_claude_md = false` the agent runs without your own
    /// CLAUDE.md, whose rules are for your sessions, not its job; and its
    /// prompt says to use git.
    #[test]
    fn the_agent_can_be_kept_from_your_claude_md() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("claude");
        let args = dir.path().join("args");
        std::fs::write(&bin, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n", args.display())).unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let session = AgentSlot { id: "s".into(), fresh: true };
        let brief = ConflictBrief {
            dir: dir.path(),
            operation: "replaying landing-2 onto landing-1",
            files: &[],
            recorded: &[],
            sides: &[],
            session: &session,
            model: "opus",
            timeout: Duration::from_secs(10),
            user_claude_md: false,
        };

        Claude { bin }.resolve(&brief).unwrap();
        let args = std::fs::read_to_string(&args).unwrap();
        assert!(args.lines().any(|a| a == "--setting-sources=project,local"), "{args}");
        assert!(args.contains("You may and should use git"), "{args}");
    }
}
