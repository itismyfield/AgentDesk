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

use crate::services::discord::{input_runtime::fence, runtime_store};
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
    if fence::lookup(provider, channel_id.get()).is_none() {
        return save_channel_queued_placeholders_unfenced(
            provider, token_hash, channel_id, entries,
        );
    }
    if let Err(error) = fence::write(provider, channel_id.get(), || {
        save_entries_checked(channel_file_path(provider, token_hash, channel_id), entries)
    }) {
        tracing::warn!(channel_id = channel_id.get(), %error, "input accessory persistence refused");
    }
}

fn save_channel_queued_placeholders_unfenced(
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
    if fence::lookup(provider, channel_id.get()).is_none() {
        return save_channel_queue_exit_placeholder_clears_unfenced(
            provider, token_hash, channel_id, entries,
        );
    }
    if let Err(error) = fence::write(provider, channel_id.get(), || {
        save_entries_checked(
            pending_clear_channel_file_path(provider, token_hash, channel_id),
            entries,
        )
    }) {
        tracing::warn!(channel_id = channel_id.get(), %error, "input accessory persistence refused");
    }
}

fn save_channel_queue_exit_placeholder_clears_unfenced(
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
            if fence::lookup(provider, channel_id.get()).is_none() {
                let _ = fs::remove_file(&path);
            } else {
                let _ = fence::write(provider, channel_id.get(), || {
                    let latest = fs::read_to_string(&path).map_err(|error| error.to_string())?;
                    if latest == content {
                        fs::remove_file(&path).map_err(|error| error.to_string())?;
                        runtime_store::fsync_parent_dir(&path)
                            .map_err(|error| error.to_string())?;
                    }
                    Ok(())
                });
            }
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
    if fence::lookup(provider, channel_id.get()).is_none() {
        return persist_channel_from_map_unfenced(map, provider, token_hash, channel_id);
    }
    if let Err(error) = fence::write(provider, channel_id.get(), || {
        save_entries_checked(
            channel_file_path(provider, token_hash, channel_id),
            &snapshot_map(map, channel_id),
        )
    }) {
        tracing::warn!(channel_id = channel_id.get(), %error, "input accessory persistence refused");
    }
}

fn persist_channel_from_map_unfenced(
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
    if fence::lookup(provider, channel_id.get()).is_none() {
        if pending_clear {
            persist_queue_exit_placeholder_clears_channel_from_map(
                map, provider, token_hash, channel_id,
            );
        } else {
            persist_channel_from_map(map, provider, token_hash, channel_id);
        }
        return;
    }
    let entries = snapshot_map(map, channel_id);
    let provider = provider.clone();
    let token_hash = token_hash.to_owned();
    fence::effect::io(move || {
        if pending_clear {
            save_channel_queue_exit_placeholder_clears(
                &provider,
                &token_hash,
                channel_id,
                &entries,
            );
        } else {
            save_channel_queued_placeholders(&provider, &token_hash, channel_id, &entries);
        }
    })
    .await;
}

pub(super) fn persist_queue_exit_placeholder_clears_channel_from_map(
    map: &dashmap::DashMap<(ChannelId, MessageId), MessageId>,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
) {
    if fence::lookup(provider, channel_id.get()).is_none() {
        return persist_queue_exit_placeholder_clears_channel_from_map_unfenced(
            map, provider, token_hash, channel_id,
        );
    }
    if let Err(error) = fence::write(provider, channel_id.get(), || {
        save_entries_checked(
            pending_clear_channel_file_path(provider, token_hash, channel_id),
            &snapshot_map(map, channel_id),
        )
    }) {
        tracing::warn!(channel_id = channel_id.get(), %error, "input accessory persistence refused");
    }
}

fn persist_queue_exit_placeholder_clears_channel_from_map_unfenced(
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
    #[tokio::test]
    async fn c1_shared_async_accessory_writes_existing_effect_after_closing() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(6_325_413);
        let gate = fence::Gate::protect(shared.provider.clone(), channel.get()).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        fence::effect::scope(Some(gate.admit().unwrap()), async {
            let _closing = gate.close().unwrap();
            let persist_lock = shared.queued_placeholders_persist_lock(channel);
            let guard = persist_lock.lock().await;
            shared
                .insert_queued_placeholder_on_worker_locked(
                    channel,
                    MessageId::new(7),
                    MessageId::new(107),
                )
                .await;
            let card_path =
                channel_file_path(&shared.provider, &shared.token_hash, channel).unwrap();
            let card: Vec<QueuedPlaceholderEntry> =
                serde_json::from_slice(&fs::read(&card_path).unwrap()).unwrap();
            assert_eq!(
                (card[0].user_message_id, card[0].placeholder_message_id),
                (7, 107)
            );
            assert_eq!(
                shared
                    .remove_queued_placeholder_on_worker_locked(channel, MessageId::new(7))
                    .await,
                Some(MessageId::new(107))
            );
            assert!(!card_path.exists());
            drop(guard);
            shared
                .add_pending_queue_exit_placeholder_clear_one(
                    channel,
                    MessageId::new(8),
                    MessageId::new(108),
                )
                .await;
            let path =
                pending_clear_channel_file_path(&shared.provider, &shared.token_hash, channel)
                    .unwrap();
            let entries: Vec<QueuedPlaceholderEntry> =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].user_message_id, 8);
            assert_eq!(entries[0].placeholder_message_id, 108);
            shared
                .remove_pending_queue_exit_placeholder_clears(
                    channel,
                    &[(MessageId::new(8), MessageId::new(108))],
                )
                .await;
            assert!(!path.exists());
        })
        .await;
    }
    #[test]
    fn b2_placeholder_off_delta_zero_and_protected_failures_are_observable() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let provider = ProviderKind::Claude;
        let off = ChannelId::new(6_325_307);
        let entries = [(MessageId::new(8), MessageId::new(108))];
        let path = channel_file_path(&provider, "off", off).unwrap();
        save_channel_queued_placeholders(&provider, "off", off, &entries);
        let expected = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        save_channel_queued_placeholders_unfenced(&provider, "off", off, &entries);
        assert_eq!(fs::read(&path).unwrap(), expected);
        let map = dashmap::DashMap::new();
        map.insert((off, entries[0].0), entries[0].1);
        persist_channel_from_map(&map, &provider, "off", off);
        assert_eq!(fs::read(&path).unwrap(), expected);
        save_channel_queue_exit_placeholder_clears(&provider, "off", off, &entries);
        let clear = pending_clear_channel_file_path(&provider, "off", off).unwrap();
        assert_eq!(fs::read(&clear).unwrap(), expected);
        persist_queue_exit_placeholder_clears_channel_from_map(&map, &provider, "off", off);
        assert_eq!(fs::read(&clear).unwrap(), expected);
        fs::write(&path, b"invalid").unwrap();
        assert!(load_queued_placeholders(&provider, "off").is_empty());
        assert!(!path.exists());
        save_channel_queued_placeholders(&provider, "off", off, &[]);
        save_channel_queue_exit_placeholder_clears(&provider, "off", off, &[]);
        assert!(!clear.exists());
        assert!(
            !root.path().join("runtime/discord_inflight").exists(),
            "off accessories never add a sidecar"
        );
        let channel = ChannelId::new(6_325_308);
        let gate = fence::Gate::protect(provider.clone(), channel.get()).unwrap();
        let _health =
            crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
        let protected = channel_file_path(&provider, "blocked", channel).unwrap();
        fs::create_dir_all(protected.parent().unwrap().parent().unwrap()).unwrap();
        fs::write(protected.parent().unwrap(), b"not a directory").unwrap();
        save_channel_queued_placeholders(&provider, "valid", channel, &entries);
        let valid = channel_file_path(&provider, "valid", channel).unwrap();
        assert_eq!(fs::read(&valid).unwrap(), expected);
        fs::write(&valid, b"invalid").unwrap();
        assert!(load_queued_placeholders(&provider, "valid").is_empty());
        assert!(!valid.exists());
        save_channel_queue_exit_placeholder_clears(&provider, "valid", channel, &entries);
        assert_eq!(
            fs::read(pending_clear_channel_file_path(&provider, "valid", channel).unwrap())
                .unwrap(),
            expected
        );
        save_channel_queued_placeholders(&provider, "blocked", channel, &entries);
        assert!(!protected.exists());
        assert!(
            fence::health_reasons()
                .iter()
                .any(|reason| reason.contains("channel=6325308") && reason.contains("Persistence"))
        );
        gate.clear_failure_for_test();
        let closing = gate.close().unwrap();
        save_channel_queued_placeholders(&provider, "closed", channel, &entries);
        assert!(
            !channel_file_path(&provider, "closed", channel)
                .unwrap()
                .exists()
        );
        gate.clear_failure_for_test();
        drop(closing);
    }
}
