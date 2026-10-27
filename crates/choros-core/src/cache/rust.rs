use std::path::Path;

use color_eyre::eyre::{Context, Result};
use toml::{map::Map, Value};

use crate::cache::tool;
use crate::choros::ProgressSink;

/// Point the workspace's cargo at sccache. sccache keeps one cache for the
/// whole machine on purpose: it reads its cache dir only when its server
/// starts, so a per-trunk dir would quietly lose to whichever build started
/// the server first — and sharing across trunks is the point anyway.
pub fn setup<P: ProgressSink>(workspace: &Path, progress: &P) -> Result<()> {
    setup_inner(workspace, progress, tool::available)
}

fn setup_inner<P, F>(workspace: &Path, progress: &P, finder: F) -> Result<()>
where
    P: ProgressSink,
    F: Fn(&str) -> bool,
{
    if !finder("sccache") {
        progress.status(
            "warning: sccache not found on PATH — Rust build cache skipped (install sccache to enable)"
                .into(),
        );
        return Ok(());
    }

    let config_dir = workspace.join(".cargo");
    std::fs::create_dir_all(&config_dir)?;
    let config_path = config_dir.join("config.toml");

    std::fs::write(&config_path, render_config()?)
        .wrap_err_with(|| format!("writing {}", config_path.display()))?;

    progress.status(format!("rust cache → {} (sccache)", config_path.display()));
    Ok(())
}

fn render_config() -> Result<String> {
    let mut build = Map::new();
    build.insert("rustc-wrapper".into(), Value::String("sccache".into()));

    let mut root = Map::new();
    root.insert("build".into(), Value::Table(build));

    toml::to_string(&Value::Table(root))
        .wrap_err("serializing .cargo/config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_skips_when_finder_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("PROJ-1");
        std::fs::create_dir_all(&workspace).unwrap();
        setup_inner(&workspace, &(), |_| false).unwrap();
        assert!(
            !workspace.join(".cargo/config.toml").exists(),
            "expected no config when sccache is missing"
        );
    }

    #[test]
    fn setup_wraps_rustc_with_the_machines_one_sccache() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("PROJ-1");
        std::fs::create_dir_all(&workspace).unwrap();
        setup_inner(&workspace, &(), |_| true).unwrap();
        let body = std::fs::read_to_string(workspace.join(".cargo/config.toml")).unwrap();
        assert!(body.contains("rustc-wrapper = \"sccache\""), "got: {body}");
        assert!(!body.contains("SCCACHE_DIR"), "got: {body}");
    }
}
