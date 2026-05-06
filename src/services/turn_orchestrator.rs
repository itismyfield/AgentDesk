use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};
use tokio::sync::{mpsc, oneshot};

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
    pub(crate) message_id: MessageId,
    pub(crate) source_message_ids: Vec<MessageId>,
    pub(crate) text: String,
    pub(crate) mode: InterventionMode,
    pub(crate) created_at: Instant,
    pub(crate) reply_context: Option<String>,
    pub(crate) has_reply_boundary: bool,
    pub(crate) merge_consecutive: bool,
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
            queue_exit_events,
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
                queue_exit_events,
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
            return EnqueueInterventionResult {
                enqueued: true,
                merged: true,
                queue_exit_events,
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
        queue_exit_events,
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
    /// Channel this item belongs to (routing snapshot — used by the kickoff guard).
    #[serde(default)]
    pub(crate) channel_id: Option<u64>,
    /// Human-readable channel name at save time (best-effort, may be None).
    #[serde(default)]
    pub(crate) channel_name: Option<String>,
    /// Active dispatch role override at save time (lost on restart; stored for diagnostics).
    #[serde(default)]
    pub(crate) override_channel_id: Option<u64>,
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

/// Write-through: save a single channel's queue to disk.
/// If the queue is empty the file is removed.
pub(crate) fn save_channel_queue(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
    queue: &[Intervention],
    dispatch_role_override: Option<u64>,
) {
    let Some(path) = pending_queue_file_path(provider, token_hash, channel_id) else {
        return;
    };
    if queue.is_empty() {
        let _ = fs::remove_file(&path);
        return;
    }
    let Some(dir) = path.parent() else {
        return;
    };
    let _ = fs::create_dir_all(dir);
    let items: Vec<PendingQueueItem> = queue
        .iter()
        .map(|i| PendingQueueItem {
            author_id: i.author_id.get(),
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
            channel_id: Some(channel_id.get()),
            channel_name: None,
            override_channel_id: dispatch_role_override,
        })
        .collect();
    if let Ok(json) = serde_json::to_string_pretty(&items) {
        let _ = crate::services::discord::runtime_store::atomic_write(&path, &json);
    }
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
        message_id: MessageId::new(item.message_id),
        source_message_ids,
        text: item.text,
        mode: InterventionMode::Soft,
        created_at: now,
        reply_context: item.reply_context,
        has_reply_boundary: item.has_reply_boundary,
        merge_consecutive: item.merge_consecutive,
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
) {
    let Some(root) = pending_queue_root() else {
        return;
    };
    let dir = root.join(provider.as_str()).join(token_hash);
    let _ = fs::create_dir_all(&dir);
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let _ = fs::remove_file(entry.path());
        }
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
                channel_id: Some(channel_id.get()),
                channel_name: None,
                override_channel_id: override_id,
            })
            .collect();
        if let Ok(json) = serde_json::to_string_pretty(&items) {
            let path = dir.join(format!("{}.json", channel_id.get()));
            let _ = crate::services::discord::runtime_store::atomic_write(&path, &json);
        }
    }
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
            save_channel_queue(provider, token_hash, channel_id, &[], None);
        }
        return None;
    }

    let intervention = next.unwrap();

    if remove_queue {
        intervention_queue.remove(&channel_id);
        dispatch_role_overrides.remove(&channel_id);
        save_channel_queue(provider, token_hash, channel_id, &[], None);
    } else if let Some(queue) = intervention_queue.get(&channel_id) {
        save_channel_queue(
            provider,
            token_hash,
            channel_id,
            queue,
            dispatch_role_overrides
                .get(&channel_id)
                .map(|override_id| override_id.value().get()),
        );
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
    save_channel_queue(
        provider,
        token_hash,
        channel_id,
        queue,
        dispatch_role_overrides
            .get(&channel_id)
            .map(|override_id| override_id.value().get()),
    );
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
}

pub(crate) struct ClearChannelResult {
    pub(crate) removed_token: Option<Arc<CancelToken>>,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
}

pub(crate) struct CancelActiveTurnResult {
    pub(crate) token: Option<Arc<CancelToken>>,
    pub(crate) already_stopping: bool,
}

pub(crate) struct HasPendingSoftQueueResult {
    pub(crate) has_pending: bool,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
}

pub(crate) struct RecoveryKickoffResult {
    pub(crate) activated_turn: bool,
}

pub(crate) struct RestartDrainResult {
    pub(crate) queued_count: usize,
}

pub(crate) struct EnqueueInterventionResult {
    pub(crate) enqueued: bool,
    /// True when the incoming intervention was folded into the previous queue
    /// entry via `should_merge_intervention` (text concatenated, source IDs
    /// accumulated). Callers use this to surface a different reaction emoji
    /// for merged messages so users can tell merged from standalone entries.
    pub(crate) merged: bool,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
}

pub(crate) struct CancelQueuedMessageResult {
    pub(crate) removed: Option<Intervention>,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
}

pub(crate) struct TakeNextSoftResult {
    pub(crate) intervention: Option<Intervention>,
    pub(crate) has_more: bool,
    pub(crate) queue_len_after: usize,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
}

pub(crate) struct RequeueInterventionResult {
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
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
                queue_exit_events: Vec::new(),
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
            },
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
            RestartDrainResult { queued_count: 0 },
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

#[derive(Clone, Default)]
pub(crate) struct ChannelMailboxRegistry {
    handles: Arc<dashmap::DashMap<ChannelId, ChannelMailboxHandle>>,
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
    ) -> usize {
        let handles: Vec<_> = self
            .handles
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect();
        let mut queued_total = 0usize;
        for (channel_id, handle) in handles {
            let persistence = QueuePersistenceContext::new(
                provider,
                token_hash,
                dispatch_role_overrides
                    .get(&channel_id)
                    .map(|override_id| override_id.value().get()),
            );
            queued_total += handle.restart_drain(persistence).await.queued_count;
        }
        queued_total
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
    CancelActiveTurnIfCurrent {
        expected_token: Arc<CancelToken>,
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
    Clear {
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<ClearChannelResult>,
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
) {
    save_channel_queue(
        &persistence.provider,
        &persistence.token_hash,
        channel_id,
        queue,
        persistence.dispatch_role_override,
    );
}

fn hydrate_pending_queue_into_state(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    disk_items: Vec<Intervention>,
    persistence: QueuePersistenceContext,
    restored_override: Option<ChannelId>,
) -> HydratePendingQueueResult {
    state.last_persistence = Some(persistence.clone());
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
        persist_queue(channel_id, &state.intervention_queue, &persistence);
    }
    HydratePendingQueueResult {
        absorbed,
        queue_len_after: state.intervention_queue.len(),
        restored_override,
    }
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
    let pending_result = has_soft_intervention(&mut state.intervention_queue);
    if let Some(persistence) = persistence {
        if state.intervention_queue.len() != previous_len || !state.intervention_queue.is_empty() {
            persist_queue(channel_id, &state.intervention_queue, persistence);
        }
    }
    FinishTurnResult {
        removed_token,
        has_pending: pending_result.has_pending,
        mailbox_online: true,
        queue_exit_events: pending_result.queue_exit_events,
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
                ChannelMailboxMsg::TryStartTurn {
                    cancel_token,
                    request_owner,
                    user_message_id,
                    reply,
                } => {
                    let started = if state.cancel_token.is_some() {
                        false
                    } else {
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
                    let enqueue_result =
                        enqueue_intervention(&mut state.intervention_queue, intervention);
                    persist_queue(channel_id, &state.intervention_queue, &persistence);
                    let _ = reply.send(enqueue_result);
                }
                ChannelMailboxMsg::HasPendingSoftQueue { persistence, reply } => {
                    state.last_persistence = Some(persistence.clone());
                    let previous_len = state.intervention_queue.len();
                    let pending_result = has_soft_intervention(&mut state.intervention_queue);
                    if state.intervention_queue.len() != previous_len {
                        persist_queue(channel_id, &state.intervention_queue, &persistence);
                    }
                    let _ = reply.send(pending_result);
                }
                ChannelMailboxMsg::TakeNextSoft { persistence, reply } => {
                    state.last_persistence = Some(persistence.clone());
                    let next_result = dequeue_next_soft_intervention(&mut state.intervention_queue);
                    let queue_len_after = state.intervention_queue.len();
                    persist_queue(channel_id, &state.intervention_queue, &persistence);
                    let _ = reply.send(TakeNextSoftResult {
                        intervention: next_result.intervention,
                        has_more: next_result.has_more,
                        queue_len_after,
                        queue_exit_events: next_result.queue_exit_events,
                    });
                }
                ChannelMailboxMsg::RequeueFront {
                    intervention,
                    persistence,
                    reply,
                } => {
                    state.last_persistence = Some(persistence.clone());
                    let requeue_result =
                        requeue_intervention_front(&mut state.intervention_queue, intervention);
                    persist_queue(channel_id, &state.intervention_queue, &persistence);
                    let _ = reply.send(RequeueInterventionResult {
                        queue_exit_events: requeue_result,
                    });
                }
                ChannelMailboxMsg::CancelQueuedMessage {
                    message_id,
                    persistence,
                    reply,
                } => {
                    state.last_persistence = Some(persistence.clone());
                    let cancel_result = cancel_soft_intervention_by_message_id(
                        &mut state.intervention_queue,
                        message_id,
                    );
                    if cancel_result.removed.is_some()
                        || !cancel_result.queue_exit_events.is_empty()
                    {
                        persist_queue(channel_id, &state.intervention_queue, &persistence);
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
                }
                ChannelMailboxMsg::HardStop { reply } => {
                    let persistence = state.last_persistence.clone();
                    let _ = reply.send(finalize_turn_state(
                        &mut state,
                        channel_id,
                        persistence.as_ref(),
                    ));
                }
                ChannelMailboxMsg::Clear { persistence, reply } => {
                    state.last_persistence = Some(persistence.clone());
                    let removed_token = state.cancel_token.take();
                    state.active_request_owner = None;
                    state.active_user_message_id = None;
                    state.recovery_started_at = None;
                    state.turn_started_at = None;
                    reset_watchdog_extension_state(&mut state);
                    let queue_exit_events = state
                        .intervention_queue
                        .drain(..)
                        .map(|intervention| {
                            QueueExitEvent::new(intervention, QueueExitKind::Superseded)
                        })
                        .collect();
                    persist_queue(channel_id, &state.intervention_queue, &persistence);
                    let _ = reply.send(ClearChannelResult {
                        removed_token,
                        queue_exit_events,
                    });
                }
                ChannelMailboxMsg::ReplaceQueue {
                    queue,
                    persistence,
                    reply,
                } => {
                    state.last_persistence = Some(persistence.clone());
                    state.intervention_queue = queue;
                    persist_queue(channel_id, &state.intervention_queue, &persistence);
                    let _ = reply.send(());
                }
                ChannelMailboxMsg::HydratePendingQueueFromDisk { persistence, reply } => {
                    // #1683: read the disk queue inside the mailbox actor so
                    // a dequeue that removes the file cannot race with a stale
                    // out-of-actor disk snapshot and reinsert an already
                    // processed item.
                    let (disk_items, restored_override) = load_channel_pending_queue(
                        &persistence.provider,
                        &persistence.token_hash,
                        channel_id,
                    );
                    let mut effective_persistence = persistence;
                    if effective_persistence.dispatch_role_override.is_none() {
                        effective_persistence.dispatch_role_override =
                            restored_override.map(|channel| channel.get());
                    }
                    let result = hydrate_pending_queue_into_state(
                        &mut state,
                        channel_id,
                        disk_items,
                        effective_persistence,
                        restored_override,
                    );
                    let _ = reply.send(result);
                }
                ChannelMailboxMsg::RestartDrain { persistence, reply } => {
                    state.last_persistence = Some(persistence.clone());
                    persist_queue(channel_id, &state.intervention_queue, &persistence);
                    let _ = reply.send(RestartDrainResult {
                        queued_count: state.intervention_queue.len(),
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
            message_id: MessageId::new(message_id),
            source_message_ids: vec![MessageId::new(message_id)],
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at,
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
        }
    }

    fn lock_test_env() -> MutexGuard<'static, ()> {
        TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
            message_id: MessageId::new(message_id),
            source_message_ids: vec![MessageId::new(message_id)],
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at,
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
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

        let queued_total = registry
            .restart_drain_all(&provider, token_hash, &dispatch_role_overrides)
            .await;

        assert_eq!(queued_total, 3);

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
