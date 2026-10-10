use super::*;
mod backstop;
pub(in crate::services) use backstop::BackstopSlot;
pub(in crate::services::discord) use backstop::spawn_turn_completion_idle_queue_listener;
#[cfg(test)]
use backstop::{
    DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY, DEFERRED_IDLE_QUEUE_KICKOFF_INITIAL_DELAY,
    deferred_idle_queue_initial_presleep, emit_idle_queue_backstop_warn,
    idle_queue_backstop_backlog_units,
};
pub(super) use backstop::{
    arm_event_backstop_after_no_start_if_queue_nonempty,
    arm_slow_idle_queue_backstop_if_queue_nonempty, schedule_deferred_idle_queue_kickoff,
    schedule_deferred_idle_queue_kickoff_immediate,
};
#[cfg(test)]
pub(in crate::services::discord) use backstop::{
    idle_queue_backstop_delay_for_tests, idle_queue_backstop_fires_for_tests,
};

#[cfg(test)]
#[path = "queue_io/cancel_reclaim_support_test.rs"]
pub(in crate::services::discord) mod cancel_backstop_test_support;

mod transport;
mod turn_admission;
use transport::QueueTransport;
pub(super) use turn_admission::{
    INPUT_PENDING_NOTICE, input_refusal_notice, mailbox_enqueue_observed_intervention,
    mailbox_recovery_kickoff, mailbox_try_start_turn_adopting, mailbox_try_start_turn_behind_queue,
    mailbox_try_start_turn_kinded_with_feedback, mailbox_try_start_turn_unless_released,
};

tokio::task_local! {
    static SUPPRESS_POST_ENQUEUE_IDLE_QUEUE_KICK: bool;
}

#[cfg(test)]
pub(in crate::services::discord) type IdleQueueKickHookForTests = std::sync::Arc<
    dyn Fn(
            Arc<SharedData>,
            ProviderKind,
            ChannelId,
            &'static str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Option<IdleQueueKickoffChannelOutcome>> + Send>,
        > + Send
        + Sync,
>;

#[cfg(test)]
static IDLE_QUEUE_KICK_HOOK_FOR_TESTS: std::sync::Mutex<Option<IdleQueueKickHookForTests>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(in crate::services::discord) struct IdleQueueKickHookResetForTests;

#[cfg(test)]
impl Drop for IdleQueueKickHookResetForTests {
    fn drop(&mut self) {
        *IDLE_QUEUE_KICK_HOOK_FOR_TESTS
            .lock()
            .expect("idle queue kick hook lock") = None;
    }
}

#[cfg(test)]
pub(in crate::services::discord) fn set_idle_queue_kick_hook_for_tests(
    hook: IdleQueueKickHookForTests,
) -> IdleQueueKickHookResetForTests {
    *IDLE_QUEUE_KICK_HOOK_FOR_TESTS
        .lock()
        .expect("idle queue kick hook lock") = Some(hook);
    IdleQueueKickHookResetForTests
}

#[cfg(test)]
async fn idle_queue_kick_hook_outcome_for_tests(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
) -> Option<IdleQueueKickoffChannelOutcome> {
    let hook = IDLE_QUEUE_KICK_HOOK_FOR_TESTS
        .lock()
        .expect("idle queue kick hook lock")
        .clone();
    hook?(shared, provider, channel_id, reason).await
}

pub(in crate::services::discord) async fn mailbox_cancel_queued_primary_message(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Option<Intervention> {
    let result: CancelQueuedMessageResult = shared
        .mailbox(channel_id)
        .cancel_queued_primary_message(
            message_id,
            queue_persistence_context(shared, provider, channel_id),
        )
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    result.removed
}

pub(super) async fn with_post_enqueue_idle_queue_kick_suppressed<F>(future: F) -> F::Output
where
    F: std::future::Future,
{
    SUPPRESS_POST_ENQUEUE_IDLE_QUEUE_KICK
        .scope(true, future)
        .await
}

fn post_enqueue_idle_queue_kick_suppressed() -> bool {
    SUPPRESS_POST_ENQUEUE_IDLE_QUEUE_KICK
        .try_with(|suppressed| *suppressed)
        .unwrap_or(false)
}

fn race_loss_requeue_snapshot_has_active_holder(snapshot: &ChannelMailboxSnapshot) -> bool {
    snapshot.cancel_token.is_some()
        || snapshot.active_request_owner.is_some()
        || snapshot.active_user_message_id.is_some()
}

fn race_loss_requeue_snapshot_has_idle_kickable_backlog(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
) -> bool {
    !race_loss_requeue_snapshot_has_active_holder(snapshot)
        && idle_queue_snapshot_has_kickable_backlog(shared, provider, channel_id, snapshot)
}

pub(super) fn schedule_race_loss_requeue_post_enqueue_idle_recheck(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
) {
    super::task_supervisor::spawn_observed("race_loss_requeue_idle_recheck", async move {
        // A race-loss recheck is an edge-trigger, not competing transition
        // authority — so it must not park in the transition lock's waiter queue.
        //
        // #5170 B: `tokio::sync::Mutex` hands a released permit straight to the
        // head of its waiter queue and only returns it to the free-permit
        // counter once that queue is empty, while `try_lock_owned()` consults
        // the free-permit counter alone. One blocking waiter parked here
        // therefore makes every non-blocking acquirer fail for as long as this
        // task waits — including intake
        // (`turn_start::try_intake_runtime_transition_after_redirect`) and the
        // kickoff dequeue gate (`idle_queue_take_next_soft_if_ready`). #4794's
        // `lock_owned().await` was meant to coalesce this recheck behind the
        // live transition; instead it starved the very kickoff it was about to
        // perform, and each starved intake requeued and spawned another waiter
        // here. Not owning the transition means not owning an edge: defer to
        // the slow fail-open backstop, the same treatment the finalize epilogue
        // transition fence already gives a transition-owned channel.
        let Ok(transition_guard) = shared.session_transition_lock(channel_id).try_lock_owned()
        else {
            tracing::debug!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                "Deferred drain: session transition owns channel; race-loss requeue recheck defers to the slow backstop"
            );
            arm_slow_idle_queue_backstop_if_queue_nonempty(
                &shared,
                &provider,
                channel_id,
                "race_loss_requeue_transition_busy",
            )
            .await;
            return;
        };

        let snapshot = super::mailbox_snapshot(&shared, channel_id).await;
        if !race_loss_requeue_snapshot_has_idle_kickable_backlog(
            &shared, &provider, channel_id, &snapshot,
        ) {
            tracing::debug!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                active_holder = race_loss_requeue_snapshot_has_active_holder(&snapshot),
                queue_len = snapshot.intervention_queue.len(),
                "Deferred drain: race-loss requeue post-enqueue recheck found no idle kickable backlog"
            );
            return;
        }

        drop(transition_guard);
        let outcome = kick_idle_queue_channel_if_context_available(
            &shared,
            &provider,
            channel_id,
            "race_loss_requeue_idle_recheck",
        )
        .await;
        arm_event_backstop_after_no_start_if_queue_nonempty(
            &shared,
            &provider,
            channel_id,
            outcome,
            "race_loss_requeue_idle_recheck",
        )
        .await;
    });
}

pub(super) fn schedule_post_enqueue_idle_queue_kick(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
) {
    if post_enqueue_idle_queue_kick_suppressed() {
        tracing::debug!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            "Deferred drain: suppressed post-enqueue idle snapshot kick for race-loss requeue"
        );
        return;
    }

    // #4048 S3 enqueue-then-check closes the lost-wakeup window that remains
    // after subscribe-then-snapshot on the completion listener: a turn can
    // publish/release before this enqueue is durable, so the event listener's
    // snapshot legitimately sees an empty queue. Once persistence succeeds, the
    // spawned task mirrors the listener by taking a fresh mailbox snapshot and
    // kicking immediately when no real active turn owns the channel. Spawning
    // keeps the dispatch future acyclic: the kick path can re-enter
    // `handle_text_message`, whose race-loss branch can enqueue again. The drain
    // is idempotent: actor-serialized dequeue plus the foreground guard prevent
    // double-starts when older race-loss compensation also schedules a kick.
    super::task_supervisor::spawn_observed("post_enqueue_idle_queue_kick", async move {
        let snapshot = super::mailbox_snapshot(&shared, channel_id).await;
        if idle_queue_snapshot_has_kickable_backlog(&shared, &provider, channel_id, &snapshot) {
            let outcome = kick_idle_queue_channel_if_context_available(
                &shared,
                &provider,
                channel_id,
                "post_enqueue_idle_snapshot",
            )
            .await;
            arm_event_backstop_after_no_start_if_queue_nonempty(
                &shared,
                &provider,
                channel_id,
                outcome,
                "post_enqueue_idle_snapshot",
            )
            .await;
        }
    });
}

pub(super) async fn kick_idle_queue_channel_if_context_available(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
) -> IdleQueueKickoffChannelOutcome {
    #[cfg(test)]
    if let Some(outcome) =
        idle_queue_kick_hook_outcome_for_tests(shared.clone(), provider.clone(), channel_id, reason)
            .await
    {
        return outcome;
    }

    let Some(transport) = QueueTransport::from_runtime(shared) else {
        tracing::debug!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            reason,
            "Deferred drain: Discord REST credentials unavailable; preserving queued work for the slow backstop"
        );
        return IdleQueueKickoffChannelOutcome::default();
    };

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🚀 Deferred drain: one-shot kick for channel {} ({reason})",
        channel_id
    );
    #[cfg(test)]
    {
        queue_dispatch::kickoff::cancel_backstop_test_support::with_origin(
            reason,
            super::kickoff_idle_queue_channel(&transport.intake_deps(shared), provider, channel_id),
        )
        .await
    }
    #[cfg(not(test))]
    super::kickoff_idle_queue_channel(&transport.intake_deps(shared), provider, channel_id).await
}

#[cfg(test)]
#[path = "queue_io/ledger_settlement_tests.rs"]
mod ledger_settlement_tests;

#[cfg(all(test, unix))]
#[path = "queue_io/cancel_backstop_tests.rs"]
mod cancel_backstop_tests;

#[cfg(test)]
#[path = "queue_io/idle_queue_tests.rs"]
mod presleep_tests;
