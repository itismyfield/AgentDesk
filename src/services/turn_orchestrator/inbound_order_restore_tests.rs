//! The stall clock at the actor API: which drain events wipe it and which leave it running.
use std::sync::Arc;
use std::sync::atomic::Ordering;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::super::actor_hydrate_regression_tests::make_intervention;
use super::super::{
    ActiveTurnKind, ChannelMailboxHandle, ChannelMailboxRegistry, QueuePersistenceContext,
};
use super::*;
use crate::services::provider::{CancelToken, ProviderKind};

/// One tick past the fail-open window; no test can reach it by waiting.
const PAST_THE_WINDOW: Duration = Duration::from_secs(INBOUND_ORDER_FAIL_OPEN_AFTER.as_secs() + 1);

struct Channel {
    _root: crate::config::TestRuntimeRootGuard,
    _registry: ChannelMailboxRegistry,
    handle: ChannelMailboxHandle,
    persistence: QueuePersistenceContext,
}

impl Channel {
    fn new(channel_id: u64) -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let registry = ChannelMailboxRegistry::default();
        Self {
            _root: root,
            handle: registry.handle(ChannelId::new(channel_id)),
            _registry: registry,
            persistence: QueuePersistenceContext::new(
                &ProviderKind::Claude,
                "restore-streak",
                None,
            ),
        }
    }

    async fn enqueue(&self, message_id: u64) {
        let queued = make_intervention(message_id, &format!("queued {message_id}"), Instant::now());
        let result = self.handle.enqueue(queued, self.persistence.clone()).await;
        assert!(
            result.enqueued,
            "the ordering assertions need {message_id} queued"
        );
    }

    /// A claim as Discord text intake makes it: behind queued work.
    async fn claim(&self, message_id: u64) -> bool {
        let token = Arc::new(CancelToken::new());
        self.claim_with(token, message_id, TurnAdmissionOrder::BehindQueue)
            .await
    }

    async fn claim_with(
        &self,
        token: Arc<CancelToken>,
        message_id: u64,
        order: TurnAdmissionOrder,
    ) -> bool {
        let (owner, message) = (UserId::new(5_985), MessageId::new(message_id));
        let claim = self.handle.try_start_turn_kinded_with_persistence(
            token,
            owner,
            message,
            ActiveTurnKind::UserOrAgent,
            order,
            self.persistence.clone(),
        );
        claim.await.started
    }

    /// One drain round the head does not survive: dequeue it, let `arrival` arm
    /// the clock and age it past the window, then hand the head back.
    async fn round_restoring_head(&self, head: u64, arrival: u64) {
        let taken = self.handle.take_next_soft(self.persistence.clone()).await;
        let promoted = taken.intervention.expect("the head must be promotable");
        assert_eq!(promoted.message_id.get(), head, "the expected head leads");
        let lease = taken.dispatch_lease.expect("promotion holds a lease");
        assert!(
            !self.claim(arrival).await,
            "the arrival waits behind the head"
        );
        self.handle
            .age_inbound_waits_for_test(PAST_THE_WINDOW)
            .await;
        let persistence = self.persistence.clone();
        let restored = self
            .handle
            .restore_dequeued_head(promoted, persistence, lease);
        assert!(restored.await.enqueued, "the head returns to the front");
    }

    /// Hand `head` back without a stalled window, as a failed dispatch does.
    async fn restore_unclaimed(&self, head: u64) {
        let taken = self.handle.take_next_soft(self.persistence.clone()).await;
        let promoted = taken.intervention.expect("the head must be promotable");
        assert_eq!(promoted.message_id.get(), head, "the expected head leads");
        let lease = taken.dispatch_lease.expect("promotion holds a lease");
        let persistence = self.persistence.clone();
        let restored = self
            .handle
            .restore_dequeued_head(promoted, persistence, lease);
        assert!(restored.await.enqueued, "the head returns to the front");
    }
}

/// A head whose dispatch keeps failing comes back every round. Only the first returns, one per
/// backstop round the window spans, wipe it; past those the stalled window fails open.
#[tokio::test]
async fn a_head_that_keeps_returning_unclaimed_stops_wiping_the_stall_window() {
    let channel = Channel::new(5_985_101);
    let head = 5_985_201;
    channel.enqueue(head).await;

    for round in 0..3 {
        channel.round_restoring_head(head, 5_985_210 + round).await;
        let latecomer = 5_985_220 + round;
        assert!(
            !channel.claim(latecomer).await,
            "return {round} of the head ends a drain round, so the window restarts"
        );
    }
    channel.round_restoring_head(head, 5_985_213).await;

    assert!(
        channel.claim(5_985_223).await,
        "a fourth unclaimed return is a wedge, so the stalled window fails open"
    );
}

/// The streak belongs to one head. Once that head leaves the queue, the next
/// head's returns end drain rounds of their own and wipe the window again.
#[tokio::test]
async fn a_new_head_at_the_front_gets_its_own_restore_streak() {
    let channel = Channel::new(5_985_102);
    let (wedged, next) = (5_985_231, 5_985_232);
    channel.enqueue(wedged).await;
    channel.enqueue(next).await;
    for _ in 0..3 {
        channel.restore_unclaimed(wedged).await;
    }

    let cancelled = channel
        .handle
        .cancel_queued_primary_message(MessageId::new(wedged), channel.persistence.clone())
        .await;
    assert!(cancelled.removed.is_some(), "the wedged head is cancelled");
    channel.round_restoring_head(next, 5_985_233).await;

    assert!(
        !channel.claim(5_985_234).await,
        "the next head's first return still restarts the window"
    );
}

/// The dequeued head claiming the slot is the drain advancing: the stall clock
/// and the head's restore streak both start over.
#[tokio::test]
async fn a_head_that_claims_the_slot_restarts_the_stall_accounting() {
    let channel = Channel::new(5_985_103);
    let (head, behind) = (5_985_241, 5_985_242);
    channel.enqueue(head).await;
    channel.enqueue(behind).await;
    for _ in 0..3 {
        channel.restore_unclaimed(head).await;
    }

    let taken = channel
        .handle
        .take_next_soft(channel.persistence.clone())
        .await;
    let promoted = taken.intervention.expect("the head must be promotable");
    let _lease = taken.dispatch_lease;
    assert!(!channel.claim(5_985_243).await, "an arrival arms the clock");
    channel
        .handle
        .age_inbound_waits_for_test(PAST_THE_WINDOW)
        .await;
    assert!(
        channel.claim(head).await,
        "the promoted head claims the slot"
    );
    channel
        .handle
        .finish_turn(channel.persistence.clone())
        .await;
    assert!(
        !channel.claim(5_985_244).await,
        "the claim restarted the window, so work queued behind it still leads"
    );

    // A busy retry hands the same message back after its turn ran.
    let retried = channel
        .handle
        .requeue_front(promoted, channel.persistence.clone());
    assert!(
        retried.await.enqueued,
        "the retried message returns to the front"
    );
    channel.round_restoring_head(head, 5_985_245).await;
    assert!(
        !channel.claim(5_985_246).await,
        "returns before the claim no longer count against the head"
    );
}

/// A force purge releases a cancelled turn's anchor the way `FinishTurn` does,
/// so the time that turn held the slot is not drain stall either.
#[tokio::test]
async fn a_force_purge_discounts_the_window_its_released_turn_held() {
    let channel = Channel::new(5_985_104);
    channel.enqueue(5_985_251).await;
    assert!(!channel.claim(5_985_252).await, "an arrival arms the clock");
    let token = Arc::new(CancelToken::new());
    let immediate = TurnAdmissionOrder::Immediate;
    let holder = channel.claim_with(token.clone(), 5_985_253, immediate);
    assert!(holder.await, "a recovery turn takes the idle slot");
    channel
        .handle
        .age_active_turn_for_test(PAST_THE_WINDOW)
        .await;
    channel
        .handle
        .age_inbound_waits_for_test(PAST_THE_WINDOW)
        .await;

    token.cancelled.store(true, Ordering::Relaxed);
    let purged = channel
        .handle
        .purge_queue(channel.persistence.clone(), true)
        .await;
    assert!(
        purged.cleared_active_anchor,
        "the force purge releases the turn"
    );
    channel.enqueue(5_985_254).await;

    assert!(
        !channel.claim(5_985_255).await,
        "the window the released turn held is discounted"
    );
}
