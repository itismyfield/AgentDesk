//! Dormant handback adapter using the existing enqueue policy and queue wire format.

use super::pending_queue_persistence::{PendingQueueItem, pending_queue_item_to_intervention};
use super::queue_enqueue::enqueue_with_settlement;
use crate::services::discord::runtime_store;
use crate::services::provider::ProviderKind;
use crate::services::tui_input::handover::EnqueueOutcome;
use serde_json::Value;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

pub(crate) struct Destination<'a> {
    pub root: &'a Path,
    pub provider: &'a ProviderKind,
    pub token_hash: &'a str,
    pub channel: u64,
    pub authorized: bool,
    pub active_sources: &'a [u64],
}

fn optional(path: &Path) -> io::Result<Option<Value>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_file() => {
            return Err(io::Error::other("queue source is not a regular file"));
        }
        Ok(_) => (),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
fn ids(item: &PendingQueueItem) -> Vec<u64> {
    let mut ids = item.source_message_ids.clone();
    if !ids.contains(&item.message_id) {
        ids.push(item.message_id);
    }
    ids
}
fn sync_existing(path: &Path) -> io::Result<()> {
    if path.exists() {
        std::fs::File::open(path)?.sync_all()?;
        runtime_store::fsync_parent_dir(path)?;
    }
    Ok(())
}

pub(crate) fn enqueue(destination: &Destination<'_>, input: &Value) -> io::Result<EnqueueOutcome> {
    if !destination.authorized
        || destination.channel == 0
        || !runtime_store::PARENT_DIR_FSYNC_FLUSHES
    {
        return Ok(EnqueueOutcome::Rejected);
    }
    let mut token = Path::new(destination.token_hash).components();
    if !matches!(token.next(), Some(std::path::Component::Normal(_))) || token.next().is_some() {
        return Err(io::Error::other("invalid queue token"));
    }
    let base: PathBuf = destination
        .root
        .join("discord_pending_queue")
        .join(destination.provider.as_str())
        .join(destination.token_hash);
    let path = base.join(format!("{}.json", destination.channel));
    let marker_path = base.join(format!("{}.dispatch", destination.channel));
    let original = optional(&path)?.unwrap_or_else(|| serde_json::json!([]));
    let items: Vec<PendingQueueItem> = serde_json::from_value(original.clone())?;
    let valid = |item: &PendingQueueItem| {
        item.author_id != 0
            && item.message_id != 0
            && !ids(item).contains(&0)
            && item
                .channel_id
                .is_none_or(|channel| channel == destination.channel)
    };
    let incoming: PendingQueueItem = serde_json::from_value(input.clone())?;
    if !valid(&incoming) || items.iter().any(|item| !valid(item)) {
        return Ok(EnqueueOutcome::Rejected);
    }
    let marker: Option<PendingQueueItem> = optional(&marker_path)?
        .map(serde_json::from_value)
        .transpose()?;
    let existing: Vec<u64> = items.iter().flat_map(ids).collect();
    let preserved: Vec<u64> = existing
        .iter()
        .copied()
        .chain(marker.iter().flat_map(ids))
        .chain(destination.active_sources.iter().copied())
        .collect();
    let incoming_ids = ids(&incoming);
    if incoming_ids.iter().all(|id| preserved.contains(id)) {
        sync_existing(&path)?;
        sync_existing(&marker_path)?;
        return Ok(EnqueueOutcome::AlreadyPreserved);
    }
    // Partial coverage must not consume unseen sources as an AlreadyPreserved refusal.
    if incoming_ids.iter().any(|id| preserved.contains(id)) {
        return Ok(EnqueueOutcome::Rejected);
    }
    let now = SystemTime::now();
    let instant = Instant::now();
    let mut queue: Vec<_> = items
        .into_iter()
        .map(|item| pending_queue_item_to_intervention(item, now, instant))
        .collect();
    let incoming = pending_queue_item_to_intervention(incoming, now, instant);
    let result = enqueue_with_settlement(&mut queue, incoming, None, None);
    if !result.enqueued || !result.queue_exit_events.is_empty() {
        return Ok(EnqueueOutcome::Rejected);
    }
    // Policy validates admission, but handback cannot merge or rewrite the existing tail.
    let mut expected: Vec<PendingQueueItem> = serde_json::from_value(original.clone())?;
    expected.push(serde_json::from_value(input.clone())?);
    let expected: Vec<_> = expected.iter().map(ids).collect();
    let resulting: Vec<Vec<u64>> = queue
        .iter()
        .map(|item| {
            let mut sources: Vec<_> = item.source_message_ids.iter().map(|id| id.get()).collect();
            if !sources.contains(&item.message_id.get()) {
                sources.push(item.message_id.get());
            }
            sources
        })
        .collect();
    if resulting != expected {
        return Ok(EnqueueOutcome::Rejected);
    }
    let mut payload = original;
    payload
        .as_array_mut()
        .ok_or_else(|| io::Error::other("queue is not an array"))?
        .push(input.clone());
    let payload = serde_json::to_string(&payload)?;
    runtime_store::critical_atomic_write(
        &path,
        &payload,
        runtime_store::AtomicWriteContext::new("discord_pending_queue")
            .provider(destination.provider.as_str())
            .token_hash(destination.token_hash)
            .channel_id(destination.channel),
    )
    .map_err(io::Error::other)?;
    runtime_store::fsync_parent_dir(&path)?;
    Ok(EnqueueOutcome::Persisted)
}

#[cfg(test)]
#[path = "input_handback_tests.rs"]
mod tests;
