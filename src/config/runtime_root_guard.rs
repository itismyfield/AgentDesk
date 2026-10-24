use std::path::PathBuf;

pub(super) fn resolve(home: impl FnOnce() -> Option<PathBuf>) -> Option<PathBuf> {
    if let Ok(override_root) = std::env::var("AGENTDESK_ROOT_DIR") {
        let trimmed = override_root.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    assert!(
        !cfg!(test) && std::env::var_os("AGENTDESK_TEST_RUNTIME").is_none(),
        "test runtime requires a non-empty AGENTDESK_ROOT_DIR; hold TestRuntimeRootGuard until all runtime work and child processes finish"
    );
    home().map(|h| h.join(".adk").join("release"))
}

#[cfg(test)]
#[path = "runtime_root_guard/test_process.rs"]
mod test_process;
