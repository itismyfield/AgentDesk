use std::sync::Arc;

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
}

pub(super) struct FinishTurnResult {
    pub(super) removed_token: Option<Arc<CancelToken>>,
    pub(super) has_pending: bool,
}

pub(super) struct ClearChannelResult {
    pub(super) removed_token: Option<Arc<CancelToken>>,
}

pub(super) struct HasPendingSoftQueueResult {
    pub(super) has_pending: bool,
}

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

    pub(super) async fn cancel_queued_message(
        &self,
        message_id: serenity::MessageId,
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
        match self.handles.entry(channel_id) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(handle.clone());
                handle
            }
        }
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
        message_id: serenity::MessageId,
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<Option<Intervention>>,
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
}

#[derive(Default)]
struct ChannelMailboxState {
    cancel_token: Option<Arc<CancelToken>>,
    active_request_owner: Option<serenity::UserId>,
    active_user_message_id: Option<serenity::MessageId>,
    intervention_queue: Vec<Intervention>,
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
                    });
                }
                ChannelMailboxMsg::HasActiveTurn { reply } => {
                    let _ = reply.send(state.cancel_token.is_some());
                }
                ChannelMailboxMsg::CancelToken { reply } => {
                    let _ = reply.send(state.cancel_token.clone());
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
                ChannelMailboxMsg::CancelQueuedMessage {
                    message_id,
                    persistence,
                    reply,
                } => {
                    let removed = super::cancel_soft_intervention_by_message_id(
                        &mut state.intervention_queue,
                        message_id,
                    );
                    if removed.is_some() {
                        persist_queue(channel_id, &state.intervention_queue, &persistence);
                    }
                    let _ = reply.send(removed);
                }
                ChannelMailboxMsg::FinishTurn { persistence, reply } => {
                    let removed_token = state.cancel_token.take();
                    state.active_request_owner = None;
                    state.active_user_message_id = None;
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
            }
        }
    });
    ChannelMailboxHandle { sender: tx }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::runtime_store::test_env_lock;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

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
            .replace_queue(
                vec![
                    make_intervention(
                        1,
                        "stale",
                        now - super::super::INTERVENTION_TTL - Duration::from_secs(1),
                    ),
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
            .replace_queue(
                vec![
                    make_intervention(
                        1,
                        "stale",
                        now - super::super::INTERVENTION_TTL - Duration::from_secs(1),
                    ),
                    make_intervention(2, "fresh", now),
                ],
                persistence.clone(),
            )
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
        assert_eq!(snapshot.intervention_queue.len(), 1);

        let items = read_saved_items(tmp.path(), &provider, token_hash, channel_id);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "fresh");

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[tokio::test]
    async fn cancel_queued_message_removes_matching_entry_and_persists() {
        let _lock = test_env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let token_hash = "mailbox-cancel-queued";
        let channel_id = serenity::ChannelId::new(43);
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
            .cancel_queued_message(serenity::MessageId::new(10), persistence)
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
}
