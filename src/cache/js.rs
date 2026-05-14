use std::path::Path;
use std::process::Command;

use color_eyre::eyre::{eyre, Result};

use crate::cache::store::{js_store, pnpm_store};
use crate::cache::tool;
use crate::cache::toolchain::Toolchain;
use crate::choros::ProgressSink;

pub fn install<P: ProgressSink>(
    root: &Path,
    repo_dir: &Path,
    toolchain: Toolchain,
    progress: &P,
) -> Result<()> {
    install_inner(root, repo_dir, toolchain, progress, tool::available)
}

fn install_inner<P, F>(
    root: &Path,
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

    std::fs::create_dir_all(js_store(root))?;
    let store = pnpm_store(root);
    std::fs::create_dir_all(&store)?;

    let repo_name = repo_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("<repo>");

    // For non-pnpm lockfiles, ask pnpm to translate first so it can install
    // deterministically from its own format.
    if matches!(toolchain, Toolchain::JsNpm | Toolchain::JsYarn) {
        progress.status(format!("pnpm import in {repo_name}…"));
        run_pnpm(repo_dir, &["import"])?;
    }

    progress.status(format!("pnpm install in {repo_name}…"));
    let store_arg = format!("--store-dir={}", store.display());
    run_pnpm(repo_dir, &["install", &store_arg])?;

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
    fn install_skips_when_finder_returns_false() {
        let root = tempdir();
        let repo = tempdir();
        install_inner(root.path(), repo.path(), Toolchain::JsPnpm, &(), |_| false).unwrap();
        // No store dirs created when pnpm is "missing".
        assert!(!root.path().join(".choros-config/store/js").exists());
    }

    // Real pnpm-install test: only runs when pnpm is on PATH. Stays out of CI
    // hosts that lack pnpm.
    #[test]
    fn install_runs_pnpm_when_available() {
        if !tool::available("pnpm") {
            eprintln!("skipping: pnpm not on PATH");
            return;
        }
        let root = tempdir();
        let repo = tempdir();
        std::fs::write(
            repo.path().join("package.json"),
            br#"{"name":"choros-cache-test","version":"0.0.0","private":true,"dependencies":{}}"#,
        )
        .unwrap();
        // Empty pnpm lockfile is fine for an empty deps set.
        std::fs::write(repo.path().join("pnpm-lock.yaml"), b"lockfileVersion: '9.0'\n").unwrap();

        install(root.path(), repo.path(), Toolchain::JsPnpm, &()).unwrap();
        assert!(
            root.path()
                .join(".choros-config/store/js/pnpm-store")
                .exists(),
            "expected pnpm store dir created"
        );
    }
}
