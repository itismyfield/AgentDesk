use std::fs;
use std::path::{Path, PathBuf};

use poise::serenity_prelude::ChannelId;
use serde::{Deserialize, Serialize};

use super::runtime_store::{atomic_write, shared_agent_memory_root};
use crate::services::provider::ProviderKind;

const SHARED_MEMORY_VERSION: u32 = 1;
const MAX_STORED_TURNS: usize = 40;
const MAX_CONTEXT_TURNS: usize = 3;
const MAX_CONTEXT_CHARS: usize = 3_000;
const MAX_STORED_USER_CHARS: usize = 1_500;
const MAX_STORED_ASSISTANT_CHARS: usize = 4_000;
const MAX_CONTEXT_USER_CHARS: usize = 280;
const MAX_CONTEXT_ASSISTANT_CHARS: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SharedAgentTurn {
    created_at: String,
    provider: String,
    channel_id: u64,
    channel_name: Option<String>,
    current_path: String,
    #[serde(default)]
    user_name: Option<String>,
    user: String,
    assistant: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SharedAgentMemoryStore {
    version: u32,
    role_id: String,
    updated_at: String,
    turns: Vec<SharedAgentTurn>,
}

fn shared_agent_memory_path(root: &Path, role_id: &str) -> PathBuf {
    root.join(format!("{role_id}.json"))
}

fn now_string() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    let trimmed = input.trim();
    let count = trimmed.chars().count();
    if count <= max_chars {
        return trimmed.to_string();
    }

    let take = max_chars.saturating_sub(3);
    let mut truncated: String = trimmed.chars().take(take).collect();
    truncated.push_str("...");
    truncated
}

fn normalize_for_storage(input: &str, max_chars: usize) -> String {
    let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
    truncate_chars(&normalized, max_chars)
}

fn load_store_from_path(path: &Path, role_id: &str) -> SharedAgentMemoryStore {
    if let Ok(content) = fs::read_to_string(path) {
        if let Ok(store) = serde_json::from_str::<SharedAgentMemoryStore>(&content) {
            if store.role_id == role_id {
                return store;
            }
        }
    }

    SharedAgentMemoryStore {
        version: SHARED_MEMORY_VERSION,
        role_id: role_id.to_string(),
        updated_at: now_string(),
        turns: Vec::new(),
    }
}

fn save_store_to_path(path: &Path, store: &SharedAgentMemoryStore) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    atomic_write(path, &json)
}

fn build_context_from_store(
    store: &SharedAgentMemoryStore,
    provider: &ProviderKind,
    channel_id: ChannelId,
    has_provider_session: bool,
    last_injected_ts: Option<&str>,
) -> Option<String> {
    let relevant: Vec<&SharedAgentTurn> = store
        .turns
        .iter()
        .filter(|turn| {
            // Skip turns already injected in this session
            if let Some(ts) = last_injected_ts {
                if turn.created_at.as_str() <= ts {
                    return false;
                }
            }

            if !has_provider_session {
                return true;
            }

            turn.provider != provider.as_str() || turn.channel_id != channel_id.get()
        })
        .collect();

    if relevant.is_empty() {
        return None;
    }

    let start = relevant.len().saturating_sub(MAX_CONTEXT_TURNS);
    let mut sections = vec![
        "[Shared Agent Memory]".to_string(),
        format!(
            "The following notes are prior execution memory for the same role `{}` across Claude/Codex channels.",
            store.role_id
        ),
        "Treat them as working memory for the same agent, but verify any detail that may be stale against the repository/files before acting.".to_string(),
        String::new(),
    ];

    for turn in &relevant[start..] {
        let channel_label = turn
            .channel_name
            .as_deref()
            .map(|name| format!("#{name}"))
            .unwrap_or_else(|| format!("channel:{}", turn.channel_id));
        sections.push(format!(
            "[{}] provider={} channel={} path={}",
            turn.created_at, turn.provider, channel_label, turn.current_path
        ));
        let user_label = turn
            .user_name
            .as_deref()
            .map(|name| format!("User({name}): "))
            .unwrap_or_else(|| "User: ".to_string());
        sections.push(format!(
            "{}{}",
            user_label,
            truncate_chars(&turn.user, MAX_CONTEXT_USER_CHARS)
        ));
        sections.push(format!(
            "Assistant: {}",
            truncate_chars(&turn.assistant, MAX_CONTEXT_ASSISTANT_CHARS)
        ));
        sections.push(String::new());
    }

    let mut rendered = sections.join("\n");
    if rendered.chars().count() > MAX_CONTEXT_CHARS {
        rendered = truncate_chars(&rendered, MAX_CONTEXT_CHARS);
    }
    Some(rendered)
}

pub(super) fn build_shared_memory_context(
    role_id: &str,
    provider: &ProviderKind,
    channel_id: ChannelId,
    has_provider_session: bool,
    last_injected_ts: Option<&str>,
) -> Option<String> {
    let root = shared_agent_memory_root()?;
    build_shared_memory_context_at_root(
        &root,
        role_id,
        provider,
        channel_id,
        has_provider_session,
        last_injected_ts,
    )
}

fn build_shared_memory_context_at_root(
    root: &Path,
    role_id: &str,
    provider: &ProviderKind,
    channel_id: ChannelId,
    has_provider_session: bool,
    last_injected_ts: Option<&str>,
) -> Option<String> {
    let path = shared_agent_memory_path(&root, role_id);
    let store = load_store_from_path(&path, role_id);
    build_context_from_store(&store, provider, channel_id, has_provider_session, last_injected_ts)
}

/// Returns the timestamp of the newest turn in context, for dedup tracking.
pub(super) fn latest_shared_memory_ts(role_id: &str) -> Option<String> {
    let root = shared_agent_memory_root()?;
    let path = shared_agent_memory_path(&root, role_id);
    let store = load_store_from_path(&path, role_id);
    store.turns.last().map(|t| t.created_at.clone())
}

pub(super) fn append_shared_memory_turn(
    role_id: &str,
    provider: &ProviderKind,
    channel_id: ChannelId,
    channel_name: Option<&str>,
    current_path: &str,
    user_name: Option<&str>,
    user_text: &str,
    assistant_text: &str,
) -> Result<(), String> {
    let Some(root) = shared_agent_memory_root() else {
        return Err("Home directory not found".to_string());
    };
    append_shared_memory_turn_at_root(
        &root,
        role_id,
        provider,
        channel_id,
        channel_name,
        current_path,
        user_name,
        user_text,
        assistant_text,
    )
}

fn append_shared_memory_turn_at_root(
    root: &Path,
    role_id: &str,
    provider: &ProviderKind,
    channel_id: ChannelId,
    channel_name: Option<&str>,
    current_path: &str,
    user_name: Option<&str>,
    user_text: &str,
    assistant_text: &str,
) -> Result<(), String> {
    let path = shared_agent_memory_path(&root, role_id);
    let mut store = load_store_from_path(&path, role_id);

    let entry = SharedAgentTurn {
        created_at: now_string(),
        provider: provider.as_str().to_string(),
        channel_id: channel_id.get(),
        channel_name: channel_name.map(str::to_string),
        current_path: current_path.to_string(),
        user_name: user_name.map(str::to_string),
        user: normalize_for_storage(user_text, MAX_STORED_USER_CHARS),
        assistant: normalize_for_storage(assistant_text, MAX_STORED_ASSISTANT_CHARS),
    };

    let duplicated = store.turns.last().map(|last| {
        last.provider == entry.provider
            && last.channel_id == entry.channel_id
            && last.current_path == entry.current_path
            && last.user == entry.user
            && last.assistant == entry.assistant
    });
    if duplicated == Some(true) {
        return Ok(());
    }

    store.turns.push(entry);
    if store.turns.len() > MAX_STORED_TURNS {
        let overflow = store.turns.len() - MAX_STORED_TURNS;
        store.turns.drain(0..overflow);
    }
    store.updated_at = now_string();
    save_store_to_path(&path, &store)
}

#[cfg(test)]
mod tests {
    use super::{
        append_shared_memory_turn_at_root, build_context_from_store,
        build_shared_memory_context_at_root, load_store_from_path, shared_agent_memory_path,
        SharedAgentMemoryStore,
    };
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::ChannelId;
    use tempfile::TempDir;

    fn temp_store(role_id: &str) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().expect("temp dir");
        let path = shared_agent_memory_path(dir.path(), role_id);
        (dir, path)
    }

    #[test]
    fn context_prefers_other_provider_turns_when_session_exists() {
        let (_dir, path) = temp_store("ch-pmd");
        let mut store = SharedAgentMemoryStore {
            version: 1,
            role_id: "ch-pmd".to_string(),
            updated_at: "2026-03-07 10:00:00".to_string(),
            turns: Vec::new(),
        };
        store.turns.push(super::SharedAgentTurn {
            created_at: "2026-03-07 10:00:00".to_string(),
            provider: "claude".to_string(),
            channel_id: 1,
            channel_name: Some("cookingheart-pm-cc".to_string()),
            current_path: "/repo".to_string(),
            user_name: None,
            user: "same-channel".to_string(),
            assistant: "claude remembered".to_string(),
        });
        store.turns.push(super::SharedAgentTurn {
            created_at: "2026-03-07 10:05:00".to_string(),
            provider: "codex".to_string(),
            channel_id: 2,
            channel_name: Some("cookingheart-pm-cdx".to_string()),
            current_path: "/repo".to_string(),
            user_name: None,
            user: "other-provider".to_string(),
            assistant: "codex remembered".to_string(),
        });
        super::save_store_to_path(&path, &store).expect("save");

        let loaded = load_store_from_path(&path, "ch-pmd");
        let rendered =
            build_context_from_store(&loaded, &ProviderKind::Claude, ChannelId::new(1), true, None)
                .expect("context");

        assert!(rendered.contains("codex remembered"));
        assert!(!rendered.contains("same-channel"));
    }

    #[test]
    fn append_shared_memory_turn_persists_entries() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join(".remotecc").join("shared_agent_memory");
        let result = append_shared_memory_turn_at_root(
            &root,
            "ch-td",
            &ProviderKind::Claude,
            ChannelId::new(10),
            Some("cookingheart-dev-cc"),
            "/repo",
            Some("testuser"),
            "next",
            "follow-up",
        );
        assert!(result.is_ok());
        let saved = load_store_from_path(&root.join("ch-td.json"), "ch-td");
        assert_eq!(saved.turns.len(), 1);
        assert_eq!(saved.turns[0].assistant, "follow-up");
    }

    #[test]
    fn build_shared_memory_context_includes_same_channel_when_no_provider_session() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join(".remotecc").join("shared_agent_memory");
        append_shared_memory_turn_at_root(
            &root,
            "ch-qad",
            &ProviderKind::Codex,
            ChannelId::new(30),
            Some("cookingheart-test-cdx"),
            "/repo",
            Some("tester"),
            "what happened?",
            "I ran the tests.",
        )
        .expect("append");

        let rendered = build_shared_memory_context_at_root(
            &root,
            "ch-qad",
            &ProviderKind::Codex,
            ChannelId::new(30),
            false,
            None,
        )
        .expect("context");

        assert!(rendered.contains("I ran the tests."));
    }
}
