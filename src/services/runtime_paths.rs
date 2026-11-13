#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

pub(crate) fn agentdesk_root() -> Option<PathBuf> {
    #[cfg(test)]
    {
        test_agentdesk_root()
    }
    #[cfg(not(test))]
    {
        crate::config::runtime_root()
    }
}

#[cfg(test)]
pub(crate) fn test_agentdesk_root() -> Option<PathBuf> {
    if let Ok(override_root) = std::env::var("AGENTDESK_ROOT_DIR") {
        let trimmed = override_root.trim();
        if !trimmed.is_empty() {
            let root = PathBuf::from(trimmed);
            if !is_live_release_runtime_root(&root) {
                return Some(root);
            }
        }
    }
    static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    Some(
        ROOT.get_or_init(|| tempfile::tempdir().expect("create isolated test runtime root"))
            .path()
            .to_path_buf(),
    )
}

#[cfg(test)]
fn is_live_release_runtime_root(root: &Path) -> bool {
    dirs::home_dir().is_some_and(|home| root == home.join(".adk").join("release"))
}

pub(crate) fn runtime_root() -> Option<PathBuf> {
    agentdesk_root().map(|root| root.join("runtime"))
}
pub(crate) fn restart_reports_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_restart_reports"))
}
