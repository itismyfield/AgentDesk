//! The two halves of the backstop: bringing the frontier back under the rotated
//! file's EOF (L4'), and reporting a cap that has stopped being enforced (L4).

use super::*;
use crate::services::discord::inflight::RelayOwnerKind;
use std::sync::atomic::Ordering;

const CAP: u64 = crate::services::tmux_common::JSONL_SIZE_CAP_BYTES;

fn park_frontier(shared: &Arc<SharedData>, channel_id: ChannelId, confirmed: u64) {
    shared
        .tmux_relay_coord(channel_id)
        .confirmed_end_offset
        .store(confirmed, Ordering::Release);
}

/// The realignment succeeds on its first try when nothing owns the incarnation,
/// and the sticky flag is left disarmed.
#[tokio::test]
async fn a_rotation_realigns_a_stale_high_frontier_immediately() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel_id = ChannelId::new(1_479_662_682_909_970_001);
    clear_sticky_frontier_realign(channel_id);
    park_frontier(&shared, channel_id, 21_000_000);

    realign_frontier_after_rotation(
        &shared,
        channel_id,
        "AgentDesk-claude-rot-realign",
        15_000_000,
    )
    .await;

    assert!(
        shared.committed_relay_offset(channel_id) <= 15_000_000,
        "a frontier above the new EOF suppresses every range of the surviving file"
    );
    assert!(
        !sticky_is_armed(channel_id),
        "nothing is left for the per-tick retry to do"
    );
}

/// An admitted frontier mutation makes `reset_confirmed_frontier` decline. The
/// retry budget is spaced under the idle-jsonl poll interval, so a mutation that
/// clears inside it is absorbed without the sticky flag ever being armed.
#[tokio::test]
async fn a_realignment_declined_by_an_admitted_mutation_is_retried() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel_id = ChannelId::new(1_479_662_682_909_970_002);
    clear_sticky_frontier_realign(channel_id);
    park_frontier(&shared, channel_id, 21_000_000);

    let token = shared.relay_frontier_token(channel_id);
    let admission = shared
        .acquire_relay_frontier_mutation(channel_id, token)
        .expect("admission");
    let releasing = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        drop(admission);
    });

    realign_frontier_after_rotation(
        &shared,
        channel_id,
        "AgentDesk-claude-rot-admitted",
        15_000_000,
    )
    .await;
    releasing.await.expect("join");

    assert!(
        shared.committed_relay_offset(channel_id) <= 15_000_000,
        "the retry budget outlasts a short-lived admission"
    );
    assert!(!sticky_is_armed(channel_id));
}

/// The regression that keying the sticky flag on the reset's return value would
/// reintroduce. An ordinary rotation — no regression to observe — makes
/// `reset_stale_...` answer `false` for a reason that has nothing to do with
/// being declined, and a bare-bool key would arm the flag on every such rotation
/// and then never release it, because the retries keep answering `false` too.
#[tokio::test]
async fn an_ordinary_rotation_does_not_arm_the_sticky_retry() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel_id = ChannelId::new(1_479_662_682_909_970_003);
    clear_sticky_frontier_realign(channel_id);
    assert_eq!(
        shared.committed_relay_offset(channel_id),
        0,
        "a channel with no confirmed delivery yet"
    );

    realign_frontier_after_rotation(
        &shared,
        channel_id,
        "AgentDesk-claude-rot-ordinary",
        15_000_000,
    )
    .await;

    assert!(
        !sticky_is_armed(channel_id),
        "there was no regression, so there is nothing to keep retrying"
    );
}

/// An admission held past the whole retry budget arms the sticky flag, and the
/// per-tick retry then closes the window once the admission clears.
///
/// It is driven at a tick that is NOT a rotation tick, which is the assertion
/// that matters: the retry block sits beside the cadence branch, not inside it.
/// Moved inside, this window would stay open for a rotation cadence — about
/// sixty idle-jsonl polls, each able to consume up to a mebibyte without sending
/// it and with no path that re-reads it.
#[tokio::test]
async fn a_sticky_realignment_retries_on_a_tick_that_is_not_a_rotation_tick() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel_id = ChannelId::new(1_479_662_682_909_970_004);
    let session = "AgentDesk-claude-rot-sticky";
    clear_sticky_frontier_realign(channel_id);
    park_frontier(&shared, channel_id, 21_000_000);

    let token = shared.relay_frontier_token(channel_id);
    let admission = shared
        .acquire_relay_frontier_mutation(channel_id, token)
        .expect("admission");
    realign_frontier_after_rotation(&shared, channel_id, session, 15_000_000).await;
    assert!(
        sticky_is_armed(channel_id),
        "the budget ran out with the frontier still high"
    );
    assert!(shared.committed_relay_offset(channel_id) > 15_000_000);

    // A tick the rotation cadence would skip. While the admission is still held
    // the retry cannot succeed, and the flag must survive to try again.
    let off_cadence = 1;
    assert_ne!(off_cadence % ROTATION_CHECK_EVERY, 0);
    let (_, _, _, rotated) = rotate_watcher_jsonl_if_due(
        off_cadence,
        "/nonexistent/agentdesk-sticky.jsonl",
        session,
        0,
        None,
        None,
        &shared,
        channel_id,
        &ProviderKind::Claude,
        true,
    )
    .await;
    assert!(!rotated, "no rotation happens off the cadence");
    assert!(sticky_is_armed(channel_id), "still declined, still armed");

    drop(admission);
    let (_, _, _, _) = rotate_watcher_jsonl_if_due(
        off_cadence,
        "/nonexistent/agentdesk-sticky.jsonl",
        session,
        0,
        None,
        None,
        &shared,
        channel_id,
        &ProviderKind::Claude,
        true,
    )
    .await;
    assert!(
        shared.committed_relay_offset(channel_id) <= 15_000_000,
        "the off-cadence retry is what closes the window"
    );
    assert!(!sticky_is_armed(channel_id));
}

/// A frontier above `new_size` is not always the stale-high this rotation left. One
/// of the other resetters can get there first, against the file as it reads after the
/// rewrite, and delivery then advances from where that left it. Re-applying this
/// rotation's `new_size` there would walk the frontier back over ranges already sent,
/// which the second relay path would send again.
#[tokio::test]
async fn a_frontier_realigned_by_another_resetter_is_not_rewound() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel_id = ChannelId::new(1_479_662_682_909_970_005);
    let session = "AgentDesk-claude-rot-realigned-elsewhere";
    clear_sticky_frontier_realign(channel_id);
    park_frontier(&shared, channel_id, 21_000_000);

    // Armed the only way production can arm it: an admission held past the whole
    // retry budget.
    let admission = shared
        .acquire_relay_frontier_mutation(channel_id, shared.relay_frontier_token(channel_id))
        .expect("admission");
    realign_frontier_after_rotation(&shared, channel_id, session, 15_000_000).await;
    assert!(
        sticky_is_armed(channel_id),
        "the budget ran out with the frontier still high"
    );
    drop(admission);

    // Somebody else realigns, and delivery advances from where they landed.
    assert!(
        reset_stale_relay_watermark_if_output_regressed(
            &shared,
            channel_id,
            session,
            16_000_000,
            "test_other_resetter",
        ),
        "the regression is still observable, so the other resetter's reset lands"
    );
    park_frontier(&shared, channel_id, 16_000_000);

    retry_sticky_frontier_realign(&shared, channel_id, session);

    assert_eq!(
        shared.committed_relay_offset(channel_id),
        16_000_000,
        "a frontier realigned after the rotation must not be pulled back to its new_size"
    );
    assert!(
        !sticky_is_armed(channel_id),
        "the stale-high this rotation published is gone, so the retry is finished"
    );
}

/// The same release predicate owed to the rotation's own retry loop, whose resets sit
/// on the far side of a 25 ms sleep. Another resetter that realigns inside that window
/// leaves a frontier measured after the rewrite, and the reset waiting on the other
/// side of the sleep would put this rotation's `new_size` back over it.
#[tokio::test]
async fn a_frontier_realigned_during_a_retry_sleep_is_not_rewound() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel_id = ChannelId::new(1_479_662_682_909_970_006);
    let session = "AgentDesk-claude-rot-realigned-mid-retry";
    clear_sticky_frontier_realign(channel_id);
    park_frontier(&shared, channel_id, 21_000_000);

    // Declines the rotation's own attempts, so the loop is still spending its budget
    // when the other resetter gets there.
    let admission = shared
        .acquire_relay_frontier_mutation(channel_id, shared.relay_frontier_token(channel_id))
        .expect("admission");
    let other = Arc::clone(&shared);
    let realigning = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        // No await between these three, so on this single-threaded runtime the retry
        // loop cannot land a reset between the admission clearing and delivery
        // advancing — what the loop observes on its next wake is the finished state.
        drop(admission);
        assert!(
            reset_stale_relay_watermark_if_output_regressed(
                &other,
                channel_id,
                session,
                16_000_000,
                "test_other_resetter",
            ),
            "the regression is still observable, so the other resetter's reset lands"
        );
        park_frontier(&other, channel_id, 16_000_000);
    });

    realign_frontier_after_rotation(&shared, channel_id, session, 15_000_000).await;
    realigning.await.expect("join");

    assert_eq!(
        shared.committed_relay_offset(channel_id),
        16_000_000,
        "a frontier realigned during a retry sleep must not be pulled back to new_size"
    );
    assert!(
        !sticky_is_armed(channel_id),
        "the loop stops on the same evidence that would release the sticky flag"
    );
}

fn sticky_is_armed(channel_id: ChannelId) -> bool {
    STICKY_FRONTIER_REALIGN
        .lock()
        .expect("lock")
        .contains_key(&channel_id)
}

/// The ladder speaks at multiples of the cap and not per tick, and what it says
/// is fields: how long the run has gone on, which term is producing it, and how
/// far past the cap the file has grown. Without the term an operator learns only
/// that rotation is not happening, with nothing to aim at.
#[test]
fn the_refusal_ladder_reports_the_sticky_term_at_each_rung() {
    let mut ladder = RotationRefusalLadder::default();

    assert_eq!(
        advance_rotation_refusal_ladder(&mut ladder, RotationBusyTerm::PendingBuffer, CAP, CAP),
        None,
        "one cap over is not a rung"
    );

    let warn = advance_rotation_refusal_ladder(
        &mut ladder,
        RotationBusyTerm::RelayOwner(RelayOwnerKind::Watcher),
        CAP * 2,
        CAP,
    )
    .expect("twice the cap warns");
    assert_eq!(warn.level, RotationRefusalLevel::Warn);
    assert_eq!(warn.consecutive_refusals, 2);
    assert_eq!(warn.last_term, "relay_owner:watcher");

    assert_eq!(
        advance_rotation_refusal_ladder(
            &mut ladder,
            RotationBusyTerm::RelayOwner(RelayOwnerKind::Watcher),
            CAP * 3,
            CAP,
        ),
        None,
        "a rung fires once per run, not on every tick past it"
    );

    let error = advance_rotation_refusal_ladder(
        &mut ladder,
        RotationBusyTerm::EmissionInFlight,
        CAP * 5,
        CAP,
    )
    .expect("five times the cap errors");
    assert_eq!(error.level, RotationRefusalLevel::Error);
    assert_eq!(error.consecutive_refusals, 4);
    assert_eq!(error.size_bytes, CAP * 5);
    assert_eq!(
        error.dominant_term, "relay_owner:watcher",
        "the run's most-refused term, not merely whichever landed last"
    );
    assert_eq!(
        error.last_term, "emission_in_flight",
        "the last term is carried too, so a run that changed character is legible"
    );
}

/// A file that shoots past both rungs at once reports the more severe one, and
/// does not then also report the milder one on the next refusal.
#[test]
fn crossing_both_rungs_at_once_reports_only_the_error() {
    let mut ladder = RotationRefusalLadder::default();
    let alarm =
        advance_rotation_refusal_ladder(&mut ladder, RotationBusyTerm::FdRefusal, CAP * 6, CAP)
            .expect("past both rungs");
    assert_eq!(alarm.level, RotationRefusalLevel::Error);
    assert_eq!(
        advance_rotation_refusal_ladder(&mut ladder, RotationBusyTerm::FdRefusal, CAP * 7, CAP),
        None,
        "both rungs are spent"
    );
}
