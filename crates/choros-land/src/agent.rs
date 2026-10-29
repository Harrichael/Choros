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
    pub sides: &'a [crate::context::Side],
    pub session: &'a AgentSlot,
    pub model: &'a str,
    /// Past this the run is stopped and counts as failed.
    pub timeout: std::time::Duration,
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
        cmd.current_dir(brief.dir)
            .args(["-p", "--output-format", "json", "--model", brief.model]);
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
        let out = output_within(cmd, brief.timeout)?.ok_or_else(|| {
            eyre!("the resync agent ran past {} minutes and was stopped", brief.timeout.as_secs() / 60)
        })?;
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

/// `cmd`'s output, or `None` when it ran past `limit` and was killed.
pub fn output_within(mut cmd: Command, limit: std::time::Duration) -> Result<Option<std::process::Output>> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    // Drained as it runs, so a chatty child never blocks on a full pipe.
    let drain = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = drain(child.stdout.take().map(|r| Box::new(r) as Box<dyn Read + Send>));
    let err = drain(child.stderr.take().map(|r| Box::new(r) as Box<dyn Read + Send>));
    let deadline = std::time::Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    Ok(Some(std::process::Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    }))
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

    /// A run past its limit is stopped; one within it is read whole.
    #[test]
    fn a_run_past_its_limit_is_stopped() {
        let mut slow = Command::new("sh");
        slow.args(["-c", "sleep 5"]);
        assert!(output_within(slow, Duration::from_millis(200)).unwrap().is_none());

        let mut quick = Command::new("sh");
        quick.args(["-c", "echo done"]);
        let out = output_within(quick, Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "done\n");
    }
}
