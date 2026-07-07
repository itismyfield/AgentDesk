use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::{
    Intervention, InterventionMode, SourceMessageQueuedGeneration, SourceMessageTextSegment,
};
use crate::services::provider::ProviderKind;

const STALE_PENDING_QUEUE_TMP_AGE: Duration = Duration::from_secs(60);

/// Serializable form of a queued intervention for disk persistence.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct PendingQueueItem {
    pub(crate) author_id: u64,
    #[serde(default)]
    pub(crate) author_is_bot: bool,
    pub(crate) message_id: u64,
    /// #4180: original wall-clock timestamp for `Intervention::created_at`,
    /// persisted as Unix milliseconds so restart restore can recreate the
    /// monotonic age instead of stamping the item as newly queued.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) created_at_wall_time_ms: Option<u64>,
    #[serde(default)]
    pub(crate) queued_generation: u64,
    #[serde(default)]
    pub(crate) source_message_ids: Vec<u64>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) source_message_queued_generations: Vec<PendingQueueSourceGeneration>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) source_text_segments: Vec<PendingQueueSourceTextSegment>,
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

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct PendingQueueSourceGeneration {
    pub(crate) message_id: u64,
    pub(crate) queued_generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct PendingQueueSourceTextSegment {
    pub(crate) message_id: u64,
    pub(crate) text: String,
}

#[derive(Clone, Debug)]
pub(crate) struct PendingDispatchMarker {
    pub(crate) channel_id: ChannelId,
    pub(crate) intervention: Intervention,
    pub(crate) restored_override: Option<ChannelId>,
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

fn system_time_to_unix_millis(time: SystemTime) -> Option<u64> {
    let millis = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_millis();
    u64::try_from(millis).ok()
}

fn system_time_from_unix_millis(millis: u64) -> Option<SystemTime> {
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(millis))
}

fn created_at_wall_time_ms_from_instant(
    created_at: Instant,
    reference_wall_time: SystemTime,
    reference_instant: Instant,
) -> Option<u64> {
    let created_at_wall_time = reference_instant
        .checked_duration_since(created_at)
        .and_then(|age| reference_wall_time.checked_sub(age))
        .unwrap_or(reference_wall_time);
    system_time_to_unix_millis(created_at_wall_time)
}

fn created_at_instant_from_wall_time_ms(
    created_at_wall_time_ms: Option<u64>,
    reference_wall_time: SystemTime,
    reference_instant: Instant,
) -> Instant {
    let Some(created_at_wall_time) = created_at_wall_time_ms.and_then(system_time_from_unix_millis)
    else {
        return reference_instant;
    };
    match reference_wall_time.duration_since(created_at_wall_time) {
        Ok(age) => reference_instant
            .checked_sub(age)
            .unwrap_or(reference_instant),
        // #4180 review: a persisted wall time in OUR future means the wall
        // clock stepped backward across the restart, so the item's true age is
        // unknowable (no cross-restart monotonic source). Clamping to "fresh"
        // (`reference_instant`) would re-open the LastItemDedup false-reject
        // this module exists to close, so restore the item as already OUTSIDE
        // the dedup window — preferring a possible duplicate dispatch over a
        // silently dropped re-send, consistent with the relay's
        // duplicate-over-loss stance. (A backward step small enough to keep
        // `duration_since` in `Ok` remains undetectable; documented residual.)
        Err(_) => reference_instant
            .checked_sub(super::INTERVENTION_DEDUP_WINDOW + Duration::from_secs(1))
            .unwrap_or(reference_instant),
    }
}

fn pending_queue_item_from_intervention(
    intervention: &Intervention,
    channel_id: ChannelId,
    dispatch_role_override: Option<u64>,
    reference_wall_time: SystemTime,
    reference_instant: Instant,
) -> PendingQueueItem {
    let source_message_queued_generations: Vec<PendingQueueSourceGeneration> = intervention
        .source_message_queued_generations()
        .into_iter()
        .map(|owner| PendingQueueSourceGeneration {
            message_id: owner.message_id.get(),
            queued_generation: owner.queued_generation,
        })
        .collect();
    let source_text_segments: Vec<PendingQueueSourceTextSegment> = intervention
        .source_text_segments()
        .into_iter()
        .map(|segment| PendingQueueSourceTextSegment {
            message_id: segment.message_id.get(),
            text: segment.text,
        })
        .collect();
    let source_text_segments = if source_text_segments.len() > 1 {
        source_text_segments
    } else {
        Vec::new()
    };
    PendingQueueItem {
        author_id: intervention.author_id.get(),
        author_is_bot: intervention.author_is_bot,
        message_id: intervention.message_id.get(),
        created_at_wall_time_ms: created_at_wall_time_ms_from_instant(
            intervention.created_at,
            reference_wall_time,
            reference_instant,
        ),
        queued_generation: intervention.queued_generation,
        source_message_ids: if intervention.source_message_ids.is_empty() {
            vec![intervention.message_id.get()]
        } else {
            intervention
                .source_message_ids
                .iter()
                .map(|id| id.get())
                .collect()
        },
        source_message_queued_generations,
        source_text_segments,
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
    let reference_instant = Instant::now();
    let reference_wall_time = SystemTime::now();
    let items: Vec<PendingQueueItem> = queue
        .iter()
        .map(|intervention| {
            pending_queue_item_from_intervention(
                intervention,
                channel_id,
                dispatch_role_override,
                reference_wall_time,
                reference_instant,
            )
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
    let reference_instant = Instant::now();
    let reference_wall_time = SystemTime::now();
    let item = pending_queue_item_from_intervention(
        intervention,
        channel_id,
        dispatch_role_override,
        reference_wall_time,
        reference_instant,
    );
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
    let filenames = [
        format!("{}.json", channel_id.get()),
        format!("{}.dispatch", channel_id.get()),
    ];
    let mut removed = 0;
    for entry in entries.flatten() {
        let token_dir = entry.path();
        if !token_dir.is_dir() {
            continue;
        }
        for filename in &filenames {
            let path = token_dir.join(filename);
            if !path.is_file() {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(error) => tracing::warn!(
                    provider = provider.as_str(),
                    channel_id = channel_id.get(),
                    path = %path.display(),
                    "failed to remove pending queue/dispatch file during force purge: {error}"
                ),
            }
        }
    }
    removed
}

fn pending_queue_item_to_intervention(
    item: PendingQueueItem,
    reference_wall_time: SystemTime,
    reference_instant: Instant,
) -> Intervention {
    let mut source_message_ids: Vec<MessageId> = item
        .source_message_ids
        .into_iter()
        .map(MessageId::new)
        .collect();
    if source_message_ids.is_empty() {
        source_message_ids.push(MessageId::new(item.message_id));
    }
    let queued_generation = if item.queued_generation == 0 {
        crate::services::discord::runtime_store::load_generation()
    } else {
        item.queued_generation
    };
    let mut source_message_queued_generations: Vec<SourceMessageQueuedGeneration> = item
        .source_message_queued_generations
        .into_iter()
        .filter(|owner| owner.message_id != 0)
        .map(|owner| {
            let generation = if owner.queued_generation == 0 {
                queued_generation
            } else {
                owner.queued_generation
            };
            SourceMessageQueuedGeneration::new(MessageId::new(owner.message_id), generation)
        })
        .collect();
    if source_message_queued_generations.is_empty() {
        source_message_queued_generations = source_message_ids
            .iter()
            .copied()
            .map(|message_id| SourceMessageQueuedGeneration::new(message_id, queued_generation))
            .collect();
    } else {
        for message_id in &source_message_ids {
            if !source_message_queued_generations
                .iter()
                .any(|owner| owner.message_id == *message_id)
            {
                source_message_queued_generations.push(SourceMessageQueuedGeneration::new(
                    *message_id,
                    queued_generation,
                ));
            }
        }
    }
    let source_text_segments: Vec<SourceMessageTextSegment> = item
        .source_text_segments
        .into_iter()
        .filter(|segment| segment.message_id != 0)
        .map(|segment| {
            SourceMessageTextSegment::new(MessageId::new(segment.message_id), segment.text)
        })
        .collect();
    let created_at = created_at_instant_from_wall_time_ms(
        item.created_at_wall_time_ms,
        reference_wall_time,
        reference_instant,
    );
    Intervention {
        author_id: UserId::new(item.author_id),
        author_is_bot: item.author_is_bot,
        message_id: MessageId::new(item.message_id),
        queued_generation,
        source_message_ids,
        source_message_queued_generations,
        source_text_segments,
        text: item.text,
        mode: InterventionMode::Soft,
        created_at,
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

pub(crate) fn load_channel_pending_dispatch_marker(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
) -> Option<(Intervention, Option<ChannelId>)> {
    let path = pending_dispatch_marker_file_path(provider, token_hash, channel_id)?;
    let Ok(content) = fs::read_to_string(&path) else {
        return None;
    };
    let Ok(item) = serde_json::from_str::<PendingQueueItem>(&content) else {
        return None;
    };
    let restored_override = item.override_channel_id.map(ChannelId::new);
    let reference_instant = Instant::now();
    let reference_wall_time = SystemTime::now();
    Some((
        pending_queue_item_to_intervention(item, reference_wall_time, reference_instant),
        restored_override,
    ))
}

fn pending_queue_items_to_interventions(
    items: Vec<PendingQueueItem>,
    reference_wall_time: SystemTime,
    reference_instant: Instant,
) -> Vec<Intervention> {
    items
        .into_iter()
        .map(|item| {
            pending_queue_item_to_intervention(item, reference_wall_time, reference_instant)
        })
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
    let reference_instant = Instant::now();
    let reference_wall_time = SystemTime::now();
    let mut result: HashMap<ChannelId, Vec<Intervention>> = HashMap::new();
    let mut restored_overrides: HashMap<ChannelId, ChannelId> = HashMap::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if pending_dispatch_marker_channel_id(&path).is_some() {
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
            continue;
        };
        if let Some(override_id) = items.iter().find_map(|item| item.override_channel_id) {
            restored_overrides.insert(ChannelId::new(channel_id), ChannelId::new(override_id));
        }
        let interventions =
            pending_queue_items_to_interventions(items, reference_wall_time, reference_instant);
        if !interventions.is_empty() {
            result.insert(ChannelId::new(channel_id), interventions);
        }
    }
    (result, restored_overrides)
}

pub(crate) fn load_pending_dispatch_markers(
    provider: &ProviderKind,
    token_hash: &str,
) -> Vec<PendingDispatchMarker> {
    let Some(root) = pending_queue_root() else {
        return Vec::new();
    };
    let dir = root.join(provider.as_str()).join(token_hash);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let channel_id = ChannelId::new(pending_dispatch_marker_channel_id(&path)?);
            let (intervention, restored_override) =
                load_channel_pending_dispatch_marker(provider, token_hash, channel_id)?;
            Some(PendingDispatchMarker {
                channel_id,
                intervention,
                restored_override,
            })
        })
        .collect()
}

pub(super) fn load_channel_pending_queue(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
) -> (Vec<Intervention>, Option<ChannelId>) {
    let Some(path) = pending_queue_file_path(provider, token_hash, channel_id) else {
        return (Vec::new(), None);
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return (Vec::new(), None);
    };
    let Ok(items) = serde_json::from_str::<Vec<PendingQueueItem>>(&content) else {
        return (Vec::new(), None);
    };
    let restored_override = items
        .iter()
        .find_map(|item| item.override_channel_id)
        .map(ChannelId::new);
    let reference_instant = Instant::now();
    let reference_wall_time = SystemTime::now();
    let interventions =
        pending_queue_items_to_interventions(items, reference_wall_time, reference_instant);
    (interventions, restored_override)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::turn_orchestrator::test_support::{AGENTDESK_ROOT_DIR_ENV, lock_test_env};

    /// #3293: `pending_queue_item_to_intervention` resolves the runtime store
    /// root (via `runtime_store::load_generation`), so these tests must pin
    /// `AGENTDESK_ROOT_DIR` to a tempdir under the shared env lock and clear
    /// it on drop like every other env-touching queue test.
    struct EnvGuard;

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
        }
    }

    fn intervention(message_id: u64, text: &str, created_at: Instant) -> Intervention {
        Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            queued_generation: 1,
            source_message_ids: vec![MessageId::new(message_id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at,
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    #[test]
    fn persisted_created_at_wall_time_restores_original_age() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        const SAVE_WALL_TIME_MS: u64 = 1_700_000_000_000;
        let save_instant = Instant::now();
        let save_wall_time = SystemTime::UNIX_EPOCH + Duration::from_millis(SAVE_WALL_TIME_MS);
        let original_age = Duration::from_secs(240);
        let downtime = Duration::from_secs(7);
        let created_at = save_instant.checked_sub(original_age).unwrap();
        let item = pending_queue_item_from_intervention(
            &intervention(10, "same text", created_at),
            ChannelId::new(20),
            None,
            save_wall_time,
            save_instant,
        );

        assert_eq!(
            item.created_at_wall_time_ms,
            Some(SAVE_WALL_TIME_MS - 240_000)
        );
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("created_at_wall_time_ms"));

        let reloaded_item = serde_json::from_str::<PendingQueueItem>(&json).unwrap();
        let reload_instant = save_instant.checked_add(downtime).unwrap();
        let reload_wall_time = save_wall_time.checked_add(downtime).unwrap();
        let restored =
            pending_queue_item_to_intervention(reloaded_item, reload_wall_time, reload_instant);

        assert_eq!(
            reload_instant.duration_since(restored.created_at),
            original_age + downtime
        );
    }

    #[test]
    fn legacy_payload_without_created_at_wall_time_falls_back_to_reload_now() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let legacy_json = r#"{"author_id":1,"message_id":10,"text":"legacy"}"#;
        let item = serde_json::from_str::<PendingQueueItem>(legacy_json).unwrap();
        assert_eq!(item.created_at_wall_time_ms, None);

        let reference_instant = Instant::now();
        let reference_wall_time = SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_010_000);
        let restored =
            pending_queue_item_to_intervention(item, reference_wall_time, reference_instant);

        assert_eq!(restored.created_at, reference_instant);
    }

    #[test]
    fn backward_clock_restore_lands_outside_dedup_window() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        // Persisted wall time is AHEAD of the reload reference wall time: the
        // wall clock stepped backward across the restart. The restored age
        // must land outside `INTERVENTION_DEDUP_WINDOW` so a distinct re-send
        // is never falsely rejected as `LastItemDedup` (duplicate-over-loss).
        const RELOAD_WALL_TIME_MS: u64 = 1_700_000_000_000;
        let json = format!(
            r#"{{"author_id":1,"message_id":10,"text":"skewed","created_at_wall_time_ms":{}}}"#,
            RELOAD_WALL_TIME_MS + 30_000
        );
        let item = serde_json::from_str::<PendingQueueItem>(&json).unwrap();

        let reference_instant = Instant::now();
        let reference_wall_time =
            SystemTime::UNIX_EPOCH + Duration::from_millis(RELOAD_WALL_TIME_MS);
        let restored =
            pending_queue_item_to_intervention(item, reference_wall_time, reference_instant);

        assert!(
            reference_instant.duration_since(restored.created_at)
                > crate::services::turn_orchestrator::INTERVENTION_DEDUP_WINDOW,
            "a backward-clock restore must not look fresh enough to suppress a re-send"
        );
    }
}
