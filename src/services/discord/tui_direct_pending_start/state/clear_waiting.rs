//! Explicit clear revokes only the selected legacy waiting anchors, never a proved episode.
use super::*;
use poise::serenity_prelude::MessageId;
use std::collections::BTreeSet;

static CLEARED: LazyLock<Mutex<HashSet<AnchorKey>>> = LazyLock::new(Default::default);

#[cfg(test)]
type BeforeUnlinkHook = Box<dyn FnOnce(&std::path::Path)>;
#[cfg(test)]
thread_local! {
    static BEFORE_UNLINK: std::cell::RefCell<Option<BeforeUnlinkHook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
#[path = "clear_waiting/clear_waiting_tests.rs"]
mod tests;

pub(super) fn cleared(key: &AnchorKey) -> bool {
    CLEARED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(key)
}

pub(in crate::services::discord::tui_direct_pending_start) fn take_retired(
    record: &TuiDirectPendingStart,
) -> bool {
    if cleared(&record.key()) {
        return true;
    }
    let key = record.key();
    let was_retired = retired().remove(&key);
    if was_retired {
        tracing::info!(
            ?key,
            "tui_direct_pending_start: retired record's worker exits"
        );
    }
    was_retired
}

pub(in crate::services::discord) fn discard_legacy_waiting(
    provider: &str,
    channel: u64,
    logical: &str,
    excluded: &BTreeSet<MessageId>,
) -> Result<Vec<MessageId>, String> {
    let _guard = PRESENCE_RECONCILE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let root = root().ok_or("pending root unavailable")?;
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let prefix = format!("{provider}_{channel}_");
    let mut discarded = Vec::new();
    let mut errors = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                errors.push(e.to_string());
                continue;
            }
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) || !name.ends_with(".json") {
            continue;
        }
        let read = std::fs::read_to_string(entry.path())
            .map_err(|e| e.to_string())
            .and_then(|raw| {
                serde_json::from_str::<TuiDirectPendingStart>(&raw).map_err(|e| e.to_string())
            });
        let record = match read {
            Ok(record) => record,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };
        if record.provider != provider
            || record.channel_id != channel
            || record.tmux_session_name != logical
            || record.anchor_message_id == 0
            || record.hosted_source_checked
            || record.hosted_source.is_some()
            || record.captured_source.is_none()
            || excluded.contains(&MessageId::new(record.anchor_message_id))
            || name != format!("{}.json", record.file_stem())
        {
            continue;
        }
        CLEARED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(record.key());
        mark_absent(&record.key());
        super::super::release_prompt_anchor_slot(&record);
        #[cfg(test)]
        BEFORE_UNLINK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook(&entry.path());
            }
        });
        let unlink = || {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            crate::services::discord::runtime_store::fsync_parent_dir(&entry.path())
        };
        let removed = unlink().or_else(|e| {
            tracing::warn!(pending_id = record.anchor_message_id, error = %e, "clear pending unlink failed; retrying once");
            unlink()
        });
        match removed {
            Ok(()) => {
                discarded.push(MessageId::new(record.anchor_message_id));
                tracing::info!(
                    channel_id = channel,
                    pending_id = record.anchor_message_id,
                    "clear discarded legacy pending input"
                );
            }
            Err(e) => errors.push(e.to_string()),
        }
    }
    if errors.is_empty() {
        Ok(discarded)
    } else {
        Err(errors.join("; "))
    }
}
