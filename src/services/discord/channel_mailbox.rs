use std::sync::{Arc, LazyLock};

use poise::serenity_prelude as serenity;
use tokio::sync::{mpsc, oneshot};

use super::*;

#[derive(Clone, Debug)]
pub(super) struct QueuePersistenceContext {
    pub(super) provider: ProviderKind,
    pub(super) token_hash: String,
    pub(super) dispatch_role_override: Option<u64>,
}

impl QueuePersistenceContext {
    pub(super) fn new(
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
pub(super) struct ChannelMailboxSnapshot {
    pub(super) cancel_token: Option<Arc<CancelToken>>,
    pub(super) active_request_owner: Option<serenity::UserId>,
    pub(super) active_user_message_id: Option<serenity::MessageId>,
    pub(super) intervention_queue: Vec<Intervention>,
    pub(super) recovery_started_at: Option<std::time::Instant>,
}

pub(super) struct FinishTurnResult {
    pub(super) removed_token: Option<Arc<CancelToken>>,
    pub(super) has_pending: bool,
}

pub(super) struct ClearChannelResult {
    pub(super) removed_token: Option<Arc<CancelToken>>,
}

pub(super) struct CancelActiveTurnResult {
    pub(super) token: Option<Arc<CancelToken>>,
    pub(super) already_stopping: bool,
}

pub(super) struct HasPendingSoftQueueResult {
    pub(super) has_pending: bool,
}

pub(super) struct RecoveryKickoffResult {
    pub(super) activated_turn: bool,
}

pub(super) struct RestartDrainResult {
    pub(super) queued_count: usize,
}

static GLOBAL_CHANNEL_MAILBOXES: LazyLock<
    dashmap::DashMap<serenity::ChannelId, ChannelMailboxHandle>,
> = LazyLock::new(dashmap::DashMap::new);

#[derive(Clone)]
pub(super) struct ChannelMailboxHandle {
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

    pub(super) async fn snapshot(&self) -> ChannelMailboxSnapshot {
        self.request(
            |reply| ChannelMailboxMsg::Snapshot { reply },
            ChannelMailboxSnapshot::default(),
        )
        .await
    }

    pub(super) async fn has_active_turn(&self) -> bool {
        self.request(|reply| ChannelMailboxMsg::HasActiveTurn { reply }, false)
            .await
    }

    pub(super) async fn cancel_token(&self) -> Option<Arc<CancelToken>> {
        self.request(|reply| ChannelMailboxMsg::CancelToken { reply }, None)
            .await
    }

    pub(super) async fn cancel_active_turn(&self) -> CancelActiveTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::CancelActiveTurn { reply },
            CancelActiveTurnResult {
                token: None,
                already_stopping: false,
            },
        )
        .await
    }

    pub(super) async fn try_start_turn(
        &self,
        cancel_token: Arc<CancelToken>,
        request_owner: serenity::UserId,
        user_message_id: serenity::MessageId,
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

    pub(super) async fn restore_active_turn(
        &self,
        cancel_token: Arc<CancelToken>,
        request_owner: serenity::UserId,
        user_message_id: serenity::MessageId,
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

    pub(super) async fn recovery_kickoff(
        &self,
        cancel_token: Arc<CancelToken>,
        request_owner: serenity::UserId,
        user_message_id: serenity::MessageId,
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

    pub(super) async fn clear_recovery_marker(&self) {
        let _ = self
            .request(|reply| ChannelMailboxMsg::ClearRecoveryMarker { reply }, ())
            .await;
    }

    pub(super) async fn enqueue(
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

    pub(super) async fn has_pending_soft_queue(
        &self,
        persistence: QueuePersistenceContext,
    ) -> HasPendingSoftQueueResult {
        self.request(
            |reply| ChannelMailboxMsg::HasPendingSoftQueue { persistence, reply },
            HasPendingSoftQueueResult { has_pending: false },
        )
        .await
    }

    pub(super) async fn take_next_soft(
        &self,
        persistence: QueuePersistenceContext,
    ) -> Option<(Intervention, bool)> {
        self.request(
            |reply| ChannelMailboxMsg::TakeNextSoft { persistence, reply },
            None,
        )
        .await
    }

    pub(super) async fn requeue_front(
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

    pub(super) async fn finish_turn(
        &self,
        persistence: QueuePersistenceContext,
    ) -> FinishTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::FinishTurn { persistence, reply },
            FinishTurnResult {
                removed_token: None,
                has_pending: false,
            },
        )
        .await
    }

    pub(super) async fn clear(&self, persistence: QueuePersistenceContext) -> ClearChannelResult {
        self.request(
            |reply| ChannelMailboxMsg::Clear { persistence, reply },
            ClearChannelResult {
                removed_token: None,
            },
        )
        .await
    }

    pub(super) async fn replace_queue(
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

    pub(super) async fn restart_drain(
        &self,
        persistence: QueuePersistenceContext,
    ) -> RestartDrainResult {
        self.request(
            |reply| ChannelMailboxMsg::RestartDrain { persistence, reply },
            RestartDrainResult { queued_count: 0 },
        )
        .await
    }

    pub(super) async fn extend_timeout(&self, extend_by_secs: u64) -> Option<i64> {
        self.request(
            |reply| ChannelMailboxMsg::ExtendTimeout {
                extend_by_secs,
                reply,
            },
            None,
        )
        .await
    }

    pub(super) async fn take_timeout_override(&self) -> Option<i64> {
        self.request(
            |reply| ChannelMailboxMsg::TakeTimeoutOverride { reply },
            None,
        )
        .await
    }

    pub(super) async fn clear_timeout_override(&self) {
        let _ = self
            .request(
                |reply| ChannelMailboxMsg::ClearTimeoutOverride { reply },
                (),
            )
            .await;
    }
}

#[derive(Clone, Default)]
pub(super) struct ChannelMailboxRegistry {
    handles: Arc<dashmap::DashMap<serenity::ChannelId, ChannelMailboxHandle>>,
}

impl ChannelMailboxRegistry {
    pub(super) fn handle(&self, channel_id: serenity::ChannelId) -> ChannelMailboxHandle {
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

    pub(super) fn global_handle(channel_id: serenity::ChannelId) -> Option<ChannelMailboxHandle> {
        GLOBAL_CHANNEL_MAILBOXES
            .get(&channel_id)
            .map(|entry| entry.value().clone())
    }

    pub(super) async fn snapshot_all(
        &self,
    ) -> std::collections::HashMap<serenity::ChannelId, ChannelMailboxSnapshot> {
        let handles: Vec<_> = self
            .handles
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect();
        let mut snapshots = std::collections::HashMap::new();
        for (channel_id, handle) in handles {
            snapshots.insert(channel_id, handle.snapshot().await);
        }
        snapshots
    }

    pub(super) fn remove(&self, channel_id: serenity::ChannelId) -> Option<ChannelMailboxHandle> {
        let local_handle = self.handles.remove(&channel_id).map(|(_, handle)| handle);
        let global_handle = GLOBAL_CHANNEL_MAILBOXES
            .remove(&channel_id)
            .map(|(_, handle)| handle);
        local_handle.or(global_handle)
    }

    pub(super) async fn restart_drain_all(
        &self,
        provider: &ProviderKind,
        token_hash: &str,
        dispatch_role_overrides: &dashmap::DashMap<serenity::ChannelId, serenity::ChannelId>,
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
        request_owner: serenity::UserId,
        user_message_id: serenity::MessageId,
        reply: oneshot::Sender<bool>,
    },
    RestoreActiveTurn {
        cancel_token: Arc<CancelToken>,
        request_owner: serenity::UserId,
        user_message_id: serenity::MessageId,
        reply: oneshot::Sender<()>,
    },
    RecoveryKickoff {
        cancel_token: Arc<CancelToken>,
        request_owner: serenity::UserId,
        user_message_id: serenity::MessageId,
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
    FinishTurn {
        persistence: QueuePersistenceContext,
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
    active_request_owner: Option<serenity::UserId>,
    active_user_message_id: Option<serenity::MessageId>,
    intervention_queue: Vec<Intervention>,
    recovery_started_at: Option<std::time::Instant>,
    watchdog_deadline_override_ms: Option<i64>,
}

fn persist_queue(
    channel_id: serenity::ChannelId,
    queue: &[Intervention],
    persistence: &QueuePersistenceContext,
) {
    super::save_channel_queue(
        &persistence.provider,
        &persistence.token_hash,
        channel_id,
        queue,
        persistence.dispatch_role_override,
    );
}

fn spawn_channel_mailbox(channel_id: serenity::ChannelId) -> ChannelMailboxHandle {
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
                    state.recovery_started_at = Some(std::time::Instant::now());
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
                    let enqueued =
                        super::enqueue_intervention(&mut state.intervention_queue, intervention);
                    persist_queue(channel_id, &state.intervention_queue, &persistence);
                    let _ = reply.send(enqueued);
                }
                ChannelMailboxMsg::HasPendingSoftQueue { persistence, reply } => {
                    let previous_len = state.intervention_queue.len();
                    let has_pending = super::has_soft_intervention(&mut state.intervention_queue);
                    if state.intervention_queue.len() != previous_len {
                        persist_queue(channel_id, &state.intervention_queue, &persistence);
                    }
                    let _ = reply.send(HasPendingSoftQueueResult { has_pending });
                }
                ChannelMailboxMsg::TakeNextSoft { persistence, reply } => {
                    let next = super::dequeue_next_soft_intervention(&mut state.intervention_queue);
                    let has_more = super::has_soft_intervention(&mut state.intervention_queue);
                    persist_queue(channel_id, &state.intervention_queue, &persistence);
                    let _ = reply.send(next.map(|intervention| (intervention, has_more)));
                }
                ChannelMailboxMsg::RequeueFront {
                    intervention,
                    persistence,
                    reply,
                } => {
                    super::requeue_intervention_front(&mut state.intervention_queue, intervention);
                    persist_queue(channel_id, &state.intervention_queue, &persistence);
                    let _ = reply.send(());
                }
                ChannelMailboxMsg::FinishTurn { persistence, reply } => {
                    let removed_token = state.cancel_token.take();
                    state.active_request_owner = None;
                    state.active_user_message_id = None;
                    state.recovery_started_at = None;
                    state.watchdog_deadline_override_ms = None;
                    let previous_len = state.intervention_queue.len();
                    let has_pending = super::has_soft_intervention(&mut state.intervention_queue);
                    if state.intervention_queue.len() != previous_len {
                        persist_queue(channel_id, &state.intervention_queue, &persistence);
                    }
                    let _ = reply.send(FinishTurnResult {
                        removed_token,
                        has_pending,
                    });
                }
                ChannelMailboxMsg::Clear { persistence, reply } => {
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
                    state.intervention_queue = queue;
                    persist_queue(channel_id, &state.intervention_queue, &persistence);
                    let _ = reply.send(());
                }
                ChannelMailboxMsg::RestartDrain { persistence, reply } => {
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
    use crate::services::provider::CancelToken;
    use poise::serenity_prelude::{ChannelId, UserId};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    #[tokio::test]
    async fn registry_remove_drops_channel_from_snapshots() {
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(42);
        let handle = registry.handle(channel_id);

        handle
            .restore_active_turn(
                Arc::new(CancelToken::new()),
                UserId::new(7),
                serenity::MessageId::new(70),
            )
            .await;
        assert!(registry.snapshot_all().await.contains_key(&channel_id));
        assert!(ChannelMailboxRegistry::global_handle(channel_id).is_some());

        let removed = registry.remove(channel_id);

        assert!(removed.is_some());
        assert!(!registry.snapshot_all().await.contains_key(&channel_id));
        assert!(ChannelMailboxRegistry::global_handle(channel_id).is_none());
    }

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

    fn queue_file_path(
        root: &Path,
        provider: &ProviderKind,
        token_hash: &str,
        channel_id: serenity::ChannelId,
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
        channel_id: serenity::ChannelId,
    ) -> Vec<super::super::PendingQueueItem> {
        let path = queue_file_path(root, provider, token_hash, channel_id);
        let json = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn make_intervention(message_id: u64, text: &str, created_at: Instant) -> Intervention {
        Intervention {
            author_id: serenity::UserId::new(1),
            message_id: serenity::MessageId::new(message_id),
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at,
        }
    }

    fn make_overflow_queue(now: Instant) -> Vec<Intervention> {
        (0..=super::super::MAX_INTERVENTIONS_PER_CHANNEL)
            .map(|idx| make_intervention(idx as u64 + 1, &format!("fresh-{idx}"), now))
            .collect()
    }

    #[tokio::test]
    async fn has_pending_soft_queue_persists_pruned_queue_state() {
        let _lock = test_env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let token_hash = "mailbox-prune-pending";
        let channel_id = serenity::ChannelId::new(41);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, token_hash, None);
        let now = Instant::now();

        handle
            .replace_queue(make_overflow_queue(now), persistence.clone())
            .await;

        assert_eq!(
            read_saved_items(tmp.path(), &provider, token_hash, channel_id).len(),
            super::super::MAX_INTERVENTIONS_PER_CHANNEL + 1
        );

        let result = handle.has_pending_soft_queue(persistence).await;
        assert!(result.has_pending);
        assert_eq!(
            handle.snapshot().await.intervention_queue.len(),
            super::super::MAX_INTERVENTIONS_PER_CHANNEL
        );

        let items = read_saved_items(tmp.path(), &provider, token_hash, channel_id);
        assert_eq!(items.len(), super::super::MAX_INTERVENTIONS_PER_CHANNEL);
        assert_eq!(items[0].text, "fresh-1");

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[tokio::test]
    async fn finish_turn_persists_pruned_queue_state() {
        let _lock = test_env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let token_hash = "mailbox-prune-finish";
        let channel_id = serenity::ChannelId::new(42);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, token_hash, None);
        let now = Instant::now();

        handle
            .replace_queue(make_overflow_queue(now), persistence.clone())
            .await;
        let active_msg_id = serenity::MessageId::new(77);
        handle
            .restore_active_turn(
                Arc::new(CancelToken::new()),
                serenity::UserId::new(7),
                active_msg_id,
            )
            .await;

        let result = handle.finish_turn(persistence).await;
        assert!(result.removed_token.is_some());
        assert!(result.has_pending);

        let snapshot = handle.snapshot().await;
        assert!(snapshot.cancel_token.is_none());
        assert_eq!(snapshot.active_user_message_id, None);
        assert_eq!(
            snapshot.intervention_queue.len(),
            super::super::MAX_INTERVENTIONS_PER_CHANNEL
        );

        let items = read_saved_items(tmp.path(), &provider, token_hash, channel_id);
        assert_eq!(items.len(), super::super::MAX_INTERVENTIONS_PER_CHANNEL);
        assert_eq!(items[0].text, "fresh-1");

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[tokio::test]
    async fn cancel_active_turn_marks_token_without_clearing_turn_state() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(serenity::ChannelId::new(44));
        let token = Arc::new(CancelToken::new());

        handle
            .try_start_turn(
                token.clone(),
                serenity::UserId::new(9),
                serenity::MessageId::new(91),
            )
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
    async fn recovery_kickoff_marks_recovery_until_finish_turn() {
        let _lock = test_env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let channel_id = serenity::ChannelId::new(45);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, "mailbox-recovery", None);

        let kickoff = handle
            .recovery_kickoff(
                Arc::new(CancelToken::new()),
                serenity::UserId::new(5),
                serenity::MessageId::new(55),
            )
            .await;
        assert!(kickoff.activated_turn);
        assert!(handle.snapshot().await.recovery_started_at.is_some());

        let finished = handle.finish_turn(persistence).await;
        assert!(finished.removed_token.is_some());
        assert!(handle.snapshot().await.recovery_started_at.is_none());

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[tokio::test]
    async fn timeout_override_round_trip_stays_in_mailbox() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(serenity::ChannelId::new(46));

        let extended = handle.extend_timeout(30).await;
        assert!(extended.is_some());
        assert_eq!(handle.take_timeout_override().await, extended);
        assert_eq!(handle.take_timeout_override().await, None);

        assert!(handle.extend_timeout(15).await.is_some());
        handle.clear_timeout_override().await;
        assert_eq!(handle.take_timeout_override().await, None);
    }
}
