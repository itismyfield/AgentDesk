//! Durable debt journal for continuation-message rollback.

use poise::serenity_prelude::{ChannelId, MessageId};
use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

// This mutex serializes both the process-local rollback map and every sidecar
// mutation for the same protocol: atomic writes, clears, empty-marker writes,
// and claim-side empty-marker GC. Keeping the fs operation under the same lock
// prevents a claimer from deleting a freshly-written durable debt.
pub(super) static REPLACE_CONTINUATION_ROLLBACKS: LazyLock<
    Mutex<HashMap<ReplaceContinuationRollbackKey, ReplaceContinuationRollback>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
static REPLACE_CONTINUATION_ROLLBACK_FORCED_REMOVE_FAILURES: LazyLock<
    Mutex<HashSet<ReplaceContinuationRollbackKey>>,
> = LazyLock::new(|| Mutex::new(HashSet::new()));

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReplaceContinuationRollback {
    message_ids: Vec<u64>,
    claimed: bool,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct PersistedReplaceContinuationRollback {
    message_ids: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PersistReplaceContinuationRollbackOutcome {
    Recorded,
    Removed,
    ClearedMarkerWritten,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ReplaceContinuationRollbackClaim {
    None,
    Owner(Vec<u64>),
    InProgress(Vec<u64>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct ReplaceContinuationRollbackKey {
    channel_id: u64,
    anchor_message_id: u64,
    response_turn_key: Option<String>,
}

fn remove_replace_continuation_rollback_file(
    _key: &ReplaceContinuationRollbackKey,
    path: &PathBuf,
) -> std::io::Result<()> {
    #[cfg(test)]
    {
        if REPLACE_CONTINUATION_ROLLBACK_FORCED_REMOVE_FAILURES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(_key)
        {
            return Err(std::io::Error::other("forced rollback remove failure"));
        }
    }
    fs::remove_file(path)
}

#[cfg(test)]
pub(super) fn force_next_replace_continuation_rollback_remove_failure(
    key: &ReplaceContinuationRollbackKey,
) {
    REPLACE_CONTINUATION_ROLLBACK_FORCED_REMOVE_FAILURES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(key.clone());
}

pub(super) fn replace_continuation_rollback_key(
    channel_id: ChannelId,
    message_id: MessageId,
) -> ReplaceContinuationRollbackKey {
    ReplaceContinuationRollbackKey {
        channel_id: channel_id.get(),
        anchor_message_id: message_id.get(),
        response_turn_key: None,
    }
}

pub(super) fn task_response_continuation_rollback_key(
    channel_id: ChannelId,
    message_id: MessageId,
    response_turn_key: &str,
) -> ReplaceContinuationRollbackKey {
    ReplaceContinuationRollbackKey {
        channel_id: channel_id.get(),
        anchor_message_id: message_id.get(),
        response_turn_key: Some(response_turn_key.to_string()),
    }
}

fn replace_continuation_rollback_root() -> Option<PathBuf> {
    crate::config::runtime_root().map(|root| {
        root.join("runtime")
            .join("discord_replace_continuation_rollbacks")
    })
}

pub(super) fn replace_continuation_rollback_path(
    key: &ReplaceContinuationRollbackKey,
) -> Option<PathBuf> {
    let file_name = match &key.response_turn_key {
        Some(response_turn_key) => {
            format!("{}.{}.json", key.anchor_message_id, response_turn_key)
        }
        None => format!("{}.json", key.anchor_message_id),
    };
    replace_continuation_rollback_root()
        .map(|root| root.join(key.channel_id.to_string()).join(file_name))
}

fn load_persisted_replace_continuation_rollback(
    key: &ReplaceContinuationRollbackKey,
) -> Option<Vec<u64>> {
    let path = replace_continuation_rollback_path(key)?;
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            // Non-UTF8 means the sidecar content is corrupt, just like a JSON
            // parse failure below. Remove it so a bad-content file cannot warn
            // forever on every future claim attempt.
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "failed to decode continuation rollback sidecar; removing corrupt sidecar and treating as no debt"
            );
            let _ = fs::remove_file(&path);
            return None;
        }
        Err(error) => {
            // Fail open WITHOUT removing the file: a read error (EIO, fd
            // exhaustion, EACCES) is transient-environment evidence, not
            // corruption evidence — removing here would permanently destroy
            // valid debt that a later claim could still read. Only the parse
            // arm below (unparseable content = a genuinely bad file) removes.
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "failed to read continuation rollback sidecar; leaving file in place and treating as no debt for this claim"
            );
            return None;
        }
    };
    let persisted: PersistedReplaceContinuationRollback = match serde_json::from_str(&content) {
        Ok(persisted) => persisted,
        Err(error) => {
            // Fail open for corrupt sidecars: runtime_store::atomic_write uses a
            // temp file, fsync, and same-dir rename, so torn files cannot be
            // self-produced. Treating corrupt data as debt would reintroduce the
            // r4 permanent send-block without a bounded probe.
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "failed to parse continuation rollback sidecar; removing corrupt sidecar and treating as no debt"
            );
            let _ = fs::remove_file(&path);
            return None;
        }
    };
    if persisted.message_ids.is_empty() {
        let _ = fs::remove_file(&path);
        return None;
    }
    (!persisted.message_ids.is_empty()).then_some(persisted.message_ids)
}

fn replace_continuation_rollback_cleared_marker() -> Result<String, String> {
    serde_json::to_string_pretty(&PersistedReplaceContinuationRollback {
        message_ids: Vec::new(),
    })
    .map_err(|error| format!("serialize cleared continuation rollback marker: {error}"))
}

fn persist_replace_continuation_rollback(
    key: &ReplaceContinuationRollbackKey,
    message_ids: &[u64],
) -> Result<PersistReplaceContinuationRollbackOutcome, String> {
    let Some(path) = replace_continuation_rollback_path(key) else {
        return Err("runtime root unavailable for continuation rollback".to_string());
    };
    if message_ids.is_empty() {
        match remove_replace_continuation_rollback_file(key, &path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                let cleared_marker = replace_continuation_rollback_cleared_marker()?;
                crate::services::discord::runtime_store::atomic_write(&path, &cleared_marker)
                    .map_err(|write_error| {
                        format!(
                            "remove continuation rollback {}: {error}; write cleared marker failed: {write_error}",
                            path.display()
                        )
                    })?;
                return Ok(PersistReplaceContinuationRollbackOutcome::ClearedMarkerWritten);
            }
        }
        return Ok(PersistReplaceContinuationRollbackOutcome::Removed);
    }
    let persisted = PersistedReplaceContinuationRollback {
        message_ids: message_ids.to_vec(),
    };
    let json = serde_json::to_string_pretty(&persisted)
        .map_err(|error| format!("serialize continuation rollback: {error}"))?;
    crate::services::discord::runtime_store::atomic_write(&path, &json)?;
    Ok(PersistReplaceContinuationRollbackOutcome::Recorded)
}

pub(super) fn claim_replace_continuation_rollback(
    key: &ReplaceContinuationRollbackKey,
) -> ReplaceContinuationRollbackClaim {
    let mut rollbacks = REPLACE_CONTINUATION_ROLLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let rollback = match rollbacks.get_mut(key) {
        Some(rollback) if rollback.message_ids.is_empty() => {
            return ReplaceContinuationRollbackClaim::None;
        }
        Some(rollback) => rollback,
        None => {
            let Some(message_ids) = load_persisted_replace_continuation_rollback(key) else {
                return ReplaceContinuationRollbackClaim::None;
            };
            rollbacks.insert(
                key.clone(),
                ReplaceContinuationRollback {
                    message_ids,
                    claimed: true,
                },
            );
            return ReplaceContinuationRollbackClaim::Owner(
                rollbacks
                    .get(key)
                    .map(|entry| entry.message_ids.clone())
                    .unwrap_or_default(),
            );
        }
    };
    if rollback.claimed {
        ReplaceContinuationRollbackClaim::InProgress(rollback.message_ids.clone())
    } else {
        rollback.claimed = true;
        ReplaceContinuationRollbackClaim::Owner(rollback.message_ids.clone())
    }
}

pub(super) fn record_replace_continuation_rollback(
    key: &ReplaceContinuationRollbackKey,
    message_ids: Vec<u64>,
) -> Result<(), String> {
    let mut rollbacks = REPLACE_CONTINUATION_ROLLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let outcome = persist_replace_continuation_rollback(key, &message_ids)?;
    if message_ids.is_empty() {
        match outcome {
            PersistReplaceContinuationRollbackOutcome::ClearedMarkerWritten => {
                rollbacks.insert(
                    key.clone(),
                    ReplaceContinuationRollback {
                        message_ids: Vec::new(),
                        claimed: false,
                    },
                );
            }
            PersistReplaceContinuationRollbackOutcome::Removed
            | PersistReplaceContinuationRollbackOutcome::Recorded => {
                rollbacks.remove(key);
            }
        }
    } else {
        rollbacks.insert(
            key.clone(),
            ReplaceContinuationRollback {
                message_ids,
                claimed: false,
            },
        );
    }
    Ok(())
}

pub(super) fn record_replace_continuation_rollback_memory_only(
    key: &ReplaceContinuationRollbackKey,
    message_ids: Vec<u64>,
) {
    if message_ids.is_empty() {
        return;
    }
    REPLACE_CONTINUATION_ROLLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(
            key.clone(),
            ReplaceContinuationRollback {
                message_ids,
                claimed: false,
            },
        );
}

pub(super) fn clear_replace_continuation_rollback_memory_only(
    key: &ReplaceContinuationRollbackKey,
) {
    REPLACE_CONTINUATION_ROLLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(
            key.clone(),
            ReplaceContinuationRollback {
                message_ids: Vec::new(),
                claimed: false,
            },
        );
}

pub(super) fn clear_replace_continuation_rollback(
    key: &ReplaceContinuationRollbackKey,
) -> Result<(), String> {
    let mut rollbacks = REPLACE_CONTINUATION_ROLLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    match persist_replace_continuation_rollback(key, &[])? {
        PersistReplaceContinuationRollbackOutcome::ClearedMarkerWritten => {
            rollbacks.insert(
                key.clone(),
                ReplaceContinuationRollback {
                    message_ids: Vec::new(),
                    claimed: false,
                },
            );
        }
        PersistReplaceContinuationRollbackOutcome::Removed
        | PersistReplaceContinuationRollbackOutcome::Recorded => {
            rollbacks.remove(key);
        }
    }
    Ok(())
}

pub(super) fn unclaim_replace_continuation_rollback(key: &ReplaceContinuationRollbackKey) {
    if let Some(rollback) = REPLACE_CONTINUATION_ROLLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get_mut(key)
    {
        rollback.claimed = false;
    }
}
