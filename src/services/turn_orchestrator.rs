use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::services::provider::{CancelToken, ProviderKind};

pub(crate) const MAX_INTERVENTIONS_PER_CHANNEL: usize = 30;
pub(crate) const INTERVENTION_TTL: Duration = Duration::from_secs(10 * 60);
pub(crate) const INTERVENTION_DEDUP_WINDOW: Duration = Duration::from_secs(10);
const STALE_PENDING_QUEUE_TMP_AGE: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InterventionMode {
    Soft,
}

#[derive(Clone, Debug)]
pub(crate) struct Intervention {
    pub(crate) author_id: UserId,
    pub(crate) author_is_bot: bool,
    pub(crate) message_id: MessageId,
    pub(crate) source_message_ids: Vec<MessageId>,
    pub(crate) text: String,
    pub(crate) mode: InterventionMode,
    pub(crate) created_at: Instant,
    pub(crate) reply_context: Option<String>,
    pub(crate) has_reply_boundary: bool,
    pub(crate) merge_consecutive: bool,
    pub(crate) pending_uploads: Vec<String>,
    /// #2266: when a voice-transcript announcement loses the
    /// `mailbox_try_start_turn` race and is enqueued for later dispatch, the
    /// per-process `voice::announce_meta` store entry is consumed by the
    /// original `handle_text_message` call before the race-loss branch runs.
    /// Embedding the full announcement here keeps the queued payload
    /// self-contained so the dispatch path (which reinserts the entry into
    /// the store before re-entering `handle_text_message`) can reconstruct
    /// the voice-transcript framing instead of falling back to plain text.
    /// `None` for non-voice paths.
    pub(crate) voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueExitKind {
    Cancelled,
    Expired,
    Superseded,
}

#[derive(Clone, Debug)]
pub(crate) struct QueueExitEvent {
    pub(crate) intervention: Intervention,
    pub(crate) kind: QueueExitKind,
}

impl QueueExitEvent {
    fn new(intervention: Intervention, kind: QueueExitKind) -> Self {
        Self { intervention, kind }
    }
}

fn prune_interventions(queue: &mut Vec<Intervention>) -> Vec<QueueExitEvent> {
    prune_interventions_at(queue, Instant::now())
}

fn prune_interventions_at(queue: &mut Vec<Intervention>, now: Instant) -> Vec<QueueExitEvent> {
    let mut queue_exit_events = Vec::new();
    queue.retain(|intervention| {
        let keep = now.duration_since(intervention.created_at) <= INTERVENTION_TTL;
        if !keep {
            queue_exit_events.push(QueueExitEvent::new(
                intervention.clone(),
                QueueExitKind::Expired,
            ));
        }
        keep
    });
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        let overflow = queue.len() - MAX_INTERVENTIONS_PER_CHANNEL;
        queue_exit_events.extend(
            queue
                .drain(0..overflow)
                .map(|intervention| QueueExitEvent::new(intervention, QueueExitKind::Superseded)),
        );
    }
    queue_exit_events
}

fn intervention_age_since(last: &Intervention, current: &Intervention) -> Duration {
    current
        .created_at
        .checked_duration_since(last.created_at)
        .unwrap_or_default()
}

fn ensure_source_message_ids(intervention: &mut Intervention) {
    if intervention.source_message_ids.is_empty() {
        intervention
            .source_message_ids
            .push(intervention.message_id);
    }
}

fn push_unique_message_ids(
    existing: &mut Vec<MessageId>,
    incoming: impl IntoIterator<Item = MessageId>,
) {
    for message_id in incoming {
        if !existing.contains(&message_id) {
            existing.push(message_id);
        }
    }
}

fn should_merge_intervention(last: &Intervention, incoming: &Intervention) -> bool {
    last.mode == InterventionMode::Soft
        && incoming.mode == InterventionMode::Soft
        && last.merge_consecutive
        && incoming.merge_consecutive
        && last.author_id == incoming.author_id
        && !last.has_reply_boundary
        && !incoming.has_reply_boundary
}

pub(crate) fn enqueue_intervention(
    queue: &mut Vec<Intervention>,
    mut intervention: Intervention,
) -> EnqueueInterventionResult {
    let mut queue_exit_events = prune_interventions(queue);
    ensure_source_message_ids(&mut intervention);

    if queue
        .iter()
        .any(|item| item.source_message_ids.contains(&intervention.message_id))
    {
        return EnqueueInterventionResult {
            enqueued: false,
            merged: false,
            refusal_reason: Some(EnqueueRefusalReason::SourceIdAlreadyQueued),
            queue_exit_events,
            persistence_error: None,
        };
    }

    if let Some(last) = queue.last() {
        if last.author_id == intervention.author_id
            && last.text == intervention.text
            && last.reply_context == intervention.reply_context
            && last.has_reply_boundary == intervention.has_reply_boundary
            && intervention_age_since(last, &intervention) <= INTERVENTION_DEDUP_WINDOW
        {
            return EnqueueInterventionResult {
                enqueued: false,
                merged: false,
                refusal_reason: Some(EnqueueRefusalReason::LastItemDedup),
                queue_exit_events,
                persistence_error: None,
            };
        }
    }

    if let Some(last) = queue.last_mut() {
        if should_merge_intervention(last, &intervention) {
            if !last.text.is_empty() && !intervention.text.is_empty() {
                last.text.push('\n');
            }
            last.text.push_str(&intervention.text);
            last.message_id = intervention.message_id;
            push_unique_message_ids(
                &mut last.source_message_ids,
                intervention.source_message_ids.into_iter(),
            );
            last.created_at = intervention.created_at;
            // #2266: on merge, the incoming voice announcement (if any)
            // matches the new HEAD `message_id`; the dispatch path reinserts
            // by the HEAD id, so the latest metadata is what we keep.
            if intervention.voice_announcement.is_some() {
                last.voice_announcement = intervention.voice_announcement;
            }
            last.pending_uploads.extend(intervention.pending_uploads);
            return EnqueueInterventionResult {
                enqueued: true,
                merged: true,
                refusal_reason: None,
                queue_exit_events,
                persistence_error: None,
            };
        }
    }

    queue.push(intervention);
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        let overflow = queue.len() - MAX_INTERVENTIONS_PER_CHANNEL;
        queue_exit_events.extend(
            queue
                .drain(0..overflow)
                .map(|intervention| QueueExitEvent::new(intervention, QueueExitKind::Superseded)),
        );
    }
    EnqueueInterventionResult {
        enqueued: true,
        merged: false,
        refusal_reason: None,
        queue_exit_events,
        persistence_error: None,
    }
}

pub(crate) fn has_soft_intervention_at(queue: &mut Vec<Intervention>, now: Instant) -> bool {
    queue.retain(|intervention| now.duration_since(intervention.created_at) <= INTERVENTION_TTL);
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        let overflow = queue.len() - MAX_INTERVENTIONS_PER_CHANNEL;
        queue.drain(0..overflow);
    }
    queue.iter().any(|item| item.mode == InterventionMode::Soft)
}

pub(crate) fn has_soft_intervention(queue: &mut Vec<Intervention>) -> HasPendingSoftQueueResult {
    let queue_exit_events = prune_interventions(queue);
    HasPendingSoftQueueResult {
        has_pending: queue.iter().any(|item| item.mode == InterventionMode::Soft),
        queue_exit_events,
        persistence_error: None,
    }
}

pub(crate) fn dequeue_next_soft_intervention(queue: &mut Vec<Intervention>) -> TakeNextSoftResult {
    let queue_exit_events = prune_interventions(queue);
    let intervention = queue
        .iter()
        .position(|item| item.mode == InterventionMode::Soft)
        .map(|index| queue.remove(index));
    let has_more = queue.iter().any(|item| item.mode == InterventionMode::Soft);
    TakeNextSoftResult {
        intervention,
        has_more,
        queue_len_after: queue.len(),
        queue_exit_events,
        persistence_error: None,
    }
}

pub(crate) fn cancel_soft_intervention_by_message_id(
    queue: &mut Vec<Intervention>,
    message_id: MessageId,
) -> CancelQueuedMessageResult {
    let mut queue_exit_events = prune_interventions(queue);
    let removed = queue
        .iter()
        .position(|item| {
            item.mode == InterventionMode::Soft
                && (item.message_id == message_id || item.source_message_ids.contains(&message_id))
        })
        .map(|index| queue.remove(index));
    if let Some(ref intervention) = removed {
        queue_exit_events.push(QueueExitEvent::new(
            intervention.clone(),
            QueueExitKind::Cancelled,
        ));
    }
    CancelQueuedMessageResult {
        removed,
        queue_exit_events,
        persistence_error: None,
    }
}

pub(crate) fn requeue_intervention_front(
    queue: &mut Vec<Intervention>,
    intervention: Intervention,
) -> Vec<QueueExitEvent> {
    let mut queue_exit_events = prune_interventions(queue);
    queue.insert(0, intervention);
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        queue_exit_events.extend(
            queue
                .drain(MAX_INTERVENTIONS_PER_CHANNEL..)
                .map(|intervention| QueueExitEvent::new(intervention, QueueExitKind::Superseded)),
        );
    }
    queue_exit_events
}

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
    /// Channel this item belongs to (routing snapshot — used by the kickoff guard).
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

fn cleanup_stale_pending_queue_tmp_files_in_dir(
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

fn cleanup_stale_pending_queue_tmp_files_under_root(
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

pub(crate) fn cleanup_stale_pending_queue_tmp_files(
    provider: &ProviderKind,
    token_hash: &str,
) -> Vec<PendingQueueTmpCleanupAudit> {
    let Some(root) = pending_queue_root() else {
        return Vec::new();
    };
    let dir = root.join(provider.as_str()).join(token_hash);
    cleanup_stale_pending_queue_tmp_files_in_dir(
        provider,
        token_hash,
        &dir,
        SystemTime::now(),
        STALE_PENDING_QUEUE_TMP_AGE,
    )
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
        .map(|i| PendingQueueItem {
            author_id: i.author_id.get(),
            author_is_bot: i.author_is_bot,
            message_id: i.message_id.get(),
            source_message_ids: if i.source_message_ids.is_empty() {
                vec![i.message_id.get()]
            } else {
                i.source_message_ids.iter().map(|id| id.get()).collect()
            },
            text: i.text.clone(),
            reply_context: i.reply_context.clone(),
            has_reply_boundary: i.has_reply_boundary,
            merge_consecutive: i.merge_consecutive,
            pending_uploads: i.pending_uploads.clone(),
            channel_id: Some(channel_id.get()),
            channel_name: None,
            override_channel_id: dispatch_role_override,
            // #2266: persist the voice-transcript metadata alongside the
            // queued intervention so post-restart hydrate restores the
            // payload and the dispatch path can still reinsert it into the
            // store. Older queue files (without this field) deserialize as
            // `None` via the `#[serde(default)]` on the field declaration.
            voice_announcement: i.voice_announcement.clone(),
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
        // the queued turn degrades to plain text — same as the prior
        // restart behavior.
        voice_announcement: item.voice_announcement,
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

/// Save all non-empty intervention queues to `{provider}/{token_hash}/`.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn save_pending_queues(
    provider: &ProviderKind,
    token_hash: &str,
    queues: &HashMap<ChannelId, Vec<Intervention>>,
    dispatch_role_overrides: &dashmap::DashMap<ChannelId, ChannelId>,
) -> Result<(), String> {
    let Some(root) = pending_queue_root() else {
        return Err(format!(
            "pending queue root unavailable for provider={} token_hash={}",
            provider.as_str(),
            token_hash
        ));
    };
    let dir = root.join(provider.as_str()).join(token_hash);
    fs::create_dir_all(&dir)
        .map_err(|error| format!("create pending queue dir {}: {error}", dir.display()))?;
    let entries = fs::read_dir(&dir)
        .map_err(|error| format!("read pending queue dir {}: {error}", dir.display()))?;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        fs::remove_file(&path).map_err(|error| {
            format!(
                "remove stale pending queue file {}: {error}",
                path.display()
            )
        })?;
    }
    for (channel_id, queue) in queues {
        if queue.is_empty() {
            continue;
        }
        let override_id = dispatch_role_overrides
            .get(channel_id)
            .map(|r| r.value().get());
        let items: Vec<PendingQueueItem> = queue
            .iter()
            .map(|i| PendingQueueItem {
                author_id: i.author_id.get(),
                author_is_bot: i.author_is_bot,
                message_id: i.message_id.get(),
                source_message_ids: if i.source_message_ids.is_empty() {
                    vec![i.message_id.get()]
                } else {
                    i.source_message_ids.iter().map(|id| id.get()).collect()
                },
                text: i.text.clone(),
                reply_context: i.reply_context.clone(),
                has_reply_boundary: i.has_reply_boundary,
                merge_consecutive: i.merge_consecutive,
                pending_uploads: i.pending_uploads.clone(),
                channel_id: Some(channel_id.get()),
                channel_name: None,
                override_channel_id: override_id,
                // #2266: persist voice metadata in the restart-drain
                // bulk-save path too (matches `save_channel_queue` above).
                voice_announcement: i.voice_announcement.clone(),
            })
            .collect();
        let json = serde_json::to_string_pretty(&items).map_err(|error| {
            format!(
                "serialize pending queue provider={} token_hash={} channel_id={}: {error}",
                provider.as_str(),
                token_hash,
                channel_id.get()
            )
        })?;
        let path = dir.join(format!("{}.json", channel_id.get()));
        let context = crate::services::discord::runtime_store::AtomicWriteContext::new(
            "discord_pending_queue",
        )
        .provider(provider.as_str())
        .token_hash(token_hash)
        .channel_id(channel_id.get());
        crate::services::discord::runtime_store::critical_atomic_write(&path, &json, context)?;
    }
    Ok(())
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
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
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
    (result, restored_overrides)
}

fn load_channel_pending_queue(
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
        let _ = fs::remove_file(&path);
        return (Vec::new(), None);
    };
    let restored_override = items
        .iter()
        .find_map(|item| item.override_channel_id)
        .map(ChannelId::new);
    let interventions = pending_queue_items_to_interventions(items, Instant::now());
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn take_next_soft_intervention_persisted(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
    intervention_queue: &mut HashMap<ChannelId, Vec<Intervention>>,
    dispatch_role_overrides: &dashmap::DashMap<ChannelId, ChannelId>,
) -> Option<(Intervention, bool)> {
    let mut remove_queue = false;
    let (next, has_more) = if let Some(queue) = intervention_queue.get_mut(&channel_id) {
        let next = dequeue_next_soft_intervention(queue);
        let has_more = has_soft_intervention(queue);
        remove_queue = queue.is_empty();
        (next.intervention, has_more.has_pending)
    } else {
        (None, false)
    };

    if next.is_none() {
        if remove_queue {
            intervention_queue.remove(&channel_id);
            dispatch_role_overrides.remove(&channel_id);
            if let Err(error) = save_channel_queue(provider, token_hash, channel_id, &[], None) {
                tracing::error!(
                    provider = provider.as_str(),
                    token_hash,
                    channel_id = channel_id.get(),
                    "failed to persist pending queue removal: {error}"
                );
            }
        }
        return None;
    }

    let intervention = next.unwrap();

    if remove_queue {
        intervention_queue.remove(&channel_id);
        dispatch_role_overrides.remove(&channel_id);
        if let Err(error) = save_channel_queue(provider, token_hash, channel_id, &[], None) {
            tracing::error!(
                provider = provider.as_str(),
                token_hash,
                channel_id = channel_id.get(),
                "failed to persist pending queue removal: {error}"
            );
        }
    } else if let Some(queue) = intervention_queue.get(&channel_id) {
        if let Err(error) = save_channel_queue(
            provider,
            token_hash,
            channel_id,
            queue,
            dispatch_role_overrides
                .get(&channel_id)
                .map(|override_id| override_id.value().get()),
        ) {
            tracing::error!(
                provider = provider.as_str(),
                token_hash,
                channel_id = channel_id.get(),
                "failed to persist pending queue after dequeue: {error}"
            );
        }
    }

    Some((intervention, has_more))
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn requeue_intervention_front_persisted(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
    intervention_queue: &mut HashMap<ChannelId, Vec<Intervention>>,
    dispatch_role_overrides: &dashmap::DashMap<ChannelId, ChannelId>,
    intervention: Intervention,
) {
    let queue = intervention_queue.entry(channel_id).or_default();
    let _ = requeue_intervention_front(queue, intervention);
    if let Err(error) = save_channel_queue(
        provider,
        token_hash,
        channel_id,
        queue,
        dispatch_role_overrides
            .get(&channel_id)
            .map(|override_id| override_id.value().get()),
    ) {
        tracing::error!(
            provider = provider.as_str(),
            token_hash,
            channel_id = channel_id.get(),
            "failed to persist pending queue after requeue: {error}"
        );
    }
}

#[derive(Clone, Debug)]
pub(crate) struct QueuePersistenceContext {
    pub(crate) provider: ProviderKind,
    pub(crate) token_hash: String,
    pub(crate) dispatch_role_override: Option<u64>,
}

impl QueuePersistenceContext {
    pub(crate) fn new(
        provider: &ProviderKind,
        token_hash: &str,
        dispatch_role_override: Option<u64>,
    ) -> Self {
        Self {
            provider: provider.clone(),
            token_hash: token_hash.to_string(),
            dispatch_role_override,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct HydratePendingQueueResult {
    pub(crate) absorbed: usize,
    pub(crate) queue_len_after: usize,
    pub(crate) restored_override: Option<ChannelId>,
    pub(crate) persistence_error: Option<String>,
}

#[derive(Clone, Default)]
pub(crate) struct ChannelMailboxSnapshot {
    pub(crate) cancel_token: Option<Arc<CancelToken>>,
    pub(crate) active_request_owner: Option<UserId>,
    pub(crate) active_user_message_id: Option<MessageId>,
    pub(crate) intervention_queue: Vec<Intervention>,
    pub(crate) recovery_started_at: Option<Instant>,
    /// #1031: wall-clock instant the current active turn began (UTC). Set by
    /// the mailbox actor whenever `cancel_token` transitions from `None` to
    /// `Some`; cleared on finalize / clear. Idle detector uses this as a
    /// freshness anchor so the banner doesn't fire within the first poll of
    /// a brand-new turn.
    pub(crate) turn_started_at: Option<DateTime<Utc>>,
}

pub(crate) struct FinishTurnResult {
    pub(crate) removed_token: Option<Arc<CancelToken>>,
    pub(crate) has_pending: bool,
    pub(crate) mailbox_online: bool,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
    pub(crate) persistence_error: Option<String>,
}

pub(crate) struct ClearChannelResult {
    pub(crate) removed_token: Option<Arc<CancelToken>>,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
    pub(crate) persistence_error: Option<String>,
}

pub(crate) struct CancelActiveTurnResult {
    pub(crate) token: Option<Arc<CancelToken>>,
    pub(crate) already_stopping: bool,
}

pub(crate) struct HasPendingSoftQueueResult {
    pub(crate) has_pending: bool,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
    pub(crate) persistence_error: Option<String>,
}

pub(crate) struct RecoveryKickoffResult {
    pub(crate) activated_turn: bool,
}

pub(crate) struct RestartDrainResult {
    pub(crate) queued_count: usize,
    pub(crate) persistence_error: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct QueuePersistenceFailure {
    pub(crate) channel_id: ChannelId,
    pub(crate) error: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RestartDrainAllResult {
    pub(crate) queued_count: usize,
    pub(crate) persistence_errors: Vec<QueuePersistenceFailure>,
}

/// #2728: identifies which guard in `enqueue_intervention` produced an
/// `enqueued = false` outcome. Callers surface this through the producer-exit
/// diagnostic JSON so the next adk-cc-style incident is one log line away from
/// path A / B / C classification instead of code-only inference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EnqueueRefusalReason {
    /// The incoming `message_id` is already present in some queued entry's
    /// `source_message_ids` — duplicate insert from a re-entry or rehydrated
    /// queue.
    SourceIdAlreadyQueued,
    /// The queue's last entry matches the incoming intervention on
    /// `(author_id, text, reply_context, has_reply_boundary)` within
    /// `INTERVENTION_DEDUP_WINDOW` — rapid-resend dedup.
    LastItemDedup,
    /// The `ChannelMailboxHandle` could not reach the mailbox actor (mpsc
    /// closed or oneshot dropped). Surfaced only at the handle layer.
    ActorUnreachable,
}

impl EnqueueRefusalReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            EnqueueRefusalReason::SourceIdAlreadyQueued => "source_id_already_queued",
            EnqueueRefusalReason::LastItemDedup => "last_item_dedup",
            EnqueueRefusalReason::ActorUnreachable => "actor_unreachable",
        }
    }
}

pub(crate) struct EnqueueInterventionResult {
    pub(crate) enqueued: bool,
    /// True when the incoming intervention was folded into the previous queue
    /// entry via `should_merge_intervention` (text concatenated, source IDs
    /// accumulated). Callers use this to surface a different reaction emoji
    /// for merged messages so users can tell merged from standalone entries.
    pub(crate) merged: bool,
    /// #2728: identifies which guard in `enqueue_intervention` (or the
    /// handle-layer actor fallback) produced the refusal. Persistence failures
    /// are reported in `persistence_error` instead so adding that path does not
    /// expand the externally matched refusal enum.
    pub(crate) refusal_reason: Option<EnqueueRefusalReason>,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
    pub(crate) persistence_error: Option<String>,
}

pub(crate) struct CancelQueuedMessageResult {
    pub(crate) removed: Option<Intervention>,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
    pub(crate) persistence_error: Option<String>,
}

pub(crate) struct TakeNextSoftResult {
    pub(crate) intervention: Option<Intervention>,
    pub(crate) has_more: bool,
    pub(crate) queue_len_after: usize,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
    pub(crate) persistence_error: Option<String>,
}

pub(crate) struct RequeueInterventionResult {
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
    pub(crate) persistence_error: Option<String>,
}

static GLOBAL_CHANNEL_MAILBOXES: LazyLock<dashmap::DashMap<ChannelId, ChannelMailboxHandle>> =
    LazyLock::new(dashmap::DashMap::new);

#[derive(Clone)]
pub(crate) struct ChannelMailboxHandle {
    sender: mpsc::UnboundedSender<ChannelMailboxMsg>,
}

impl ChannelMailboxHandle {
    async fn request<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<T>) -> ChannelMailboxMsg,
        fallback: T,
    ) -> T {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.sender.send(build(reply_tx)).is_err() {
            return fallback;
        }
        reply_rx.await.unwrap_or(fallback)
    }

    pub(crate) async fn snapshot(&self) -> ChannelMailboxSnapshot {
        self.request(
            |reply| ChannelMailboxMsg::Snapshot { reply },
            ChannelMailboxSnapshot::default(),
        )
        .await
    }

    pub(crate) async fn has_active_turn(&self) -> bool {
        self.request(|reply| ChannelMailboxMsg::HasActiveTurn { reply }, false)
            .await
    }

    pub(crate) async fn cancel_token(&self) -> Option<Arc<CancelToken>> {
        self.request(|reply| ChannelMailboxMsg::CancelToken { reply }, None)
            .await
    }

    pub(crate) async fn cancel_active_turn(&self) -> CancelActiveTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::CancelActiveTurn { reply },
            CancelActiveTurnResult {
                token: None,
                already_stopping: false,
            },
        )
        .await
    }

    /// #2374 — atomic "set cancel reason + flip cancelled" performed by
    /// the mailbox actor. PR #2373 (#2335) set `cancel_source` from the
    /// caller task before sending the actor a `CancelActiveTurn`; that
    /// kept the writes ordered for the common path but left a small
    /// reorder window where two concurrent cancellers could both fetch
    /// the same `cancel_token`, race to call `set_cancel_source`, then
    /// have the actor flip `cancelled` based on whichever message it
    /// dequeued first. Moving the reason write INTO the actor makes the
    /// reason-then-flip sequence genuinely sequential per channel and
    /// eliminates the small ordering window the previous design left.
    ///
    /// Semantics:
    ///  - If the active token is already cancelled (`already_stopping`),
    ///    the reason is NOT overwritten — earlier attribution wins, the
    ///    same protection PR #2373 added to the caller-side write.
    ///  - If no active token exists, this is a no-op (returns
    ///    `token: None`).
    pub(crate) async fn cancel_active_turn_with_reason(
        &self,
        reason: String,
    ) -> CancelActiveTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::CancelActiveTurnWithReason { reason, reply },
            CancelActiveTurnResult {
                token: None,
                already_stopping: false,
            },
        )
        .await
    }

    pub(crate) async fn cancel_active_turn_if_current(
        &self,
        expected_token: Arc<CancelToken>,
    ) -> CancelActiveTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::CancelActiveTurnIfCurrent {
                expected_token,
                reply,
            },
            CancelActiveTurnResult {
                token: None,
                already_stopping: false,
            },
        )
        .await
    }

    /// #2374 — see [`Self::cancel_active_turn_with_reason`]. This variant
    /// preserves the `if_current` guard so a stale caller cannot cancel
    /// a freshly-restarted turn that happens to live on the same channel.
    pub(crate) async fn cancel_active_turn_if_current_with_reason(
        &self,
        expected_token: Arc<CancelToken>,
        reason: String,
    ) -> CancelActiveTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::CancelActiveTurnIfCurrentWithReason {
                expected_token,
                reason,
                reply,
            },
            CancelActiveTurnResult {
                token: None,
                already_stopping: false,
            },
        )
        .await
    }

    /// #2374 Codex round-1 fix (HIGH-1) — actor-owned guarded cancel
    /// keyed by `user_message_id`. The handoff cancel-tombstone retry
    /// path must only cancel the target-channel turn that was actually
    /// started by the original handoff prompt; an unguarded cancel
    /// would also kill an unrelated turn that happened to start on the
    /// same target channel after the original handoff turn finalized.
    /// The actor performs the identity check inline so the read of
    /// `active_user_message_id` and the cancel flip are observed as a
    /// single per-channel transition.
    ///
    /// Returns `token: None` when the active turn's `user_message_id`
    /// does not match `expected_user_message_id` (or no active turn
    /// exists at all).
    pub(crate) async fn cancel_active_turn_if_user_message_with_reason(
        &self,
        expected_user_message_id: MessageId,
        reason: String,
    ) -> CancelActiveTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::CancelActiveTurnIfUserMessageWithReason {
                expected_user_message_id,
                reason,
                reply,
            },
            CancelActiveTurnResult {
                token: None,
                already_stopping: false,
            },
        )
        .await
    }

    pub(crate) async fn try_start_turn(
        &self,
        cancel_token: Arc<CancelToken>,
        request_owner: UserId,
        user_message_id: MessageId,
    ) -> bool {
        self.request(
            |reply| ChannelMailboxMsg::TryStartTurn {
                cancel_token,
                request_owner,
                user_message_id,
                reply,
            },
            false,
        )
        .await
    }

    pub(crate) async fn restore_active_turn(
        &self,
        cancel_token: Arc<CancelToken>,
        request_owner: UserId,
        user_message_id: MessageId,
    ) {
        let _ = self
            .request(
                |reply| ChannelMailboxMsg::RestoreActiveTurn {
                    cancel_token,
                    request_owner,
                    user_message_id,
                    reply,
                },
                (),
            )
            .await;
    }

    pub(crate) async fn recovery_kickoff(
        &self,
        cancel_token: Arc<CancelToken>,
        request_owner: UserId,
        user_message_id: MessageId,
    ) -> RecoveryKickoffResult {
        self.request(
            |reply| ChannelMailboxMsg::RecoveryKickoff {
                cancel_token,
                request_owner,
                user_message_id,
                reply,
            },
            RecoveryKickoffResult {
                activated_turn: false,
            },
        )
        .await
    }

    pub(crate) async fn clear_recovery_marker(&self) {
        let _ = self
            .request(|reply| ChannelMailboxMsg::ClearRecoveryMarker { reply }, ())
            .await;
    }

    pub(crate) async fn enqueue(
        &self,
        intervention: Intervention,
        persistence: QueuePersistenceContext,
    ) -> EnqueueInterventionResult {
        self.request(
            |reply| ChannelMailboxMsg::Enqueue {
                intervention,
                persistence,
                reply,
            },
            EnqueueInterventionResult {
                enqueued: false,
                merged: false,
                refusal_reason: Some(EnqueueRefusalReason::ActorUnreachable),
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    pub(crate) async fn has_pending_soft_queue(
        &self,
        persistence: QueuePersistenceContext,
    ) -> HasPendingSoftQueueResult {
        self.request(
            |reply| ChannelMailboxMsg::HasPendingSoftQueue { persistence, reply },
            HasPendingSoftQueueResult {
                has_pending: false,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    pub(crate) async fn take_next_soft(
        &self,
        persistence: QueuePersistenceContext,
    ) -> TakeNextSoftResult {
        self.request(
            |reply| ChannelMailboxMsg::TakeNextSoft { persistence, reply },
            TakeNextSoftResult {
                intervention: None,
                has_more: false,
                queue_len_after: 0,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    pub(crate) async fn requeue_front(
        &self,
        intervention: Intervention,
        persistence: QueuePersistenceContext,
    ) -> RequeueInterventionResult {
        self.request(
            |reply| ChannelMailboxMsg::RequeueFront {
                intervention,
                persistence,
                reply,
            },
            RequeueInterventionResult {
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    pub(crate) async fn cancel_queued_message(
        &self,
        message_id: MessageId,
        persistence: QueuePersistenceContext,
    ) -> CancelQueuedMessageResult {
        self.request(
            |reply| ChannelMailboxMsg::CancelQueuedMessage {
                message_id,
                persistence,
                reply,
            },
            CancelQueuedMessageResult {
                removed: None,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    pub(crate) async fn finish_turn(
        &self,
        persistence: QueuePersistenceContext,
    ) -> FinishTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::FinishTurn { persistence, reply },
            FinishTurnResult {
                removed_token: None,
                has_pending: false,
                mailbox_online: false,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    pub(crate) async fn hard_stop(&self) -> FinishTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::HardStop { reply },
            FinishTurnResult {
                removed_token: None,
                has_pending: false,
                mailbox_online: false,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    pub(crate) async fn finish_cancelled_turn(&self) -> FinishTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::FinishCancelledTurn { reply },
            FinishTurnResult {
                removed_token: None,
                has_pending: false,
                mailbox_online: false,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    pub(crate) async fn clear(&self, persistence: QueuePersistenceContext) -> ClearChannelResult {
        self.request(
            |reply| ChannelMailboxMsg::Clear { persistence, reply },
            ClearChannelResult {
                removed_token: None,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    /// #2706: queue-only purge. Drains the intervention queue without
    /// touching the active `cancel_token`, so a turn that entered the
    /// mailbox between a sibling force-kill and this call is not
    /// collaterally cancelled.
    pub(crate) async fn purge_queue(&self, persistence: QueuePersistenceContext) -> usize {
        self.request(
            |reply| ChannelMailboxMsg::PurgeQueue { persistence, reply },
            0,
        )
        .await
    }

    pub(crate) async fn replace_queue(
        &self,
        queue: Vec<Intervention>,
        persistence: QueuePersistenceContext,
    ) {
        let _ = self
            .request(
                |reply| ChannelMailboxMsg::ReplaceQueue {
                    queue,
                    persistence,
                    reply,
                },
                (),
            )
            .await;
    }

    pub(crate) async fn hydrate_pending_queue_from_disk(
        &self,
        persistence: QueuePersistenceContext,
    ) -> HydratePendingQueueResult {
        self.request(
            |reply| ChannelMailboxMsg::HydratePendingQueueFromDisk { persistence, reply },
            HydratePendingQueueResult::default(),
        )
        .await
    }

    pub(crate) async fn restart_drain(
        &self,
        persistence: QueuePersistenceContext,
    ) -> RestartDrainResult {
        self.request(
            |reply| ChannelMailboxMsg::RestartDrain { persistence, reply },
            RestartDrainResult {
                queued_count: 0,
                persistence_error: None,
            },
        )
        .await
    }

    pub(crate) async fn extend_timeout(
        &self,
        extend_by_secs: u64,
    ) -> Result<WatchdogDeadlineExtension, WatchdogDeadlineExtensionError> {
        self.request(
            |reply| ChannelMailboxMsg::ExtendTimeout {
                extend_by_secs,
                reply,
            },
            Err(WatchdogDeadlineExtensionError::MailboxUnavailable),
        )
        .await
    }

    pub(crate) async fn take_timeout_override(&self) -> Option<WatchdogDeadlineExtension> {
        self.request(
            |reply| ChannelMailboxMsg::TakeTimeoutOverride { reply },
            None,
        )
        .await
    }

    pub(crate) async fn clear_timeout_override(&self) {
        let _ = self
            .request(
                |reply| ChannelMailboxMsg::ClearTimeoutOverride { reply },
                (),
            )
            .await;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WatchdogDeadlineExtension {
    pub(crate) requested_deadline_ms: i64,
    pub(crate) new_deadline_ms: i64,
    pub(crate) max_deadline_ms: i64,
    pub(crate) applied_extend_secs: u64,
    pub(crate) requested_extend_secs: u64,
    pub(crate) extension_count: u32,
    pub(crate) extension_count_limit: u32,
    pub(crate) extension_total_secs: u64,
    pub(crate) extension_total_secs_limit: u64,
    pub(crate) clamped: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WatchdogDeadlineExtensionError {
    MailboxUnavailable,
    NoActiveTurn,
}

/// #2443 — deterministic "recovery finished" signal per channel.
///
/// Pairs a `tokio::sync::Notify` with a one-shot `latched` flag so a
/// `recovery_done` event raised before a watcher subscribes is still
/// observable. Without the latch, `Notify::notify_waiters` would lose the
/// signal whenever recovery completes BEFORE the watcher reaches its
/// `notified()` await, re-introducing exactly the race the 60s timeout was
/// papering over. The latch flips on the first `mark_done` call and
/// `wait()` short-circuits on subsequent observers — recovery sessions are
/// monotonic per channel within the lifetime of this signal.
///
/// Callers reset the latch when a *new* recovery begins (so the next watcher
/// wave doesn't see a stale "already done"). `reset()` is idempotent.
pub(crate) struct RecoveryDoneSignal {
    notify: Notify,
    latched: std::sync::atomic::AtomicBool,
}

impl RecoveryDoneSignal {
    fn new() -> Self {
        Self {
            notify: Notify::new(),
            latched: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Mark recovery as finished. Wakes all current waiters and latches the
    /// signal so subsequent `wait()` calls return immediately until `reset()`.
    pub(crate) fn mark_done(&self) {
        self.latched.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Reset the latch so the next recovery cycle starts clean. Should be
    /// called at recovery kickoff so an old "done" flag does not satisfy a
    /// watcher waiting for the new run.
    pub(crate) fn reset(&self) {
        self.latched.store(false, Ordering::Release);
    }

    /// Wait until `mark_done` is observed. Returns immediately if the latch
    /// is already set (race-free for observers that subscribe after the
    /// notification fires).
    pub(crate) async fn wait(&self) {
        if self.latched.load(Ordering::Acquire) {
            return;
        }
        // Subscribe BEFORE the second check to close the
        // observe-then-subscribe window. `Notify::notified()` returns a
        // future that registers a waiter on first poll; recheck the flag
        // afterwards in case `mark_done` ran between the load and the
        // subscribe.
        let notified = self.notify.notified();
        tokio::pin!(notified);
        if self.latched.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

/// #2424 — generic "active turn finished" signal per channel.
///
/// Same latch shape as `RecoveryDoneSignal`: a terminal mailbox transition
/// can happen before a deferred monitor auto-turn subscribes, so late
/// subscribers must observe the already-finished state without falling back
/// to mailbox-state polling.
pub(crate) struct TurnFinishedSignal {
    notify: Notify,
    latched: std::sync::atomic::AtomicBool,
}

impl TurnFinishedSignal {
    fn new() -> Self {
        Self {
            notify: Notify::new(),
            latched: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(crate) fn mark_done(&self) {
        self.latched.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub(crate) fn reset(&self) {
        self.latched.store(false, Ordering::Release);
    }

    pub(crate) async fn wait(&self) {
        if self.latched.load(Ordering::Acquire) {
            return;
        }
        let notified = self.notify.notified();
        tokio::pin!(notified);
        if self.latched.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

static GLOBAL_RECOVERY_DONE_SIGNALS: LazyLock<
    dashmap::DashMap<ChannelId, Arc<RecoveryDoneSignal>>,
> = LazyLock::new(dashmap::DashMap::new);
static GLOBAL_TURN_FINISHED_SIGNALS: LazyLock<
    dashmap::DashMap<ChannelId, Arc<TurnFinishedSignal>>,
> = LazyLock::new(dashmap::DashMap::new);

fn turn_finished_signal(channel_id: ChannelId) -> Arc<TurnFinishedSignal> {
    if let Some(existing) = GLOBAL_TURN_FINISHED_SIGNALS.get(&channel_id) {
        return existing.value().clone();
    }
    let signal = Arc::new(TurnFinishedSignal::new());
    match GLOBAL_TURN_FINISHED_SIGNALS.entry(channel_id) {
        dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            entry.insert(signal.clone());
            signal
        }
    }
}

fn reset_turn_finished_signal(channel_id: ChannelId) {
    turn_finished_signal(channel_id).reset();
}

fn mark_turn_finished_signal_done(channel_id: ChannelId) {
    turn_finished_signal(channel_id).mark_done();
}

#[derive(Clone, Default)]
pub(crate) struct ChannelMailboxRegistry {
    handles: Arc<dashmap::DashMap<ChannelId, ChannelMailboxHandle>>,
    /// #2443 — per-channel "recovery finished" signals consumed by
    /// `watchers/lifecycle.rs` to graduate the 60s `recovery_started_at < 60s`
    /// skip heuristic. Stored in a separate map (rather than fields on the
    /// mailbox actor state) so both the recovery_engine producer and the
    /// watchers/lifecycle consumer can take a clone without round-tripping
    /// through the actor's message channel.
    recovery_done: Arc<dashmap::DashMap<ChannelId, Arc<RecoveryDoneSignal>>>,
    /// #2424 — per-channel generic "turn finished" signals consumed by
    /// deferred monitor auto-turn. Stored beside `recovery_done` so callers
    /// can clone the signal without actor round-trips.
    turn_finished: Arc<dashmap::DashMap<ChannelId, Arc<TurnFinishedSignal>>>,
}

impl ChannelMailboxRegistry {
    pub(crate) fn handle(&self, channel_id: ChannelId) -> ChannelMailboxHandle {
        if let Some(existing) = self.handles.get(&channel_id) {
            return existing.clone();
        }

        let handle = spawn_channel_mailbox(channel_id);
        let resolved = match self.handles.entry(channel_id) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(handle.clone());
                handle
            }
        };
        GLOBAL_CHANNEL_MAILBOXES.insert(channel_id, resolved.clone());
        resolved
    }

    pub(crate) fn global_handle(channel_id: ChannelId) -> Option<ChannelMailboxHandle> {
        GLOBAL_CHANNEL_MAILBOXES
            .get(&channel_id)
            .map(|entry| entry.value().clone())
    }

    /// #2443 — fetch or create the recovery-done signal for this channel.
    /// Cloning the `Arc` is cheap; the signal lives for the lifetime of the
    /// registry. The same `Arc` is mirrored into `GLOBAL_RECOVERY_DONE_SIGNALS`
    /// so callers that only have a `ChannelId` (no registry handle, e.g.
    /// helper free functions outside `SharedData`) can resolve via
    /// `global_recovery_done`.
    pub(crate) fn recovery_done(&self, channel_id: ChannelId) -> Arc<RecoveryDoneSignal> {
        if let Some(existing) = self.recovery_done.get(&channel_id) {
            return existing.clone();
        }
        let signal = Arc::new(RecoveryDoneSignal::new());
        let resolved = match self.recovery_done.entry(channel_id) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(signal.clone());
                signal
            }
        };
        GLOBAL_RECOVERY_DONE_SIGNALS.insert(channel_id, resolved.clone());
        resolved
    }

    /// #2443 — globally resolvable variant. Returns `None` only when no
    /// `recovery_done()` call has happened yet for this channel; callers
    /// that need a signal regardless should use the per-instance accessor.
    pub(crate) fn global_recovery_done(channel_id: ChannelId) -> Option<Arc<RecoveryDoneSignal>> {
        GLOBAL_RECOVERY_DONE_SIGNALS
            .get(&channel_id)
            .map(|entry| entry.value().clone())
    }

    pub(crate) fn turn_finished(&self, channel_id: ChannelId) -> Arc<TurnFinishedSignal> {
        if let Some(existing) = self.turn_finished.get(&channel_id) {
            return existing.clone();
        }
        let signal = turn_finished_signal(channel_id);
        let resolved = match self.turn_finished.entry(channel_id) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(signal.clone());
                signal
            }
        };
        GLOBAL_TURN_FINISHED_SIGNALS.insert(channel_id, resolved.clone());
        resolved
    }

    pub(crate) fn global_turn_finished(channel_id: ChannelId) -> Option<Arc<TurnFinishedSignal>> {
        GLOBAL_TURN_FINISHED_SIGNALS
            .get(&channel_id)
            .map(|entry| entry.value().clone())
    }

    pub(crate) async fn snapshot_all(&self) -> HashMap<ChannelId, ChannelMailboxSnapshot> {
        let handles: Vec<_> = self
            .handles
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect();
        let mut snapshots = HashMap::new();
        for (channel_id, handle) in handles {
            snapshots.insert(channel_id, handle.snapshot().await);
        }
        snapshots
    }

    pub(crate) async fn restart_drain_all(
        &self,
        provider: &ProviderKind,
        token_hash: &str,
        dispatch_role_overrides: &dashmap::DashMap<ChannelId, ChannelId>,
    ) -> RestartDrainAllResult {
        let handles: Vec<_> = self
            .handles
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect();
        let mut queued_total = 0usize;
        let mut persistence_errors = Vec::new();
        for (channel_id, handle) in handles {
            let persistence = QueuePersistenceContext::new(
                provider,
                token_hash,
                dispatch_role_overrides
                    .get(&channel_id)
                    .map(|override_id| override_id.value().get()),
            );
            let result = handle.restart_drain(persistence).await;
            queued_total += result.queued_count;
            if let Some(error) = result.persistence_error {
                persistence_errors.push(QueuePersistenceFailure { channel_id, error });
            }
        }
        RestartDrainAllResult {
            queued_count: queued_total,
            persistence_errors,
        }
    }
}

enum ChannelMailboxMsg {
    Snapshot {
        reply: oneshot::Sender<ChannelMailboxSnapshot>,
    },
    HasActiveTurn {
        reply: oneshot::Sender<bool>,
    },
    CancelToken {
        reply: oneshot::Sender<Option<Arc<CancelToken>>>,
    },
    CancelActiveTurn {
        reply: oneshot::Sender<CancelActiveTurnResult>,
    },
    /// #2374 — atomic reason-write + cancel flip performed by the actor.
    CancelActiveTurnWithReason {
        reason: String,
        reply: oneshot::Sender<CancelActiveTurnResult>,
    },
    CancelActiveTurnIfCurrent {
        expected_token: Arc<CancelToken>,
        reply: oneshot::Sender<CancelActiveTurnResult>,
    },
    /// #2374 — see `CancelActiveTurnWithReason`. Variant that also matches
    /// `expected_token` so a stale caller cannot cancel a restarted turn.
    CancelActiveTurnIfCurrentWithReason {
        expected_token: Arc<CancelToken>,
        reason: String,
        reply: oneshot::Sender<CancelActiveTurnResult>,
    },
    /// #2374 Codex round-1 fix (HIGH-1) — identity-guarded cancel by
    /// active `user_message_id`. See
    /// `ChannelMailboxHandle::cancel_active_turn_if_user_message_with_reason`.
    CancelActiveTurnIfUserMessageWithReason {
        expected_user_message_id: MessageId,
        reason: String,
        reply: oneshot::Sender<CancelActiveTurnResult>,
    },
    TryStartTurn {
        cancel_token: Arc<CancelToken>,
        request_owner: UserId,
        user_message_id: MessageId,
        reply: oneshot::Sender<bool>,
    },
    RestoreActiveTurn {
        cancel_token: Arc<CancelToken>,
        request_owner: UserId,
        user_message_id: MessageId,
        reply: oneshot::Sender<()>,
    },
    RecoveryKickoff {
        cancel_token: Arc<CancelToken>,
        request_owner: UserId,
        user_message_id: MessageId,
        reply: oneshot::Sender<RecoveryKickoffResult>,
    },
    ClearRecoveryMarker {
        reply: oneshot::Sender<()>,
    },
    Enqueue {
        intervention: Intervention,
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<EnqueueInterventionResult>,
    },
    HasPendingSoftQueue {
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<HasPendingSoftQueueResult>,
    },
    TakeNextSoft {
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<TakeNextSoftResult>,
    },
    RequeueFront {
        intervention: Intervention,
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<RequeueInterventionResult>,
    },
    CancelQueuedMessage {
        message_id: MessageId,
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<CancelQueuedMessageResult>,
    },
    FinishTurn {
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<FinishTurnResult>,
    },
    HardStop {
        reply: oneshot::Sender<FinishTurnResult>,
    },
    FinishCancelledTurn {
        reply: oneshot::Sender<FinishTurnResult>,
    },
    Clear {
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<ClearChannelResult>,
    },
    /// #2706: drain the intervention queue without touching the active
    /// `cancel_token`. Used by `cancel_turn(force=true)` so the in-memory
    /// channel mailbox is emptied even if a fresh turn entered the actor
    /// between `force_kill_turn_without_cancel_event` and this purge.
    PurgeQueue {
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<usize>,
    },
    ReplaceQueue {
        queue: Vec<Intervention>,
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<()>,
    },
    HydratePendingQueueFromDisk {
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<HydratePendingQueueResult>,
    },
    RestartDrain {
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<RestartDrainResult>,
    },
    ExtendTimeout {
        extend_by_secs: u64,
        reply: oneshot::Sender<Result<WatchdogDeadlineExtension, WatchdogDeadlineExtensionError>>,
    },
    TakeTimeoutOverride {
        reply: oneshot::Sender<Option<WatchdogDeadlineExtension>>,
    },
    ClearTimeoutOverride {
        reply: oneshot::Sender<()>,
    },
}

#[derive(Default)]
struct ChannelMailboxState {
    cancel_token: Option<Arc<CancelToken>>,
    active_request_owner: Option<UserId>,
    active_user_message_id: Option<MessageId>,
    intervention_queue: Vec<Intervention>,
    last_persistence: Option<QueuePersistenceContext>,
    recovery_started_at: Option<Instant>,
    /// #1031: see `ChannelMailboxSnapshot::turn_started_at`. Mirrors the
    /// `cancel_token.is_some()` lifetime so the idle-detector freshness
    /// anchor is always source-of-truth from the mailbox actor itself.
    turn_started_at: Option<DateTime<Utc>>,
    watchdog_deadline_override: Option<WatchdogDeadlineExtension>,
    watchdog_extension_count: u32,
    watchdog_extension_total_secs: u64,
}

fn persist_queue(
    channel_id: ChannelId,
    queue: &[Intervention],
    persistence: &QueuePersistenceContext,
) -> Result<(), String> {
    save_channel_queue(
        &persistence.provider,
        &persistence.token_hash,
        channel_id,
        queue,
        persistence.dispatch_role_override,
    )
}

fn log_queue_persistence_rollback(
    operation: &str,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
    error: &str,
) {
    tracing::error!(
        operation,
        provider = persistence.provider.as_str(),
        token_hash = %persistence.token_hash,
        channel_id = channel_id.get(),
        error = %error,
        "rolled back in-memory pending queue mutation after durable persistence failed"
    );
}

fn persist_queue_or_restore(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
    previous_queue: Vec<Intervention>,
    operation: &str,
) -> Result<(), String> {
    match persist_queue(channel_id, &state.intervention_queue, persistence) {
        Ok(()) => Ok(()),
        Err(error) => {
            state.intervention_queue = previous_queue;
            log_queue_persistence_rollback(operation, channel_id, persistence, &error);
            Err(error)
        }
    }
}

fn hydrate_pending_queue_into_state(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    disk_items: Vec<Intervention>,
    persistence: QueuePersistenceContext,
    restored_override: Option<ChannelId>,
) -> HydratePendingQueueResult {
    state.last_persistence = Some(persistence.clone());
    let previous_queue = state.intervention_queue.clone();
    let mut existing_ids: HashSet<MessageId> = state
        .intervention_queue
        .iter()
        .map(|item| item.message_id)
        .collect();
    let mut absorbed = 0usize;
    // Walk in reverse so repeated `insert(0, …)` ends up with disk
    // items in their original order.
    for item in disk_items.into_iter().rev() {
        if !existing_ids.insert(item.message_id) {
            continue;
        }
        state.intervention_queue.insert(0, item);
        absorbed += 1;
    }
    if absorbed > 0 {
        if let Err(error) = persist_queue_or_restore(
            state,
            channel_id,
            &persistence,
            previous_queue,
            "hydrate_pending_queue_from_disk",
        ) {
            return HydratePendingQueueResult {
                absorbed: 0,
                queue_len_after: state.intervention_queue.len(),
                restored_override,
                persistence_error: Some(error),
            };
        }
    }
    HydratePendingQueueResult {
        absorbed,
        queue_len_after: state.intervention_queue.len(),
        restored_override,
        persistence_error: None,
    }
}

fn hydrate_pending_queue_from_disk_if_present(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
) -> HydratePendingQueueResult {
    let (disk_items, restored_override) =
        load_channel_pending_queue(&persistence.provider, &persistence.token_hash, channel_id);
    if disk_items.is_empty() {
        return HydratePendingQueueResult {
            absorbed: 0,
            queue_len_after: state.intervention_queue.len(),
            restored_override,
            persistence_error: None,
        };
    }

    let mut effective_persistence = persistence.clone();
    if effective_persistence.dispatch_role_override.is_none() {
        effective_persistence.dispatch_role_override =
            restored_override.map(|channel| channel.get());
    }
    hydrate_pending_queue_into_state(
        state,
        channel_id,
        disk_items,
        effective_persistence,
        restored_override,
    )
}

fn finalize_turn_state(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: Option<&QueuePersistenceContext>,
) -> FinishTurnResult {
    let removed_token = state.cancel_token.take();
    state.active_request_owner = None;
    state.active_user_message_id = None;
    state.recovery_started_at = None;
    state.turn_started_at = None;
    reset_watchdog_extension_state(state);
    let previous_len = state.intervention_queue.len();
    let previous_queue = state.intervention_queue.clone();
    let pending_result = has_soft_intervention(&mut state.intervention_queue);
    if let Some(persistence) = persistence {
        if state.intervention_queue.len() != previous_len || !state.intervention_queue.is_empty() {
            if let Err(error) = persist_queue_or_restore(
                state,
                channel_id,
                persistence,
                previous_queue,
                "finish_turn",
            ) {
                return FinishTurnResult {
                    removed_token,
                    has_pending: state
                        .intervention_queue
                        .iter()
                        .any(|item| item.mode == InterventionMode::Soft),
                    mailbox_online: true,
                    queue_exit_events: Vec::new(),
                    persistence_error: Some(error),
                };
            }
        }
    }
    FinishTurnResult {
        removed_token,
        has_pending: pending_result.has_pending,
        mailbox_online: true,
        queue_exit_events: pending_result.queue_exit_events,
        persistence_error: None,
    }
}

fn reset_watchdog_extension_state(state: &mut ChannelMailboxState) {
    state.watchdog_deadline_override = None;
    state.watchdog_extension_count = 0;
    state.watchdog_extension_total_secs = 0;
}

fn extend_active_watchdog_deadline(
    state: &mut ChannelMailboxState,
    requested_extend_secs: u64,
) -> Result<WatchdogDeadlineExtension, WatchdogDeadlineExtensionError> {
    let Some(cancel_token) = state.cancel_token.as_ref() else {
        return Err(WatchdogDeadlineExtensionError::NoActiveTurn);
    };

    let count_limit = u32::MAX;
    let total_secs_limit = u64::MAX;
    let applied_extend_secs = requested_extend_secs;

    let now_ms = Utc::now().timestamp_millis();
    let current_deadline = cancel_token.watchdog_deadline_ms.load(Ordering::Relaxed);
    let current_deadline = if current_deadline > 0 {
        current_deadline
    } else {
        now_ms
    };
    let current_max_deadline = cancel_token
        .watchdog_max_deadline_ms
        .load(Ordering::Relaxed);
    let current_max_deadline = if current_max_deadline > 0 {
        current_max_deadline
    } else {
        current_deadline
    };
    let requested_deadline_ms =
        std::cmp::max(current_deadline, now_ms) + requested_extend_secs as i64 * 1000;
    let new_deadline_ms =
        std::cmp::max(current_deadline, now_ms) + applied_extend_secs as i64 * 1000;
    let max_deadline_ms = std::cmp::max(current_max_deadline, new_deadline_ms);

    cancel_token
        .watchdog_deadline_ms
        .store(new_deadline_ms, Ordering::Relaxed);
    cancel_token
        .watchdog_max_deadline_ms
        .store(max_deadline_ms, Ordering::Relaxed);

    state.watchdog_extension_count = state.watchdog_extension_count.saturating_add(1);
    state.watchdog_extension_total_secs = state
        .watchdog_extension_total_secs
        .saturating_add(applied_extend_secs);

    let extension = WatchdogDeadlineExtension {
        requested_deadline_ms,
        new_deadline_ms,
        max_deadline_ms,
        applied_extend_secs,
        requested_extend_secs,
        extension_count: state.watchdog_extension_count,
        extension_count_limit: count_limit,
        extension_total_secs: state.watchdog_extension_total_secs,
        extension_total_secs_limit: total_secs_limit,
        clamped: false,
    };
    state.watchdog_deadline_override = Some(extension);
    Ok(extension)
}

#[cfg(test)]
mod turn_finished_signal_tests {
    use super::*;

    #[tokio::test]
    async fn turn_finished_latch_short_circuits_late_subscribers() {
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(242_411);
        let handle = registry.handle(channel_id);

        assert!(
            handle
                .try_start_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(24),
                    MessageId::new(2411),
                )
                .await
        );
        let finished = handle.hard_stop().await;
        assert!(finished.removed_token.is_some());

        let signal =
            ChannelMailboxRegistry::global_turn_finished(channel_id).expect("global signal");
        tokio::time::timeout(std::time::Duration::from_millis(25), signal.wait())
            .await
            .expect("late subscriber should observe latched turn-finished signal");
    }

    #[tokio::test]
    async fn turn_finished_reset_unlatches_on_new_turn() {
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(242_412);
        let handle = registry.handle(channel_id);

        assert!(
            handle
                .try_start_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(24),
                    MessageId::new(2412),
                )
                .await
        );
        let _ = handle.hard_stop().await;
        let signal = registry.turn_finished(channel_id);
        tokio::time::timeout(std::time::Duration::from_millis(25), signal.wait())
            .await
            .expect("finished turn should latch signal");

        assert!(
            handle
                .try_start_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(24),
                    MessageId::new(2413),
                )
                .await
        );
        let still_waiting =
            tokio::time::timeout(std::time::Duration::from_millis(25), signal.wait()).await;
        assert!(
            still_waiting.is_err(),
            "new active turn should reset the previous finished latch"
        );

        let _ = handle.hard_stop().await;
        tokio::time::timeout(std::time::Duration::from_millis(250), signal.wait())
            .await
            .expect("fresh finish should wake reset waiter");
    }
}

fn spawn_channel_mailbox(channel_id: ChannelId) -> ChannelMailboxHandle {
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut state = ChannelMailboxState::default();
        while let Some(msg) = rx.recv().await {
            match msg {
                ChannelMailboxMsg::Snapshot { reply } => {
                    let _ = reply.send(ChannelMailboxSnapshot {
                        cancel_token: state.cancel_token.clone(),
                        active_request_owner: state.active_request_owner,
                        active_user_message_id: state.active_user_message_id,
                        intervention_queue: state.intervention_queue.clone(),
                        recovery_started_at: state.recovery_started_at,
                        turn_started_at: state.turn_started_at,
                    });
                }
                ChannelMailboxMsg::HasActiveTurn { reply } => {
                    let _ = reply.send(state.cancel_token.is_some());
                }
                ChannelMailboxMsg::CancelToken { reply } => {
                    let _ = reply.send(state.cancel_token.clone());
                }
                ChannelMailboxMsg::CancelActiveTurn { reply } => {
                    let token = state.cancel_token.clone();
                    let already_stopping = token.as_ref().is_some_and(|token| {
                        token.cancelled.load(std::sync::atomic::Ordering::Relaxed)
                    });
                    if let Some(token) = token.as_ref()
                        && !already_stopping
                    {
                        token
                            .cancelled
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    let _ = reply.send(CancelActiveTurnResult {
                        token,
                        already_stopping,
                    });
                }
                ChannelMailboxMsg::CancelActiveTurnWithReason { reason, reply } => {
                    // #2374 — atomic, actor-serialized "reason then flip"
                    // for the active turn. The previous design (#2373)
                    // wrote `cancel_source` from the caller task BEFORE
                    // sending the actor a `CancelActiveTurn`. That kept
                    // the writes ordered for the common path but two
                    // concurrent cancellers could both observe the same
                    // pre-flip token, race on `set_cancel_source`, then
                    // have the actor flip `cancelled` on whichever
                    // message it dequeued first — losing one of the
                    // reasons. By owning the reason write here, the
                    // mailbox actor serializes both writes per channel.
                    //
                    // Existing-cancellation guard mirrors #2373: do NOT
                    // overwrite a reason once `cancelled` is set so
                    // earlier attribution (e.g. a watchdog timeout that
                    // already fired) wins over a later voice cancel.
                    let token = state.cancel_token.clone();
                    let already_stopping = token.as_ref().is_some_and(|token| {
                        token.cancelled.load(std::sync::atomic::Ordering::Relaxed)
                    });
                    if let Some(token) = token.as_ref()
                        && !already_stopping
                    {
                        token.set_cancel_source(reason.clone());
                        token
                            .cancelled
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    let _ = reply.send(CancelActiveTurnResult {
                        token,
                        already_stopping,
                    });
                }
                ChannelMailboxMsg::CancelActiveTurnIfCurrent {
                    expected_token,
                    reply,
                } => {
                    let token = state
                        .cancel_token
                        .clone()
                        .filter(|token| Arc::ptr_eq(token, &expected_token));
                    let already_stopping = token.as_ref().is_some_and(|token| {
                        token.cancelled.load(std::sync::atomic::Ordering::Relaxed)
                    });
                    if let Some(token) = token.as_ref()
                        && !already_stopping
                    {
                        token
                            .cancelled
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    let _ = reply.send(CancelActiveTurnResult {
                        token,
                        already_stopping,
                    });
                }
                ChannelMailboxMsg::CancelActiveTurnIfCurrentWithReason {
                    expected_token,
                    reason,
                    reply,
                } => {
                    // #2374 — atomic reason-then-flip with the
                    // `if_current` guard preserved. See the unguarded
                    // variant above for the broader rationale.
                    let token = state
                        .cancel_token
                        .clone()
                        .filter(|token| Arc::ptr_eq(token, &expected_token));
                    let already_stopping = token.as_ref().is_some_and(|token| {
                        token.cancelled.load(std::sync::atomic::Ordering::Relaxed)
                    });
                    if let Some(token) = token.as_ref()
                        && !already_stopping
                    {
                        token.set_cancel_source(reason.clone());
                        token
                            .cancelled
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    let _ = reply.send(CancelActiveTurnResult {
                        token,
                        already_stopping,
                    });
                }
                ChannelMailboxMsg::CancelActiveTurnIfUserMessageWithReason {
                    expected_user_message_id,
                    reason,
                    reply,
                } => {
                    // #2374 Codex round-1 fix (HIGH-1): only cancel
                    // when the active turn's `user_message_id` matches
                    // the caller's expected handoff message id. The
                    // tombstone retry path uses this so a still-later
                    // cancel for the same handoff cannot accidentally
                    // kill an unrelated turn that happened to start on
                    // the same target channel after the original
                    // handoff turn finalized. The actor performs the
                    // identity check + cancel as a single serialized
                    // step so no concurrent caller can swap the active
                    // turn between the check and the flip.
                    let identity_matches = state
                        .active_user_message_id
                        .is_some_and(|id| id == expected_user_message_id);
                    let token = if identity_matches {
                        state.cancel_token.clone()
                    } else {
                        None
                    };
                    let already_stopping = token.as_ref().is_some_and(|token| {
                        token.cancelled.load(std::sync::atomic::Ordering::Relaxed)
                    });
                    if let Some(token) = token.as_ref()
                        && !already_stopping
                    {
                        token.set_cancel_source(reason.clone());
                        token
                            .cancelled
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    let _ = reply.send(CancelActiveTurnResult {
                        token,
                        already_stopping,
                    });
                }
                ChannelMailboxMsg::TryStartTurn {
                    cancel_token,
                    request_owner,
                    user_message_id,
                    reply,
                } => {
                    let started = if state.cancel_token.is_some() {
                        false
                    } else {
                        reset_turn_finished_signal(channel_id);
                        state.cancel_token = Some(cancel_token);
                        state.active_request_owner = Some(request_owner);
                        state.active_user_message_id = Some(user_message_id);
                        state.recovery_started_at = None;
                        state.turn_started_at = Some(Utc::now());
                        reset_watchdog_extension_state(&mut state);
                        true
                    };
                    let _ = reply.send(started);
                }
                ChannelMailboxMsg::RestoreActiveTurn {
                    cancel_token,
                    request_owner,
                    user_message_id,
                    reply,
                } => {
                    reset_turn_finished_signal(channel_id);
                    let was_idle = state.cancel_token.is_none();
                    state.cancel_token = Some(cancel_token);
                    state.active_request_owner = Some(request_owner);
                    state.active_user_message_id = Some(user_message_id);
                    if was_idle || state.turn_started_at.is_none() {
                        state.turn_started_at = Some(Utc::now());
                    }
                    reset_watchdog_extension_state(&mut state);
                    let _ = reply.send(());
                }
                ChannelMailboxMsg::RecoveryKickoff {
                    cancel_token,
                    request_owner,
                    user_message_id,
                    reply,
                } => {
                    reset_turn_finished_signal(channel_id);
                    let activated_turn = state.cancel_token.is_none();
                    state.cancel_token = Some(cancel_token);
                    state.active_request_owner = Some(request_owner);
                    state.active_user_message_id = Some(user_message_id);
                    state.recovery_started_at = Some(Instant::now());
                    if activated_turn || state.turn_started_at.is_none() {
                        state.turn_started_at = Some(Utc::now());
                    }
                    reset_watchdog_extension_state(&mut state);
                    let _ = reply.send(RecoveryKickoffResult { activated_turn });
                }
                ChannelMailboxMsg::ClearRecoveryMarker { reply } => {
                    state.recovery_started_at = None;
                    let _ = reply.send(());
                }
                ChannelMailboxMsg::Enqueue {
                    intervention,
                    persistence,
                    reply,
                } => {
                    state.last_persistence = Some(persistence.clone());
                    let hydrate_result = hydrate_pending_queue_from_disk_if_present(
                        &mut state,
                        channel_id,
                        &persistence,
                    );
                    if let Some(error) = hydrate_result.persistence_error {
                        let _ = reply.send(EnqueueInterventionResult {
                            enqueued: false,
                            merged: false,
                            refusal_reason: None,
                            queue_exit_events: Vec::new(),
                            persistence_error: Some(error),
                        });
                        continue;
                    }
                    let previous_queue = state.intervention_queue.clone();
                    let mut enqueue_result =
                        enqueue_intervention(&mut state.intervention_queue, intervention);
                    if enqueue_result.enqueued
                        && let Err(error) = persist_queue_or_restore(
                            &mut state,
                            channel_id,
                            &persistence,
                            previous_queue,
                            "enqueue",
                        )
                    {
                        enqueue_result = EnqueueInterventionResult {
                            enqueued: false,
                            merged: false,
                            refusal_reason: None,
                            queue_exit_events: Vec::new(),
                            persistence_error: Some(error),
                        };
                    }
                    let _ = reply.send(enqueue_result);
                }
                ChannelMailboxMsg::HasPendingSoftQueue { persistence, reply } => {
                    state.last_persistence = Some(persistence.clone());
                    let previous_len = state.intervention_queue.len();
                    let previous_queue = state.intervention_queue.clone();
                    let mut pending_result = has_soft_intervention(&mut state.intervention_queue);
                    if state.intervention_queue.len() != previous_len
                        && let Err(error) = persist_queue_or_restore(
                            &mut state,
                            channel_id,
                            &persistence,
                            previous_queue,
                            "has_pending_soft_queue",
                        )
                    {
                        pending_result = HasPendingSoftQueueResult {
                            has_pending: state
                                .intervention_queue
                                .iter()
                                .any(|item| item.mode == InterventionMode::Soft),
                            queue_exit_events: Vec::new(),
                            persistence_error: Some(error),
                        };
                    }
                    let _ = reply.send(pending_result);
                }
                ChannelMailboxMsg::TakeNextSoft { persistence, reply } => {
                    state.last_persistence = Some(persistence.clone());
                    let previous_queue = state.intervention_queue.clone();
                    let next_result = dequeue_next_soft_intervention(&mut state.intervention_queue);
                    let queue_len_after = state.intervention_queue.len();
                    let result = if let Err(error) = persist_queue_or_restore(
                        &mut state,
                        channel_id,
                        &persistence,
                        previous_queue,
                        "take_next_soft",
                    ) {
                        TakeNextSoftResult {
                            intervention: None,
                            has_more: state
                                .intervention_queue
                                .iter()
                                .any(|item| item.mode == InterventionMode::Soft),
                            queue_len_after: state.intervention_queue.len(),
                            queue_exit_events: Vec::new(),
                            persistence_error: Some(error),
                        }
                    } else {
                        TakeNextSoftResult {
                            intervention: next_result.intervention,
                            has_more: next_result.has_more,
                            queue_len_after,
                            queue_exit_events: next_result.queue_exit_events,
                            persistence_error: None,
                        }
                    };
                    let _ = reply.send(result);
                }
                ChannelMailboxMsg::RequeueFront {
                    intervention,
                    persistence,
                    reply,
                } => {
                    state.last_persistence = Some(persistence.clone());
                    let previous_queue = state.intervention_queue.clone();
                    let requeue_result =
                        requeue_intervention_front(&mut state.intervention_queue, intervention);
                    let result = if let Err(error) = persist_queue_or_restore(
                        &mut state,
                        channel_id,
                        &persistence,
                        previous_queue,
                        "requeue_front",
                    ) {
                        RequeueInterventionResult {
                            queue_exit_events: Vec::new(),
                            persistence_error: Some(error),
                        }
                    } else {
                        RequeueInterventionResult {
                            queue_exit_events: requeue_result,
                            persistence_error: None,
                        }
                    };
                    let _ = reply.send(result);
                }
                ChannelMailboxMsg::CancelQueuedMessage {
                    message_id,
                    persistence,
                    reply,
                } => {
                    state.last_persistence = Some(persistence.clone());
                    let previous_queue = state.intervention_queue.clone();
                    let mut cancel_result = cancel_soft_intervention_by_message_id(
                        &mut state.intervention_queue,
                        message_id,
                    );
                    if cancel_result.removed.is_some()
                        || !cancel_result.queue_exit_events.is_empty()
                    {
                        if let Err(error) = persist_queue_or_restore(
                            &mut state,
                            channel_id,
                            &persistence,
                            previous_queue,
                            "cancel_queued_message",
                        ) {
                            cancel_result = CancelQueuedMessageResult {
                                removed: None,
                                queue_exit_events: Vec::new(),
                                persistence_error: Some(error),
                            };
                        }
                    }
                    let _ = reply.send(cancel_result);
                }
                ChannelMailboxMsg::FinishTurn { persistence, reply } => {
                    state.last_persistence = Some(persistence.clone());
                    let _ = reply.send(finalize_turn_state(
                        &mut state,
                        channel_id,
                        Some(&persistence),
                    ));
                    mark_turn_finished_signal_done(channel_id);
                }
                ChannelMailboxMsg::HardStop { reply } => {
                    let persistence = state.last_persistence.clone();
                    let _ = reply.send(finalize_turn_state(
                        &mut state,
                        channel_id,
                        persistence.as_ref(),
                    ));
                    mark_turn_finished_signal_done(channel_id);
                }
                ChannelMailboxMsg::FinishCancelledTurn { reply } => {
                    let should_finish = state.cancel_token.as_ref().is_some_and(|token| {
                        token.cancelled.load(std::sync::atomic::Ordering::Relaxed)
                    });
                    if should_finish {
                        let persistence = state.last_persistence.clone();
                        let _ = reply.send(finalize_turn_state(
                            &mut state,
                            channel_id,
                            persistence.as_ref(),
                        ));
                        mark_turn_finished_signal_done(channel_id);
                    } else {
                        let _ = reply.send(FinishTurnResult {
                            removed_token: None,
                            has_pending: state
                                .intervention_queue
                                .iter()
                                .any(|item| item.mode == InterventionMode::Soft),
                            mailbox_online: true,
                            queue_exit_events: Vec::new(),
                            persistence_error: None,
                        });
                    }
                }
                ChannelMailboxMsg::Clear { persistence, reply } => {
                    state.last_persistence = Some(persistence.clone());
                    let removed_token = state.cancel_token.take();
                    state.active_request_owner = None;
                    state.active_user_message_id = None;
                    state.recovery_started_at = None;
                    state.turn_started_at = None;
                    reset_watchdog_extension_state(&mut state);
                    let previous_queue = state.intervention_queue.clone();
                    let queue_exit_events = state
                        .intervention_queue
                        .drain(..)
                        .map(|intervention| {
                            QueueExitEvent::new(intervention, QueueExitKind::Superseded)
                        })
                        .collect();
                    let result = if let Err(error) = persist_queue_or_restore(
                        &mut state,
                        channel_id,
                        &persistence,
                        previous_queue,
                        "clear",
                    ) {
                        ClearChannelResult {
                            removed_token,
                            queue_exit_events: Vec::new(),
                            persistence_error: Some(error),
                        }
                    } else {
                        ClearChannelResult {
                            removed_token,
                            queue_exit_events,
                            persistence_error: None,
                        }
                    };
                    let _ = reply.send(result);
                    mark_turn_finished_signal_done(channel_id);
                }
                ChannelMailboxMsg::PurgeQueue { persistence, reply } => {
                    // #2706: queue-only purge. Leaves `cancel_token`,
                    // `active_request_owner`, `active_user_message_id`
                    // untouched so a turn that entered the actor in
                    // between force-kill and purge is not collaterally
                    // cancelled.
                    state.last_persistence = Some(persistence.clone());
                    let previous_queue = state.intervention_queue.clone();
                    let drained = state.intervention_queue.drain(..).count();
                    let result = if persist_queue_or_restore(
                        &mut state,
                        channel_id,
                        &persistence,
                        previous_queue,
                        "purge_queue",
                    )
                    .is_err()
                    {
                        0
                    } else {
                        drained
                    };
                    let _ = reply.send(result);
                }
                ChannelMailboxMsg::ReplaceQueue {
                    queue,
                    persistence,
                    reply,
                } => {
                    state.last_persistence = Some(persistence.clone());
                    let previous_queue = state.intervention_queue.clone();
                    state.intervention_queue = queue;
                    let _ = persist_queue_or_restore(
                        &mut state,
                        channel_id,
                        &persistence,
                        previous_queue,
                        "replace_queue",
                    );
                    let _ = reply.send(());
                }
                ChannelMailboxMsg::HydratePendingQueueFromDisk { persistence, reply } => {
                    // #1683: read the disk queue inside the mailbox actor so
                    // a dequeue that removes the file cannot race with a stale
                    // out-of-actor disk snapshot and reinsert an already
                    // processed item.
                    let result = hydrate_pending_queue_from_disk_if_present(
                        &mut state,
                        channel_id,
                        &persistence,
                    );
                    let _ = reply.send(result);
                }
                ChannelMailboxMsg::RestartDrain { persistence, reply } => {
                    state.last_persistence = Some(persistence.clone());
                    let persistence_error =
                        persist_queue(channel_id, &state.intervention_queue, &persistence).err();
                    let _ = reply.send(RestartDrainResult {
                        queued_count: if persistence_error.is_some() {
                            0
                        } else {
                            state.intervention_queue.len()
                        },
                        persistence_error,
                    });
                }
                ChannelMailboxMsg::ExtendTimeout {
                    extend_by_secs,
                    reply,
                } => {
                    let _ = reply.send(extend_active_watchdog_deadline(&mut state, extend_by_secs));
                }
                ChannelMailboxMsg::TakeTimeoutOverride { reply } => {
                    let _ = reply.send(state.watchdog_deadline_override.take());
                }
                ChannelMailboxMsg::ClearTimeoutOverride { reply } => {
                    state.watchdog_deadline_override = None;
                    let _ = reply.send(());
                }
            }
        }
    });
    ChannelMailboxHandle { sender: tx }
}

#[cfg(test)]
mod actor_hydrate_regression_tests {
    use super::*;
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};
    use std::time::SystemTime;

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";
    static TEST_ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct EnvGuard;

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
        }
    }

    fn queue_file_path(
        root: &Path,
        provider: &ProviderKind,
        token_hash: &str,
        channel_id: ChannelId,
    ) -> PathBuf {
        root.join("runtime")
            .join("discord_pending_queue")
            .join(provider.as_str())
            .join(token_hash)
            .join(format!("{}.json", channel_id.get()))
    }

    fn make_intervention(message_id: u64, text: &str, created_at: Instant) -> Intervention {
        Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            source_message_ids: vec![MessageId::new(message_id)],
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

    fn lock_test_env() -> MutexGuard<'static, ()> {
        TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn remove_channel_pending_queue_files_all_tokens_only_removes_target_channel() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(2708);
        let other_channel_id = ChannelId::new(2709);
        let first = queue_file_path(tmp.path(), &provider, "token-a", channel_id);
        let second = queue_file_path(tmp.path(), &provider, "token-b", channel_id);
        let other = queue_file_path(tmp.path(), &provider, "token-a", other_channel_id);
        for path in [&first, &second, &other] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "[]").unwrap();
        }

        let removed = remove_channel_pending_queue_files_all_tokens(&provider, channel_id);

        assert_eq!(removed, 2);
        assert!(!first.exists());
        assert!(!second.exists());
        assert!(other.exists());
    }

    #[tokio::test]
    async fn hydrate_from_disk_does_not_reinsert_after_actor_dequeue_removed_file() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let provider = ProviderKind::Claude;
        let token_hash = "mailbox-hydrate-after-dequeue";
        let channel_id = ChannelId::new(45);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, token_hash, None);

        handle
            .replace_queue(
                vec![make_intervention(10, "already-processed", Instant::now())],
                persistence.clone(),
            )
            .await;
        let path = queue_file_path(tmp.path(), &provider, token_hash, channel_id);
        assert!(path.exists(), "queue file must exist before dequeue");

        let taken = handle.take_next_soft(persistence.clone()).await;
        assert_eq!(
            taken.intervention.as_ref().map(|item| item.message_id),
            Some(MessageId::new(10))
        );
        assert_eq!(taken.queue_len_after, 0);
        assert!(
            !path.exists(),
            "actor dequeue must remove the disk file once the queue is empty"
        );

        let hydrate = handle.hydrate_pending_queue_from_disk(persistence).await;
        assert_eq!(
            hydrate.absorbed, 0,
            "#1683: actor-local disk hydrate must see the removed file, not reinsert a stale pre-dequeue snapshot"
        );
        assert_eq!(hydrate.queue_len_after, 0);
        assert!(handle.snapshot().await.intervention_queue.is_empty());
    }

    #[tokio::test]
    async fn cancel_active_turn_if_current_ignores_stale_watchdog_token() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(46));
        let active_token = Arc::new(CancelToken::new());
        let stale_token = Arc::new(CancelToken::new());

        handle
            .try_start_turn(active_token.clone(), UserId::new(9), MessageId::new(91))
            .await;

        let stale = handle.cancel_active_turn_if_current(stale_token).await;
        assert!(stale.token.is_none());
        assert!(!stale.already_stopping);
        assert!(
            !active_token
                .cancelled
                .load(std::sync::atomic::Ordering::Relaxed)
        );

        let current = handle
            .cancel_active_turn_if_current(active_token.clone())
            .await;
        assert!(current.token.is_some());
        assert!(!current.already_stopping);
        assert!(
            active_token
                .cancelled
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }

    #[test]
    fn cleanup_stale_pending_queue_tmp_files_removes_only_stale_tmp_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = ProviderKind::Claude;
        let token_hash = "tmp-cleanup-direct";
        let stale_tmp_a = tmp.path().join(".12345.json.interrupted.tmp");
        let stale_tmp_b = tmp.path().join(".23456.json.interrupted.tmp");
        let queue_json = tmp.path().join("34567.json");
        std::fs::write(&stale_tmp_a, b"partial").unwrap();
        std::fs::write(&stale_tmp_b, b"partial").unwrap();
        std::fs::write(&queue_json, b"[]").unwrap();

        let audits = cleanup_stale_pending_queue_tmp_files_in_dir(
            &provider,
            token_hash,
            tmp.path(),
            SystemTime::now() + Duration::from_secs(120),
            Duration::from_secs(60),
        );

        assert_eq!(audits.len(), 2);
        assert!(
            audits.iter().any(|audit| {
                audit.channel_id == Some(12345) && audit.action == "removed_stale"
            })
        );
        assert!(
            audits.iter().any(|audit| {
                audit.channel_id == Some(23456) && audit.action == "removed_stale"
            })
        );
        assert!(!stale_tmp_a.exists());
        assert!(!stale_tmp_b.exists());
        assert!(
            queue_json.exists(),
            "cleanup must not touch real queue files"
        );
    }

    #[test]
    fn cleanup_stale_pending_queue_tmp_files_preserves_active_tmp_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = ProviderKind::Claude;
        let token_hash = "tmp-cleanup-active";
        let active_tmp = tmp.path().join(".45678.json.inflight.tmp");
        std::fs::write(&active_tmp, b"partial").unwrap();

        let audits = cleanup_stale_pending_queue_tmp_files_in_dir(
            &provider,
            token_hash,
            tmp.path(),
            SystemTime::now(),
            Duration::from_secs(60),
        );

        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].channel_id, Some(45678));
        assert_eq!(audits[0].action, "preserved_active");
        assert!(active_tmp.exists(), "fresh tmp writes must be preserved");
    }

    #[test]
    fn cleanup_stale_pending_queue_tmp_files_under_root_scans_all_token_dirs() {
        let root = tempfile::tempdir().unwrap();
        let claude_token_dir = root.path().join("claude").join("token-a");
        let codex_token_dir = root.path().join("codex").join("token-b");
        std::fs::create_dir_all(&claude_token_dir).unwrap();
        std::fs::create_dir_all(&codex_token_dir).unwrap();

        let stale_tmp = claude_token_dir.join(".11111.json.interrupted.tmp");
        let stale_tmp_other_provider = codex_token_dir.join(".22222.json.inflight.tmp");
        let queue_json = claude_token_dir.join("33333.json");
        let out_of_scope_tmp = root.path().join(".44444.json.interrupted.tmp");
        std::fs::write(&stale_tmp, b"partial").unwrap();
        std::fs::write(&stale_tmp_other_provider, b"partial").unwrap();
        std::fs::write(&queue_json, b"[]").unwrap();
        std::fs::write(&out_of_scope_tmp, b"partial").unwrap();

        let audits = cleanup_stale_pending_queue_tmp_files_under_root(
            root.path(),
            SystemTime::now() + Duration::from_secs(120),
            Duration::from_secs(60),
        );

        assert_eq!(audits.len(), 2);
        assert!(
            audits.iter().any(|audit| {
                audit.channel_id == Some(11111) && audit.action == "removed_stale"
            }),
            "stale tmp files in token directories should be removed"
        );
        assert!(
            audits.iter().any(|audit| {
                audit.channel_id == Some(22222) && audit.action == "removed_stale"
            }),
            "old tmp files for every provider/token should be checked"
        );
        assert!(!stale_tmp.exists());
        assert!(!stale_tmp_other_provider.exists());
        assert!(queue_json.exists(), "real queue files must be preserved");
        assert!(
            out_of_scope_tmp.exists(),
            "root-level tmp files are not pending queue token snapshots"
        );
    }

    /// #2374 — the mailbox actor must own the reason-write so that the
    /// reason and the `cancelled` flip happen as one serialized
    /// transition per channel. Verifies: after a single
    /// `cancel_active_turn_with_reason` round trip, the returned token
    /// is cancelled AND carries the supplied label.
    #[tokio::test]
    async fn cancel_active_turn_with_reason_writes_label_and_flips_atomically() {
        let channel_id = ChannelId::new(2374001);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);

        let cancel_token = Arc::new(CancelToken::new());
        let started = handle
            .try_start_turn(cancel_token.clone(), UserId::new(1), MessageId::new(11))
            .await;
        assert!(started, "fresh channel must accept the new turn");

        let result = handle
            .cancel_active_turn_with_reason("voice_foreground_cancel_during_handoff".to_string())
            .await;

        let returned = result.token.expect("cancel returned the active token");
        assert!(
            returned.cancelled.load(Ordering::Relaxed),
            "actor must flip `cancelled` as part of the reason-owned transition"
        );
        assert_eq!(
            returned.cancel_source().as_deref(),
            Some("voice_foreground_cancel_during_handoff"),
            "actor must write the reason label inside the same actor step \
             (not from the caller task)"
        );
        assert!(
            !result.already_stopping,
            "first cancel must not report already_stopping"
        );
    }

    /// #2374 — two concurrent cancellers must not trample each other's
    /// reason. The first cancel wins both the flip and the label; a
    /// second cancel observing `already_stopping=true` must NOT
    /// overwrite the recorded reason. Without actor ownership of the
    /// reason write, the caller-side `set_cancel_source` from the second
    /// canceller could race with the first canceller's write between
    /// the "is it already cancelled?" read and the actual store.
    #[tokio::test]
    async fn concurrent_cancels_do_not_trample_each_others_reason() {
        let channel_id = ChannelId::new(2374002);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);

        let cancel_token = Arc::new(CancelToken::new());
        handle
            .try_start_turn(cancel_token.clone(), UserId::new(1), MessageId::new(22))
            .await;

        // Fire two concurrent cancel attempts with different reasons.
        // Whichever the actor dequeues first must win the attribution;
        // the loser must observe `already_stopping=true` AND find the
        // recorded reason unchanged.
        let handle_a = handle.clone();
        let handle_b = handle.clone();
        let task_a = tokio::spawn(async move {
            handle_a
                .cancel_active_turn_with_reason("voice_barge_in_live_cut".to_string())
                .await
        });
        let task_b = tokio::spawn(async move {
            handle_b
                .cancel_active_turn_with_reason("watchdog_timeout".to_string())
                .await
        });
        let res_a = task_a.await.expect("task a panicked");
        let res_b = task_b.await.expect("task b panicked");

        // Exactly one of the two cancellers must observe
        // `already_stopping=false` (the winner). The other must observe
        // `already_stopping=true` (the loser).
        let winner_count = [&res_a, &res_b]
            .iter()
            .filter(|r| !r.already_stopping)
            .count();
        assert_eq!(
            winner_count, 1,
            "exactly one canceller can win the actor's serialized flip"
        );

        // The winner's reason must be the one persisted. Since the
        // actor is the sole writer, the winner's label is whichever
        // task the actor dequeued first; the loser's later message
        // must NOT mutate the label.
        let winner_label = if !res_a.already_stopping {
            "voice_barge_in_live_cut"
        } else {
            "watchdog_timeout"
        };
        assert_eq!(
            cancel_token.cancel_source().as_deref(),
            Some(winner_label),
            "loser's reason must NOT overwrite the winner's (actor-owned write)"
        );
        assert!(
            cancel_token.cancelled.load(Ordering::Relaxed),
            "token must be cancelled after either cancel returns"
        );
    }

    /// #2374 — `cancel_active_turn_if_current_with_reason` keeps the
    /// stale-caller guard. A token that no longer matches the active
    /// turn must NOT flip `cancelled` on the live turn nor write a
    /// reason.
    #[tokio::test]
    async fn cancel_if_current_with_reason_rejects_stale_token() {
        let channel_id = ChannelId::new(2374003);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);

        let stale_token = Arc::new(CancelToken::new());
        let live_token = Arc::new(CancelToken::new());
        handle
            .try_start_turn(live_token.clone(), UserId::new(1), MessageId::new(33))
            .await;

        let result = handle
            .cancel_active_turn_if_current_with_reason(
                stale_token.clone(),
                "stale_caller_reason".to_string(),
            )
            .await;

        assert!(
            result.token.is_none(),
            "stale `if_current` caller must not match the live turn"
        );
        assert!(
            !live_token.cancelled.load(Ordering::Relaxed),
            "live turn must NOT be cancelled by a stale caller"
        );
        assert!(
            live_token.cancel_source().is_none(),
            "live turn must NOT carry the stale caller's reason"
        );
    }

    /// #2374 Codex round-1 fix (HIGH-1) —
    /// `cancel_active_turn_if_user_message_with_reason` MUST cancel
    /// only when the active turn's `user_message_id` matches.
    #[tokio::test]
    async fn cancel_if_user_message_matches_cancels_with_reason() {
        let channel_id = ChannelId::new(2374004);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);

        let live_token = Arc::new(CancelToken::new());
        let handoff_msg = MessageId::new(987_654);
        handle
            .try_start_turn(live_token.clone(), UserId::new(1), handoff_msg)
            .await;

        let result = handle
            .cancel_active_turn_if_user_message_with_reason(
                handoff_msg,
                "voice_foreground_cancel_during_handoff".to_string(),
            )
            .await;

        assert!(
            result.token.is_some(),
            "matching user_message_id must cancel the active turn"
        );
        assert!(live_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(
            live_token.cancel_source().as_deref(),
            Some("voice_foreground_cancel_during_handoff"),
        );
    }

    /// #2374 Codex round-1 fix (HIGH-1) — identity-guarded cancel MUST
    /// NOT touch the live turn when the active `user_message_id`
    /// belongs to a DIFFERENT message id than the caller's expected
    /// handoff id. This is the exact scenario the original PR missed:
    /// a tombstone retry arriving after the original handoff turn
    /// finalized and an unrelated turn started on the same target
    /// channel.
    #[tokio::test]
    async fn cancel_if_user_message_rejects_unrelated_active_turn() {
        let channel_id = ChannelId::new(2374005);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);

        // Active turn is an UNRELATED message (e.g. the original
        // handoff turn finalized and a new turn started).
        let live_token = Arc::new(CancelToken::new());
        let unrelated_msg = MessageId::new(111_111);
        let handoff_msg = MessageId::new(999_999);
        handle
            .try_start_turn(live_token.clone(), UserId::new(1), unrelated_msg)
            .await;

        let result = handle
            .cancel_active_turn_if_user_message_with_reason(
                handoff_msg,
                "voice_foreground_cancel_during_handoff".to_string(),
            )
            .await;

        assert!(
            result.token.is_none(),
            "identity-guarded cancel must NOT match an unrelated active turn"
        );
        assert!(
            !live_token.cancelled.load(Ordering::Relaxed),
            "unrelated active turn must NOT be cancelled by a tombstone retry"
        );
        assert!(
            live_token.cancel_source().is_none(),
            "unrelated active turn must NOT carry the handoff reason"
        );
    }

    /// #2374 Codex round-1 fix (HIGH-1) — identity-guarded cancel
    /// returns `None` when no active turn exists. This is the
    /// "handoff turn already finalized" case: the tombstone retry
    /// must observe no live token AND not affect any future turn.
    #[tokio::test]
    async fn cancel_if_user_message_returns_none_when_no_active_turn() {
        let channel_id = ChannelId::new(2374006);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);

        let result = handle
            .cancel_active_turn_if_user_message_with_reason(
                MessageId::new(42),
                "voice_foreground_cancel_during_handoff".to_string(),
            )
            .await;

        assert!(
            result.token.is_none(),
            "no-active-turn case must return None — no work to cancel"
        );
        assert!(
            !result.already_stopping,
            "no-active-turn case must not report already_stopping"
        );
    }
}

// #2728 — verify the refusal_reason field correctly tags each of the
// three false-return paths in `enqueue_intervention` / the handle layer.
// Without this signal callers could only infer the path from code
// archaeology (cf. the adk-cc 07:27 KST 2026-05-20 incident).
#[cfg(test)]
mod enqueue_refusal_reason_tests {
    use super::*;

    fn intervention(message_id: u64, text: &str, created_at: Instant) -> Intervention {
        Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            source_message_ids: vec![MessageId::new(message_id)],
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
    fn source_id_already_queued_is_tagged() {
        let now = Instant::now();
        let mut queue = vec![intervention(1, "hello", now)];
        let incoming = intervention(1, "hello again", now);
        let result = enqueue_intervention(&mut queue, incoming);
        assert!(!result.enqueued);
        assert_eq!(
            result.refusal_reason,
            Some(EnqueueRefusalReason::SourceIdAlreadyQueued),
        );
    }

    #[test]
    fn last_item_dedup_is_tagged() {
        let now = Instant::now();
        let mut queue = vec![intervention(1, "same text", now)];
        let incoming = intervention(2, "same text", now);
        let result = enqueue_intervention(&mut queue, incoming);
        assert!(!result.enqueued);
        assert_eq!(
            result.refusal_reason,
            Some(EnqueueRefusalReason::LastItemDedup),
        );
    }

    #[test]
    fn refusal_reason_absent_on_success() {
        let now = Instant::now();
        let mut queue: Vec<Intervention> = Vec::new();
        let incoming = intervention(1, "first", now);
        let result = enqueue_intervention(&mut queue, incoming);
        assert!(result.enqueued);
        assert_eq!(result.refusal_reason, None);
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";
    static TEST_ENV_LOCK: LazyLock<std::sync::Mutex<()>> =
        LazyLock::new(|| std::sync::Mutex::new(()));

    struct EnvGuard {
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set_root(root: &Path) -> Self {
            let previous = std::env::var(AGENTDESK_ROOT_DIR_ENV).ok();
            unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, root.to_str().unwrap()) };
            Self { previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.previous.as_ref() {
                unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, previous) };
            } else {
                unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
            }
        }
    }

    fn queue_file_path(
        root: &Path,
        provider: &ProviderKind,
        token_hash: &str,
        channel_id: ChannelId,
    ) -> PathBuf {
        root.join("runtime")
            .join("discord_pending_queue")
            .join(provider.as_str())
            .join(token_hash)
            .join(format!("{}.json", channel_id.get()))
    }

    fn read_saved_items(
        root: &Path,
        provider: &ProviderKind,
        token_hash: &str,
        channel_id: ChannelId,
    ) -> Vec<PendingQueueItem> {
        let path = queue_file_path(root, provider, token_hash, channel_id);
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn voice_announcement(
        transcript: &str,
        utterance_id: &str,
    ) -> crate::voice::prompt::VoiceTranscriptAnnouncement {
        crate::voice::prompt::VoiceTranscriptAnnouncement {
            transcript: transcript.to_string(),
            user_id: "42".to_string(),
            utterance_id: utterance_id.to_string(),
            language: "ko-KR".to_string(),
            verbose_progress: true,
            started_at: Some("2026-05-24T21:00:00+09:00".to_string()),
            completed_at: Some("2026-05-24T21:00:01+09:00".to_string()),
            samples_written: Some(48_000),
            control_channel_id: Some(300),
            stt_mode: Some("file".to_string()),
            stt_latency_ms: Some(120),
        }
    }

    fn make_intervention(
        message_id: u64,
        text: &str,
        voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
    ) -> Intervention {
        Intervention {
            author_id: UserId::new(100),
            author_is_bot: voice_announcement.is_some(),
            message_id: MessageId::new(message_id),
            source_message_ids: vec![MessageId::new(message_id)],
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement,
        }
    }

    #[tokio::test]
    async fn enqueue_rolls_back_when_pending_queue_persistence_fails() {
        let _lock = TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());
        std::fs::write(tmp.path().join("runtime"), "not-a-directory").unwrap();

        let provider = ProviderKind::Codex;
        let token_hash = "unwritable-pending-queue";
        let channel_id = ChannelId::new(2_867_001);
        let persistence = QueuePersistenceContext::new(&provider, token_hash, None);
        let direct_error = save_channel_queue(
            &provider,
            token_hash,
            channel_id,
            &[make_intervention(2_867_002, "must persist", None)],
            None,
        )
        .expect_err("direct pending queue write must surface persistence failure");
        assert!(
            direct_error.contains("create_dir_all") || direct_error.contains("Not a directory")
        );

        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let result = handle
            .enqueue(
                make_intervention(2_867_003, "must not be accepted without disk", None),
                persistence,
            )
            .await;

        assert!(!result.enqueued);
        assert_eq!(result.refusal_reason, None);
        assert!(result.persistence_error.is_some());
        let snapshot = handle.snapshot().await;
        assert!(
            snapshot.intervention_queue.is_empty(),
            "mailbox must roll back non-durable queued work"
        );
    }

    #[test]
    fn pending_queue_roundtrip_preserves_author_is_bot() {
        let _lock = TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        let provider = ProviderKind::Codex;
        let token_hash = "author_bot_roundtrip";
        let channel_id = ChannelId::new(4242);
        let message_id = MessageId::new(9001);
        let intervention = Intervention {
            author_id: UserId::new(100),
            author_is_bot: true,
            message_id,
            source_message_ids: vec![message_id],
            text: "DISPATCH: restore me".to_string(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        };

        save_channel_queue(&provider, token_hash, channel_id, &[intervention], None).unwrap();

        let path = tmp
            .path()
            .join("runtime")
            .join("discord_pending_queue")
            .join(provider.as_str())
            .join(token_hash)
            .join(format!("{}.json", channel_id.get()));
        let saved: Vec<PendingQueueItem> =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(saved[0].author_is_bot);

        let (loaded, _) = load_pending_queues(&provider, token_hash);
        assert!(loaded[&channel_id][0].author_is_bot);
    }

    #[test]
    fn pending_queue_roundtrip_preserves_voice_announcement_payload() {
        let _lock = TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        let provider = ProviderKind::Codex;
        let token_hash = "voice_announcement_roundtrip";
        let channel_id = ChannelId::new(2_777_001);
        let announcement =
            voice_announcement("큐에 들어간 음성 요청 처리해줘", "issue-2777-roundtrip");
        let intervention = make_intervention(
            2_777_002,
            "ADK_VOICE_TRANSCRIPT v1\n큐에 들어간 음성 요청 처리해줘",
            Some(announcement.clone()),
        );

        save_channel_queue(
            &provider,
            token_hash,
            channel_id,
            std::slice::from_ref(&intervention),
            None,
        )
        .unwrap();

        let saved = read_saved_items(tmp.path(), &provider, token_hash, channel_id);
        assert_eq!(saved[0].voice_announcement.as_ref(), Some(&announcement));

        let (loaded, _) = load_pending_queues(&provider, token_hash);
        assert_eq!(
            loaded[&channel_id][0].voice_announcement.as_ref(),
            Some(&announcement),
            "post-restart disk load must not depend on the in-memory announcement TTL"
        );
    }

    #[test]
    fn pending_queue_roundtrip_preserves_upload_context() {
        let _lock = TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        let provider = ProviderKind::Codex;
        let token_hash = "upload_context_roundtrip";
        let channel_id = ChannelId::new(2_840_001);
        let mut intervention = make_intervention(2_840_002, "", None);
        intervention.pending_uploads = vec![
            "[File uploaded] report.pdf → /runtime/discord_uploads/1/report.pdf (123 bytes)"
                .to_string(),
        ];

        save_channel_queue(
            &provider,
            token_hash,
            channel_id,
            std::slice::from_ref(&intervention),
            None,
        )
        .unwrap();

        let saved = read_saved_items(tmp.path(), &provider, token_hash, channel_id);
        assert_eq!(saved[0].pending_uploads, intervention.pending_uploads);

        let (loaded, _) = load_pending_queues(&provider, token_hash);
        assert_eq!(
            loaded[&channel_id][0].pending_uploads, intervention.pending_uploads,
            "queued attachment-only turns must carry their own upload context"
        );
    }

    #[tokio::test]
    async fn actor_hydrate_from_disk_preserves_voice_announcement_payload() {
        let _lock = TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        let provider = ProviderKind::Claude;
        let token_hash = "voice_announcement_actor_hydrate";
        let channel_id = ChannelId::new(2_777_011);
        let announcement = voice_announcement(
            "재시작 후 hydrate 된 음성 요청 처리해줘",
            "issue-2777-hydrate",
        );
        let intervention = make_intervention(
            2_777_012,
            "ADK_VOICE_TRANSCRIPT v1\n재시작 후 hydrate 된 음성 요청 처리해줘",
            Some(announcement.clone()),
        );
        save_channel_queue(
            &provider,
            token_hash,
            channel_id,
            std::slice::from_ref(&intervention),
            None,
        )
        .unwrap();

        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let result = handle
            .hydrate_pending_queue_from_disk(QueuePersistenceContext::new(
                &provider, token_hash, None,
            ))
            .await;

        assert_eq!(result.absorbed, 1);
        assert_eq!(result.queue_len_after, 1);
        let snapshot = handle.snapshot().await;
        assert_eq!(
            snapshot.intervention_queue[0].voice_announcement.as_ref(),
            Some(&announcement)
        );
    }

    #[tokio::test]
    async fn restart_drain_persists_voice_announcement_payload() {
        let _lock = TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        let provider = ProviderKind::Claude;
        let token_hash = "voice_announcement_restart_drain";
        let channel_id = ChannelId::new(2_777_021);
        let announcement = voice_announcement(
            "restart drain 중인 음성 요청 처리해줘",
            "issue-2777-restart-drain",
        );
        let intervention = make_intervention(
            2_777_022,
            "ADK_VOICE_TRANSCRIPT v1\nrestart drain 중인 음성 요청 처리해줘",
            Some(announcement.clone()),
        );
        let persistence = QueuePersistenceContext::new(&provider, token_hash, None);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        handle
            .replace_queue(vec![intervention], persistence.clone())
            .await;

        let path = queue_file_path(tmp.path(), &provider, token_hash, channel_id);
        std::fs::remove_file(&path).unwrap();
        let result = handle.restart_drain(persistence).await;

        assert_eq!(result.queued_count, 1);
        let saved = read_saved_items(tmp.path(), &provider, token_hash, channel_id);
        assert_eq!(saved[0].voice_announcement.as_ref(), Some(&announcement));
    }

    #[tokio::test]
    async fn restart_drain_all_reports_pending_queue_persistence_errors() {
        let _lock = TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        let provider = ProviderKind::Claude;
        let token_hash = "mailbox-restart-drain-failure";
        let channel_id = ChannelId::new(143);
        let registry = ChannelMailboxRegistry::default();

        registry
            .handle(channel_id)
            .replace_queue(
                vec![make_intervention(1, "queued item", None)],
                QueuePersistenceContext::new(&provider, token_hash, None),
            )
            .await;

        std::fs::remove_dir_all(tmp.path().join("runtime")).unwrap();
        std::fs::write(tmp.path().join("runtime"), "not-a-directory").unwrap();

        let drain = registry
            .restart_drain_all(&provider, token_hash, &dashmap::DashMap::new())
            .await;

        assert_eq!(drain.queued_count, 0);
        assert_eq!(drain.persistence_errors.len(), 1);
        assert_eq!(drain.persistence_errors[0].channel_id, channel_id);
        assert!(
            drain.persistence_errors[0].error.contains("create_dir_all")
                || drain.persistence_errors[0]
                    .error
                    .contains("Not a directory")
        );
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::services::discord::runtime_store::test_env_lock;

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

    fn queue_file_path(
        root: &Path,
        provider: &ProviderKind,
        token_hash: &str,
        channel_id: ChannelId,
    ) -> PathBuf {
        root.join("runtime")
            .join("discord_pending_queue")
            .join(provider.as_str())
            .join(token_hash)
            .join(format!("{}.json", channel_id.get()))
    }

    fn read_saved_items(
        root: &Path,
        provider: &ProviderKind,
        token_hash: &str,
        channel_id: ChannelId,
    ) -> Vec<PendingQueueItem> {
        let path = queue_file_path(root, provider, token_hash, channel_id);
        let json = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn make_intervention(message_id: u64, text: &str, created_at: Instant) -> Intervention {
        Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            source_message_ids: vec![MessageId::new(message_id)],
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

    fn make_mergeable_intervention(
        message_id: u64,
        text: &str,
        created_at: Instant,
    ) -> Intervention {
        let mut intervention = make_intervention(message_id, text, created_at);
        intervention.merge_consecutive = true;
        intervention
    }

    fn make_overflow_queue(now: Instant) -> Vec<Intervention> {
        std::iter::once(make_intervention(1, "trimmed", now))
            .chain(
                (2..=(MAX_INTERVENTIONS_PER_CHANNEL as u64 + 1)).map(|message_id| {
                    make_intervention(message_id, &format!("queued-{message_id}"), now)
                }),
            )
            .collect()
    }

    fn lock_test_env() -> std::sync::MutexGuard<'static, ()> {
        test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[tokio::test]
    async fn has_pending_soft_queue_persists_pruned_queue_state() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let token_hash = "mailbox-prune-pending";
        let channel_id = ChannelId::new(41);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, token_hash, None);
        let queue = make_overflow_queue(Instant::now());

        handle.replace_queue(queue, persistence.clone()).await;

        assert_eq!(
            read_saved_items(tmp.path(), &provider, token_hash, channel_id).len(),
            MAX_INTERVENTIONS_PER_CHANNEL + 1
        );

        let result = handle.has_pending_soft_queue(persistence).await;
        assert!(result.has_pending);
        assert_eq!(result.queue_exit_events.len(), 1);
        assert_eq!(result.queue_exit_events[0].kind, QueueExitKind::Superseded);
        assert_eq!(
            result.queue_exit_events[0].intervention.message_id,
            MessageId::new(1)
        );
        assert_eq!(
            handle.snapshot().await.intervention_queue.len(),
            MAX_INTERVENTIONS_PER_CHANNEL
        );

        let items = read_saved_items(tmp.path(), &provider, token_hash, channel_id);
        assert_eq!(items.len(), MAX_INTERVENTIONS_PER_CHANNEL);
        assert!(items.iter().all(|item| item.text != "trimmed"));
        assert!(items.iter().any(|item| item.text == "queued-2"));

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[tokio::test]
    async fn finish_turn_persists_pruned_queue_state() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let token_hash = "mailbox-prune-finish";
        let channel_id = ChannelId::new(42);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, token_hash, None);
        let queue = make_overflow_queue(Instant::now());

        handle.replace_queue(queue, persistence.clone()).await;
        let active_msg_id = MessageId::new(77);
        handle
            .restore_active_turn(Arc::new(CancelToken::new()), UserId::new(7), active_msg_id)
            .await;

        let result = handle.finish_turn(persistence).await;
        assert!(result.removed_token.is_some());
        assert!(result.has_pending);
        assert!(result.mailbox_online);
        assert_eq!(result.queue_exit_events.len(), 1);
        assert_eq!(result.queue_exit_events[0].kind, QueueExitKind::Superseded);
        assert_eq!(
            result.queue_exit_events[0].intervention.message_id,
            MessageId::new(1)
        );

        let snapshot = handle.snapshot().await;
        assert!(snapshot.cancel_token.is_none());
        assert_eq!(snapshot.active_user_message_id, None);
        assert_eq!(
            snapshot.intervention_queue.len(),
            MAX_INTERVENTIONS_PER_CHANNEL
        );

        let items = read_saved_items(tmp.path(), &provider, token_hash, channel_id);
        assert_eq!(items.len(), MAX_INTERVENTIONS_PER_CHANNEL);
        assert!(items.iter().all(|item| item.text != "trimmed"));
        assert!(items.iter().any(|item| item.text == "queued-2"));

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[tokio::test]
    async fn hard_stop_reuses_last_persistence_and_persists_pruned_queue_state() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let token_hash = "mailbox-prune-hard-stop";
        let channel_id = ChannelId::new(47);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, token_hash, None);
        let queue = make_overflow_queue(Instant::now());

        handle.replace_queue(queue, persistence).await;
        handle
            .restore_active_turn(
                Arc::new(CancelToken::new()),
                UserId::new(8),
                MessageId::new(88),
            )
            .await;

        let result = handle.hard_stop().await;
        assert!(result.removed_token.is_some());
        assert!(result.has_pending);
        assert!(result.mailbox_online);
        assert_eq!(result.queue_exit_events.len(), 1);
        assert_eq!(result.queue_exit_events[0].kind, QueueExitKind::Superseded);
        assert_eq!(
            result.queue_exit_events[0].intervention.message_id,
            MessageId::new(1)
        );

        let snapshot = handle.snapshot().await;
        assert!(snapshot.cancel_token.is_none());
        assert_eq!(snapshot.active_user_message_id, None);
        assert_eq!(
            snapshot.intervention_queue.len(),
            MAX_INTERVENTIONS_PER_CHANNEL
        );

        let items = read_saved_items(tmp.path(), &provider, token_hash, channel_id);
        assert_eq!(items.len(), MAX_INTERVENTIONS_PER_CHANNEL);
        assert!(items.iter().all(|item| item.text != "trimmed"));
        assert!(items.iter().any(|item| item.text == "queued-2"));

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[test]
    fn has_soft_intervention_at_prunes_expired_entries_without_boot_time_dependency() {
        let created_at = Instant::now();
        let mut queue = vec![
            make_intervention(1, "expired", created_at),
            make_intervention(2, "fresh", created_at + Duration::from_secs(1)),
        ];

        assert!(has_soft_intervention_at(
            &mut queue,
            created_at + INTERVENTION_TTL + Duration::from_secs(1)
        ));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].message_id, MessageId::new(2));
        assert_eq!(queue[0].text, "fresh");
    }

    #[tokio::test]
    async fn restart_drain_all_persists_every_mailbox_queue() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let token_hash = "mailbox-restart-drain";
        let channel_a = ChannelId::new(141);
        let channel_b = ChannelId::new(142);
        let registry = ChannelMailboxRegistry::default();
        let now = Instant::now();

        registry
            .handle(channel_a)
            .replace_queue(
                vec![make_intervention(1, "first queued item", now)],
                QueuePersistenceContext::new(&provider, token_hash, None),
            )
            .await;
        registry
            .handle(channel_b)
            .replace_queue(
                vec![
                    make_intervention(2, "second queued item", now),
                    make_intervention(3, "third queued item", now),
                ],
                QueuePersistenceContext::new(&provider, token_hash, Some(9_999)),
            )
            .await;

        let dispatch_role_overrides = dashmap::DashMap::new();
        dispatch_role_overrides.insert(channel_b, ChannelId::new(9_999));

        let drain = registry
            .restart_drain_all(&provider, token_hash, &dispatch_role_overrides)
            .await;

        assert_eq!(drain.queued_count, 3);
        assert!(drain.persistence_errors.is_empty());

        let items_a = read_saved_items(tmp.path(), &provider, token_hash, channel_a);
        assert_eq!(items_a.len(), 1);
        assert_eq!(items_a[0].text, "first queued item");
        assert_eq!(items_a[0].override_channel_id, None);

        let items_b = read_saved_items(tmp.path(), &provider, token_hash, channel_b);
        assert_eq!(items_b.len(), 2);
        assert_eq!(items_b[0].text, "second queued item");
        assert_eq!(items_b[1].text, "third queued item");
        assert_eq!(items_b[0].override_channel_id, Some(9_999));
        assert_eq!(items_b[1].override_channel_id, Some(9_999));

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[tokio::test]
    async fn restart_drain_all_reports_pending_queue_persistence_errors() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let token_hash = "mailbox-restart-drain-failure";
        let channel_id = ChannelId::new(143);
        let registry = ChannelMailboxRegistry::default();
        let now = Instant::now();

        registry
            .handle(channel_id)
            .replace_queue(
                vec![make_intervention(1, "queued item", now)],
                QueuePersistenceContext::new(&provider, token_hash, None),
            )
            .await;

        std::fs::remove_dir_all(tmp.path().join("runtime")).unwrap();
        std::fs::write(tmp.path().join("runtime"), "not-a-directory").unwrap();

        let drain = registry
            .restart_drain_all(&provider, token_hash, &dashmap::DashMap::new())
            .await;

        assert_eq!(drain.queued_count, 0);
        assert_eq!(drain.persistence_errors.len(), 1);
        assert_eq!(drain.persistence_errors[0].channel_id, channel_id);
        assert!(
            drain.persistence_errors[0].error.contains("create_dir_all")
                || drain.persistence_errors[0]
                    .error
                    .contains("Not a directory")
        );

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[tokio::test]
    async fn cancel_active_turn_marks_token_without_clearing_turn_state() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(44));
        let token = Arc::new(CancelToken::new());

        handle
            .try_start_turn(token.clone(), UserId::new(9), MessageId::new(91))
            .await;

        let first = handle.cancel_active_turn().await;
        assert!(first.token.is_some());
        assert!(!first.already_stopping);
        assert!(token.cancelled.load(std::sync::atomic::Ordering::Relaxed));
        assert!(handle.snapshot().await.cancel_token.is_some());

        let second = handle.cancel_active_turn().await;
        assert!(second.already_stopping);
        assert!(second.token.is_some());
    }

    #[tokio::test]
    async fn cancel_queued_message_removes_matching_entry_and_persists() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let token_hash = "mailbox-cancel-queued";
        let channel_id = ChannelId::new(43);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, token_hash, None);
        let now = Instant::now();

        handle
            .replace_queue(
                vec![
                    make_intervention(10, "first", now),
                    make_intervention(11, "second", now),
                ],
                persistence.clone(),
            )
            .await;

        let result = handle
            .cancel_queued_message(MessageId::new(10), persistence)
            .await;
        assert_eq!(
            result.removed.as_ref().map(|item| item.text.as_str()),
            Some("first")
        );
        assert_eq!(result.queue_exit_events.len(), 1);
        assert_eq!(result.queue_exit_events[0].kind, QueueExitKind::Cancelled);
        assert_eq!(
            result.queue_exit_events[0].intervention.message_id,
            MessageId::new(10)
        );

        let snapshot = handle.snapshot().await;
        assert_eq!(snapshot.intervention_queue.len(), 1);
        assert_eq!(snapshot.intervention_queue[0].message_id.get(), 11);

        let items = read_saved_items(tmp.path(), &provider, token_hash, channel_id);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].message_id, 11);
        assert_eq!(items[0].text, "second");

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[tokio::test]
    async fn enqueue_reports_superseded_overflow_entry() {
        let provider = ProviderKind::Claude;
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(48);
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, "mailbox-overflow", None);
        let now = Instant::now();

        handle
            .replace_queue(
                (0..MAX_INTERVENTIONS_PER_CHANNEL)
                    .map(|idx| make_intervention(idx as u64 + 1, "queued", now))
                    .collect(),
                persistence.clone(),
            )
            .await;

        let result = handle
            .enqueue(
                make_intervention(999, "latest", now + Duration::from_secs(1)),
                persistence,
            )
            .await;

        assert!(result.enqueued);
        assert_eq!(result.queue_exit_events.len(), 1);
        assert_eq!(result.queue_exit_events[0].kind, QueueExitKind::Superseded);
        assert_eq!(
            result.queue_exit_events[0].intervention.message_id,
            MessageId::new(1)
        );
        assert_eq!(
            handle.snapshot().await.intervention_queue.len(),
            MAX_INTERVENTIONS_PER_CHANNEL
        );
    }

    #[tokio::test]
    async fn requeue_front_reports_superseded_overflow_entry() {
        let provider = ProviderKind::Claude;
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(50);
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, "mailbox-requeue-overflow", None);
        let now = Instant::now();

        handle
            .replace_queue(
                (0..MAX_INTERVENTIONS_PER_CHANNEL)
                    .map(|idx| make_intervention(idx as u64 + 1, "queued", now))
                    .collect(),
                persistence.clone(),
            )
            .await;

        let result = handle
            .requeue_front(
                make_intervention(999, "retry", now + Duration::from_secs(1)),
                persistence,
            )
            .await;

        assert_eq!(result.queue_exit_events.len(), 1);
        assert_eq!(result.queue_exit_events[0].kind, QueueExitKind::Superseded);
        assert_eq!(
            result.queue_exit_events[0].intervention.message_id,
            MessageId::new(MAX_INTERVENTIONS_PER_CHANNEL as u64)
        );

        let snapshot = handle.snapshot().await;
        assert_eq!(
            snapshot.intervention_queue.len(),
            MAX_INTERVENTIONS_PER_CHANNEL
        );
        assert_eq!(
            snapshot.intervention_queue[0].message_id,
            MessageId::new(999)
        );
    }

    #[tokio::test]
    async fn clear_marks_remaining_queue_as_superseded() {
        let provider = ProviderKind::Claude;
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(49);
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, "mailbox-clear", None);
        let now = Instant::now();

        handle
            .replace_queue(
                vec![
                    make_intervention(10, "first", now),
                    make_intervention(11, "second", now),
                ],
                persistence.clone(),
            )
            .await;
        handle
            .restore_active_turn(
                Arc::new(CancelToken::new()),
                UserId::new(9),
                MessageId::new(91),
            )
            .await;

        let result = handle.clear(persistence).await;

        assert!(result.removed_token.is_some());
        assert_eq!(result.queue_exit_events.len(), 2);
        assert!(
            result
                .queue_exit_events
                .iter()
                .all(|event| event.kind == QueueExitKind::Superseded)
        );
        assert!(handle.snapshot().await.intervention_queue.is_empty());
    }

    #[tokio::test]
    async fn enqueue_merges_consecutive_non_reply_messages_and_persists_source_ids() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let token_hash = "mailbox-merge-consecutive";
        let channel_id = ChannelId::new(143);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, token_hash, None);
        let now = Instant::now();

        let first = handle
            .enqueue(
                make_mergeable_intervention(20, "first", now),
                persistence.clone(),
            )
            .await;
        assert!(first.enqueued);
        // First message into an empty queue must not be classified as merged
        // — there is nothing to merge with. The reaction emoji selector relies
        // on this distinction (📬 for standalone vs ➕ for merged).
        assert!(!first.merged);

        let second = handle
            .enqueue(
                make_mergeable_intervention(21, "second", now + Duration::from_secs(1)),
                persistence.clone(),
            )
            .await;
        assert!(second.enqueued);
        // Second mergeable message folds into the first → merged=true so the
        // caller can pick the ➕ reaction emoji.
        assert!(second.merged);

        let snapshot = handle.snapshot().await;
        assert_eq!(snapshot.intervention_queue.len(), 1);
        assert_eq!(snapshot.intervention_queue[0].message_id.get(), 21);
        assert_eq!(
            snapshot.intervention_queue[0]
                .source_message_ids
                .iter()
                .map(|id| id.get())
                .collect::<Vec<_>>(),
            vec![20, 21]
        );
        assert_eq!(snapshot.intervention_queue[0].text, "first\nsecond");

        let items = read_saved_items(tmp.path(), &provider, token_hash, channel_id);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].message_id, 21);
        assert_eq!(items[0].source_message_ids, vec![20, 21]);
        assert_eq!(items[0].text, "first\nsecond");

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[tokio::test]
    async fn enqueue_reply_boundary_breaks_merge_chain() {
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(144);
        let handle = registry.handle(channel_id);
        let persistence =
            QueuePersistenceContext::new(&ProviderKind::Claude, "reply-boundary", None);
        let now = Instant::now();

        let mut reply = make_mergeable_intervention(31, "reply", now + Duration::from_secs(1));
        reply.has_reply_boundary = true;
        reply.reply_context = Some("[Reply context]".to_string());

        assert!(
            handle
                .enqueue(
                    make_mergeable_intervention(30, "first", now),
                    persistence.clone(),
                )
                .await
                .enqueued
        );
        assert!(handle.enqueue(reply, persistence.clone()).await.enqueued);
        assert!(
            handle
                .enqueue(
                    make_mergeable_intervention(32, "after", now + Duration::from_secs(2)),
                    persistence.clone(),
                )
                .await
                .enqueued
        );
        assert!(
            handle
                .enqueue(
                    make_mergeable_intervention(33, "tail", now + Duration::from_secs(3)),
                    persistence,
                )
                .await
                .enqueued
        );

        let snapshot = handle.snapshot().await;
        assert_eq!(snapshot.intervention_queue.len(), 3);
        assert_eq!(snapshot.intervention_queue[0].text, "first");
        assert_eq!(snapshot.intervention_queue[1].text, "reply");
        assert_eq!(snapshot.intervention_queue[2].text, "after\ntail");
        assert_eq!(
            snapshot.intervention_queue[2]
                .source_message_ids
                .iter()
                .map(|id| id.get())
                .collect::<Vec<_>>(),
            vec![32, 33]
        );
    }

    #[tokio::test]
    async fn cancel_queued_message_matches_any_merged_source_message_id() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let token_hash = "mailbox-cancel-merged";
        let channel_id = ChannelId::new(145);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, token_hash, None);
        let now = Instant::now();

        assert!(
            handle
                .enqueue(
                    make_mergeable_intervention(40, "first", now),
                    persistence.clone(),
                )
                .await
                .enqueued
        );
        assert!(
            handle
                .enqueue(
                    make_mergeable_intervention(41, "second", now + Duration::from_secs(1)),
                    persistence.clone(),
                )
                .await
                .enqueued
        );

        let removed = handle
            .cancel_queued_message(MessageId::new(40), persistence.clone())
            .await;
        assert_eq!(removed.queue_exit_events.len(), 1);
        assert_eq!(removed.queue_exit_events[0].kind, QueueExitKind::Cancelled);
        let removed = removed
            .removed
            .expect("merged item should be removable by original source id");
        assert_eq!(removed.message_id.get(), 41);
        assert_eq!(
            removed
                .source_message_ids
                .iter()
                .map(|id| id.get())
                .collect::<Vec<_>>(),
            vec![40, 41]
        );

        let snapshot = handle.snapshot().await;
        assert!(snapshot.intervention_queue.is_empty());
        let path = queue_file_path(tmp.path(), &provider, token_hash, channel_id);
        assert!(!path.exists());

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[tokio::test]
    async fn recovery_kickoff_marks_recovery_until_finish_turn() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(45);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, "mailbox-recovery", None);

        let kickoff = handle
            .recovery_kickoff(
                Arc::new(CancelToken::new()),
                UserId::new(5),
                MessageId::new(55),
            )
            .await;
        assert!(kickoff.activated_turn);
        assert!(handle.snapshot().await.recovery_started_at.is_some());

        let finished = handle.finish_turn(persistence).await;
        assert!(finished.removed_token.is_some());
        assert!(finished.mailbox_online);
        assert!(handle.snapshot().await.recovery_started_at.is_none());

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[tokio::test]
    async fn timeout_override_round_trip_stays_in_mailbox() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(46));

        assert_eq!(
            handle.extend_timeout(30).await,
            Err(WatchdogDeadlineExtensionError::NoActiveTurn)
        );

        let token = Arc::new(CancelToken::new());
        let now_ms = Utc::now().timestamp_millis();
        token
            .watchdog_deadline_ms
            .store(now_ms + 60_000, Ordering::Relaxed);
        token
            .watchdog_max_deadline_ms
            .store(now_ms + 120_000, Ordering::Relaxed);
        assert!(
            handle
                .try_start_turn(token.clone(), UserId::new(7), MessageId::new(11))
                .await
        );

        let extended = handle.extend_timeout(30).await.unwrap();
        assert_eq!(extended.applied_extend_secs, 30);
        assert_eq!(extended.extension_count_limit, u32::MAX);
        assert_eq!(extended.extension_total_secs_limit, u64::MAX);
        assert!(!extended.clamped);
        assert!(extended.new_deadline_ms >= now_ms + 90_000);
        assert_eq!(
            token.watchdog_deadline_ms.load(Ordering::Relaxed),
            extended.new_deadline_ms
        );
        assert_eq!(
            token.watchdog_max_deadline_ms.load(Ordering::Relaxed),
            extended.max_deadline_ms
        );
        assert!(extended.max_deadline_ms >= extended.new_deadline_ms);
        assert_eq!(handle.take_timeout_override().await, Some(extended));
        assert_eq!(handle.take_timeout_override().await, None);

        assert!(handle.extend_timeout(15).await.is_ok());
        handle.clear_timeout_override().await;
        assert_eq!(handle.take_timeout_override().await, None);
    }
}

// #2706: PurgeQueue regression guards. Kept in a plain `#[cfg(test)]` module so
// they run under the default `cargo test` invocation — the legacy-sqlite-tests
// feature is *not* enabled in CI by default, so the regression coverage for
// queue-only purge cannot live next to the rest of the mailbox tests.
#[cfg(test)]
mod purge_queue_tests {
    use std::sync::Arc;
    use std::time::Instant;

    use poise::serenity_prelude::{ChannelId, MessageId, UserId};

    use crate::services::provider::ProviderKind;
    use crate::services::turn_orchestrator::{
        CancelToken, ChannelMailboxRegistry, Intervention, InterventionMode,
        QueuePersistenceContext,
    };

    fn make_intervention(message_id: u64, text: &str, created_at: Instant) -> Intervention {
        Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            source_message_ids: vec![MessageId::new(message_id)],
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

    // PurgeQueue empties the intervention queue without touching the
    // active cancel_token, so a turn that entered the mailbox between
    // force-kill and the purge survives.
    #[tokio::test]
    async fn purge_queue_drains_queue_without_disturbing_active_turn() {
        let provider = ProviderKind::Claude;
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(2706);
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, "mailbox-purge-2706", None);
        let now = Instant::now();

        handle
            .replace_queue(
                vec![
                    make_intervention(20, "first", now),
                    make_intervention(21, "second", now),
                    make_intervention(22, "third", now),
                ],
                persistence.clone(),
            )
            .await;

        let active_token = Arc::new(CancelToken::new());
        handle
            .restore_active_turn(active_token.clone(), UserId::new(7), MessageId::new(70))
            .await;

        let drained = handle.purge_queue(persistence).await;
        assert_eq!(drained, 3);

        let snapshot = handle.snapshot().await;
        assert!(snapshot.intervention_queue.is_empty());

        // Active turn (its token and ownership) must survive the queue purge.
        let surviving = handle.cancel_token().await;
        assert!(surviving.is_some());
        assert!(Arc::ptr_eq(&surviving.unwrap(), &active_token));
    }

    // purge_queue is a no-op on an empty mailbox.
    #[tokio::test]
    async fn purge_queue_is_idempotent_on_empty_mailbox() {
        let provider = ProviderKind::Claude;
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(2707);
        let handle = registry.handle(channel_id);
        let persistence =
            QueuePersistenceContext::new(&provider, "mailbox-purge-idempotent-2706", None);

        let drained_first = handle.purge_queue(persistence.clone()).await;
        let drained_second = handle.purge_queue(persistence).await;
        assert_eq!(drained_first, 0);
        assert_eq!(drained_second, 0);
        assert!(handle.snapshot().await.intervention_queue.is_empty());
    }
}

#[cfg(test)]
mod recovery_done_signal_tests {
    use super::*;

    /// #2443 — verify the latch-then-wait race-free contract.
    #[tokio::test]
    async fn recovery_done_latch_short_circuits_late_subscribers() {
        let signal = RecoveryDoneSignal::new();
        signal.mark_done();
        // Subscriber registers AFTER mark_done — must still complete.
        tokio::time::timeout(std::time::Duration::from_millis(100), signal.wait())
            .await
            .expect("late subscriber should observe latched done state");
    }

    /// #2443 — verify the reset clears the latch so the next recovery
    /// cycle's watcher does not see a stale signal.
    #[tokio::test]
    async fn recovery_done_reset_unlatches_for_next_cycle() {
        let signal = std::sync::Arc::new(RecoveryDoneSignal::new());
        signal.mark_done();
        signal.reset();
        // After reset, wait should NOT short-circuit — must time out.
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(50), signal.wait()).await;
        assert!(
            result.is_err(),
            "reset() should clear the latch so subsequent waits block until next mark_done"
        );
        // Now fire mark_done in a background task and confirm a fresh
        // waiter wakes up.
        let signal_for_task = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            signal_for_task.mark_done();
        });
        tokio::time::timeout(std::time::Duration::from_millis(500), signal.wait())
            .await
            .expect("wait after reset should resolve when mark_done fires again");
    }

    /// #2443 — global resolution path used by watchers/lifecycle.rs.
    #[tokio::test]
    async fn registry_recovery_done_is_globally_resolvable() {
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(99_443);
        let signal = registry.recovery_done(channel_id);
        let resolved =
            ChannelMailboxRegistry::global_recovery_done(channel_id).expect("global signal");
        // Identity check via mark_done propagation: marking one wakes
        // the other if they point to the same underlying Arc.
        signal.mark_done();
        tokio::time::timeout(std::time::Duration::from_millis(50), resolved.wait())
            .await
            .expect("global_recovery_done should resolve to the same Arc");
    }
}
