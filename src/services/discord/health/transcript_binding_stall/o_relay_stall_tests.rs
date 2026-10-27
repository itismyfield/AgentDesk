//! An O-owned channel's watcher frontier is not its delivery, so it never plans a watcher reattach.

use std::time::Duration;

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::inflight;
use crate::services::discord::relay_health::RelayStallState;
use crate::services::discord::relay_recovery::unread_tail_seed::{
    UnreadTailSeed, UnreadTailShape, bind_watcher,
};
use crate::services::discord::relay_recovery::{
    RelayRecoveryActionKind, RelayRecoveryApplySource, auto_apply_relay_recovery_for_shared,
    plan_relay_recovery,
};

/// The relay stall state `/api/health`'s detail publishes for `seed`'s channel.
async fn detail_relay_stall_state(seed: &UnreadTailSeed) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let detail = serde_json::to_value(seed.registry.health_snapshot_by(true, deadline).await)
        .expect("health detail serializes");
    let mailbox = detail["mailboxes"]
        .as_array()
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry["channel_id"] == seed.channel.get())
        })
        .unwrap_or_else(|| panic!("the seeded channel is in the detail: {detail}"));
    mailbox["relay_stall_state"].as_str().unwrap().to_string()
}

/// 10-26 06:37Z adk-cdx: O posted every reply while the attached watcher's frontier sat at 0 under
/// an unread tail. Recovery reattached the watcher twice, then told the user it had given up.
#[tokio::test]
async fn an_o_owned_channel_gets_no_reattach_from_a_watcher_frontier_left_at_zero() {
    let Some(seed) = UnreadTailSeed::start(7_333_501, UnreadTailShape::MeasuredBacklog).await
    else {
        return;
    };
    let channel = seed.channel;
    let output = inflight::load_inflight_state(&seed.provider, channel.get())
        .and_then(|row| row.output_path)
        .expect("the seeded row names its transcript");
    let now_ms = chrono::Utc::now().timestamp_millis();
    bind_watcher(
        &seed.shared,
        channel,
        &seed.tmux_session,
        std::path::Path::new(&output),
        now_ms,
    );
    seed.shared
        .mailbox_peek(channel)
        .expect("mailbox actor")
        .age_active_turn_for_test(Duration::from_secs(400))
        .await;
    let read = || {
        seed.registry.snapshot_watcher_state_for_shared(
            &seed.provider,
            seed.shared.clone(),
            channel.get(),
        )
    };

    let legacy = read().await.expect("watcher-state snapshot");
    let relay = &legacy.relay_health;
    assert!(
        relay.watcher_attached && !relay.watcher_attached_stale,
        "{relay:?}"
    );
    assert_eq!(relay.last_relay_offset, 0, "{relay:?}");
    assert!(
        relay.unread_bytes.is_some_and(|bytes| bytes > 0),
        "{relay:?}"
    );
    assert_eq!(
        legacy.relay_stall_state,
        RelayStallState::TmuxAliveRelayDead
    );
    let plan = plan_relay_recovery(relay, legacy.relay_stall_state, now_ms);
    assert_eq!(plan.action, RelayRecoveryActionKind::ReattachWatcher);
    assert!(
        plan.auto_heal.eligible,
        "a Legacy channel keeps its reattach"
    );
    assert_eq!(
        detail_relay_stall_state(&seed).await,
        "tmux_alive_relay_dead"
    );

    let _owned = crate::services::tui_o::cutover::test_override::force_channels(&[(
        channel.get(),
        RuntimeHandoffKind::ClaudeTui,
    )]);
    let owned = read().await.expect("watcher-state snapshot");
    assert!(
        owned.relay_health.desynced,
        "the lag itself stays published"
    );
    assert_eq!(
        owned.relay_stall_state,
        RelayStallState::ActiveForegroundStream
    );
    assert_eq!(
        owned.transcript_binding_stall, "o_unwitnessed",
        "O's own verdict still answers for the open turn"
    );
    assert_eq!(
        detail_relay_stall_state(&seed).await,
        "active_foreground_stream"
    );
    let response = auto_apply_relay_recovery_for_shared(
        &seed.registry,
        seed.shared.clone(),
        &seed.provider,
        channel.get(),
        RelayRecoveryActionKind::ReattachWatcher,
        RelayRecoveryApplySource::ProbeAutoHeal,
    )
    .await
    .expect("auto-heal answers");
    assert!(!response.applied, "{:?}", response.decision);
    assert_ne!(
        response.decision.action,
        RelayRecoveryActionKind::ReattachWatcher
    );
    assert!(seed.turn_kept());
}
