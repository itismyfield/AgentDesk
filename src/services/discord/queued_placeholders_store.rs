//! #1332 round-3 codex review P2: persistence for the `queued_placeholders`
//! handoff map.
//!
//! The mailbox queue is already saved/restored across restarts via
//! `turn_orchestrator::{save,load}_pending_queues`, but the
//! `queued_placeholders` map (linking a mailbox-queued user message id to the
//! Discord placeholder message id displaying `📬 메시지 대기 중`) was previously
//! in-memory only. On dcserver restart while a foreground message was queued,
//! the visible queued card stayed in Discord but the restored queue had no
//! placeholder id to consume — the dispatch path then posted a fresh
//! placeholder, leaving the old `📬` card stale forever.
//!
//! This module mirrors the directory layout of `discord_pending_queue/` so the
//! restart path can iterate channels in parallel:
//!
//! ```text
//! runtime/discord_queued_placeholders/<provider>/<token_hash>/<channel_id>.json
//! ```
//!
//! Each file holds a JSON array of `{user_message_id, placeholder_message_id}`
//! pairs scoped to that channel. Writes use the same temp-file + rename
//! pattern (`runtime_store::atomic_write`) as the queue snapshot so a crash
//! mid-write cannot corrupt the file.
//!
//! Write-through is invoked from the same call sites that mutate the in-memory
//! `DashMap` (`insert`, `remove`, drain helpers), so the persisted state stays
//! a tight superset/subset of memory and `load_queued_placeholders` only
//! returns mappings whose corresponding queue file still exists at boot.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use poise::serenity_prelude::{ChannelId, MessageId};
use serde::{Deserialize, Serialize};

use crate::services::discord::runtime_store;
use crate::services::provider::ProviderKind;

/// Wire format for a single queued-placeholder mapping. Stored as a JSON
/// array of these entries, one file per channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct QueuedPlaceholderEntry {
    pub(super) user_message_id: u64,
    pub(super) placeholder_message_id: u64,
}

fn store_root() -> Option<PathBuf> {
    runtime_store::discord_queued_placeholders_root()
}

fn pending_clear_store_root() -> Option<PathBuf> {
    runtime_store::discord_queue_exit_placeholder_clears_root()
}

fn channel_file_path(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
) -> Option<PathBuf> {
    store_root().map(|root| {
        root.join(provider.as_str())
            .join(token_hash)
            .join(format!("{}.json", channel_id.get()))
    })
}

fn pending_clear_channel_file_path(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
) -> Option<PathBuf> {
    pending_clear_store_root().map(|root| {
        root.join(provider.as_str())
            .join(token_hash)
            .join(format!("{}.json", channel_id.get()))
    })
}

/// Snapshot every mapping for a single channel and write it through to disk.
/// Empty channels remove the file so the load path returns nothing for them.
///
/// `entries` is `(user_msg_id, placeholder_msg_id)` pairs.
pub(super) fn save_channel_queued_placeholders(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
    entries: &[(MessageId, MessageId)],
) {
    let Some(path) = channel_file_path(provider, token_hash, channel_id) else {
        return;
    };
    if entries.is_empty() {
        let _ = fs::remove_file(&path);
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let payload: Vec<QueuedPlaceholderEntry> = entries
        .iter()
        .map(|(user_msg_id, placeholder_msg_id)| QueuedPlaceholderEntry {
            user_message_id: user_msg_id.get(),
            placeholder_message_id: placeholder_msg_id.get(),
        })
        .collect();
    if let Ok(json) = serde_json::to_string_pretty(&payload) {
        let _ = runtime_store::atomic_write(&path, &json);
    }
}

fn save_entries_checked(
    path: Option<PathBuf>,
    entries: &[(MessageId, MessageId)],
) -> Result<(), String> {
    let path = path.ok_or_else(|| "AgentDesk runtime root unavailable".to_owned())?;
    if entries.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => runtime_store::fsync_parent_dir(&path).map_err(|error| error.to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    } else {
        let payload: Vec<_> = entries
            .iter()
            .map(|(user, placeholder)| QueuedPlaceholderEntry {
                user_message_id: user.get(),
                placeholder_message_id: placeholder.get(),
            })
            .collect();
        let json = serde_json::to_string_pretty(&payload).map_err(|error| error.to_string())?;
        runtime_store::atomic_write(&path, &json)?;
        runtime_store::fsync_parent_dir(&path).map_err(|error| error.to_string())
    }
}

fn snapshot_map(
    map: &dashmap::DashMap<(ChannelId, MessageId), MessageId>,
    channel_id: ChannelId,
) -> Vec<(MessageId, MessageId)> {
    map.iter()
        .filter_map(|kv| {
            let (channel, user) = *kv.key();
            (channel == channel_id).then_some((user, *kv.value()))
        })
        .collect()
}

fn save_channel_entries(path: Option<PathBuf>, entries: &[(MessageId, MessageId)]) {
    let Some(path) = path else {
        return;
    };
    if entries.is_empty() {
        let _ = fs::remove_file(&path);
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let payload: Vec<QueuedPlaceholderEntry> = entries
        .iter()
        .map(|(user_msg_id, placeholder_msg_id)| QueuedPlaceholderEntry {
            user_message_id: user_msg_id.get(),
            placeholder_message_id: placeholder_msg_id.get(),
        })
        .collect();
    if let Ok(json) = serde_json::to_string_pretty(&payload) {
        let _ = runtime_store::atomic_write(&path, &json);
    }
}

pub(super) fn save_channel_queue_exit_placeholder_clears(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
    entries: &[(MessageId, MessageId)],
) {
    save_channel_entries(
        pending_clear_channel_file_path(provider, token_hash, channel_id),
        entries,
    );
}

/// Load every persisted mapping under this bot's namespace and return them as
/// a `(channel_id, user_msg_id) -> placeholder_msg_id` map ready for direct
/// import into `SharedData::queued_placeholders`.
///
/// On read error or stale file, the file is removed so a future write starts
/// from a clean slate.
pub(super) fn load_queued_placeholders(
    provider: &ProviderKind,
    token_hash: &str,
) -> HashMap<(ChannelId, MessageId), MessageId> {
    load_entries(store_root(), provider, token_hash)
}

pub(super) fn load_channel_queued_placeholders_checked(
    provider: &ProviderKind,
    token_hash: &str,
    channel: ChannelId,
) -> Result<HashMap<(ChannelId, MessageId), MessageId>, String> {
    #[cfg(test)]
    if crate::services::discord::inflight::scoped_restore_mutant("post-filter") {
        return Ok(load_queued_placeholders(provider, token_hash)
            .into_iter()
            .filter(|((id, _), _)| *id == channel)
            .collect());
    }
    let path =
        channel_file_path(provider, token_hash, channel).ok_or("placeholder root unavailable")?;
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(error.to_string()),
    };
    let entries: Vec<QueuedPlaceholderEntry> =
        serde_json::from_str(&content).map_err(|error| error.to_string())?;
    let mut loaded = HashMap::new();
    for entry in entries {
        if entry.user_message_id == 0 || entry.placeholder_message_id == 0 {
            return Err("zero placeholder identity".into());
        }
        let key = (channel, MessageId::new(entry.user_message_id));
        let card = MessageId::new(entry.placeholder_message_id);
        if loaded
            .insert(key, card)
            .is_some_and(|previous| previous != card)
        {
            return Err("conflicting persisted placeholder owner".into());
        }
    }
    Ok(loaded)
}

pub(super) fn persist_channel_from_map_checked(
    map: &dashmap::DashMap<(ChannelId, MessageId), MessageId>,
    provider: &ProviderKind,
    token_hash: &str,
    channel: ChannelId,
) -> Result<(), String> {
    let result = save_entries_checked(
        channel_file_path(provider, token_hash, channel),
        &snapshot_map(map, channel),
    );
    #[cfg(test)]
    if crate::services::discord::inflight::scoped_restore_mutant("persist-error-complete") {
        return Ok(());
    }
    result
}

pub(super) fn load_queue_exit_placeholder_clears(
    provider: &ProviderKind,
    token_hash: &str,
) -> HashMap<(ChannelId, MessageId), MessageId> {
    load_entries(pending_clear_store_root(), provider, token_hash)
}

fn load_entries(
    root: Option<PathBuf>,
    provider: &ProviderKind,
    token_hash: &str,
) -> HashMap<(ChannelId, MessageId), MessageId> {
    let mut result: HashMap<(ChannelId, MessageId), MessageId> = HashMap::new();
    let Some(root) = root else {
        return result;
    };
    let dir = root.join(provider.as_str()).join(token_hash);
    let Ok(entries) = fs::read_dir(&dir) else {
        return result;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(channel_id) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok())
            .map(ChannelId::new)
        else {
            continue;
        };
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(items) = serde_json::from_str::<Vec<QueuedPlaceholderEntry>>(&content) else {
            // Malformed file — drop it so future writes succeed cleanly.
            let _ = fs::remove_file(&path);
            continue;
        };
        for item in items {
            result.insert(
                (channel_id, MessageId::new(item.user_message_id)),
                MessageId::new(item.placeholder_message_id),
            );
        }
    }
    result
}

/// Snapshot every in-memory mapping for `channel_id` from a `DashMap` and
/// persist it. Used as the write-through helper after each insert/remove.
pub(super) fn persist_channel_from_map(
    map: &dashmap::DashMap<(ChannelId, MessageId), MessageId>,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
) {
    let entries: Vec<(MessageId, MessageId)> = map
        .iter()
        .filter_map(|kv| {
            let (ch, user) = *kv.key();
            if ch == channel_id {
                Some((user, *kv.value()))
            } else {
                None
            }
        })
        .collect();
    save_channel_queued_placeholders(provider, token_hash, channel_id, &entries);
}

pub(super) async fn persist_map_on_worker(
    map: &dashmap::DashMap<(ChannelId, MessageId), MessageId>,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
    pending_clear: bool,
) {
    if pending_clear {
        persist_queue_exit_placeholder_clears_channel_from_map(
            map, provider, token_hash, channel_id,
        );
    } else {
        persist_channel_from_map(map, provider, token_hash, channel_id);
    }
}

pub(super) fn persist_queue_exit_placeholder_clears_channel_from_map(
    map: &dashmap::DashMap<(ChannelId, MessageId), MessageId>,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
) {
    let entries: Vec<(MessageId, MessageId)> = map
        .iter()
        .filter_map(|kv| {
            let (ch, user) = *kv.key();
            if ch == channel_id {
                Some((user, *kv.value()))
            } else {
                None
            }
        })
        .collect();
    save_channel_queue_exit_placeholder_clears(provider, token_hash, channel_id, &entries);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn placeholder_snapshots_match_and_malformed_or_empty_files_are_removed() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(6_325_307);
        let entries = [(MessageId::new(8), MessageId::new(108))];
        let path = channel_file_path(&provider, "off", channel).unwrap();
        save_channel_queued_placeholders(&provider, "off", channel, &entries);
        let expected = fs::read(&path).unwrap();
        let map = dashmap::DashMap::new();
        map.insert((channel, entries[0].0), entries[0].1);
        persist_channel_from_map(&map, &provider, "off", channel);
        assert_eq!(fs::read(&path).unwrap(), expected);
        save_channel_queue_exit_placeholder_clears(&provider, "off", channel, &entries);
        let clear = pending_clear_channel_file_path(&provider, "off", channel).unwrap();
        assert_eq!(fs::read(&clear).unwrap(), expected);
        persist_queue_exit_placeholder_clears_channel_from_map(&map, &provider, "off", channel);
        assert_eq!(fs::read(&clear).unwrap(), expected);
        fs::write(&path, b"invalid").unwrap();
        assert!(load_queued_placeholders(&provider, "off").is_empty());
        assert!(!path.exists());
        save_channel_queued_placeholders(&provider, "off", channel, &[]);
        save_channel_queue_exit_placeholder_clears(&provider, "off", channel, &[]);
        assert!(!clear.exists());
    }
}
