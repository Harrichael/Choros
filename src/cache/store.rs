use std::path::{Path, PathBuf};

pub const STORE_REL: &str = ".choros-config/store";

pub fn store_dir(root: &Path) -> PathBuf {
    root.join(STORE_REL)
}

pub fn rust_store(root: &Path) -> PathBuf {
    store_dir(root).join("rust")
}

pub fn sccache_dir(root: &Path) -> PathBuf {
    rust_store(root).join("sccache")
}

pub fn js_store(root: &Path) -> PathBuf {
    store_dir(root).join("js")
}

pub fn pnpm_store(root: &Path) -> PathBuf {
    js_store(root).join("pnpm-store")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn paths_compose_under_choros_config() {
        let root = Path::new("/proj");
        assert_eq!(store_dir(root), Path::new("/proj/.choros-config/store"));
        assert_eq!(
            sccache_dir(root),
            Path::new("/proj/.choros-config/store/rust/sccache")
        );
        assert_eq!(
            pnpm_store(root),
            Path::new("/proj/.choros-config/store/js/pnpm-store")
        );
    }
}
