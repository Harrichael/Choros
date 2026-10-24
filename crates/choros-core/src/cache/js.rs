use std::path::Path;
use std::process::Command;

use color_eyre::eyre::{eyre, Result};

use crate::cache::store::{js_store, pnpm_store};
use crate::cache::tool;
use crate::cache::toolchain::Toolchain;
use crate::choros::ProgressSink;

pub fn install<P: ProgressSink>(
    trunk: &Path,
    repo_dir: &Path,
    toolchain: Toolchain,
    progress: &P,
) -> Result<()> {
    install_inner(trunk, repo_dir, toolchain, progress, tool::available)
}

fn install_inner<P, F>(
    trunk: &Path,
    repo_dir: &Path,
    toolchain: Toolchain,
    progress: &P,
    finder: F,
) -> Result<()>
where
    P: ProgressSink,
    F: Fn(&str) -> bool,
{
    if !finder("pnpm") {
        progress.status(
            "warning: pnpm not found on PATH — JS install skipped (install pnpm to enable)"
                .into(),
        );
        return Ok(());
    }

    std::fs::create_dir_all(js_store(trunk))?;
    let store = pnpm_store(trunk);
    std::fs::create_dir_all(&store)?;

    let repo_name = repo_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("<repo>");

    // For non-pnpm lockfiles, ask pnpm to translate first so it can install
    // deterministically from its own format. The generated pnpm-lock.yaml is
    // ours, not the repo's: exclude it locally so it can't end up committed to
    // the task branch (the choros-archive skill commits dirty trees wholesale).
    if matches!(toolchain, Toolchain::JsNpm | Toolchain::JsYarn) {
        exclude_from_git(repo_dir, "pnpm-lock.yaml")?;
        progress.status(format!("pnpm import in {repo_name}…"));
        run_pnpm(repo_dir, &["import"])?;
    }

    progress.status(format!("pnpm install in {repo_name}…"));
    let store_arg = format!("--store-dir={}", store.display());
    run_pnpm(repo_dir, &["install", &store_arg])?;

    Ok(())
}

/// Append `pattern` to the clone's `.git/info/exclude` — like .gitignore, but
/// local to this clone, so the upstream repo never sees the entry.
fn exclude_from_git(repo_dir: &Path, pattern: &str) -> Result<()> {
    let info = repo_dir.join(".git/info");
    std::fs::create_dir_all(&info)?;
    let path = info.join("exclude");
    let mut body = std::fs::read_to_string(&path).unwrap_or_default();
    if body.lines().any(|l| l.trim() == pattern) {
        return Ok(());
    }
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(pattern);
    body.push('\n');
    std::fs::write(&path, body)?;
    Ok(())
}

fn run_pnpm(cwd: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("pnpm").current_dir(cwd).args(args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!(
            "pnpm {} failed in {:?}\n{}",
            args.join(" "),
            cwd,
            stderr.trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn exclude_from_git_appends_once_and_preserves_existing() {
        let repo = tempdir();
        let info = repo.path().join(".git/info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(info.join("exclude"), b"*.swp").unwrap();

        exclude_from_git(repo.path(), "pnpm-lock.yaml").unwrap();
        exclude_from_git(repo.path(), "pnpm-lock.yaml").unwrap();

        let body = std::fs::read_to_string(info.join("exclude")).unwrap();
        assert_eq!(body, "*.swp\npnpm-lock.yaml\n");
    }

    #[test]
    fn install_skips_when_finder_returns_false() {
        let trunk = tempdir();
        let repo = tempdir();
        install_inner(trunk.path(), repo.path(), Toolchain::JsPnpm, &(), |_| false).unwrap();
        // No store dirs created when pnpm is "missing".
        assert!(!trunk.path().join(".choros-config/store/js").exists());
    }

    // Real pnpm-install test: only runs when pnpm is on PATH. Stays out of CI
    // hosts that lack pnpm.
    #[test]
    fn install_runs_pnpm_when_available() {
        if !tool::available("pnpm") {
            eprintln!("skipping: pnpm not on PATH");
            return;
        }
        let trunk = tempdir();
        let repo = tempdir();
        std::fs::write(
            repo.path().join("package.json"),
            br#"{"name":"choros-cache-test","version":"0.0.0","private":true,"dependencies":{}}"#,
        )
        .unwrap();
        // Empty pnpm lockfile is fine for an empty deps set.
        std::fs::write(repo.path().join("pnpm-lock.yaml"), b"lockfileVersion: '9.0'\n").unwrap();

        install(trunk.path(), repo.path(), Toolchain::JsPnpm, &()).unwrap();
        assert!(
            trunk.path()
                .join(".choros-config/store/js/pnpm-store")
                .exists(),
            "expected pnpm store dir created"
        );
    }
}
