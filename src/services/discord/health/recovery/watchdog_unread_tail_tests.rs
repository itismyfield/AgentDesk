//! #5996 P-L2a (I20) production-entry test for the explicit-background watchdog
//! force-clean, the third decision site that reads `unread_bytes` as destructive
//! permission. Real tmux, real `run_stall_watchdog_pass`; skipped (NO VERDICT)
//! without tmux. PR-L2a-1 changes no decision: the row pins today's refusal and
//! the record it now leaves.

use crate::services::discord::relay_health::RelayStallState;
use crate::services::discord::relay_recovery::UNREAD_TAIL_SITE_WATCHDOG_EXPLICIT_BACKGROUND;
use crate::services::discord::relay_recovery::unread_tail_seed::{UnreadTailSeed, UnreadTailShape};

/// T4(n1): an owned, quiet explicit-background turn whose transcript is missing
/// arms every force-clean conjunct but the tail. The watchdog keeps the turn (as
/// before) and records the UNMEASURED refusal once across passes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_background_watchdog_records_an_unmeasured_tail_refusal_once() {
    let Some(seed) =
        UnreadTailSeed::start(5_996_130_001, UnreadTailShape::RowOutputMissing, true).await
    else {
        return;
    };
    let channel_id = seed.channel.get();
    let snapshot = seed
        .registry
        .snapshot_watcher_state_for_provider(&seed.provider, channel_id)
        .await
        .expect("fixture snapshot");
    assert_eq!(
        snapshot.relay_stall_state,
        RelayStallState::ExplicitBackgroundWork
    );
    assert_eq!(snapshot.unread_bytes, None);
    let now = chrono::Utc::now().timestamp();
    let force_clean = |unread_bytes| {
        super::stall_watchdog_should_force_clean_orphan_explicit_background_work(
            snapshot.relay_stall_state,
            snapshot.attached,
            snapshot.watcher_owner_channel_id,
            channel_id,
            snapshot.desynced,
            snapshot.inflight_state_present,
            snapshot.inflight_updated_at.as_deref(),
            snapshot.tmux_session_alive,
            unread_bytes,
            snapshot.relay_health.last_outbound_activity_ms,
            now,
            super::STALL_WATCHDOG_THRESHOLD_SECS,
        )
    };
    assert!(
        force_clean(Some(0)),
        "the fixture must arm every conjunct but the tail"
    );
    assert!(
        !force_clean(None),
        "an UNMEASURED tail refuses the force-clean"
    );

    for _ in 0..2 {
        super::super::run_stall_watchdog_pass(&seed.registry, &seed.provider).await;
        assert!(
            seed.turn_kept(),
            "the explicit-background turn must survive"
        );
    }
    let refusals = seed.refusals();
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert_eq!(
        refusals[0]["site"],
        UNREAD_TAIL_SITE_WATCHDOG_EXPLICIT_BACKGROUND
    );
    assert_eq!(refusals[0]["decided_by"], "tail_not_measured");
    assert_eq!(refusals[0]["watcher_attached"], true);
    assert_eq!(refusals[0]["tmux_alive"], true);

    // Only the tail as the refusing conjunct is recorded: a fresh episode whose
    // outbound activity is still recent refuses on age and records nothing.
    let mut fresh_episode = snapshot.clone();
    fresh_episode.mailbox_active_user_msg_id = Some(1);
    let recent = fresh_episode
        .relay_health
        .last_outbound_activity_ms
        .unwrap()
        / 1_000;
    assert!(!super::explicit_background_force_clean_decided(
        &seed.provider,
        channel_id,
        &fresh_episode,
        recent,
    ));
    assert_eq!(seed.refusals().len(), 1, "{:?}", seed.refusals());
}
