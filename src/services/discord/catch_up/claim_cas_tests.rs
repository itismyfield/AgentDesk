//! A claim after the classifying snapshot (same actor, fresh actor, or past the
//! retained window) refuses the enqueue instead of requeueing taken work.

use super::absorbed_active_tests::absorb_and_claim;
use super::*;
use crate::services::turn_orchestrator::registry_purge::MailboxPurgeOutcome;

const CLAIMED: Option<EnqueueRefusalReason> = Some(EnqueueRefusalReason::ClaimedSinceObservation);

/// `primary`'s merged head absorbs `absorbed` and claims, then its turn ends —
/// with a ledger row for `primary` when `delivered`.
pub(super) async fn absorb_and_end(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    absorbed: MessageId,
    primary: MessageId,
    delivered: bool,
) {
    absorb_and_claim(shared, provider, channel_id, &[absorbed], primary).await;
    if delivered {
        completed_turn_ledger::append_completed_turn(provider, channel_id.get(), primary.get());
    }
    discord::mailbox_finish_turn(shared, provider, channel_id).await;
}

pub(super) async fn purge(shared: &Arc<SharedData>, channel_id: ChannelId) {
    let outcome = shared.mailboxes.remove_idle_entry(channel_id).await;
    assert!(
        matches!(outcome, MailboxPurgeOutcome::Removed),
        "{outcome:?}"
    );
}

async fn claim_and_finish(fx: &Fixture, channel_id: ChannelId, message_id: MessageId) {
    let token = Arc::new(crate::services::provider::CancelToken::new());
    let owner = serenity::UserId::new(HUMAN_ID);
    let started = discord::mailbox_try_start_turn(&fx.shared, channel_id, token, owner, message_id);
    assert!(started.await, "{message_id} claims");
    discord::mailbox_finish_turn(&fx.shared, &fx.provider, channel_id).await;
}

async fn observed(fx: &Fixture, channel_id: ChannelId) -> ClaimObservation {
    discord::mailbox_snapshot(&fx.shared, channel_id)
        .await
        .claim_observation
}

async fn enqueue_seen(
    fx: &Fixture,
    channel_id: ChannelId,
    message_id: MessageId,
    seen: ClaimObservation,
) -> Option<EnqueueRefusalReason> {
    let intervention = queued_intervention(message_id, 7);
    let outcome = observed_enqueue(
        &fx.shared,
        &fx.provider,
        channel_id,
        intervention,
        Some(seen),
    );
    let outcome = outcome.await;
    assert_eq!(outcome.enqueued, outcome.refusal_reason.is_none());
    outcome.refusal_reason
}

async fn queued_ids(fx: &Fixture, channel_id: ChannelId) -> Vec<Vec<MessageId>> {
    let snapshot = discord::mailbox_snapshot(&fx.shared, channel_id).await;
    let queue = snapshot.intervention_queue.iter();
    queue
        .map(|entry| entry.source_message_ids.clone())
        .collect()
}

fn race_history(
    channel_id: ChannelId,
    ids: (MessageId, MessageId, MessageId),
) -> Vec<serenity::Message> {
    let (checkpoint, h, p) = ids;
    vec![
        own_reply(channel_id, checkpoint),
        human(channel_id, h),
        human(channel_id, p),
    ]
}

/// Phase 1 refuses the race's ids on its stale snapshot; phase 2 re-classifies
/// them from a fresh one and enqueues `rerun`. A later retry offers nothing.
async fn assert_refused_then_reclassified(
    fx: &Fixture,
    api: &StrictApi,
    (channel_id, h, p): (ChannelId, MessageId, MessageId),
    rerun: &[MessageId],
    queued: &[&[MessageId]],
) {
    let mut expected = vec![(h.get(), false, CLAIMED), (p.get(), false, CLAIMED)];
    expected.extend(rerun.iter().map(|id| (id.get(), true, None)));
    assert_eq!(api.enqueue_log(), expected);
    let queue = queued_ids(fx, channel_id).await;
    assert_eq!(
        queue,
        queued.iter().map(|ids| ids.to_vec()).collect::<Vec<_>>()
    );

    let history = race_history(channel_id, (id(1, 600), h, p));
    let retry = StrictApi::new(&fx.shared).with_history(channel_id, history);
    fx.retry_sweep(&retry, channel_id).await;
    assert_eq!(accepted(&retry), Vec::<u64>::new(), "offered twice");
}

/// P absorbs, claims and delivers unknown H before H's enqueue lands: P is never
/// requeued; H is re-offered once (alias settlement is out of scope).
#[tokio::test(flavor = "current_thread")]
async fn t_f2a_delivered_race_is_refused_and_p_settles_on_its_row() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_561);
    let ids @ (checkpoint, h, p) = (id(1, 600), id(2, 200), id(3, 120));
    fx.seed_checkpoint(channel_id, checkpoint);
    let delivered = Hook::AbsorbAndEnd {
        primary: p,
        delivered: true,
    };
    let api = StrictApi::new(&fx.shared)
        .with_history(channel_id, race_history(channel_id, ids))
        .with_hooks(h, &[delivered]);

    fx.sweep(&api).await;

    assert_refused_then_reclassified(&fx, &api, (channel_id, h, p), &[h], &[&[h]]).await;
}

/// The same race, but P ends undelivered; the fresh snapshot recovers
/// both ids instead of refusing them again.
#[tokio::test(flavor = "current_thread")]
async fn t_f2b_undelivered_race_is_recovered_from_a_fresh_snapshot() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_562);
    let ids @ (checkpoint, h, p) = (id(1, 600), id(2, 200), id(3, 120));
    fx.seed_checkpoint(channel_id, checkpoint);
    let undelivered = Hook::AbsorbAndEnd {
        primary: p,
        delivered: false,
    };
    let api = StrictApi::new(&fx.shared)
        .with_history(channel_id, race_history(channel_id, ids))
        .with_hooks(h, &[undelivered]);

    fx.sweep(&api).await;

    assert_refused_then_reclassified(&fx, &api, (channel_id, h, p), &[h, p], &[&[h, p]]).await;
}

/// After a purge, fresh actor B claims from seq 0: A's observation must not
/// pass on B, and a fresh snapshot of B recovers both ids.
#[tokio::test(flavor = "current_thread")]
async fn t_f6_an_observation_of_a_purged_actor_refuses_on_its_successor() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_563);
    let ids @ (checkpoint, h, p) = (id(1, 600), id(3, 200), id(4, 120));
    fx.seed_checkpoint(channel_id, checkpoint);
    claim_and_finish(&fx, channel_id, id(2, 900)).await;
    let api = StrictApi::new(&fx.shared)
        .with_history(channel_id, race_history(channel_id, ids))
        .with_hooks(h, &[Hook::PurgeAbsorbAndEnd(p)]);

    fx.sweep(&api).await;

    assert_refused_then_reclassified(&fx, &api, (channel_id, h, p), &[h, p], &[&[h, p]]).await;
}

/// A snapshot that saw no actor trusts any actor minted after it — unless a
/// purge on the channel means an intermediate actor may have claimed.
#[tokio::test(flavor = "current_thread")]
async fn no_actor_observation_refuses_across_a_purge() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_564);
    let (h, p, x) = (id(2, 200), id(3, 120), id(4, 100));
    let seen = observed(&fx, channel_id).await;
    absorb_and_end(&fx.shared, &fx.provider, channel_id, h, p, true).await;
    purge(&fx.shared, channel_id).await;

    assert_eq!(enqueue_seen(&fx, channel_id, x, seen).await, CLAIMED);
    let fresh = observed(&fx, channel_id).await;
    assert_eq!(enqueue_seen(&fx, channel_id, x, fresh).await, None);
}

/// More claims since the snapshot than the actor retains refuses even
/// an unrelated id; exactly the retained window still decides by overlap.
#[tokio::test(flavor = "current_thread")]
async fn t_f2c_claims_past_the_retained_window_refuse_the_enqueue() {
    let fx = Fixture::new().await;
    let cap = crate::services::turn_orchestrator::RECENT_CLAIMS_CAP as u64;
    for (channel_id, claims, refusal) in [
        (ChannelId::new(4_603_565), cap, None),
        (ChannelId::new(4_603_568), cap + 1, CLAIMED),
    ] {
        claim_and_finish(&fx, channel_id, id(1, 900)).await;
        let seen = observed(&fx, channel_id).await;
        for seq in 0..claims {
            claim_and_finish(&fx, channel_id, id(100 + seq, 800)).await;
        }
        let outcome = enqueue_seen(&fx, channel_id, id(50, 700), seen).await;
        assert_eq!(outcome, refusal, "{claims} claims since the snapshot");
    }
}

/// A snapshot taken before a rebind of the absorbing turn must
/// see the rebind's claim cover the absorbed id, not just its primary.
#[tokio::test(flavor = "current_thread")]
async fn snapshot_before_a_rebind_is_refused_for_the_absorbed_id() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_566);
    let (h, p) = (id(2, 200), id(3, 120));
    absorb_and_claim(&fx.shared, &fx.provider, channel_id, &[h], p).await;
    let seen = observed(&fx, channel_id).await;
    let token = Arc::new(crate::services::provider::CancelToken::new());
    let owner = serenity::UserId::new(HUMAN_ID);
    fx.shared
        .mailbox(channel_id)
        .restore_active_turn(token, owner, p)
        .await;
    discord::mailbox_finish_turn(&fx.shared, &fx.provider, channel_id).await;

    assert_eq!(enqueue_seen(&fx, channel_id, h, seen).await, CLAIMED);
}

/// A restore onto an idle actor is a claim too: an older snapshot cannot
/// requeue the restored id, but unrelated work still passes (no wedge).
#[tokio::test(flavor = "current_thread")]
async fn snapshot_before_a_restore_is_refused_for_the_restored_id() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_567);
    let (p, x) = (id(3, 120), id(4, 100));
    claim_and_finish(&fx, channel_id, id(1, 900)).await;
    let seen = observed(&fx, channel_id).await;
    let token = Arc::new(crate::services::provider::CancelToken::new());
    let owner = serenity::UserId::new(HUMAN_ID);
    fx.shared
        .mailbox(channel_id)
        .restore_active_turn(token, owner, p)
        .await;
    discord::mailbox_finish_turn(&fx.shared, &fx.provider, channel_id).await;

    assert_eq!(enqueue_seen(&fx, channel_id, p, seen).await, CLAIMED);
    assert_eq!(enqueue_seen(&fx, channel_id, x, seen).await, None);
}

/// The undelivered race at the actor: P absorbed H and ended undelivered before the snapshot;
/// only claims after it count, so an unrelated one does not refuse H again.
#[tokio::test(flavor = "current_thread")]
async fn an_unrelated_claim_since_the_snapshot_does_not_refuse_again() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_569);
    let (h, p, q) = (id(2, 200), id(3, 120), id(4, 100));
    absorb_and_end(&fx.shared, &fx.provider, channel_id, h, p, false).await;
    let seen = observed(&fx, channel_id).await;
    claim_and_finish(&fx, channel_id, q).await;

    assert_eq!(enqueue_seen(&fx, channel_id, h, seen).await, None);
}
