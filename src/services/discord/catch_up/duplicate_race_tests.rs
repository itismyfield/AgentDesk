//! Phase-2 duplicate commits raced in after the scan snapshot; the snapshot
//! membership path is covered in `classification_order_tests` and `absorbed_active_tests`.

use super::absorbed_active_tests::phase2_only;
use super::*;
use crate::services::discord::catch_up::frontier_evidence::FrontierEvidence;
use crate::services::discord::catch_up::phase2::{Phase2EnqueueCommit, Phase2Frontier};

const LAST_BOT_REPLY: u64 = 90;

/// T13: the commit policy alone. A queued refusal keeps the checkpoint and
/// seals at its id, so a later recovery cannot carry `durable()` past it.
#[test]
fn t13_only_the_active_turn_duplicate_commit_advances() {
    let (b, m, later) = (100, 150, 160);
    let queued = Phase2EnqueueCommit::DuplicateQueued;
    let active = Phase2EnqueueCommit::DuplicateActiveTurn;
    assert_eq!(FrontierEvidence::of_commit(queued), FrontierEvidence::Open);

    let mut frontier = Phase2Frontier::new(None, LAST_BOT_REPLY);
    assert_eq!(frontier.after_duplicate_commit(queued, Some(b), m), Some(b));
    assert_eq!(frontier.retry_cursor(Some(b)), b);
    frontier.recovered(later);
    assert_eq!(frontier.durable(), None, "a recovery leapt the queued id");

    let mut frontier = Phase2Frontier::new(None, LAST_BOT_REPLY);
    assert_eq!(frontier.after_duplicate_commit(active, Some(b), m), Some(m));
    frontier.recovered(later);
    assert_eq!(frontier.durable(), Some(later));

    let mut frontier = Phase2Frontier::new(None, LAST_BOT_REPLY);
    assert_eq!(frontier.after_duplicate_commit(queued, None, m), None);
    assert_eq!(frontier.retry_cursor(None), LAST_BOT_REPLY);

    let mut frontier = Phase2Frontier::new(None, LAST_BOT_REPLY);
    let max = u64::MAX;
    assert_eq!(
        frontier.after_duplicate_commit(active, Some(max), m),
        Some(max)
    );
}

/// Phase 1 reads nothing past B; `m` and `n` reach the phase-2 page alone.
fn assert_phase2_page(api: &StrictApi, b: MessageId, page: &[MessageId]) {
    let reads: Vec<_> = (api.fetch_log().into_iter())
        .map(|(_, request, returned)| (request, returned))
        .collect();
    let page = page.iter().map(|id| id.get()).collect();
    let phase2 = (CatchUpFetchRequest::new(20), page);
    assert_eq!(reads, [(after(b), Vec::new()), phase2]);
}

/// T14: M is queued after the phase-2 snapshot, so the real enqueue refuses it
/// as `SourceIdAlreadyQueued`; N's defer must retry from B itself, not M − 1.
#[tokio::test(flavor = "current_thread")]
async fn t14_phase2_queued_race_refusal_keeps_the_retry_at_b() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_605_114);
    let (b, m, n) = (id(1, 400), id(2, 150), id(3, 130));
    fx.seed_checkpoint(channel_id, b);
    let api = phase2_only(&fx, channel_id, &[m, n])
        .with_hooks(m, &[Hook::PreQueue])
        .with_hooks(n, &[Hook::Defer]);

    fx.sweep(&api).await;

    assert_phase2_page(&api, b, &[n, m, b]);
    let queued = (
        m.get(),
        false,
        Some(EnqueueRefusalReason::SourceIdAlreadyQueued),
    );
    let deferred = (n.get(), false, Some(EnqueueRefusalReason::ActorUnreachable));
    assert_eq!(api.enqueue_log(), [queued, deferred]);
    assert_eq!(fx.surfaces(channel_id), (Some(b.get()), Some(b.get())));
    let retry = fx.pending(channel_id).expect("the deferred N arms a retry");
    assert_eq!(retry.checkpoint, b.get(), "retry must stay at B (M = {m})");
}

/// T15: M's own turn is claimed first, so the real `AlreadyActiveTurn` moves the
/// retry to M; not every duplicate commit holds the checkpoint.
#[tokio::test(flavor = "current_thread")]
async fn t15_phase2_active_turn_race_refusal_advances_the_retry_to_m() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_605_115);
    let (b, m, n) = (id(1, 400), id(2, 150), id(3, 130));
    fx.seed_checkpoint(channel_id, b);
    let api = phase2_only(&fx, channel_id, &[m, n])
        .with_hooks(m, &[Hook::StartTurn])
        .with_hooks(n, &[Hook::Defer]);

    fx.sweep(&api).await;

    assert_phase2_page(&api, b, &[n, m, b]);
    let active = (
        m.get(),
        false,
        Some(EnqueueRefusalReason::AlreadyActiveTurn),
    );
    let deferred = (n.get(), false, Some(EnqueueRefusalReason::ActorUnreachable));
    assert_eq!(api.enqueue_log(), [active, deferred]);
    assert_eq!(fx.surfaces(channel_id), (Some(b.get()), Some(b.get())));
    let retry = fx.pending(channel_id).expect("the deferred N arms a retry");
    assert_eq!(retry.checkpoint, m.get());
}
