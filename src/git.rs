use std::path::Path;
use std::process::Command;

use color_eyre::eyre::{eyre, Result};

fn run(cmd: &mut Command) -> Result<String> {
    let output = cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!(
            "git command failed: {:?}\n{}",
            cmd,
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn remote_get_url(repo: &Path, remote: &str) -> Result<String> {
    run(Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["remote", "get-url", remote]))
}

pub fn fetch(repo: &Path, remote: &str) -> Result<()> {
    run(Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["fetch", "--quiet", remote]))?;
    Ok(())
}

pub fn clone(url: &str, dest: &Path) -> Result<()> {
    // `--` stops git from treating a url starting with `-` as an option
    // (CVE-2017-1000117-shaped risk, since url comes from user input).
    run(Command::new("git").args([
        "clone",
        "--",
        url,
        dest.to_str().ok_or_else(|| eyre!("non-utf8 path"))?,
    ]))?;
    Ok(())
}

pub fn clone_with_reference(reference: &Path, url: &str, dest: &Path) -> Result<()> {
    run(Command::new("git").args([
        "clone",
        "--reference-if-able",
        reference.to_str().ok_or_else(|| eyre!("non-utf8 path"))?,
        "--",
        url,
        dest.to_str().ok_or_else(|| eyre!("non-utf8 path"))?,
    ]))?;
    Ok(())
}

/// Number of paths `git status` reports as modified, staged, or untracked.
pub fn changed_count(repo: &Path) -> Result<usize> {
    // Without --no-optional-locks, status refreshes and rewrites the index,
    // so merely listing would look like activity to other tools.
    let out = run(Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"]))?;
    Ok(out.lines().count())
}

pub fn checkout_new_branch(repo: &Path, branch: &str) -> Result<()> {
    run(Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["checkout", "-b", branch]))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn init_repo_with_remote(path: &Path, remote_url: &str) {
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(path)
            .status()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["remote", "add", "origin", remote_url])
            .status()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["config", "user.email", "t@t"])
            .status()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["config", "user.name", "t"])
            .status()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["commit", "--allow-empty", "-m", "init"])
            .status()
            .unwrap();
    }

    #[test]
    fn read_origin_url() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("r");
        std::fs::create_dir(&repo).unwrap();
        init_repo_with_remote(&repo, "git@example.com:o/r.git");
        let url = remote_get_url(&repo, "origin").unwrap();
        assert_eq!(url, "git@example.com:o/r.git");
    }

    #[test]
    fn clone_works_against_local_source() {
        let tmp = tempfile::tempdir().unwrap();

        let src = tmp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(&src)
            .status()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&src)
            .args(["config", "user.email", "t@t"])
            .status()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&src)
            .args(["config", "user.name", "t"])
            .status()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&src)
            .args(["commit", "--allow-empty", "-m", "init"])
            .status()
            .unwrap();

        let dest = tmp.path().join("dest");
        clone(src.to_str().unwrap(), &dest).unwrap();
        assert!(dest.join(".git").exists());
        let origin = remote_get_url(&dest, "origin").unwrap();
        assert_eq!(origin, src.to_str().unwrap());
    }

    #[test]
    fn clone_treats_dash_prefixed_url_as_positional_not_an_option() {
        // Without `--` before the url, git would parse this as an
        // `--upload-pack` option and execute its argument as a command.
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dest");
        let err = clone("--upload-pack=touch /tmp/choros-clone-test-pwned", &dest)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--upload-pack=touch /tmp/choros-clone-test-pwned"),
            "expected git to report the url as a missing repository, got: {err}"
        );
        assert!(!std::path::Path::new("/tmp/choros-clone-test-pwned").exists());
    }

    #[test]
    fn clone_with_reference_works_against_local_source() {
        let tmp = tempfile::tempdir().unwrap();

        // Source repo (acts as the "github URL" — we use its filesystem path).
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(&src)
            .status()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&src)
            .args(["config", "user.email", "t@t"])
            .status()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&src)
            .args(["config", "user.name", "t"])
            .status()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&src)
            .args(["commit", "--allow-empty", "-m", "init"])
            .status()
            .unwrap();

        // Registry clone (acts as the reference).
        let reg = tmp.path().join("registry");
        Command::new("git")
            .args(["clone", "--quiet"])
            .arg(&src)
            .arg(&reg)
            .status()
            .unwrap();

        // Now perform the choros clone.
        let dest = tmp.path().join("dest");
        clone_with_reference(&reg, src.to_str().unwrap(), &dest).unwrap();
        assert!(dest.join(".git").exists());
        let origin = remote_get_url(&dest, "origin").unwrap();
        assert_eq!(origin, src.to_str().unwrap());
    }
}
