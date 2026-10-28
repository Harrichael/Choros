use std::path::Path;

use color_eyre::eyre::Result;

pub mod js;
pub mod rust;
pub mod store;
pub mod toolchain;

pub use toolchain::Toolchain;

use crate::choros::ProgressSink;

/// Detect toolchains across cloned repos and wire up caches.
///
/// Best-effort: missing tools (sccache, pnpm) skip with a warning. Hard
/// failures (e.g. pnpm install errors) propagate and abort workspace creation.
pub fn setup_workspace_caches<P: ProgressSink>(
    trunk: &Path,
    workspace: &Path,
    repos: &[String],
    progress: &P,
) -> Result<()> {
    let mut repo_toolchains: Vec<(String, Vec<Toolchain>)> = Vec::new();
    for repo in repos {
        let dir = workspace.join(repo);
        let detected = toolchain::detect(&dir);
        repo_toolchains.push((repo.clone(), detected));
    }

    let any_rust = repo_toolchains
        .iter()
        .any(|(_, tcs)| tcs.iter().any(|t| *t == Toolchain::Rust));
    if any_rust {
        rust::setup(workspace, progress)?;
    }

    for (repo, tcs) in &repo_toolchains {
        for tc in tcs {
            if tc.is_js() {
                let repo_dir = workspace.join(repo);
                js::install(trunk, &repo_dir, *tc, progress)?;
            }
        }
    }

    Ok(())
}

mod tool {
    use std::path::{Path, PathBuf};

    pub fn available(name: &str) -> bool {
        find(name).is_some()
    }

    /// The executable `name` resolves to on PATH, as a spawn would find it.
    pub fn find(name: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| is_executable(candidate))
    }

    #[cfg(unix)]
    fn is_executable(path: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        match path.metadata() {
            Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111) != 0,
            Err(_) => false,
        }
    }

    #[cfg(not(unix))]
    fn is_executable(path: &Path) -> bool {
        path.is_file()
    }
}
