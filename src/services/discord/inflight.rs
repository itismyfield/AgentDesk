use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::runtime_store::{atomic_write, discord_inflight_root};
use crate::services::provider::ProviderKind;

const INFLIGHT_STATE_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct InflightTurnState {
    pub version: u32,
    pub provider: String,
    pub channel_id: u64,
    pub channel_name: Option<String>,
    #[serde(default)]
    pub logical_channel_id: Option<u64>,
    #[serde(default)]
    pub thread_id: Option<u64>,
    #[serde(default)]
    pub thread_title: Option<String>,
    pub request_owner_user_id: u64,
    pub user_msg_id: u64,
    pub current_msg_id: u64,
    pub current_msg_len: usize,
    pub user_text: String,
    pub session_id: Option<String>,
    pub tmux_session_name: Option<String>,
    pub output_path: Option<String>,
    pub input_fifo_path: Option<String>,
    pub last_offset: u64,
    /// Stable start offset for the current turn's output JSONL slice.
    #[serde(default)]
    pub turn_start_offset: Option<u64>,
    pub full_response: String,
    pub response_sent_offset: usize,
    #[serde(default)]
    pub current_tool_line: Option<String>,
    #[serde(default)]
    pub prev_tool_status: Option<String>,
    pub started_at: String,
    pub updated_at: String,
    /// Restart generation at which this turn was born.
    #[serde(default)]
    pub born_generation: u64,
    /// Whether any tool_use was seen during this turn (persisted for restart recovery).
    #[serde(default)]
    pub any_tool_used: bool,
    /// Whether text was streamed after the last tool_use (persisted for restart recovery).
    #[serde(default)]
    pub has_post_tool_text: bool,
    /// ADK session key (hostname:session-name) for long-turn diagnostics.
    #[serde(default)]
    pub session_key: Option<String>,
    /// Active dispatch ID for long-turn diagnostics.
    #[serde(default)]
    pub dispatch_id: Option<String>,
    /// Last tmux output offset from which a watcher relayed a response.
    /// Persisted so that replacement watcher instances can skip already-delivered output.
    #[serde(default)]
    pub last_watcher_relayed_offset: Option<u64>,
}

impl InflightTurnState {
    pub fn new(
        provider: ProviderKind,
        channel_id: u64,
        channel_name: Option<String>,
        request_owner_user_id: u64,
        user_msg_id: u64,
        current_msg_id: u64,
        user_text: String,
        session_id: Option<String>,
        tmux_session_name: Option<String>,
        output_path: Option<String>,
        input_fifo_path: Option<String>,
        last_offset: u64,
    ) -> Self {
        let now = now_string();
        Self {
            version: INFLIGHT_STATE_VERSION,
            provider: provider.as_str().to_string(),
            channel_id,
            channel_name,
            logical_channel_id: Some(channel_id),
            thread_id: None,
            thread_title: None,
            request_owner_user_id,
            user_msg_id,
            current_msg_id,
            current_msg_len: 0,
            user_text,
            session_id,
            tmux_session_name,
            output_path,
            input_fifo_path,
            last_offset,
            turn_start_offset: Some(last_offset),
            full_response: String::new(),
            response_sent_offset: 0,
            current_tool_line: None,
            prev_tool_status: None,
            started_at: now.clone(),
            updated_at: now,
            born_generation: super::runtime_store::load_generation(),
            any_tool_used: false,
            has_post_tool_text: false,
            session_key: None,
            dispatch_id: None,
            last_watcher_relayed_offset: None,
        }
    }

    pub fn provider_kind(&self) -> Option<ProviderKind> {
        ProviderKind::from_str(&self.provider)
    }
}

pub(super) fn inflight_runtime_root() -> Option<PathBuf> {
    discord_inflight_root()
}

fn inflight_provider_dir(root: &Path, provider: &ProviderKind) -> PathBuf {
    root.join(provider.as_str())
}

fn inflight_state_path(root: &Path, provider: &ProviderKind, channel_id: u64) -> PathBuf {
    inflight_provider_dir(root, provider).join(format!("{channel_id}.json"))
}

fn now_string() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub(super) fn save_inflight_state(state: &InflightTurnState) -> Result<(), String> {
    let Some(root) = inflight_runtime_root() else {
        return Err("Home directory not found".to_string());
    };
    save_inflight_state_in_root(&root, state)
}

fn save_inflight_state_in_root(root: &Path, state: &InflightTurnState) -> Result<(), String> {
    let Some(provider) = state.provider_kind() else {
        return Err(format!("Unknown provider '{}'", state.provider));
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut updated = state.clone();
    updated.updated_at = now_string();
    let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
    atomic_write(&path, &json)
}

pub(super) fn clear_inflight_state(provider: &ProviderKind, channel_id: u64) {
    let Some(root) = inflight_runtime_root() else {
        return;
    };
    let path = inflight_state_path(&root, provider, channel_id);
    let _ = fs::remove_file(path);
}

pub(super) fn clear_inflight_by_tmux_name(provider: &ProviderKind, tmux_name: &str) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };

    let provider_dir = inflight_provider_dir(&root, provider);
    let Ok(entries) = fs::read_dir(&provider_dir) else {
        return false;
    };

    let mut cleared = false;
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(state) = serde_json::from_str::<InflightTurnState>(&content) else {
            continue;
        };
        if state.tmux_session_name.as_deref() != Some(tmux_name) {
            continue;
        }
        if fs::remove_file(&path).is_ok() {
            cleared = true;
        }
    }

    cleared
}

/// Load a single inflight state by provider + channel_id (returns None if missing).
pub(super) fn load_inflight_state(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<InflightTurnState> {
    let root = inflight_runtime_root()?;
    let path = inflight_state_path(&root, provider, channel_id);
    let data = fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

pub(super) fn load_inflight_states(provider: &ProviderKind) -> Vec<InflightTurnState> {
    let Some(root) = inflight_runtime_root() else {
        return Vec::new();
    };
    load_inflight_states_from_root(&root, provider)
}

pub(crate) fn latest_request_owner_user_id_for_channel(channel_id: u64) -> Option<u64> {
    let providers = [
        ProviderKind::Claude,
        ProviderKind::Codex,
        ProviderKind::Gemini,
        ProviderKind::Qwen,
    ];

    providers
        .iter()
        .flat_map(load_inflight_states)
        .filter(|state| state.channel_id == channel_id)
        .max_by(|left, right| left.updated_at.cmp(&right.updated_at))
        .map(|state| state.request_owner_user_id)
}

/// Maximum age for inflight state files before they are considered stale and removed.
const INFLIGHT_MAX_AGE_SECS: u64 = 300; // 5 minutes

fn load_inflight_states_from_root(root: &Path, provider: &ProviderKind) -> Vec<InflightTurnState> {
    let dir = inflight_provider_dir(root, provider);
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut states = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // Check file age — remove files older than INFLIGHT_MAX_AGE_SECS
        if let Ok(meta) = fs::metadata(&path) {
            if let Ok(modified) = meta.modified() {
                if let Ok(age) = modified.elapsed() {
                    if age.as_secs() > INFLIGHT_MAX_AGE_SECS {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ⚠ removing stale inflight state file ({:.0}s old): {}",
                            age.as_secs_f64(),
                            path.display()
                        );
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                }
            }
        }
        let Ok(content) = fs::read_to_string(&path) else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ failed to read inflight state file: {}",
                path.display()
            );
            continue;
        };
        let Ok(state) = serde_json::from_str::<InflightTurnState>(&content) else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ removing malformed inflight state file: {}",
                path.display()
            );
            let _ = fs::remove_file(&path);
            continue;
        };
        if state.provider_kind().as_ref() != Some(provider) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ removing inflight state with provider mismatch: {}",
                path.display()
            );
            let _ = fs::remove_file(&path);
            continue;
        }
        states.push(state);
    }
    states
}

#[cfg(test)]
mod tests {
    use super::{
        InflightTurnState, latest_request_owner_user_id_for_channel,
        load_inflight_states_from_root, save_inflight_state_in_root,
    };
    use crate::services::provider::ProviderKind;
    use tempfile::TempDir;

    #[test]
    fn test_save_and_load_inflight_state() {
        let temp = TempDir::new().unwrap();

        let state = InflightTurnState::new(
            ProviderKind::Codex,
            123,
            Some("adk-cdx".to_string()),
            456,
            789,
            999,
            "hello".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            42,
        );
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].channel_id, 123);
        assert_eq!(loaded[0].current_msg_id, 999);
        assert_eq!(loaded[0].last_offset, 42);
        assert_eq!(loaded[0].turn_start_offset, Some(42));
    }

    #[test]
    fn latest_request_owner_user_id_prefers_most_recent_state_across_providers() {
        let temp = TempDir::new().unwrap();
        let inflight_root = temp.path().join("runtime").join("discord_inflight");

        let mut claude_state = InflightTurnState::new(
            ProviderKind::Claude,
            123,
            Some("adk-cc".to_string()),
            111,
            789,
            999,
            "hello".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        claude_state.updated_at = "2026-04-11 00:00:00".to_string();
        save_inflight_state_in_root(&inflight_root, &claude_state).unwrap();

        let mut codex_state = InflightTurnState::new(
            ProviderKind::Codex,
            123,
            Some("adk-cdx".to_string()),
            222,
            790,
            1000,
            "world".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        codex_state.updated_at = "2026-04-11 00:00:05".to_string();
        save_inflight_state_in_root(&inflight_root, &codex_state).unwrap();

        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        let owner = latest_request_owner_user_id_for_channel(123);
        match previous {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }

        assert_eq!(owner, Some(222));
    }
}
