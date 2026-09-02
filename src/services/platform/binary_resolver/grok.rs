use std::collections::BTreeSet;
use std::path::PathBuf;

pub(super) fn append_provider_specific_fallback_dirs(
    provider: &str,
    dirs: &mut Vec<PathBuf>,
    seen: &mut BTreeSet<String>,
) {
    if super::normalize_name(provider) != "grok" {
        return;
    }

    super::push_env_dir("GROK_BIN_DIR", None, dirs, seen);
    super::push_env_dir("GROK_HOME", Some("bin"), dirs, seen);
    if let Some(home) = dirs::home_dir() {
        super::push_unique_path(home.join(".grok").join("bin"), dirs, seen);
    }
}
