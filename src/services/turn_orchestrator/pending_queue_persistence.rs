use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::{Intervention, InterventionMode, intervention_dedup_ids};
use crate::services::provider::ProviderKind;

const STALE_PENDING_QUEUE_TMP_AGE: Duration = Duration::from_secs(60);

/// Serializable form of a queued intervention for disk persistence.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct PendingQueueItem {
    pub(crate) author_id: u64,
    #[serde(default)]
    pub(crate) author_is_bot: bool,
    pub(crate) message_id: u64,
    #[serde(default)]
    pub(crate) source_message_ids: Vec<u64>,
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) reply_context: Option<String>,
    #[serde(default)]
    pub(crate) has_reply_boundary: bool,
    #[serde(default)]
    pub(crate) merge_consecutive: bool,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) pending_uploads: Vec<String>,
    /// Channel this item belongs to (routing snapshot - used by the kickoff guard).
    #[serde(default)]
    pub(crate) channel_id: Option<u64>,
    /// Human-readable channel name at save time (best-effort, may be None).
    #[serde(default)]
    pub(crate) channel_name: Option<String>,
    /// Active dispatch role override at save time (lost on restart; stored for diagnostics).
    #[serde(default)]
    pub(crate) override_channel_id: Option<u64>,
    /// #2266: voice-transcript announcement metadata embedded in the queued
    /// intervention so the durable on-disk queue stays in sync with the
    /// in-memory enrichment. `#[serde(default)]` (and `skip_serializing_if`)
    /// makes the field invisible on non-voice items and forward-compatible
    /// with queue files written by older binaries.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
}

fn pending_queue_root() -> Option<PathBuf> {
    crate::services::discord::runtime_store::discord_pending_queue_root()
}

fn pending_queue_file_path(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
) -> Option<PathBuf> {
    Some(
        pending_queue_root()?
            .join(provider.as_str())
            .join(token_hash)
            .join(format!("{}.json", channel_id.get())),
    )
}

fn pending_dispatch_marker_file_path(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
) -> Option<PathBuf> {
    Some(
        pending_queue_root()?
            .join(provider.as_str())
            .join(token_hash)
            .join(format!("{}.dispatch", channel_id.get())),
    )
}

fn pending_dispatch_marker_channel_id(path: &Path) -> Option<u64> {
    if path.extension().and_then(|ext| ext.to_str()) != Some("dispatch") {
        return None;
    }
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.parse().ok())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingQueueTmpCleanupAudit {
    pub(crate) channel_id: Option<u64>,
    pub(crate) path: PathBuf,
    pub(crate) age_secs: Option<u64>,
    pub(crate) action: &'static str,
    pub(crate) error: Option<String>,
}

fn pending_queue_tmp_channel_id(path: &Path) -> Option<u64> {
    let file_name = path.file_name()?.to_str()?;
    let trimmed = file_name.strip_prefix('.').unwrap_or(file_name);
    let channel_part = trimmed
        .split_once(".json.")
        .map(|(channel, _)| channel)
        .or_else(|| trimmed.split_once(".json.tmp").map(|(channel, _)| channel))
        .or_else(|| trimmed.split_once(".json").map(|(channel, _)| channel))?;
    channel_part.parse().ok()
}

fn pending_queue_tmp_file_age(path: &Path, now: SystemTime) -> Option<Duration> {
    fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| now.duration_since(modified).ok())
}

pub(super) fn cleanup_stale_pending_queue_tmp_files_in_dir(
    provider: &ProviderKind,
    token_hash: &str,
    dir: &Path,
    now: SystemTime,
    stale_after: Duration,
) -> Vec<PendingQueueTmpCleanupAudit> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut audits = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("tmp") {
            continue;
        }

        let channel_id = pending_queue_tmp_channel_id(&path);
        let age = pending_queue_tmp_file_age(&path, now);
        let age_secs = age.map(|age| age.as_secs());
        let should_remove = age.map(|age| age >= stale_after).unwrap_or(false);

        let (action, error) = if should_remove {
            match fs::remove_file(&path) {
                Ok(()) => ("removed_stale", None),
                Err(error) => ("remove_failed", Some(error.to_string())),
            }
        } else {
            ("preserved_active", None)
        };

        let audit = PendingQueueTmpCleanupAudit {
            channel_id,
            path,
            age_secs,
            action,
            error,
        };
        let ts = chrono::Local::now().format("%H:%M:%S");
        match audit.action {
            "removed_stale" => tracing::warn!(
                "  [{ts}] 🧹 PENDING-QUEUE-TMP: provider={} token_hash={} channel_id={:?} path='{}' age_secs={:?} action={}",
                provider.as_str(),
                token_hash,
                audit.channel_id,
                audit.path.display(),
                audit.age_secs,
                audit.action
            ),
            "remove_failed" => tracing::warn!(
                "  [{ts}] ⚠ PENDING-QUEUE-TMP: provider={} token_hash={} channel_id={:?} path='{}' age_secs={:?} action={} error={:?}",
                provider.as_str(),
                token_hash,
                audit.channel_id,
                audit.path.display(),
                audit.age_secs,
                audit.action,
                audit.error
            ),
            _ => tracing::info!(
                "  [{ts}] 🧹 PENDING-QUEUE-TMP: provider={} token_hash={} channel_id={:?} path='{}' age_secs={:?} action={}",
                provider.as_str(),
                token_hash,
                audit.channel_id,
                audit.path.display(),
                audit.age_secs,
                audit.action
            ),
        }
        audits.push(audit);
    }
    audits
}

pub(super) fn cleanup_stale_pending_queue_tmp_files_under_root(
    root: &Path,
    now: SystemTime,
    stale_after: Duration,
) -> Vec<PendingQueueTmpCleanupAudit> {
    let Ok(provider_entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut audits = Vec::new();
    for provider_entry in provider_entries.flatten() {
        let provider_path = provider_entry.path();
        if !provider_path.is_dir() {
            continue;
        }
        let Some(provider_name) = provider_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let provider = ProviderKind::from_str_or_unsupported(provider_name);
        let Ok(token_entries) = fs::read_dir(&provider_path) else {
            continue;
        };
        for token_entry in token_entries.flatten() {
            let token_path = token_entry.path();
            if !token_path.is_dir() {
                continue;
            }
            let Some(token_hash) = token_path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            audits.extend(cleanup_stale_pending_queue_tmp_files_in_dir(
                &provider,
                token_hash,
                &token_path,
                now,
                stale_after,
            ));
        }
    }
    audits
}

pub(crate) fn cleanup_stale_pending_queue_tmp_files_all_tokens() -> Vec<PendingQueueTmpCleanupAudit>
{
    let Some(root) = pending_queue_root() else {
        return Vec::new();
    };
    cleanup_stale_pending_queue_tmp_files_under_root(
        &root,
        SystemTime::now(),
        STALE_PENDING_QUEUE_TMP_AGE,
    )
}

fn pending_queue_item_from_intervention(
    intervention: &Intervention,
    channel_id: ChannelId,
    dispatch_role_override: Option<u64>,
) -> PendingQueueItem {
    PendingQueueItem {
        author_id: intervention.author_id.get(),
        author_is_bot: intervention.author_is_bot,
        message_id: intervention.message_id.get(),
        source_message_ids: if intervention.source_message_ids.is_empty() {
            vec![intervention.message_id.get()]
        } else {
            intervention
                .source_message_ids
                .iter()
                .map(|id| id.get())
                .collect()
        },
        text: intervention.text.clone(),
        reply_context: intervention.reply_context.clone(),
        has_reply_boundary: intervention.has_reply_boundary,
        merge_consecutive: intervention.merge_consecutive,
        pending_uploads: intervention.pending_uploads.clone(),
        channel_id: Some(channel_id.get()),
        channel_name: None,
        override_channel_id: dispatch_role_override,
        voice_announcement: intervention.voice_announcement.clone(),
    }
}

/// Write-through: save a single channel's queue to disk.
/// If the queue is empty the file is removed.
pub(crate) fn save_channel_queue(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
    queue: &[Intervention],
    dispatch_role_override: Option<u64>,
) -> Result<(), String> {
    let Some(path) = pending_queue_file_path(provider, token_hash, channel_id) else {
        return Err(format!(
            "pending queue root unavailable for provider={} token_hash={} channel_id={}",
            provider.as_str(),
            token_hash,
            channel_id.get()
        ));
    };
    if queue.is_empty() {
        return match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                let message = format!("remove pending queue file {}: {error}", path.display());
                tracing::error!(
                    provider = provider.as_str(),
                    token_hash,
                    channel_id = channel_id.get(),
                    path = %path.display(),
                    error = %message,
                    "recovery-critical pending queue removal failed"
                );
                Err(message)
            }
        };
    }
    let items: Vec<PendingQueueItem> = queue
        .iter()
        .map(|intervention| {
            pending_queue_item_from_intervention(intervention, channel_id, dispatch_role_override)
        })
        .collect();
    let json = serde_json::to_string_pretty(&items)
        .map_err(|error| format!("serialize pending queue {}: {error}", path.display()))?;
    let context =
        crate::services::discord::runtime_store::AtomicWriteContext::new("discord_pending_queue")
            .provider(provider.as_str())
            .token_hash(token_hash)
            .channel_id(channel_id.get());
    crate::services::discord::runtime_store::critical_atomic_write(&path, &json, context)
}

pub(super) fn save_channel_pending_dispatch_marker(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
    intervention: &Intervention,
    dispatch_role_override: Option<u64>,
) -> Result<(), String> {
    let Some(path) = pending_dispatch_marker_file_path(provider, token_hash, channel_id) else {
        return Err(format!(
            "pending dispatch marker root unavailable for provider={} token_hash={} channel_id={}",
            provider.as_str(),
            token_hash,
            channel_id.get()
        ));
    };
    let item =
        pending_queue_item_from_intervention(intervention, channel_id, dispatch_role_override);
    let json = serde_json::to_string_pretty(&item)
        .map_err(|error| format!("serialize pending dispatch {}: {error}", path.display()))?;
    let context = crate::services::discord::runtime_store::AtomicWriteContext::new(
        "discord_pending_dispatch",
    )
    .provider(provider.as_str())
    .token_hash(token_hash)
    .channel_id(channel_id.get());
    crate::services::discord::runtime_store::critical_atomic_write(&path, &json, context)
}

pub(super) fn remove_channel_pending_dispatch_marker(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
) -> Result<(), String> {
    let Some(path) = pending_dispatch_marker_file_path(provider, token_hash, channel_id) else {
        return Ok(());
    };
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "remove pending dispatch marker {}: {error}",
            path.display()
        )),
    }
}

/// Remove persisted pending-queue files for one channel across all token
/// namespaces for the provider. Used by force-cancel recovery when the live
/// session key is unavailable or stale but the channel still owns queued work.
pub(crate) fn remove_channel_pending_queue_files_all_tokens(
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> usize {
    let Some(root) = pending_queue_root() else {
        return 0;
    };
    let provider_dir = root.join(provider.as_str());
    let Ok(entries) = fs::read_dir(&provider_dir) else {
        return 0;
    };
    let filename = format!("{}.json", channel_id.get());
    let mut removed = 0;
    for entry in entries.flatten() {
        let token_dir = entry.path();
        if !token_dir.is_dir() {
            continue;
        }
        let path = token_dir.join(&filename);
        if !path.is_file() {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(error) => tracing::warn!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                path = %path.display(),
                "failed to remove pending queue file during force purge: {error}"
            ),
        }
    }
    removed
}

fn pending_queue_item_to_intervention(item: PendingQueueItem, now: Instant) -> Intervention {
    let mut source_message_ids: Vec<MessageId> = item
        .source_message_ids
        .into_iter()
        .map(MessageId::new)
        .collect();
    if source_message_ids.is_empty() {
        source_message_ids.push(MessageId::new(item.message_id));
    }
    Intervention {
        author_id: UserId::new(item.author_id),
        author_is_bot: item.author_is_bot,
        message_id: MessageId::new(item.message_id),
        source_message_ids,
        text: item.text,
        mode: InterventionMode::Soft,
        created_at: now,
        reply_context: item.reply_context,
        has_reply_boundary: item.has_reply_boundary,
        merge_consecutive: item.merge_consecutive,
        pending_uploads: item.pending_uploads,
        // #2266: durable on-disk queue restores the voice-transcript
        // metadata so the dispatch path on the next run can reinsert it
        // into the per-process announce_meta store. Older queue files that
        // predate this field deserialize as `None` (#[serde(default)]) and
        // the queued turn degrades to plain text - same as the prior
        // restart behavior.
        voice_announcement: item.voice_announcement,
    }
}

fn load_channel_pending_dispatch_marker(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
) -> Option<(Intervention, Option<ChannelId>)> {
    let path = pending_dispatch_marker_file_path(provider, token_hash, channel_id)?;
    let Ok(content) = fs::read_to_string(&path) else {
        return None;
    };
    let Ok(item) = serde_json::from_str::<PendingQueueItem>(&content) else {
        let _ = fs::remove_file(&path);
        return None;
    };
    let restored_override = item.override_channel_id.map(ChannelId::new);
    Some((
        pending_queue_item_to_intervention(item, Instant::now()),
        restored_override,
    ))
}

fn recover_pending_dispatch_marker_into_queue(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
    queue: &mut Vec<Intervention>,
    restored_override: &mut Option<ChannelId>,
) {
    let Some((marker, marker_override)) =
        load_channel_pending_dispatch_marker(provider, token_hash, channel_id)
    else {
        return;
    };
    if restored_override.is_none() {
        *restored_override = marker_override;
    }
    let marker_message_id = marker.message_id;
    let already_queued = queue
        .iter()
        .any(|item| intervention_dedup_ids(item).contains(&marker_message_id));
    if already_queued {
        if let Err(error) = remove_channel_pending_dispatch_marker(provider, token_hash, channel_id)
        {
            tracing::warn!(
                provider = provider.as_str(),
                token_hash,
                channel_id = channel_id.get(),
                error = %error,
                "failed to remove duplicate pending dispatch marker during recovery"
            );
        }
        return;
    }

    queue.insert(0, marker);
    if let Err(error) = save_channel_queue(
        provider,
        token_hash,
        channel_id,
        queue,
        restored_override.map(|id| id.get()),
    ) {
        tracing::warn!(
            provider = provider.as_str(),
            token_hash,
            channel_id = channel_id.get(),
            error = %error,
            "failed to persist pending dispatch marker recovery; marker retained for retry"
        );
        return;
    }
    if let Err(error) = remove_channel_pending_dispatch_marker(provider, token_hash, channel_id) {
        tracing::warn!(
            provider = provider.as_str(),
            token_hash,
            channel_id = channel_id.get(),
            error = %error,
            "failed to remove pending dispatch marker after recovery"
        );
    }
}

fn pending_queue_items_to_interventions(
    items: Vec<PendingQueueItem>,
    now: Instant,
) -> Vec<Intervention> {
    items
        .into_iter()
        .map(|item| pending_queue_item_to_intervention(item, now))
        .collect()
}

/// Only reads files in this bot's token-namespaced subdirectory.
/// Returns `(queues, dispatch_role_overrides)` so the caller can restore both.
pub(crate) fn load_pending_queues(
    provider: &ProviderKind,
    token_hash: &str,
) -> (
    HashMap<ChannelId, Vec<Intervention>>,
    HashMap<ChannelId, ChannelId>,
) {
    let Some(root) = pending_queue_root() else {
        return (HashMap::new(), HashMap::new());
    };
    let dir = root.join(provider.as_str()).join(token_hash);
    let _ = cleanup_stale_pending_queue_tmp_files_in_dir(
        provider,
        token_hash,
        &dir,
        SystemTime::now(),
        STALE_PENDING_QUEUE_TMP_AGE,
    );
    let Ok(entries) = fs::read_dir(&dir) else {
        return (HashMap::new(), HashMap::new());
    };
    let now = Instant::now();
    let mut result: HashMap<ChannelId, Vec<Intervention>> = HashMap::new();
    let mut restored_overrides: HashMap<ChannelId, ChannelId> = HashMap::new();
    let mut marker_channels = HashSet::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if let Some(channel_id) = pending_dispatch_marker_channel_id(&path) {
            marker_channels.insert(ChannelId::new(channel_id));
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let channel_id: u64 = match path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse().ok())
        {
            Some(id) => id,
            None => continue,
        };
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(items) = serde_json::from_str::<Vec<PendingQueueItem>>(&content) else {
            let _ = fs::remove_file(&path);
            continue;
        };
        if let Some(override_id) = items.iter().find_map(|item| item.override_channel_id) {
            restored_overrides.insert(ChannelId::new(channel_id), ChannelId::new(override_id));
        }
        let interventions = pending_queue_items_to_interventions(items, now);
        if !interventions.is_empty() {
            result.insert(ChannelId::new(channel_id), interventions);
        }
    }
    for channel_id in marker_channels {
        let mut restored_override = restored_overrides.get(&channel_id).copied();
        let queue = result.entry(channel_id).or_default();
        recover_pending_dispatch_marker_into_queue(
            provider,
            token_hash,
            channel_id,
            queue,
            &mut restored_override,
        );
        if let Some(override_id) = restored_override {
            restored_overrides.insert(channel_id, override_id);
        } else {
            restored_overrides.remove(&channel_id);
        }
    }
    (result, restored_overrides)
}

pub(super) fn load_channel_pending_queue_with_marker_recovery(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
    recover_dispatch_marker: bool,
) -> (Vec<Intervention>, Option<ChannelId>) {
    let Some(path) = pending_queue_file_path(provider, token_hash, channel_id) else {
        let mut interventions = Vec::new();
        let mut restored_override = None;
        if recover_dispatch_marker {
            recover_pending_dispatch_marker_into_queue(
                provider,
                token_hash,
                channel_id,
                &mut interventions,
                &mut restored_override,
            );
        }
        return (interventions, restored_override);
    };
    let Ok(content) = fs::read_to_string(&path) else {
        let mut interventions = Vec::new();
        let mut restored_override = None;
        if recover_dispatch_marker {
            recover_pending_dispatch_marker_into_queue(
                provider,
                token_hash,
                channel_id,
                &mut interventions,
                &mut restored_override,
            );
        }
        return (interventions, restored_override);
    };
    let Ok(items) = serde_json::from_str::<Vec<PendingQueueItem>>(&content) else {
        let _ = fs::remove_file(&path);
        let mut interventions = Vec::new();
        let mut restored_override = None;
        if recover_dispatch_marker {
            recover_pending_dispatch_marker_into_queue(
                provider,
                token_hash,
                channel_id,
                &mut interventions,
                &mut restored_override,
            );
        }
        return (interventions, restored_override);
    };
    let mut restored_override = items
        .iter()
        .find_map(|item| item.override_channel_id)
        .map(ChannelId::new);
    let mut interventions = pending_queue_items_to_interventions(items, Instant::now());
    if recover_dispatch_marker {
        recover_pending_dispatch_marker_into_queue(
            provider,
            token_hash,
            channel_id,
            &mut interventions,
            &mut restored_override,
        );
    }
    (interventions, restored_override)
}

#[cfg(test)]
pub(super) fn load_channel_pending_queue(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
) -> (Vec<Intervention>, Option<ChannelId>) {
    load_channel_pending_queue_with_marker_recovery(provider, token_hash, channel_id, true)
}

/// Log a structured warning for legacy pending queue files at the old flat path.
pub(crate) fn warn_legacy_pending_queue_files(provider: &ProviderKind) {
    let Some(root) = pending_queue_root() else {
        return;
    };
    let dir = root.join(provider.as_str());
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("json") {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ LEGACY-QUEUE: found legacy pending queue file '{}' — \
                predates bot-identity namespacing and will NOT be restored. \
                Remove manually if no longer needed.",
                path.display()
            );
        }
    }
}
