use std::collections::HashMap;
use std::fs;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};
use tokio::sync::{mpsc, oneshot};

use crate::services::provider::{CancelToken, ProviderKind};

pub(crate) const MAX_INTERVENTIONS_PER_CHANNEL: usize = 30;
pub(crate) const INTERVENTION_TTL: Duration = Duration::from_secs(10 * 60);
pub(crate) const INTERVENTION_DEDUP_WINDOW: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InterventionMode {
    Soft,
}

#[derive(Clone, Debug)]
pub(crate) struct Intervention {
    pub(crate) author_id: UserId,
    pub(crate) message_id: MessageId,
    pub(crate) text: String,
    pub(crate) mode: InterventionMode,
    pub(crate) created_at: Instant,
}

fn prune_interventions(queue: &mut Vec<Intervention>) {
    let now = Instant::now();
    queue.retain(|i| now.duration_since(i.created_at) <= INTERVENTION_TTL);
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        let overflow = queue.len() - MAX_INTERVENTIONS_PER_CHANNEL;
        queue.drain(0..overflow);
    }
}

pub(crate) fn enqueue_intervention(
    queue: &mut Vec<Intervention>,
    intervention: Intervention,
) -> bool {
    prune_interventions(queue);

    if let Some(last) = queue.last() {
        if last.author_id == intervention.author_id
            && last.text == intervention.text
            && intervention.created_at.duration_since(last.created_at) <= INTERVENTION_DEDUP_WINDOW
        {
            return false;
        }
    }

    queue.push(intervention);
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        let overflow = queue.len() - MAX_INTERVENTIONS_PER_CHANNEL;
        queue.drain(0..overflow);
    }
    true
}

pub(crate) fn has_soft_intervention(queue: &mut Vec<Intervention>) -> bool {
    prune_interventions(queue);
    queue.iter().any(|item| item.mode == InterventionMode::Soft)
}

pub(crate) fn dequeue_next_soft_intervention(
    queue: &mut Vec<Intervention>,
) -> Option<Intervention> {
    prune_interventions(queue);
    let index = queue
        .iter()
        .position(|item| item.mode == InterventionMode::Soft)?;
    Some(queue.remove(index))
}

pub(crate) fn cancel_soft_intervention_by_message_id(
    queue: &mut Vec<Intervention>,
    message_id: MessageId,
) -> Option<Intervention> {
    prune_interventions(queue);
    let index = queue
        .iter()
        .position(|item| item.mode == InterventionMode::Soft && item.message_id == message_id)?;
    Some(queue.remove(index))
}

pub(crate) fn requeue_intervention_front(
    queue: &mut Vec<Intervention>,
    intervention: Intervention,
) {
    prune_interventions(queue);
    queue.insert(0, intervention);
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        queue.truncate(MAX_INTERVENTIONS_PER_CHANNEL);
    }
}

/// Serializable form of a queued intervention for disk persistence.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct PendingQueueItem {
    pub(crate) author_id: u64,
    pub(crate) message_id: u64,
    pub(crate) text: String,
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

/// Write-through: save a single channel's queue to disk.
/// If the queue is empty the file is removed.
pub(crate) fn save_channel_queue(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
    queue: &[Intervention],
    dispatch_role_override: Option<u64>,
) {
    let Some(root) = pending_queue_root() else {
        return;
    };
    let dir = root.join(provider.as_str()).join(token_hash);
    let path = dir.join(format!("{}.json", channel_id.get()));
    if queue.is_empty() {
        let _ = fs::remove_file(&path);
        return;
    }
    let _ = fs::create_dir_all(&dir);
    let items: Vec<PendingQueueItem> = queue
        .iter()
        .map(|i| PendingQueueItem {
            author_id: i.author_id.get(),
            message_id: i.message_id.get(),
            text: i.text.clone(),
            channel_id: Some(channel_id.get()),
            channel_name: None,
            override_channel_id: dispatch_role_override,
        })
        .collect();
    if let Ok(json) = serde_json::to_string_pretty(&items) {
        let _ = crate::services::discord::runtime_store::atomic_write(&path, &json);
    }
}

/// Save all non-empty intervention queues to `{provider}/{token_hash}/`.
#[cfg(test)]
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
                text: i.text.clone(),
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
        let interventions: Vec<Intervention> = items
            .into_iter()
            .map(|item| Intervention {
                author_id: UserId::new(item.author_id),
                message_id: MessageId::new(item.message_id),
                text: item.text,
                mode: InterventionMode::Soft,
                created_at: now,
            })
            .collect();
        if !interventions.is_empty() {
            result.insert(ChannelId::new(channel_id), interventions);
        }
        let _ = fs::remove_file(&path);
    }
    (result, restored_overrides)
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
            eprintln!(
                "  [{ts}] ⚠ LEGACY-QUEUE: found legacy pending queue file '{}' — \
                predates bot-identity namespacing and will NOT be restored. \
                Remove manually if no longer needed.",
                path.display()
            );
        }
    }
}

#[cfg(test)]
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
        (next, has_more)
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

#[cfg(test)]
pub(crate) fn requeue_intervention_front_persisted(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
    intervention_queue: &mut HashMap<ChannelId, Vec<Intervention>>,
    dispatch_role_overrides: &dashmap::DashMap<ChannelId, ChannelId>,
    intervention: Intervention,
) {
    let queue = intervention_queue.entry(channel_id).or_default();
    requeue_intervention_front(queue, intervention);
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

#[derive(Clone, Default)]
pub(crate) struct ChannelMailboxSnapshot {
    pub(crate) cancel_token: Option<Arc<CancelToken>>,
    pub(crate) active_request_owner: Option<UserId>,
    pub(crate) active_user_message_id: Option<MessageId>,
    pub(crate) intervention_queue: Vec<Intervention>,
    pub(crate) recovery_started_at: Option<Instant>,
}

pub(crate) struct FinishTurnResult {
    pub(crate) removed_token: Option<Arc<CancelToken>>,
    pub(crate) has_pending: bool,
    pub(crate) mailbox_online: bool,
}

pub(crate) struct ClearChannelResult {
    pub(crate) removed_token: Option<Arc<CancelToken>>,
}

pub(crate) struct CancelActiveTurnResult {
    pub(crate) token: Option<Arc<CancelToken>>,
    pub(crate) already_stopping: bool,
}

pub(crate) struct HasPendingSoftQueueResult {
    pub(crate) has_pending: bool,
}

pub(crate) struct RecoveryKickoffResult {
    pub(crate) activated_turn: bool,
}

pub(crate) struct RestartDrainResult {
    pub(crate) queued_count: usize,
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
    ) -> bool {
        self.request(
            |reply| ChannelMailboxMsg::Enqueue {
                intervention,
                persistence,
                reply,
            },
            false,
        )
        .await
    }

    pub(crate) async fn has_pending_soft_queue(
        &self,
        persistence: QueuePersistenceContext,
    ) -> HasPendingSoftQueueResult {
        self.request(
            |reply| ChannelMailboxMsg::HasPendingSoftQueue { persistence, reply },
            HasPendingSoftQueueResult { has_pending: false },
        )
        .await
    }

    pub(crate) async fn take_next_soft(
        &self,
        persistence: QueuePersistenceContext,
    ) -> Option<(Intervention, bool)> {
        self.request(
            |reply| ChannelMailboxMsg::TakeNextSoft { persistence, reply },
            None,
        )
        .await
    }

    pub(crate) async fn requeue_front(
        &self,
        intervention: Intervention,
        persistence: QueuePersistenceContext,
    ) {
        let _ = self
            .request(
                |reply| ChannelMailboxMsg::RequeueFront {
                    intervention,
                    persistence,
                    reply,
                },
                (),
            )
            .await;
    }

    pub(crate) async fn cancel_queued_message(
        &self,
        message_id: MessageId,
        persistence: QueuePersistenceContext,
    ) -> Option<Intervention> {
        self.request(
            |reply| ChannelMailboxMsg::CancelQueuedMessage {
                message_id,
                persistence,
                reply,
            },
            None,
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
            },
        )
        .await
    }

    pub(crate) async fn clear(&self, persistence: QueuePersistenceContext) -> ClearChannelResult {
        self.request(
            |reply| ChannelMailboxMsg::Clear { persistence, reply },
            ClearChannelResult {
                removed_token: None,
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

    pub(crate) async fn extend_timeout(&self, extend_by_secs: u64) -> Option<i64> {
        self.request(
            |reply| ChannelMailboxMsg::ExtendTimeout {
                extend_by_secs,
                reply,
            },
            None,
        )
        .await
    }

    pub(crate) async fn take_timeout_override(&self) -> Option<i64> {
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
        reply: oneshot::Sender<bool>,
    },
    HasPendingSoftQueue {
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<HasPendingSoftQueueResult>,
    },
    TakeNextSoft {
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<Option<(Intervention, bool)>>,
    },
    RequeueFront {
        intervention: Intervention,
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<()>,
    },
    CancelQueuedMessage {
        message_id: MessageId,
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<Option<Intervention>>,
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
    RestartDrain {
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<RestartDrainResult>,
    },
    ExtendTimeout {
        extend_by_secs: u64,
        reply: oneshot::Sender<Option<i64>>,
    },
    TakeTimeoutOverride {
        reply: oneshot::Sender<Option<i64>>,
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
    watchdog_deadline_override_ms: Option<i64>,
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

fn finalize_turn_state(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: Option<&QueuePersistenceContext>,
) -> FinishTurnResult {
    let removed_token = state.cancel_token.take();
    state.active_request_owner = None;
    state.active_user_message_id = None;
    state.recovery_started_at = None;
    state.watchdog_deadline_override_ms = None;
    let previous_len = state.intervention_queue.len();
    let has_pending = has_soft_intervention(&mut state.intervention_queue);
    if let Some(persistence) = persistence {
        if state.intervention_queue.len() != previous_len || !state.intervention_queue.is_empty() {
            persist_queue(channel_id, &state.intervention_queue, persistence);
        }
    }
    FinishTurnResult {
        removed_token,
        has_pending,
        mailbox_online: true,
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
                        state.watchdog_deadline_override_ms = None;
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
                    state.cancel_token = Some(cancel_token);
                    state.active_request_owner = Some(request_owner);
                    state.active_user_message_id = Some(user_message_id);
                    state.watchdog_deadline_override_ms = None;
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
                    state.watchdog_deadline_override_ms = None;
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
                    let enqueued =
                        enqueue_intervention(&mut state.intervention_queue, intervention);
                    persist_queue(channel_id, &state.intervention_queue, &persistence);
                    let _ = reply.send(enqueued);
                }
                ChannelMailboxMsg::HasPendingSoftQueue { persistence, reply } => {
                    state.last_persistence = Some(persistence.clone());
                    let previous_len = state.intervention_queue.len();
                    let has_pending = has_soft_intervention(&mut state.intervention_queue);
                    if state.intervention_queue.len() != previous_len {
                        persist_queue(channel_id, &state.intervention_queue, &persistence);
                    }
                    let _ = reply.send(HasPendingSoftQueueResult { has_pending });
                }
                ChannelMailboxMsg::TakeNextSoft { persistence, reply } => {
                    state.last_persistence = Some(persistence.clone());
                    let next = dequeue_next_soft_intervention(&mut state.intervention_queue);
                    let has_more = has_soft_intervention(&mut state.intervention_queue);
                    persist_queue(channel_id, &state.intervention_queue, &persistence);
                    let _ = reply.send(next.map(|intervention| (intervention, has_more)));
                }
                ChannelMailboxMsg::RequeueFront {
                    intervention,
                    persistence,
                    reply,
                } => {
                    state.last_persistence = Some(persistence.clone());
                    requeue_intervention_front(&mut state.intervention_queue, intervention);
                    persist_queue(channel_id, &state.intervention_queue, &persistence);
                    let _ = reply.send(());
                }
                ChannelMailboxMsg::CancelQueuedMessage {
                    message_id,
                    persistence,
                    reply,
                } => {
                    state.last_persistence = Some(persistence.clone());
                    let removed = cancel_soft_intervention_by_message_id(
                        &mut state.intervention_queue,
                        message_id,
                    );
                    if removed.is_some() {
                        persist_queue(channel_id, &state.intervention_queue, &persistence);
                    }
                    let _ = reply.send(removed);
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
                    state.watchdog_deadline_override_ms = None;
                    state.intervention_queue.clear();
                    persist_queue(channel_id, &state.intervention_queue, &persistence);
                    let _ = reply.send(ClearChannelResult { removed_token });
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
                    let extend_ms = extend_by_secs as i64 * 1000;
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    let current = state.watchdog_deadline_override_ms.unwrap_or(now_ms);
                    let new_deadline = std::cmp::max(current, now_ms) + extend_ms;
                    state.watchdog_deadline_override_ms = Some(new_deadline);
                    let _ = reply.send(Some(new_deadline));
                }
                ChannelMailboxMsg::TakeTimeoutOverride { reply } => {
                    let _ = reply.send(state.watchdog_deadline_override_ms.take());
                }
                ChannelMailboxMsg::ClearTimeoutOverride { reply } => {
                    state.watchdog_deadline_override_ms = None;
                    let _ = reply.send(());
                }
            }
        }
    });
    ChannelMailboxHandle { sender: tx }
}

#[cfg(test)]
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
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at,
        }
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
        let now = Instant::now();

        handle
            .replace_queue(
                vec![
                    make_intervention(1, "stale", now - INTERVENTION_TTL - Duration::from_secs(1)),
                    make_intervention(2, "fresh", now),
                ],
                persistence.clone(),
            )
            .await;

        assert_eq!(
            read_saved_items(tmp.path(), &provider, token_hash, channel_id).len(),
            2
        );

        let result = handle.has_pending_soft_queue(persistence).await;
        assert!(result.has_pending);
        assert_eq!(handle.snapshot().await.intervention_queue.len(), 1);

        let items = read_saved_items(tmp.path(), &provider, token_hash, channel_id);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "fresh");

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
        let now = Instant::now();

        handle
            .replace_queue(
                vec![
                    make_intervention(1, "stale", now - INTERVENTION_TTL - Duration::from_secs(1)),
                    make_intervention(2, "fresh", now),
                ],
                persistence.clone(),
            )
            .await;
        let active_msg_id = MessageId::new(77);
        handle
            .restore_active_turn(Arc::new(CancelToken::new()), UserId::new(7), active_msg_id)
            .await;

        let result = handle.finish_turn(persistence).await;
        assert!(result.removed_token.is_some());
        assert!(result.has_pending);
        assert!(result.mailbox_online);

        let snapshot = handle.snapshot().await;
        assert!(snapshot.cancel_token.is_none());
        assert_eq!(snapshot.active_user_message_id, None);
        assert_eq!(snapshot.intervention_queue.len(), 1);

        let items = read_saved_items(tmp.path(), &provider, token_hash, channel_id);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "fresh");

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
        let now = Instant::now();

        handle
            .replace_queue(
                vec![
                    make_intervention(1, "stale", now - INTERVENTION_TTL - Duration::from_secs(1)),
                    make_intervention(2, "fresh", now),
                ],
                persistence,
            )
            .await;
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

        let snapshot = handle.snapshot().await;
        assert!(snapshot.cancel_token.is_none());
        assert_eq!(snapshot.active_user_message_id, None);
        assert_eq!(snapshot.intervention_queue.len(), 1);

        let items = read_saved_items(tmp.path(), &provider, token_hash, channel_id);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "fresh");

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

        let removed = handle
            .cancel_queued_message(MessageId::new(10), persistence)
            .await;
        assert_eq!(
            removed.as_ref().map(|item| item.text.as_str()),
            Some("first")
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

        let extended = handle.extend_timeout(30).await;
        assert!(extended.is_some());
        assert_eq!(handle.take_timeout_override().await, extended);
        assert_eq!(handle.take_timeout_override().await, None);

        assert!(handle.extend_timeout(15).await.is_some());
        handle.clear_timeout_override().await;
        assert_eq!(handle.take_timeout_override().await, None);
    }
}
