use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const REMOTECC_ROOT_DIR_ENV: &str = "REMOTECC_ROOT_DIR";

pub(super) fn remotecc_root() -> Option<PathBuf> {
    if let Ok(override_root) = std::env::var(REMOTECC_ROOT_DIR_ENV) {
        let trimmed = override_root.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }

    dirs::home_dir().map(|h| h.join(".remotecc"))
}

pub(super) fn runtime_root() -> Option<PathBuf> {
    remotecc_root().map(|root| root.join("runtime"))
}

pub(super) fn workspace_root() -> Option<PathBuf> {
    remotecc_root().map(|root| root.join("workspace"))
}

pub(super) fn worktrees_root() -> Option<PathBuf> {
    remotecc_root().map(|root| root.join("worktrees"))
}

pub(super) fn bot_settings_path() -> Option<PathBuf> {
    remotecc_root().map(|root| {
        legacy_fallback(root.join("config").join("bot_settings.json"), root.join("bot_settings.json"))
    })
}

pub(super) fn role_map_path() -> Option<PathBuf> {
    remotecc_root().map(|root| {
        legacy_fallback(root.join("config").join("role_map.json"), root.join("role_map.json"))
    })
}

pub(super) fn org_schema_path() -> Option<PathBuf> {
    remotecc_root().map(|root| {
        legacy_fallback(root.join("config").join("org.yaml"), root.join("org.yaml"))
    })
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

pub(super) fn shared_agent_memory_root() -> Option<PathBuf> {
    remotecc_root().map(|root| root.join("shared_agent_memory"))
}

/// Path to the generation counter file.
pub fn generation_path() -> Option<PathBuf> {
    remotecc_root().map(|root| {
        legacy_fallback(root.join("runtime").join("generation"), root.join("generation"))
    })
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
    let Some(root) = last_message_root() else { return };
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

/// Shared mutex for tests that manipulate REMOTECC_ROOT_DIR env var.
/// All test modules must use this to avoid env var races.
#[cfg(test)]
pub(super) fn test_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

pub(super) fn atomic_write(path: &Path, data: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp).map_err(|e| e.to_string())?;
    file.write_all(data.as_bytes()).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Temporary helper for ~/.remotecc/ folder restructuring migration.
/// Returns new_path if it exists, otherwise legacy_path if it exists, otherwise new_path.
/// TODO: Remove after 2026-03-26 (legacy fallback cleanup)
fn legacy_fallback(new_path: PathBuf, legacy_path: PathBuf) -> PathBuf {
    if new_path.exists() {
        new_path
    } else if legacy_path.exists() {
        legacy_path
    } else {
        new_path
    }
}
