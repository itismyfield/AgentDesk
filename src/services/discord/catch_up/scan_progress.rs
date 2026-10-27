//! Discord scan retirement, independent of intake checkpoints and turn completion.

use std::{fs, path::PathBuf};

use poise::serenity_prelude::ChannelId;
use serde::{Deserialize, Serialize};

use super::super::{is_synthetic_headless_message_id_raw, runtime_store};
use crate::services::provider::ProviderKind;

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Origin {
    ObservedFrontier,
    RecentLowerBound,
    ExplicitDiscard,
}

#[derive(Serialize, Deserialize)]
struct Progress {
    version: u8,
    provider: String,
    channel_id: u64,
    after: u64,
    origin: Origin,
}

fn path(provider: &ProviderKind, channel: ChannelId) -> Option<PathBuf> {
    runtime_store::last_message_root().map(|root| {
        root.join(provider.as_str())
            .join(format!("{channel}.sweep.json"))
    })
}

pub(super) fn read(provider: &ProviderKind, channel: ChannelId) -> Option<u64> {
    read_at(&path(provider, channel)?, provider, channel)
}

pub(super) fn has_file(provider: &ProviderKind, channel: ChannelId) -> bool {
    path(provider, channel).is_some_and(|path| path.is_file())
}

fn read_at(path: &std::path::Path, provider: &ProviderKind, channel: ChannelId) -> Option<u64> {
    let progress: Progress = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    (progress.version == 1
        && progress.provider == provider.as_str()
        && progress.channel_id == channel.get()
        && progress.after > 0
        && !is_synthetic_headless_message_id_raw(progress.after))
    .then_some(progress.after)
}

pub(super) fn advance(provider: &ProviderKind, channel: ChannelId, after: u64, origin: Origin) {
    if after == 0 || is_synthetic_headless_message_id_raw(after) {
        return;
    }
    let Some(path) = path(provider, channel) else {
        return;
    };
    let result = (|| -> Result<(), String> {
        // Lock the whole read/merge/publish so an older scan cannot undo a clear.
        fs::create_dir_all(path.parent().ok_or("scanner path has no parent")?)
            .map_err(|error| error.to_string())?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("json.lock"))
            .map_err(|error| error.to_string())?;
        lock.lock().map_err(|error| error.to_string())?;
        if read_at(&path, provider, channel).is_some_and(|prior| prior >= after) {
            return Ok(());
        }
        let progress = Progress {
            version: 1,
            provider: provider.as_str().to_string(),
            channel_id: channel.get(),
            after,
            origin,
        };
        let data = serde_json::to_string(&progress).map_err(|error| error.to_string())?;
        runtime_store::atomic_write(&path, &data)?;
        runtime_store::fsync_parent_and_ancestors(&path).map_err(|error| error.to_string())
    })();
    if let Err(error) = result {
        tracing::warn!(channel_id = channel.get(), provider = provider.as_str(), %error,
            "catch-up scanner progress not durably published; next scan may reread inputs");
    }
}
