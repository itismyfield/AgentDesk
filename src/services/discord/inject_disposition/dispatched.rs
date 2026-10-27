//! Successful claims for scanner retirement, independent of injection and delivery journals.
use super::super::{is_synthetic_headless_message_id_raw, runtime_store};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::{ChannelId, MessageId};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs, path::PathBuf};

#[derive(Default, Serialize, Deserialize)]
struct Ring {
    entries: Vec<Entry>,
}
#[derive(Serialize, Deserialize)]
struct Entry {
    channel_id: u64,
    message_id: u64,
    at_epoch_ms: i64,
}

fn path(provider: &ProviderKind) -> Option<PathBuf> {
    Some(
        runtime_store::last_message_root()?
            .join(provider.as_str())
            .join("dispatched_inputs.json"),
    )
}
fn load(path: &std::path::Path) -> Vec<Entry> {
    match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Ring>(&bytes) {
            Ok(ring) => ring.entries,
            Err(error) => {
                tracing::warn!(%error, "dispatch ring malformed; rescanning");
                Vec::new()
            }
        },
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%error, "dispatch ring unreadable; rescanning");
            }
            Vec::new()
        }
    }
}
fn valid(id: u64) -> bool {
    id != 0 && !is_synthetic_headless_message_id_raw(id)
}
fn fresh(entry: &Entry, now: i64) -> bool {
    (0..super::DISK_TTL_MS).contains(&now.saturating_sub(entry.at_epoch_ms))
}

pub(in crate::services::discord) fn record(
    provider: &ProviderKind,
    channel: ChannelId,
    primary: MessageId,
    absorbed: &[MessageId],
) -> Result<(), String> {
    let ids: HashSet<u64> = std::iter::once(primary)
        .chain(absorbed.iter().copied())
        .map(|id| id.get())
        .filter(|id| valid(*id))
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let path = path(provider).ok_or("runtime root unavailable")?;
    #[cfg(test)]
    if FAIL_WRITES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&(path.clone(), channel.get()))
    {
        return Err("injected dispatch write failure".into());
    }
    fs::create_dir_all(path.parent().ok_or("dispatch path has no parent")?)
        .map_err(|e| e.to_string())?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("json.lock"))
        .map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    let now = chrono::Utc::now().timestamp_millis();
    let mut entries = load(&path);
    entries.retain(|entry| {
        fresh(entry, now) && !(entry.channel_id == channel.get() && ids.contains(&entry.message_id))
    });
    entries.extend(ids.into_iter().map(|message_id| Entry {
        channel_id: channel.get(),
        message_id,
        at_epoch_ms: now,
    }));
    entries.drain(..entries.len().saturating_sub(super::DISK_CAP));
    runtime_store::atomic_write(
        &path,
        &serde_json::to_string(&Ring { entries }).map_err(|e| e.to_string())?,
    )?;
    runtime_store::fsync_parent_dir(&path).map_err(|e| e.to_string())
}

pub(in crate::services::discord) fn read(
    provider: &ProviderKind,
    channel: ChannelId,
) -> HashSet<u64> {
    let Some(path) = path(provider) else {
        return HashSet::new();
    };
    let now = chrono::Utc::now().timestamp_millis();
    load(&path)
        .into_iter()
        .filter(|entry| {
            entry.channel_id == channel.get() && valid(entry.message_id) && fresh(entry, now)
        })
        .map(|entry| entry.message_id)
        .collect()
}

#[cfg(test)]
static FAIL_WRITES: std::sync::Mutex<Vec<(PathBuf, u64)>> = std::sync::Mutex::new(Vec::new());
#[cfg(test)]
pub(crate) struct FailureGuard(PathBuf, u64);
#[cfg(test)]
impl Drop for FailureGuard {
    fn drop(&mut self) {
        FAIL_WRITES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|entry| entry != &(self.0.clone(), self.1));
    }
}
#[cfg(test)]
pub(crate) fn fail_writes_for_tests(provider: &ProviderKind, channel: ChannelId) -> FailureGuard {
    let path = path(provider).expect("test runtime root");
    FAIL_WRITES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((path.clone(), channel.get()));
    FailureGuard(path, channel.get())
}

#[cfg(test)]
#[test]
fn i7441a2_dispatch_retention_boundary() {
    let now = super::DISK_TTL_MS + 10;
    let entry = |age| Entry {
        channel_id: 1,
        message_id: 2,
        at_epoch_ms: now - age,
    };
    assert!(fresh(&entry(0), now));
    assert!(fresh(&entry(super::DISK_TTL_MS - 1), now));
    assert!(!fresh(&entry(super::DISK_TTL_MS), now));
    assert!(!fresh(&entry(-1), now));
}
