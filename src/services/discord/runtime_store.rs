use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg_attr(not(test), allow(dead_code))]
const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

pub(super) fn agentdesk_root() -> Option<PathBuf> {
    crate::config::runtime_root()
}

pub(super) fn runtime_root() -> Option<PathBuf> {
    agentdesk_root().map(|root| root.join("runtime"))
}

pub(super) fn workspace_root() -> Option<PathBuf> {
    agentdesk_root().map(|root| root.join("workspaces"))
}

pub(super) fn worktrees_root() -> Option<PathBuf> {
    agentdesk_root().map(|root| root.join("worktrees"))
}

pub(super) fn bot_settings_path() -> Option<PathBuf> {
    agentdesk_root().map(|root| crate::runtime_layout::config_dir(&root).join("bot_settings.json"))
}

pub(super) fn role_map_path() -> Option<PathBuf> {
    agentdesk_root().map(|root| crate::runtime_layout::role_map_path(&root))
}

pub(super) fn org_schema_path() -> Option<PathBuf> {
    agentdesk_root().map(|root| org_schema_path_for_root(&root))
}

pub(crate) fn org_schema_path_for_root(root: &Path) -> PathBuf {
    crate::runtime_layout::org_schema_path(root)
}

pub(super) fn discord_uploads_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_uploads"))
}

pub(super) fn discord_inflight_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_inflight"))
}

pub(super) fn discord_restart_reports_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_restart_reports"))
}

pub(super) fn discord_pending_queue_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_pending_queue"))
}

pub(super) fn discord_handoff_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_handoff"))
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn shared_agent_memory_root() -> Option<PathBuf> {
    agentdesk_root().map(|root| crate::runtime_layout::shared_agent_memory_root(&root))
}

pub(super) fn shared_agent_knowledge_path() -> Option<PathBuf> {
    agentdesk_root().map(|root| crate::runtime_layout::shared_agent_knowledge_path(&root))
}

pub(super) fn long_term_memory_root() -> Option<PathBuf> {
    agentdesk_root().map(|root| crate::runtime_layout::long_term_memory_root(&root))
}

/// Path to the generation counter file.
pub fn generation_path() -> Option<PathBuf> {
    agentdesk_root().map(|root| root.join("runtime").join("generation"))
}

/// Load the current generation counter (returns 0 if file missing/corrupt).
pub fn load_generation() -> u64 {
    generation_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Increment the generation counter and return the new value.
pub fn increment_generation() -> u64 {
    let current = load_generation();
    let next = current + 1;
    if let Some(path) = generation_path() {
        let _ = atomic_write(&path, &next.to_string());
    }
    next
}

pub(super) fn last_message_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("last_message"))
}

/// Save the last processed message ID for a channel.
pub(super) fn save_last_message_id(provider: &str, channel_id: u64, message_id: u64) {
    let Some(root) = last_message_root() else {
        return;
    };
    let dir = root.join(provider);
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(format!("{}.txt", channel_id));
    let _ = atomic_write(&path, &message_id.to_string());
}

/// Save all last_message_ids from a map (used during SIGTERM).
pub(super) fn save_all_last_message_ids(provider: &str, ids: &std::collections::HashMap<u64, u64>) {
    for (channel_id, message_id) in ids {
        save_last_message_id(provider, *channel_id, *message_id);
    }
}

/// Shared mutex for tests that manipulate AGENTDESK_ROOT_DIR env var.
/// All test modules must use this to avoid env var races.
#[cfg(test)]
pub(crate) fn test_env_lock() -> &'static std::sync::Mutex<()> {
    crate::config::shared_test_env_lock()
}

#[cfg(test)]
pub(crate) fn lock_test_env() -> std::sync::MutexGuard<'static, ()> {
    test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

pub(super) fn atomic_write(path: &Path, data: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp).map_err(|e| e.to_string())?;
    file.write_all(data.as_bytes()).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Acquire the shared env lock to avoid races between tests that mutate
    /// AGENTDESK_ROOT_DIR.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        lock_test_env()
    }

    #[test]
    fn test_agentdesk_root_env_override() {
        let _lock = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let override_path = tmp.path().join("custom_root");
        fs::create_dir_all(&override_path).unwrap();

        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, override_path.to_str().unwrap()) };
        let root = agentdesk_root().expect("should return Some");
        assert_eq!(root, override_path);

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[test]
    fn test_agentdesk_root_env_empty_falls_back() {
        let _lock = env_lock();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, "   ") };
        // Empty/whitespace-only override should fall back to home-based default
        let root = agentdesk_root().expect("should return Some");
        let expected = dirs::home_dir().unwrap().join(".adk").join("release");
        assert_eq!(root, expected);

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[test]
    fn test_bot_settings_path_uses_config_location() {
        let _lock = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, root.to_str().unwrap()) };

        let new_path = root.join("config").join("bot_settings.json");
        fs::create_dir_all(new_path.parent().unwrap()).unwrap();
        fs::write(&new_path, "new").unwrap();

        let result = bot_settings_path().expect("should return Some");
        assert_eq!(result, new_path, "Should use config/ path");

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[test]
    fn test_runtime_paths_consistent() {
        let _lock = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, root.to_str().unwrap()) };

        // All path functions should return paths under the root
        let paths: Vec<(&str, Option<PathBuf>)> = vec![
            ("runtime_root", runtime_root()),
            ("workspace_root", workspace_root()),
            ("worktrees_root", worktrees_root()),
            ("discord_uploads_root", discord_uploads_root()),
            ("discord_inflight_root", discord_inflight_root()),
            (
                "discord_restart_reports_root",
                discord_restart_reports_root(),
            ),
            ("discord_pending_queue_root", discord_pending_queue_root()),
            ("discord_handoff_root", discord_handoff_root()),
            ("shared_agent_memory_root", shared_agent_memory_root()),
            ("last_message_root", last_message_root()),
        ];

        for (name, path) in paths {
            let p = path.unwrap_or_else(|| panic!("{} should return Some", name));
            assert!(
                p.starts_with(&root),
                "{} path {:?} should be under root {:?}",
                name,
                p,
                root,
            );
        }

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[test]
    fn test_generation_roundtrip() {
        let _lock = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, root.to_str().unwrap()) };

        // generation_path lives under runtime/
        let runtime_dir = root.join("runtime");
        fs::create_dir_all(&runtime_dir).unwrap();

        // Initially 0 (file missing)
        assert_eq!(load_generation(), 0);

        // Increment should return 1
        assert_eq!(increment_generation(), 1);
        assert_eq!(load_generation(), 1);

        // Increment again
        assert_eq!(increment_generation(), 2);
        assert_eq!(load_generation(), 2);

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[test]
    fn test_fallback_returns_new_when_neither_exists() {
        let _lock = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, root.to_str().unwrap()) };

        // Neither config/bot_settings.json nor bot_settings.json exists
        let result = bot_settings_path().expect("should return Some");
        let expected_new = root.join("config").join("bot_settings.json");
        assert_eq!(
            result, expected_new,
            "Should return new path when neither exists"
        );

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }
}
