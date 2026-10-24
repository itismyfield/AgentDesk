use std::path::PathBuf;

pub(super) fn resolve(home: impl FnOnce() -> Option<PathBuf>) -> Option<PathBuf> {
    if let Ok(override_root) = std::env::var("AGENTDESK_ROOT_DIR") {
        let trimmed = override_root.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    if cfg!(test) || std::env::var_os("AGENTDESK_TEST_RUNTIME").is_some() {
        static ROOT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
        return ROOT
            .get_or_init(|| {
                let temp = std::env::temp_dir();
                let physical_temp = temp.canonicalize().ok()?;
                if let Some(release) = home().map(|home| home.join(".adk").join("release")) {
                    let physical_release = release.canonicalize().unwrap_or(release);
                    if physical_temp.starts_with(physical_release) {
                        return None;
                    }
                }
                // Keep fallback paths stable across threads without changing the process environment.
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                Some(temp.join(format!(
                    "agentdesk-test-runtime-{}-{stamp}",
                    std::process::id()
                )))
            })
            .clone();
    }
    home().map(|h| h.join(".adk").join("release"))
}

#[cfg(test)]
#[path = "runtime_root_guard/test_process.rs"]
mod test_process;
