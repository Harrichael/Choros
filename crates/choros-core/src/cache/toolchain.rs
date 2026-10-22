use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toolchain {
    Rust,
    JsNpm,
    JsYarn,
    JsPnpm,
}

impl Toolchain {
    pub fn is_js(&self) -> bool {
        matches!(self, Toolchain::JsNpm | Toolchain::JsYarn | Toolchain::JsPnpm)
    }
}

pub fn detect(repo_dir: &Path) -> Vec<Toolchain> {
    let mut out = Vec::new();
    if repo_dir.join("Cargo.toml").exists() {
        out.push(Toolchain::Rust);
    }
    // Prefer pnpm > yarn > npm if multiple lockfiles exist (rare but possible
    // during migrations). pnpm can read/import the others.
    if repo_dir.join("pnpm-lock.yaml").exists() {
        out.push(Toolchain::JsPnpm);
    } else if repo_dir.join("yarn.lock").exists() {
        out.push(Toolchain::JsYarn);
    } else if repo_dir.join("package-lock.json").exists() {
        out.push(Toolchain::JsNpm);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn touch(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"").unwrap();
    }

    #[test]
    fn empty_dir_detects_nothing() {
        let tmp = tempdir();
        assert!(detect(tmp.path()).is_empty());
    }

    #[test]
    fn rust_only() {
        let tmp = tempdir();
        touch(tmp.path(), "Cargo.toml");
        assert_eq!(detect(tmp.path()), vec![Toolchain::Rust]);
    }

    #[test]
    fn pnpm_only() {
        let tmp = tempdir();
        touch(tmp.path(), "pnpm-lock.yaml");
        assert_eq!(detect(tmp.path()), vec![Toolchain::JsPnpm]);
    }

    #[test]
    fn yarn_only() {
        let tmp = tempdir();
        touch(tmp.path(), "yarn.lock");
        assert_eq!(detect(tmp.path()), vec![Toolchain::JsYarn]);
    }

    #[test]
    fn npm_only() {
        let tmp = tempdir();
        touch(tmp.path(), "package-lock.json");
        assert_eq!(detect(tmp.path()), vec![Toolchain::JsNpm]);
    }

    #[test]
    fn pnpm_wins_over_others() {
        let tmp = tempdir();
        touch(tmp.path(), "pnpm-lock.yaml");
        touch(tmp.path(), "yarn.lock");
        touch(tmp.path(), "package-lock.json");
        assert_eq!(detect(tmp.path()), vec![Toolchain::JsPnpm]);
    }

    #[test]
    fn rust_plus_js() {
        let tmp = tempdir();
        touch(tmp.path(), "Cargo.toml");
        touch(tmp.path(), "pnpm-lock.yaml");
        assert_eq!(detect(tmp.path()), vec![Toolchain::Rust, Toolchain::JsPnpm]);
    }
}
