//! Messages catch-up left to their live arrival, kept on disk until a live path or a scan handled
//! each one, so every scan, after a restart too, reads from before the oldest.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use super::super::{router, runtime_store};
use super::CatchUpFetchMode;
use crate::services::provider::ProviderKind;

/// Serializes this process's read-modify-write of every floor file.
static LOCK: Mutex<()> = Mutex::new(());

/// Beside the channel's checkpoint; the checkpoint scan and stale prune skip this name.
fn floor_path(provider: &ProviderKind, channel: ChannelId) -> Option<PathBuf> {
    let file = format!("{}.yield_floor.json", channel.get());
    runtime_store::last_message_root().map(|root| root.join(provider.as_str()).join(file))
}

/// Held ids, oldest first; a missing file holds none.
fn load(path: &Path) -> Result<Vec<u64>, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.to_string()),
    }
}

fn store(path: &Path, ids: &[u64]) -> Result<(), String> {
    if ids.is_empty() {
        return match std::fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.to_string()),
            _ => Ok(()),
        };
    }
    let raw = serde_json::to_string(ids).expect("u64 ids serialize");
    runtime_store::atomic_write(path, &raw)?;
    runtime_store::fsync_parent_dir(path).map_err(|error| error.to_string())
}

/// Rewrites the held ids at `path` with `change`; an error leaves the file as it was.
fn update(path: &Path, change: impl FnOnce(&mut Vec<u64>)) -> Result<(), String> {
    let _held = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let mut ids = load(path)?;
    let before = ids.clone();
    change(&mut ids);
    if ids == before {
        return Ok(());
    }
    store(path, &ids)
}

/// Whether catch-up leaves `message` to its live arrival: a fresh message on a gated channel,
/// held until handled. One that cannot be held is recovered now, not left to a lost cursor.
pub(super) fn leave_to_live(
    provider: &ProviderKind,
    channel: ChannelId,
    message: MessageId,
    phase: &str,
) -> bool {
    if !router::catch_up_yields(channel, message) || !hold(provider, channel, message.get()) {
        return false;
    }
    tracing::info!(
        phase,
        channel_id = channel.get(),
        message_id = message.get(),
        "catch-up left a fresh message on a busy-inject channel to its live arrival"
    );
    true
}

/// Holds `message` until it is handled; false when that could not be saved.
fn hold(provider: &ProviderKind, channel: ChannelId, message: u64) -> bool {
    let path = floor_path(provider, channel).ok_or_else(|| "runtime root unavailable".to_string());
    let held = path.and_then(|path| {
        update(&path, |ids| {
            if !ids.contains(&message) {
                ids.push(message);
                ids.sort_unstable();
            }
        })
    });
    if let Err(error) = &held {
        tracing::warn!(channel_id = channel.get(), message, %error, "catch-up yield floor not saved");
    }
    held.is_ok()
}

/// Ends the hold on each of `settled`, which a live path or a scan handled.
pub(in crate::services::discord) fn release(
    provider: &ProviderKind,
    channel: ChannelId,
    settled: impl IntoIterator<Item = u64>,
) {
    let Some(path) = floor_path(provider, channel) else {
        return;
    };
    let settled: Vec<u64> = settled.into_iter().collect();
    if let Err(error) = update(&path, |ids| ids.retain(|id| !settled.contains(id))) {
        tracing::warn!(channel_id = channel.get(), %error, "catch-up yield floor not released");
    }
}

/// Ends the hold on each fetched message up to `newest`, which the scan settled.
pub(super) fn release_through(
    provider: &ProviderKind,
    channel: ChannelId,
    fetched: &[serenity::Message],
    newest: u64,
) {
    let settled = fetched.iter().map(|message| message.id.get());
    release(provider, channel, settled.filter(|id| *id <= newest));
}

/// A scan lowered to its channel's oldest held message. Up to the checkpoint it would have read
/// from it reads only held messages, so input that checkpoint passed is not replayed.
#[derive(Default)]
pub(super) struct FloorScan {
    held: Vec<u64>,
    checkpoint: u64,
}

impl FloorScan {
    /// Whether the scan passes `message` as its unlowered checkpoint already did.
    pub(super) fn passes(&self, message: u64) -> bool {
        message <= self.checkpoint && !self.held.contains(&message)
    }
}

/// `mode` lowered to read the channel's held messages again. A Recent scan already reads
/// everything in the age window.
pub(super) fn lower(
    provider: &ProviderKind,
    channel: ChannelId,
    mode: CatchUpFetchMode,
) -> (CatchUpFetchMode, FloorScan) {
    let CatchUpFetchMode::After(checkpoint) = mode else {
        return (mode, FloorScan::default());
    };
    let Some(path) = floor_path(provider, channel) else {
        return (mode, FloorScan::default());
    };
    let held = {
        let _held = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        load(&path)
    };
    let held = match held {
        Ok(held) => held,
        Err(error) => {
            tracing::warn!(channel_id = channel.get(), %error, "catch-up yield floor unreadable");
            return (mode, FloorScan::default());
        }
    };
    match held.first() {
        Some(oldest) if *oldest <= checkpoint => {
            let lowered = CatchUpFetchMode::After(oldest.saturating_sub(1));
            (lowered, FloorScan { held, checkpoint })
        }
        _ => (mode, FloorScan::default()),
    }
}

/// Ids the channel's floor holds, oldest first.
#[cfg(test)]
pub(in crate::services::discord) fn held(provider: &ProviderKind, channel: ChannelId) -> Vec<u64> {
    let path = floor_path(provider, channel).expect("an isolated runtime root");
    load(&path).expect("a readable floor")
}

/// Blocks the channel's floor file so the next hold cannot save it.
#[cfg(test)]
pub(in crate::services::discord) fn block(provider: &ProviderKind, channel: ChannelId) {
    let path = floor_path(provider, channel).expect("an isolated runtime root");
    std::fs::create_dir_all(path).expect("a directory where the floor file goes");
}
