//! Channel mailbox: enqueue, dequeue, duplicate delivery, restart, concurrent workers.

use std::collections::BTreeSet;
use std::sync::Arc;

use proptest::prelude::*;
use serenity::model::id::{ChannelId, MessageId, UserId};

use super::{IsolatedRoot, fresh_channel, runtime};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::{
    ChannelMailboxRegistry, Intervention, InterventionMode, QueuePersistenceContext,
    load_pending_dispatch_markers,
};

#[derive(Clone, Debug)]
enum MailOp {
    Enqueue(u8),
    Start(u8),
    Finish,
    Take,
    Cancel(u8),
    Restart,
}

fn op_strategy() -> impl Strategy<Value = MailOp> {
    prop_oneof![
        (1u8..8).prop_map(MailOp::Enqueue),
        (1u8..8u8).prop_map(MailOp::Start),
        Just(MailOp::Finish),
        Just(MailOp::Take),
        (1u8..8u8).prop_map(MailOp::Cancel),
        Just(MailOp::Restart),
    ]
}

fn intervention(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 1,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("state-property {id}"),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: true,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    }
}

fn ids_of(queue: &[Intervention]) -> Vec<u64> {
    queue
        .iter()
        .flat_map(|item| {
            let mut ids = item.source_message_ids.iter().map(|id| id.get()).collect::<Vec<_>>();
            if ids.is_empty() {
                ids.push(item.message_id.get());
            }
            ids
        })
        .collect()
}

struct World {
    registry: ChannelMailboxRegistry,
    channel: ChannelId,
    persistence: QueuePersistenceContext,
    provider: ProviderKind,
    token: String,
    accepted: BTreeSet<u64>,
    removed: BTreeSet<u64>,
}

impl World {
    fn new(channel: u64) -> Self {
        Self {
            registry: ChannelMailboxRegistry::default(),
            channel: ChannelId::new(channel),
            persistence: QueuePersistenceContext::new(&ProviderKind::Claude, "state-prop", None),
            provider: ProviderKind::Claude,
            token: "state-prop".into(),
            accepted: BTreeSet::new(),
            removed: BTreeSet::new(),
        }
    }

    fn handle(&self) -> crate::services::turn_orchestrator::ChannelMailboxHandle {
        self.registry.handle(self.channel)
    }

    async fn recover(&self) {
        let handle = self.handle();
        let drained = handle.restart_drain(self.persistence.clone()).await;
        assert!(
            drained.persistence_error.is_none(),
            "restart_drain failed: {:?}",
            drained.persistence_error
        );
        for marker in load_pending_dispatch_markers(&self.provider, &self.token) {
            if marker.channel_id != self.channel {
                continue;
            }
            let merged = handle
                .merge_restored_dispatch_marker_or_refused(
                    marker.intervention,
                    marker.restored_override,
                    self.persistence.clone(),
                )
                .await
                .unwrap_or_default();
            assert!(
                merged.persistence_error.is_none(),
                "marker merge failed: {:?}",
                merged.persistence_error
            );
        }
    }

    async fn check(&self) {
        let snap = self.handle().snapshot().await;
        let queued = ids_of(&snap.intervention_queue);
        let mut seen = BTreeSet::new();
        for id in &queued {
            assert!(seen.insert(*id), "duplicate queued id {id}");
            assert!(
                self.accepted.contains(id) && !self.removed.contains(id),
                "queued id {id} was not an accepted live input"
            );
        }
        let pending = snap
            .pending_user_dispatch
            .map(|id| id.get())
            .into_iter()
            .chain(snap.pending_user_dispatch_source_ids.iter().map(|id| id.get()));
        for id in pending {
            assert!(
                self.accepted.contains(&id),
                "dispatch reservation {id} was never accepted"
            );
            assert!(!queued.contains(&id), "reserved id {id} is also queued");
        }
    }

    async fn apply(&mut self, op: MailOp) {
        let handle = self.handle();
        match op {
            MailOp::Enqueue(raw) => {
                let id = u64::from(raw);
                let result = handle
                    .enqueue_observed(intervention(id), self.persistence.clone(), None)
                    .await;
                assert!(
                    result.persistence_error.is_none(),
                    "enqueue persist: {:?}",
                    result.persistence_error
                );
                if result.enqueued {
                    self.removed.remove(&id);
                    self.accepted.insert(id);
                }
            }
            MailOp::Start(raw) => {
                let id = u64::from(raw);
                let started = handle
                    .try_start_turn_with_persistence(
                        Arc::new(CancelToken::new()),
                        UserId::new(7),
                        MessageId::new(id),
                        self.persistence.clone(),
                    )
                    .await
                    .started;
                if started {
                    self.accepted.insert(id);
                }
            }
            MailOp::Finish => {
                let before = self.handle().snapshot().await;
                let finished = handle.finish_turn(self.persistence.clone()).await;
                assert!(finished.mailbox_online);
                assert!(finished.persistence_error.is_none());
                let after = self.handle().snapshot().await;
                assert!(after.cancel_token.is_none(), "finish left an active token");
                let before_ids = ids_of(&before.intervention_queue);
                let after_ids = ids_of(&after.intervention_queue);
                for id in &after_ids {
                    assert!(
                        before_ids.contains(id),
                        "finish invented queued id {id}"
                    );
                }
            }
            MailOp::Take => {
                let before = ids_of(&self.handle().snapshot().await.intervention_queue);
                let taken = handle.take_next_soft(self.persistence.clone()).await;
                assert!(taken.persistence_error.is_none(), "{:?}", taken.persistence_error);
                if let Some(item) = taken.intervention {
                    let id = item.message_id.get();
                    assert!(
                        before.contains(&id),
                        "take returned {id} which was not queued: {before:?}"
                    );
                    let after = ids_of(&self.handle().snapshot().await.intervention_queue);
                    assert!(!after.contains(&id), "taken id {id} still queued");
                }
            }
            MailOp::Cancel(raw) => {
                let id = u64::from(raw);
                let cancelled = handle
                    .cancel_queued_primary_message(MessageId::new(id), self.persistence.clone())
                    .await;
                assert!(cancelled.persistence_error.is_none());
                if cancelled.removed.is_some() {
                    self.removed.insert(id);
                }
            }
            MailOp::Restart => {
                let live = self.durable_ids().await;
                drop(handle);
                self.registry = ChannelMailboxRegistry::default();
                self.recover().await;
                let restored = self.durable_ids().await;
                let missing: Vec<_> = live.difference(&restored).copied().collect();
                assert!(
                    missing.is_empty(),
                    "restart lost ids {missing:?}; before={live:?} after={restored:?}"
                );
            }
        }
        self.check().await;
    }

    async fn durable_ids(&self) -> BTreeSet<u64> {
        let snap = self.handle().snapshot().await;
        let mut live = ids_of(&snap.intervention_queue)
            .into_iter()
            .collect::<BTreeSet<_>>();
        if let Some(pending) = snap.pending_user_dispatch {
            live.insert(pending.get());
        }
        for id in &snap.pending_user_dispatch_source_ids {
            live.insert(id.get());
        }
        for marker in load_pending_dispatch_markers(&self.provider, &self.token) {
            if marker.channel_id == self.channel {
                live.insert(marker.intervention.message_id.get());
            }
        }
        live
    }
}

async fn run_ops(ops: Vec<MailOp>) {
    let mut world = World::new(fresh_channel());
    for op in ops {
        world.apply(op).await;
    }
    let snap = world.handle().snapshot().await;
    let queued = ids_of(&snap.intervention_queue);
    assert_eq!(queued.len(), queued.iter().collect::<BTreeSet<_>>().len());
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 20, ..ProptestConfig::default() })]
    fn mailbox_sequences_do_not_lose_or_duplicate_inputs(
        ops in prop::collection::vec(op_strategy(), 1..14)
    ) {
        let _root = IsolatedRoot::acquire();
        runtime().block_on(run_ops(ops));
    }
}

fn reserved_head_is_not_enqueued_again() {
    let _root = super::IsolatedRoot::acquire();
    super::runtime().block_on(async {
        let mut world = World::new(super::fresh_channel());
        world.apply(MailOp::Enqueue(6)).await;
        world.apply(MailOp::Take).await;
        world.apply(MailOp::Enqueue(6)).await;
    });
}

pub(super) fn run() -> Vec<String> {
    [
        (
            "reserved_head_is_not_enqueued_again",
            reserved_head_is_not_enqueued_again as fn(),
        ),
        (
            "mailbox_sequences_do_not_lose_or_duplicate_inputs",
            mailbox_sequences_do_not_lose_or_duplicate_inputs as fn(),
        ),
        (
            "concurrent_enqueues_keep_every_distinct_id",
            concurrent_enqueues_keep_every_distinct_id,
        ),
        (
            "concurrent_duplicate_enqueue_is_idempotent",
            concurrent_duplicate_enqueue_is_idempotent,
        ),
    ]
    .into_iter()
    .filter_map(|(name, test)| super::exercise(name, test))
    .collect()
}

fn concurrent_enqueues_keep_every_distinct_id() {
    let _root = IsolatedRoot::acquire();
    runtime().block_on(async {
        for round in 0..20 {
            let world = World::new(fresh_channel());
            let mut joins = Vec::new();
            for slot in 1..=8u64 {
                let handle = world.handle();
                let persistence = world.persistence.clone();
                joins.push(tokio::spawn(async move {
                    handle
                        .enqueue_observed(intervention(100 + slot + round * 10), persistence, None)
                        .await
                }));
            }
            for join in joins {
                let result = join.await.expect("enqueue task");
                assert!(result.persistence_error.is_none());
                assert!(
                    result.enqueued,
                    "enqueued={} err={:?}",
                    result.enqueued, result.persistence_error
                );
            }
            let snap = world.handle().snapshot().await;
            assert_eq!(ids_of(&snap.intervention_queue).len(), 8, "round {round}");
        }
    });
}

fn concurrent_duplicate_enqueue_is_idempotent() {
    let _root = IsolatedRoot::acquire();
    runtime().block_on(async {
        for _ in 0..20 {
            let world = World::new(fresh_channel());
            let mut joins = Vec::new();
            for _ in 0..6 {
                let handle = world.handle();
                let persistence = world.persistence.clone();
                joins.push(tokio::spawn(async move {
                    handle
                        .enqueue_observed(intervention(4), persistence, None)
                        .await
                }));
            }
            let mut accepted = 0;
            for join in joins {
                let result = join.await.expect("duplicate enqueue");
                assert!(result.persistence_error.is_none());
                if result.enqueued {
                    accepted += 1;
                }
            }
            assert_eq!(accepted, 1);
            assert_eq!(ids_of(&world.handle().snapshot().await.intervention_queue), vec![4]);
        }
    });
}
