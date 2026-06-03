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
