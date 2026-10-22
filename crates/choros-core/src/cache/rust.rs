use std::path::Path;

use color_eyre::eyre::{Context, Result};
use toml::{map::Map, Value};

use crate::cache::store::{rust_store, sccache_dir};
use crate::cache::tool;
use crate::choros::ProgressSink;

pub fn setup<P: ProgressSink>(root: &Path, workspace: &Path, progress: &P) -> Result<()> {
    setup_inner(root, workspace, progress, tool::available)
}

fn setup_inner<P, F>(root: &Path, workspace: &Path, progress: &P, finder: F) -> Result<()>
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

    let sccache = sccache_dir(root);
    std::fs::create_dir_all(&sccache)
        .wrap_err_with(|| format!("creating {sccache:?}"))?;
    std::fs::create_dir_all(rust_store(root))?;

    let config_dir = workspace.join(".cargo");
    std::fs::create_dir_all(&config_dir)?;
    let config_path = config_dir.join("config.toml");

    let body = render_config(&sccache)?;
    std::fs::write(&config_path, body)
        .wrap_err_with(|| format!("writing {config_path:?}"))?;

    progress.status(format!(
        "rust cache → {} (sccache @ {})",
        config_path.display(),
        sccache.display()
    ));
    Ok(())
}

fn render_config(sccache: &Path) -> Result<String> {
    let mut build = Map::new();
    build.insert("rustc-wrapper".into(), Value::String("sccache".into()));

    let mut env = Map::new();
    env.insert(
        "SCCACHE_DIR".into(),
        Value::String(sccache.display().to_string()),
    );

    let mut root = Map::new();
    root.insert("build".into(), Value::Table(build));
    root.insert("env".into(), Value::Table(env));

    toml::to_string(&Value::Table(root))
        .wrap_err("serializing .cargo/config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn render_config_contains_expected_keys() {
        let body = render_config(Path::new("/x/sccache")).unwrap();
        assert!(body.contains("rustc-wrapper = \"sccache\""), "got: {body}");
        assert!(body.contains("SCCACHE_DIR = \"/x/sccache\""), "got: {body}");
    }

    #[test]
    fn setup_skips_when_finder_returns_false() {
        let root = tempdir();
        let workspace = root.path().join("PROJ-1");
        std::fs::create_dir_all(&workspace).unwrap();
        setup_inner(root.path(), &workspace, &(), |_| false).unwrap();
        assert!(
            !workspace.join(".cargo/config.toml").exists(),
            "expected no config when sccache is missing"
        );
        assert!(
            !root.path().join(".choros-config/store/rust").exists(),
            "expected no store dir when sccache is missing"
        );
    }

    #[test]
    fn setup_writes_config_when_finder_returns_true() {
        let root = tempdir();
        let workspace = root.path().join("PROJ-1");
        std::fs::create_dir_all(&workspace).unwrap();
        setup_inner(root.path(), &workspace, &(), |_| true).unwrap();
        let config = workspace.join(".cargo/config.toml");
        assert!(config.exists(), "expected {config:?}");
        let body = std::fs::read_to_string(&config).unwrap();
        assert!(body.contains("rustc-wrapper = \"sccache\""));
        assert!(body.contains("SCCACHE_DIR"));
        let expected_sccache = root.path().join(".choros-config/store/rust/sccache");
        assert!(
            body.contains(&expected_sccache.display().to_string()),
            "got: {body}"
        );
        assert!(expected_sccache.exists(), "expected store dir created");
    }
}
