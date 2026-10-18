//! A completion may retire only its own actor, claim history and captured signals.

use std::sync::Arc;
use std::time::Instant;

use futures::FutureExt;
use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::super::{
    ChannelMailboxHandle, ChannelMailboxMsg, ChannelMailboxRegistry, ChannelMailboxSnapshot,
    ChannelMailboxState, ClaimObservation, GLOBAL_CHANNEL_MAILBOXES, GLOBAL_RECOVERY_DONE_SIGNALS,
    GLOBAL_TURN_FINISHED_SIGNALS, turn_finished_signal,
};
use super::MailboxPurgeOutcome;
use crate::services::provider::CancelToken;

fn assert_registered(
    registry: &ChannelMailboxRegistry,
    channel: ChannelId,
    expected: &ChannelMailboxHandle,
) {
    assert!(
        registry
            .peek(channel)
            .is_some_and(|current| current.same_actor(expected))
    );
    assert!(
        ChannelMailboxRegistry::global_handle(channel)
            .is_some_and(|current| current.same_actor(expected))
    );
    assert!(
        registry
            .recovery_done
            .get(&channel)
            .is_some_and(|signal| Arc::ptr_eq(signal.value(), expected.recovery_done()))
    );
    assert!(
        ChannelMailboxRegistry::global_recovery_done(channel)
            .is_some_and(|signal| Arc::ptr_eq(&signal, expected.recovery_done()))
    );
}

fn purge_epoch(channel: ChannelId) -> u64 {
    let ClaimObservation::NoActor { purge_epoch, .. } =
        ChannelMailboxSnapshot::no_actor(channel).claim_observation
    else {
        unreachable!("no-actor snapshot carries its purge epoch");
    };
    purge_epoch
}

async fn start(handle: &ChannelMailboxHandle, message: u64) {
    assert!(
        handle
            .try_start_turn(
                Arc::new(CancelToken::new()),
                UserId::new(7),
                MessageId::new(message)
            )
            .await
    );
}

#[tokio::test]
async fn current_idle_purge_unlinks_exact_actor_and_all_signals() {
    let registry = ChannelMailboxRegistry::default();
    let channel = ChannelId::new(93_293_201);
    let handle = registry.handle(channel);
    let signal = registry.turn_finished(channel);
    let observed = handle.snapshot().await.claim_observation;

    assert_eq!(
        registry
            .remove_idle_entry_if_current(channel, &handle, observed)
            .await,
        MailboxPurgeOutcome::Removed
    );

    assert!(registry.peek(channel).is_none());
    assert!(!registry.recovery_done.contains_key(&channel));
    assert!(!registry.turn_finished.contains_key(&channel));
    assert!(!GLOBAL_CHANNEL_MAILBOXES.contains_key(&channel));
    assert!(!GLOBAL_RECOVERY_DONE_SIGNALS.contains_key(&channel));
    assert!(!GLOBAL_TURN_FINISHED_SIGNALS.contains_key(&channel));
    assert!(
        !handle
            .try_start_turn(
                Arc::new(CancelToken::new()),
                UserId::new(7),
                MessageId::new(11)
            )
            .await
    );
    assert!(signal.wait().now_or_never().is_none());
}

#[tokio::test]
async fn current_purge_of_absent_entry_does_not_advance_epoch() {
    let registry = ChannelMailboxRegistry::default();
    let channel = ChannelId::new(93_293_202);
    let handle = registry.handle(channel);
    let observed = handle.snapshot().await.claim_observation;
    assert_eq!(
        registry.remove_idle_entry(channel).await,
        MailboxPurgeOutcome::Removed
    );
    let epoch = purge_epoch(channel);

    assert_eq!(
        registry
            .remove_idle_entry_if_current(channel, &handle, observed)
            .await,
        MailboxPurgeOutcome::NoEntry
    );
    assert_eq!(purge_epoch(channel), epoch);
}

#[tokio::test]
async fn stale_actor_cannot_retire_replacement() {
    let registry = ChannelMailboxRegistry::default();
    let channel = ChannelId::new(93_293_203);
    let old = registry.handle(channel);
    let observed = old.snapshot().await.claim_observation;
    assert_eq!(
        registry.remove_idle_entry(channel).await,
        MailboxPurgeOutcome::Removed
    );
    let replacement = registry.handle(channel);
    let signal = registry.turn_finished(channel);
    let epoch = purge_epoch(channel);

    assert_eq!(
        registry
            .remove_idle_entry_if_current(channel, &old, observed)
            .await,
        MailboxPurgeOutcome::RefusedLiveWork("actor_changed")
    );

    assert_registered(&registry, channel, &replacement);
    assert!(Arc::ptr_eq(&registry.turn_finished(channel), &signal));
    assert_eq!(purge_epoch(channel), epoch);
    start(&replacement, 11).await;
    assert!(replacement.hard_stop().await.removed_token.is_some());
    assert_eq!(
        registry.remove_idle_entry(channel).await,
        MailboxPurgeOutcome::Removed
    );
}

#[tokio::test]
async fn newer_completed_claim_on_same_actor_refuses_stale_purge() {
    let registry = ChannelMailboxRegistry::default();
    let channel = ChannelId::new(93_293_204);
    let handle = registry.handle(channel);
    let observed = handle.snapshot().await.claim_observation;
    let signal = registry.turn_finished(channel);
    let epoch = purge_epoch(channel);
    start(&handle, 11).await;
    assert!(handle.hard_stop().await.removed_token.is_some());
    assert!(handle.snapshot().await.cancel_token.is_none());

    assert_eq!(
        registry
            .remove_idle_entry_if_current(channel, &handle, observed)
            .await,
        MailboxPurgeOutcome::RefusedLiveWork("claim_changed")
    );

    assert_registered(&registry, channel, &handle);
    assert!(Arc::ptr_eq(&registry.turn_finished(channel), &signal));
    assert_eq!(purge_epoch(channel), epoch);
    start(&handle, 12).await;
    assert!(handle.hard_stop().await.removed_token.is_some());
    let latest = handle.snapshot().await.claim_observation;
    assert_eq!(
        registry
            .remove_idle_entry_if_current(channel, &handle, latest)
            .await,
        MailboxPurgeOutcome::Removed
    );
}

#[test]
fn observed_close_preserves_every_live_work_gate() {
    let active = ChannelMailboxState {
        cancel_token: Some(Arc::new(CancelToken::new())),
        ..ChannelMailboxState::default()
    };
    let queued = ChannelMailboxState {
        intervention_queue: vec![super::tests::make_intervention(11, "still queued")],
        ..ChannelMailboxState::default()
    };
    let recovering = ChannelMailboxState {
        recovery_started_at: Some(Instant::now()),
        ..ChannelMailboxState::default()
    };
    let dispatching = ChannelMailboxState {
        pending_user_dispatch: Some(MessageId::new(11)),
        ..ChannelMailboxState::default()
    };
    for (mut state, reason) in [
        (active, "live_cancel_token"),
        (queued, "queue_not_empty"),
        (recovering, "recovery_in_progress"),
        (dispatching, "pending_user_dispatch"),
    ] {
        let observed = state.claim_log.observation();
        let (reply, mut response) = tokio::sync::oneshot::channel();
        super::answer_close_if_idle(
            &mut state,
            ChannelMailboxMsg::CloseIfIdle {
                expected_claim: Some(observed),
                reply,
            },
        );
        assert_eq!(response.try_recv().unwrap(), Err(reason));
        assert!(!state.closed, "{reason} must leave the actor open");
    }
}

#[tokio::test]
async fn late_duplicate_purge_preserves_reminted_actor_signals_and_epoch() {
    let registry = ChannelMailboxRegistry::default();
    let channel = ChannelId::new(93_293_205);
    let old = registry.handle(channel);
    let old_signal = registry.turn_finished(channel);
    let observed = old.snapshot().await.claim_observation;
    let mut late = Box::pin(registry.remove_idle_entry_if_current(channel, &old, observed));
    // Send CloseIfIdle, but leave its reply unconsumed while a second purge wins.
    assert!(late.as_mut().now_or_never().is_none());
    assert_eq!(
        registry
            .remove_idle_entry_if_current(channel, &old, observed)
            .await,
        MailboxPurgeOutcome::Removed
    );
    let replacement = registry.handle(channel);
    let signal = registry.turn_finished(channel);
    assert!(!Arc::ptr_eq(&old_signal, &signal));
    assert!(!old.same_actor(&replacement));
    let epoch = purge_epoch(channel);

    assert_eq!(
        late.await,
        MailboxPurgeOutcome::RefusedLiveWork("actor_changed")
    );

    assert_registered(&registry, channel, &replacement);
    assert!(Arc::ptr_eq(&registry.turn_finished(channel), &signal));
    assert!(
        ChannelMailboxRegistry::global_turn_finished(channel)
            .is_some_and(|current| Arc::ptr_eq(&current, &signal))
    );
    assert_eq!(purge_epoch(channel), epoch);
    start(&replacement, 11).await;
    assert!(signal.wait().now_or_never().is_none());
    assert!(replacement.hard_stop().await.removed_token.is_some());
    assert!(signal.wait().now_or_never().is_some());
    assert_eq!(
        registry.remove_idle_entry(channel).await,
        MailboxPurgeOutcome::Removed
    );
}

#[tokio::test]
async fn guarded_purge_requires_actor_reply_but_operator_can_remove_dead_actor() {
    let registry = ChannelMailboxRegistry::default();
    let channel = ChannelId::new(93_293_207);
    registry.insert_unreachable_for_test(channel);
    let handle = registry.peek(channel).unwrap();
    let epoch = purge_epoch(channel);

    assert_eq!(
        registry
            .remove_idle_entry_if_current(channel, &handle, ClaimObservation::default())
            .await,
        MailboxPurgeOutcome::RefusedLiveWork("actor_unreachable")
    );
    assert!(
        registry
            .peek(channel)
            .is_some_and(|current| current.same_actor(&handle))
    );
    assert_eq!(purge_epoch(channel), epoch);
    assert_eq!(
        registry.remove_idle_entry(channel).await,
        MailboxPurgeOutcome::Removed
    );
}

#[tokio::test]
async fn successful_purge_preserves_signal_replaced_during_verdict_await() {
    let registry = ChannelMailboxRegistry::default();
    let channel = ChannelId::new(93_293_206);
    let handle = registry.handle(channel);
    let old_signal = registry.turn_finished(channel);
    let observed = handle.snapshot().await.claim_observation;
    let mut purge = Box::pin(registry.remove_idle_entry_if_current(channel, &handle, observed));
    assert!(purge.as_mut().now_or_never().is_none());
    GLOBAL_TURN_FINISHED_SIGNALS.remove(&channel);
    let replacement_signal = turn_finished_signal(channel);
    registry
        .turn_finished
        .insert(channel, replacement_signal.clone());
    assert!(!Arc::ptr_eq(&old_signal, &replacement_signal));

    assert_eq!(purge.await, MailboxPurgeOutcome::Removed);

    assert!(registry.peek(channel).is_none());
    assert!(Arc::ptr_eq(
        &registry.turn_finished(channel),
        &replacement_signal
    ));
    assert!(
        ChannelMailboxRegistry::global_turn_finished(channel)
            .is_some_and(|current| Arc::ptr_eq(&current, &replacement_signal))
    );
    registry.turn_finished.remove(&channel);
    GLOBAL_TURN_FINISHED_SIGNALS.remove(&channel);
}
