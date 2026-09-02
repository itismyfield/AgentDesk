use std::collections::BTreeSet;
use std::path::PathBuf;

pub(super) fn provider_specific_fallback_dirs(provider: &str) -> Vec<PathBuf> {
    if super::normalize_name(provider) != "grok" {
        return Vec::new();
    }

    let mut dirs = Vec::new();
    let mut seen = BTreeSet::new();
    super::push_env_dir("GROK_BIN_DIR", None, &mut dirs, &mut seen);
    super::push_env_dir("GROK_HOME", Some("bin"), &mut dirs, &mut seen);
    if let Some(home) = dirs::home_dir() {
        super::push_unique_path(home.join(".grok").join("bin"), &mut dirs, &mut seen);
    }
    dirs
}
