//! #5951 C3t-0g — a purge-closed actor keeps answering the handles that still
//! point at it, but the disk queue, the dispatch marker and the
//! `turn_finished` signal are keyed by channel, so they already belong to the
//! successor. No request reaching the closed actor may change them.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::FutureExt;
use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::super::test_support::{AGENTDESK_ROOT_DIR_ENV, lock_test_env};
use super::super::{
    ChannelMailboxHandle, ChannelMailboxRegistry, GLOBAL_CHANNEL_MAILBOXES, Intervention,
    QueuePersistenceContext, load_channel_pending_dispatch_marker,
    load_channel_pending_queue_for_tests, save_channel_pending_dispatch_marker,
    turn_finished_signal,
};
use super::MailboxPurgeOutcome;
use crate::services::provider::{CancelToken, ProviderKind};

const TOKEN_HASH: &str = "c3t0g-closed-gate";
const QUEUED: u64 = 11;
const MARKER: u64 = 12;
const ACTIVE: u64 = 13;
const OFFERED: u64 = 14;

type Request = fn(ChannelMailboxHandle) -> Pin<Box<dyn Future<Output = ()>>>;

macro_rules! row {
    ($name:literal, |$old:ident| $call:expr) => {{
        let request: Request = |$old| {
            Box::pin(async move {
                let _ = $call.await;
            })
        };
        ($name, request)
    }};
}

/// Every arm that is not a read, sent to the closed actor.
fn rows() -> [(&'static str, Request); 15] {
    [
        row!("Clear", |old| old.clear(p())),
        row!("PurgeQueue", |old| old.purge_queue(p(), false)),
        row!("RestartDrain", |old| old.restart_drain(p())),
        row!("RequeueFront", |old| old.requeue_front(item(OFFERED), p())),
        row!("TakeNextSoft", |old| old.take_next_soft(p())),
        row!("MergeQueueItems", |old| old
            .merge_restored_queue_items(vec![item(OFFERED)], p())),
        row!("MergeDispatchMarker", |old| {
            old.merge_restored_dispatch_marker(item(MARKER), None, p())
        }),
        row!("Hydrate", |old| old.hydrate_pending_queue_from_disk(p())),
        row!("AbandonPendingDispatch", |old| {
            old.abandon_pending_dispatch(MessageId::new(MARKER), p())
        }),
        row!("FinishTurn", |old| old.finish_turn(p())),
        row!("HardStop", |old| old.hard_stop()),
        row!("FinishTurnIfMatches", |old| old
            .finish_turn_if_matches(MessageId::new(ACTIVE), p())),
        row!("FinishCancelledTurn", |old| old.finish_cancelled_turn()),
        row!("ClearRecoveryMarker", |old| old.clear_recovery_marker()),
        row!("CancelQueued", |old| {
            old.cancel_queued_primary_message(MessageId::new(QUEUED), p())
        }),
    ]
}

fn p() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, TOKEN_HASH, None)
}

fn item(message_id: u64) -> Intervention {
    super::tests::make_intervention(message_id, "c3t0g")
}

fn ids(queue: &[Intervention]) -> Vec<u64> {
    queue.iter().map(|item| item.message_id.get()).collect()
}

/// Purges `old`, gives the successor a live turn, one queued item and a
/// dispatch marker on disk, sends `request` to `old`, and returns what broke.
async fn violations(channel: ChannelId, request: Request) -> Vec<String> {
    let registry = ChannelMailboxRegistry::default();
    let old = registry.handle(channel);
    assert_eq!(
        registry.remove_idle_entry(channel).await,
        MailboxPurgeOutcome::Removed
    );
    let successor = registry.handle(channel);
    let turn = Arc::new(CancelToken::new());
    let user = UserId::new(7);
    assert!(
        successor
            .try_start_turn(turn, user, MessageId::new(ACTIVE))
            .await
    );
    assert!(successor.enqueue(item(QUEUED), p()).await.enqueued);
    let provider = ProviderKind::Claude;
    save_channel_pending_dispatch_marker(&provider, TOKEN_HASH, channel, &item(MARKER), None)
        .unwrap();

    request(old.clone()).await;

    let mut broken = Vec::new();
    let memory = successor.snapshot().await;
    let (disk, _) = load_channel_pending_queue_for_tests(&provider, TOKEN_HASH, channel);
    for (place, queue) in [("memory", &memory.intervention_queue), ("disk", &disk)] {
        if ids(queue) != [QUEUED] {
            broken.push(format!("{place}={:?}", ids(queue)));
        }
    }
    let marker = load_channel_pending_dispatch_marker(&provider, TOKEN_HASH, channel);
    if marker.map(|(marker, _)| marker.message_id.get()) != Some(MARKER) {
        broken.push("marker removed".to_string());
    }
    if turn_finished_signal(channel)
        .wait()
        .now_or_never()
        .is_some()
    {
        broken.push("turn_finished latched".to_string());
    }
    if memory.cancel_token.is_none() {
        broken.push("active turn released".to_string());
    }
    let closed = old.snapshot().await;
    if !closed.intervention_queue.is_empty() || closed.pending_user_dispatch.is_some() {
        broken.push("closed actor took work".to_string());
    }
    let _ = successor.hard_stop().await;
    let _ = successor.purge_queue(p(), false).await;
    GLOBAL_CHANNEL_MAILBOXES.remove(&channel);
    broken
}

/// T-P6 — every request that is not a read and reaches a purge-closed actor
/// leaves the successor's channel state untouched.
#[test]
fn purge_closed_actor_never_mutates_successor_channel_state() {
    let _lock = lock_test_env();
    let root = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, root.path()) };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let broken: Vec<_> = runtime.block_on(async {
        let mut broken = Vec::new();
        for (index, (name, request)) in rows().into_iter().enumerate() {
            let channel = ChannelId::new(95_951_301 + index as u64);
            let row = violations(channel, request).await;
            if !row.is_empty() {
                broken.push(format!("{name}: {}", row.join(", ")));
            }
        }
        broken
    });
    unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    assert!(
        broken.is_empty(),
        "a purge-closed actor changed successor state:\n{}",
        broken.join("\n")
    );
}
