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
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)?
            .sync_all()?;
        runtime_store::fsync_parent_dir(path)?;
    }
    Ok(())
}

pub(crate) fn enqueue(destination: &Destination<'_>, input: &Value) -> io::Result<EnqueueOutcome> {
    if crate::services::discord::input_runtime::fence::lookup(
        destination.provider,
        destination.channel,
    )
    .is_some()
    {
        return Err(io::Error::other(
            "protected handback requires borrowed population guard",
        ));
    }
    enqueue_locked(destination, input)
}

pub(crate) fn enqueue_borrowed(
    destination: &Destination<'_>,
    input: &Value,
    guard: &crate::services::discord::input_runtime::fence::ClosedPopulationGuard,
) -> io::Result<EnqueueOutcome> {
    if input.get("blob_pins").is_some() {
        return Err(io::Error::other(
            "handback must materialize before population lock",
        ));
    }
    guard.borrowed(
        destination.root,
        destination.provider,
        destination.channel,
        || enqueue_locked(destination, input),
    )
}

fn enqueue_locked(destination: &Destination<'_>, input: &Value) -> io::Result<EnqueueOutcome> {
    if !destination.authorized
        || destination.channel == 0
        || !runtime_store::PARENT_DIR_FSYNC_FLUSHES
    {
        return Ok(EnqueueOutcome::Rejected);
    }
    let ledger_input = input;
    let mut input = input.get("legacy_input").unwrap_or(input).clone();
    let Some(fields) = input.as_object_mut() else {
        return Ok(EnqueueOutcome::Rejected);
    };
    #[cfg(test)]
    let clean = !crate::services::tui_input::transition::mutant("raw_queue");
    #[cfg(not(test))]
    let clean = true;
    fields.retain(|key, _| {
        if !clean {
            return true;
        }
        matches!(
            key.as_str(),
            "author_id"
                | "author_is_bot"
                | "message_id"
                | "created_at_wall_time_ms"
                | "queued_generation"
                | "source_message_ids"
                | "source_message_queued_generations"
                | "source_text_segments"
                | "text"
                | "reply_context"
                | "has_reply_boundary"
                | "merge_consecutive"
                | "pending_uploads"
                | "channel_id"
                | "channel_name"
                | "override_channel_id"
                | "voice_announcement"
        )
    });
    let mut token = Path::new(destination.token_hash).components();
    if !matches!(token.next(), Some(std::path::Component::Normal(_))) || token.next().is_some() {
        return Err(io::Error::other("invalid queue token"));
    }
    let valid = |item: &PendingQueueItem| {
        item.author_id != 0
            && item.message_id != 0
            && !ids(item).contains(&0)
            && item
                .channel_id
                .is_none_or(|channel| channel == destination.channel)
    };
    if !valid(&serde_json::from_value(input.clone())?) {
        return Ok(EnqueueOutcome::Rejected);
    }
    if ledger_input.get("blob_pins").is_some() {
        let pins: Vec<crate::services::tui_input::blob::BlobPin> =
            serde_json::from_value(ledger_input["blob_pins"].clone())?;
        if !pins.is_empty() {
            let ledger = crate::services::tui_input::ledger::Ledger::open(
                destination.root,
                destination.channel,
            )?;
            let copy_root = destination
                .root
                .join("discord_uploads")
                .join(destination.channel.to_string());
            std::fs::create_dir_all(&copy_root)?;
            runtime_store::fsync_parent_dir(&copy_root)?;
            runtime_store::fsync_parent_dir(
                copy_root
                    .parent()
                    .ok_or_else(|| io::Error::other("upload parent unavailable"))?,
            )?;
            let mut uploads = Vec::new();
            for pin in pins {
                let bytes = ledger.read_blob(&pin)?;
                let name = pin
                    .local_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or_else(|| io::Error::other("invalid blob path"))?;
                let row = pin
                    .local_path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|n| n.to_str())
                    .ok_or_else(|| io::Error::other("invalid blob row"))?;
                let filename = name
                    .split_once('_')
                    .map(|(_, name)| name)
                    .ok_or_else(|| io::Error::other("invalid pinned filename"))?;
                let path = copy_root.join(format!("handback-{row}-{name}"));
                match std::fs::symlink_metadata(&path) {
                    Ok(meta) if meta.is_file() && std::fs::read(&path)? == bytes => {}
                    Ok(_) => return Err(io::Error::other("handback upload conflict")),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        use std::io::Write;
                        let mut file = tempfile::NamedTempFile::new_in(&copy_root)?;
                        file.write_all(&bytes)?;
                        file.as_file().sync_all()?;
                        file.persist_noclobber(&path).map_err(|e| e.error)?;
                    }
                    Err(error) => return Err(error),
                }
                sync_existing(&path)?;
                uploads.push(Value::String(format!(
                    "[File uploaded] {} → {} ({} bytes)",
                    filename,
                    path.display(),
                    bytes.len()
                )));
            }
            input["pending_uploads"] = Value::Array(uploads);
        }
    }
    let input = &input;
    let base: PathBuf = destination
        .root
        .join("discord_pending_queue")
        .join(destination.provider.as_str())
        .join(destination.token_hash);
    let path = base.join(format!("{}.json", destination.channel));
    let marker_path = base.join(format!("{}.dispatch", destination.channel));
    let original = optional(&path)?.unwrap_or_else(|| serde_json::json!([]));
    let items: Vec<PendingQueueItem> = serde_json::from_value(original.clone())?;
    let incoming: PendingQueueItem = serde_json::from_value(input.clone())?;
    if !valid(&incoming) || items.iter().any(|item| !valid(item)) {
        return Ok(EnqueueOutcome::Rejected);
    }
    let marker: Option<PendingQueueItem> = optional(&marker_path)?
        .map(serde_json::from_value)
        .transpose()?;
    if marker.as_ref().is_some_and(|item| !valid(item)) {
        return Ok(EnqueueOutcome::Rejected);
    }
    let existing: Vec<u64> = items.iter().flat_map(ids).collect();
    let preserved: Vec<u64> = existing
        .iter()
        .copied()
        .chain(marker.iter().flat_map(ids))
        .collect();
    let incoming_ids = ids(&incoming);
    if incoming_ids.iter().all(|id| preserved.contains(id)) {
        let copies: Vec<_> = items
            .iter()
            .chain(marker.iter())
            .filter(|item| ids(item).iter().any(|id| incoming_ids.contains(id)))
            .collect();
        let normalized = |item: &PendingQueueItem| serde_json::to_value(item);
        if copies.len() != 1
            || ids(copies[0]).len() != incoming_ids.len()
            || normalized(copies[0])? != normalized(&incoming)?
        {
            return Ok(EnqueueOutcome::Rejected);
        }
        sync_existing(&path)?;
        sync_existing(&marker_path)?;
        return Ok(EnqueueOutcome::AlreadyPreserved);
    }
    if incoming_ids
        .iter()
        .any(|id| destination.active_sources.contains(id))
    {
        return Ok(EnqueueOutcome::Rejected);
    }
    // Partial coverage must not consume unseen sources as an AlreadyPreserved refusal.
    if incoming_ids.iter().any(|id| preserved.contains(id)) {
        return Ok(EnqueueOutcome::Rejected);
    }
    let now = SystemTime::now();
    let instant = Instant::now();
    // Test admission on the incoming item alone; distinct IDs must not merge or text-dedup.
    if items.len() >= super::MAX_INTERVENTIONS_PER_CHANNEL {
        return Ok(EnqueueOutcome::Rejected);
    }
    let mut queue = Vec::new();
    let incoming = pending_queue_item_to_intervention(incoming, now, instant);
    let result = enqueue_with_settlement(&mut queue, incoming, None, None);
    if !result.enqueued || !result.queue_exit_events.is_empty() {
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
