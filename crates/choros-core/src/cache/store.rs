use std::path::{Path, PathBuf};

pub const STORE_REL: &str = ".choros-config/store";

pub fn store_dir(trunk: &Path) -> PathBuf {
    trunk.join(STORE_REL)
}

pub fn js_store(trunk: &Path) -> PathBuf {
    store_dir(trunk).join("js")
}

pub fn pnpm_store(trunk: &Path) -> PathBuf {
    js_store(trunk).join("pnpm-store")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn paths_compose_under_choros_config() {
        let trunk = Path::new("/proj");
        assert_eq!(store_dir(trunk), Path::new("/proj/.choros-config/store"));
        assert_eq!(
            pnpm_store(trunk),
            Path::new("/proj/.choros-config/store/js/pnpm-store")
        );
    }
}
