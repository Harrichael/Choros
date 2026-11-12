//! Running a program that talks to the network, where a dead connection
//! can hang it for hours: under a time limit, so it fails instead.

use std::process::{Command, Output};
use std::time::Duration;

use color_eyre::eyre::{eyre, Result};

/// Long enough for any fetch, push or API call over a live connection.
pub const NETWORK: Duration = Duration::from_secs(5 * 60);

/// `cmd`'s output, or an error once it has run past `limit`.
pub fn output_limited(cmd: Command, limit: Duration) -> Result<Output> {
    let what = std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    // A PR body rides in `gh pr create`'s arguments.
    let what: String = match what.char_indices().nth(120) {
        Some((i, _)) => format!("{}…", &what[..i]),
        None => what,
    };
    output_within(cmd, limit)?.ok_or_else(|| {
        eyre!("{what} gave no answer in {} minutes and was stopped; is the network up?", limit.as_secs() / 60)
    })
}

/// `cmd`'s output, or `None` when it ran past `limit` and was killed.
pub fn output_within(mut cmd: Command, limit: std::time::Duration) -> Result<Option<std::process::Output>> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
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
            stop(&mut child);
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

/// Ask first: on SIGTERM git drops its lock files and a half-made clone,
/// which a SIGKILL would leave to break every later fetch.
fn stop(child: &mut std::process::Child) {
    // A child started as the leader of its own process group takes what it
    // started with it; for any other, there is no such group to signal.
    let group = format!("-{}", child.id());
    let signal = |sig: &str, to: &str| {
        let _ = Command::new("kill").args([sig, "--", to]).stderr(std::process::Stdio::null()).status();
    };
    signal("-TERM", &group);
    signal("-TERM", &child.id().to_string());
    let grace = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < grace {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    signal("-KILL", &group);
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// A hung network call ends in an error naming it, not in a wait.
    #[test]
    fn a_hung_call_fails_with_its_name() {
        let mut hung = Command::new("sleep");
        hung.arg("5");
        let err = output_limited(hung, Duration::from_millis(200)).unwrap_err().to_string();
        assert!(err.starts_with("sleep 5 gave no answer"), "{err}");
    }
}

/// A running process. A pid is reused once its process is gone, so when it
/// began tells this one from a later one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Proc {
    pub pid: u32,
    pub started: String,
}

impl Proc {
    pub fn of(pid: u32) -> Option<Proc> {
        let started = ps(pid, "lstart=")?;
        Some(Proc { pid, started })
    }

    pub fn alive(&self) -> bool {
        Proc::of(self.pid).as_ref() == Some(self)
    }
}

/// The agent session this process runs under: the nearest process above
/// it named `claude`.
pub fn agent_above() -> Option<Proc> {
    let mut pid = std::process::id();
    // Deeper than any shell nests; a guard against a loop in what ps says.
    for _ in 0..64 {
        pid = ps(pid, "ppid=")?.parse().ok().filter(|&p| p > 1)?;
        let name = ps(pid, "comm=")?;
        if name.rsplit('/').next() == Some("claude") {
            return Proc::of(pid);
        }
    }
    None
}

fn ps(pid: u32, field: &str) -> Option<String> {
    let out = Command::new("ps")
        .args(["-o", field, "-p", &pid.to_string()])
        // `lstart` is written in the caller's zone and tongue, and the
        // daemon's may not be the session's.
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}
