//! #6035 — ids a claimed merged head absorbed are held while its turn runs and
//! re-offered once it ends: an absorbing turn is neither evidence nor a leap.

use super::*;
use crate::services::discord::recovery_known_ids::{
    RecoveryKnownIdArm, recovery_known_arms_and_ids,
};

/// A real merge (`absorbed` first, newer `primary` folds it in), then its claim.
pub(super) async fn absorb_and_claim(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    absorbed: &[MessageId],
    primary: MessageId,
) {
    for (index, message_id) in absorbed.iter().chain([&primary]).enumerate() {
        let mut intervention = queued_intervention(*message_id, index);
        intervention.merge_consecutive = true;
        let outcome =
            discord::mailbox_enqueue_intervention(shared, provider, channel_id, intervention);
        assert!(super::super::super::catch_up_enqueue_accepted(
            &outcome.await
        ));
    }
    let taken = discord::mailbox_take_next_soft_intervention(shared, provider, channel_id).await;
    let (head, _, _lease) = taken.into_intervention().expect("the merged head is taken");
    let mut sources = absorbed.to_vec();
    sources.push(primary);
    assert_eq!(
        (head.message_id, head.source_message_ids),
        (primary, sources)
    );
    let token = Arc::new(crate::services::provider::CancelToken::new());
    let owner = serenity::UserId::new(HUMAN_ID);
    let started = discord::mailbox_try_start_turn(shared, channel_id, token, owner, primary);
    assert!(started.await, "the merged head claims the turn");
}

async fn enqueue(
    fx: &Fixture,
    channel_id: ChannelId,
    message_id: MessageId,
) -> MailboxEnqueueOutcome {
    let intervention = queued_intervention(message_id, 9);
    discord::mailbox_enqueue_intervention(&fx.shared, &fx.provider, channel_id, intervention).await
}

const ABSORBED: Option<EnqueueRefusalReason> = Some(EnqueueRefusalReason::AbsorbedByActiveTurn);

/// Phase 1 commit: H's enqueue races P's claim, so `NotYetEvidenced` seals and
/// P's active-turn evidence cannot carry the checkpoint past H.
#[tokio::test(flavor = "current_thread")]
async fn phase1_absorbed_refusal_seals_and_holds_the_checkpoint() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_541);
    let (checkpoint, h, p) = (id(1, 600), id(2, 150), id(3, 120));
    fx.seed_checkpoint(channel_id, checkpoint);
    let history = vec![
        own_reply(channel_id, checkpoint),
        human(channel_id, h),
        human(channel_id, p),
    ];
    let api = StrictApi::new(&fx.shared)
        .with_history(channel_id, history)
        .with_hooks(h, &[Hook::AbsorbInto(p)]);

    fx.sweep(&api).await;

    assert_phase1_read(&api, after(checkpoint), &[h, p]);
    let active = Some(EnqueueRefusalReason::AlreadyActiveTurn);
    assert_eq!(
        api.enqueue_log()[..2],
        [(h.get(), false, ABSORBED), (p.get(), false, active)]
    );
    let held = (Some(checkpoint.get()), Some(checkpoint.get()));
    assert_eq!(fx.surfaces(channel_id), held);
    let retry = fx
        .pending(channel_id)
        .expect("the seal keeps a barrier retry");
    assert_eq!(retry.checkpoint, checkpoint.get());
}

/// Phase 2 commit: H only reaches the phase-2 page; its `NotYetEvidenced` must
/// not move the scan cursor, so a later defer's retry still rereads H.
#[tokio::test(flavor = "current_thread")]
async fn phase2_absorbed_refusal_keeps_the_retry_cursor_before_h() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_542);
    let (checkpoint, h, n, p) = (id(1, 400), id(2, 150), id(3, 130), id(4, 120));
    fx.seed_checkpoint(channel_id, checkpoint);
    let api = StrictApi::new(&fx.shared)
        .with_history(channel_id, vec![own_reply(channel_id, checkpoint)])
        .arriving_at(1, human(channel_id, h))
        .arriving_at(1, human(channel_id, n))
        .arriving_at(1, human(channel_id, p))
        .with_hooks(h, &[Hook::AbsorbInto(p)])
        .with_hooks(n, &[Hook::Defer]);

    fx.sweep(&api).await;

    let deferred = (n.get(), false, Some(EnqueueRefusalReason::ActorUnreachable));
    assert_eq!(api.enqueue_log(), [(h.get(), false, ABSORBED), deferred]);
    let held = (Some(checkpoint.get()), Some(checkpoint.get()));
    assert_eq!(fx.surfaces(channel_id), held);
    let retry = fx.pending(channel_id).expect("the deferred N arms a retry");
    assert!(
        retry.checkpoint < h.get(),
        "retry {} passed H {h}",
        retry.checkpoint
    );
}

/// T-F1a: H < X < P. P's claim absorbs H before H's enqueue and X defers; H
/// stays below the retry, and once P ends undelivered the retry recovers H.
#[tokio::test(flavor = "current_thread")]
async fn t_f1a_undelivered_absorbing_turn_does_not_lose_h() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_543);
    let (checkpoint, h, x, p) = (id(1, 600), id(2, 200), id(3, 150), id(4, 120));
    fx.seed_checkpoint(channel_id, checkpoint);
    let history = vec![
        own_reply(channel_id, checkpoint),
        human(channel_id, h),
        human(channel_id, x),
        human(channel_id, p),
    ];
    let first = StrictApi::new(&fx.shared)
        .with_history(channel_id, history.clone())
        .with_hooks(h, &[Hook::AbsorbInto(p)])
        .with_hooks(x, &[Hook::Defer, Hook::Defer]);

    fx.sweep(&first).await;

    assert_eq!(first.enqueue_log()[0], (h.get(), false, ABSORBED));
    let held = (Some(checkpoint.get()), Some(checkpoint.get()));
    assert_eq!(fx.surfaces(channel_id), held);
    let retry = fx
        .pending(channel_id)
        .expect("the deferred X keeps a retry");
    assert!(
        retry.checkpoint < h.get(),
        "retry {} passed H {h}",
        retry.checkpoint
    );

    discord::mailbox_finish_turn(&fx.shared, &fx.provider, channel_id).await;
    let second = StrictApi::new(&fx.shared).with_history(channel_id, history);
    fx.retry_sweep(&second, channel_id).await;
    assert!(
        accepted(&second).contains(&h.get()),
        "{:?}",
        second.enqueue_log()
    );
}

/// T-F3a: a head absorbing 257 ids still names its oldest one; no ring evicts it.
#[tokio::test(flavor = "current_thread")]
async fn t_f3a_every_id_of_a_wide_merged_head_is_absorbed() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_544);
    let absorbed: Vec<MessageId> = (2..259).map(|seq| id(seq, 200)).collect();
    let primary = id(300, 100);
    absorb_and_claim(&fx.shared, &fx.provider, channel_id, &absorbed, primary).await;

    let snapshot = discord::mailbox_snapshot(&fx.shared, channel_id).await;
    let arm = recovery_known_arms_and_ids(&snapshot)
        .0
        .get(&absorbed[0].get())
        .copied();
    assert_eq!(arm, Some(RecoveryKnownIdArm::AbsorbedActiveTurn));
    let outcome = enqueue(&fx, channel_id, absorbed[0]).await;
    assert_eq!(
        (outcome.enqueued, outcome.refusal_reason),
        (false, ABSORBED)
    );
}

/// T-rel: release clears the absorbed set, so the next turn does not refuse H.
#[tokio::test(flavor = "current_thread")]
async fn t_rel_release_forgets_the_absorbed_ids() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_545);
    let (h, p, q) = (id(2, 200), id(3, 150), id(4, 120));
    absorb_and_claim(&fx.shared, &fx.provider, channel_id, &[h], p).await;
    discord::mailbox_finish_turn(&fx.shared, &fx.provider, channel_id).await;
    let token = Arc::new(crate::services::provider::CancelToken::new());
    let owner = serenity::UserId::new(HUMAN_ID);
    assert!(discord::mailbox_try_start_turn(&fx.shared, channel_id, token, owner, q).await);

    let outcome = enqueue(&fx, channel_id, h).await;
    assert!(outcome.enqueued, "{:?}", outcome.refusal_reason);
}

/// T-F5 (r8 repro): P's first episode was delivered; its second absorbs H and
/// ends undelivered. The earlier ledger row must not settle H.
#[tokio::test(flavor = "current_thread")]
async fn t_f5_an_earlier_episode_row_does_not_settle_a_later_absorption() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_546);
    let (checkpoint, h, p) = (id(1, 600), id(3, 150), id(4, 120));
    fx.seed_checkpoint(channel_id, checkpoint);
    let token = Arc::new(crate::services::provider::CancelToken::new());
    let owner = serenity::UserId::new(HUMAN_ID);
    assert!(discord::mailbox_try_start_turn(&fx.shared, channel_id, token, owner, p).await);
    completed_turn_ledger::append_completed_turn(&fx.provider, channel_id.get(), p.get());
    discord::mailbox_finish_turn(&fx.shared, &fx.provider, channel_id).await;
    absorb_and_claim(&fx.shared, &fx.provider, channel_id, &[h], p).await;
    discord::mailbox_finish_turn(&fx.shared, &fx.provider, channel_id).await;
    let history = vec![
        own_reply(channel_id, checkpoint),
        human(channel_id, h),
        human(channel_id, p),
    ];
    let api = StrictApi::new(&fx.shared).with_history(channel_id, history);

    fx.sweep(&api).await;

    assert!(accepted(&api).contains(&h.get()), "{:?}", api.enqueue_log());
}

/// A2: a long absorbing turn plus a repeatedly deferred N exhausts the rearm
/// budget; H is never leapt, and the periodic backstop recovers it afterwards.
#[tokio::test(flavor = "current_thread")]
async fn a2_exhausted_rearm_budget_leaves_h_to_the_periodic_backstop() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_547);
    let (checkpoint, h, n, p) = (id(1, 600), id(2, 200), id(3, 150), id(4, 120));
    fx.seed_checkpoint(channel_id, checkpoint);
    absorb_and_claim(&fx.shared, &fx.provider, channel_id, &[h], p).await;
    let history = vec![
        own_reply(channel_id, checkpoint),
        human(channel_id, h),
        human(channel_id, n),
        human(channel_id, p),
    ];
    let defers = [Hook::Defer; 64];
    let api = StrictApi::new(&fx.shared)
        .with_history(channel_id, history.clone())
        .with_hooks(n, &defers);
    fx.sweep(&api).await;
    let mut retries = 0;
    while fx.pending(channel_id).is_some() {
        retries += 1;
        assert!(
            retries <= 2 * CATCH_UP_RETRY_DEFERRED_REARM_LIMIT,
            "budget never exhausted"
        );
        fx.retry_sweep(&api, channel_id).await;
        let held = (Some(checkpoint.get()), Some(checkpoint.get()));
        assert_eq!(fx.surfaces(channel_id), held, "retry {retries} leapt H");
    }
    assert!(retries > 1, "the long turn outlived several retries");
    assert!(
        !accepted(&api).contains(&h.get()),
        "H ran twice during its turn"
    );

    discord::mailbox_finish_turn(&fx.shared, &fx.provider, channel_id).await;
    let backstop = StrictApi::new(&fx.shared).with_history(channel_id, history);
    fx.sweep(&backstop).await;
    assert_phase1_read(&backstop, after(checkpoint), &[h]);
    assert!(
        accepted(&backstop).contains(&h.get()),
        "{:?}",
        backstop.enqueue_log()
    );
}
