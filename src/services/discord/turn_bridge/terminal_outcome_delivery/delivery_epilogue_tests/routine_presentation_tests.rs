use super::*;
use serde_json::json;

fn pin(row: &mut InflightTurnState, terminal: &str) {
    let mut encoded = serde_json::to_value(&*row).unwrap();
    encoded["routine_presentation"] = json!({"policy":"claude_process_c1", "terminal":terminal});
    *row = serde_json::from_value(encoded).unwrap();
    row.tmux_session_name = None;
    row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ProcessBackend);
    crate::services::discord::inflight::save_inflight_state(row).unwrap();
}

#[tokio::test]
async fn i8117_no_reply_settles_without_body_banner_or_retry() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let body = "진행1\n진행2\n진행3\n진행4\nNO_REPLY";
    let mut driver =
        TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0).with_body(body.into());
    pin(&mut driver.inflight, "no_reply");
    let (ctx, state) = driver.parts();
    let output = crate::test_support::wait::await_within(
        run_terminal_outcome_delivery(ctx, state),
        crate::test_support::wait::WAIT_CAP,
    )
    .await;
    assert!(output.terminal_delivery_committed);
    assert!(
        driver.published_bodies.lock().unwrap().is_empty(),
        "NO_REPLY must make no body call"
    );
    assert!(driver.observations().iter().all(|o| !matches!(
        o.call,
        DriverCall::Send | DriverCall::Edit | DriverCall::Replace
    )));
}

#[tokio::test]
async fn i8117_non_no_reply_and_unknown_evidence_preserve_full_body() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    for terminal in ["full", "missing", "ambiguous"] {
        let body = "보고 메시지\n[tool]\n완료: 인용 `NO_REPLY`는 정상 결과입니다.";
        let mut driver =
            TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0).with_body(body.into());
        pin(&mut driver.inflight, terminal);
        assert!(
            driver
                .shared
                .ui
                .placeholder_live_events
                .set_session_panel_lifecycle_event(
                    ChannelId::new(DRIVER_CHANNEL_ID),
                    Some("fixture-session-nonce"),
                    "session_fresh",
                    &json!({"provider_session_id":"fixture-provider-session","tmux_reused":false}),
                )
        );
        let (ctx, state) = driver.parts();
        let output = crate::test_support::wait::await_within(
            run_terminal_outcome_delivery(ctx, state),
            crate::test_support::wait::WAIT_CAP,
        )
        .await;
        assert!(output.terminal_delivery_committed);
        assert_eq!(
            *driver.published_bodies.lock().unwrap(),
            vec![body.to_string()]
        );
    }
}

#[test]
fn i8117_restart_preserves_policy_evidence_and_raw_coordinates() {
    let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
    driver.inflight.full_response = "진행\nNO_REPLY".into();
    driver.inflight.turn_start_offset = Some(1234);
    driver.inflight.last_offset = 5678;
    driver.inflight.response_sent_offset = 3;
    pin(&mut driver.inflight, "no_reply");
    let raw = serde_json::to_value(&driver.inflight).unwrap();
    assert_eq!(
        raw["routine_presentation"],
        json!({"policy":"claude_process_c1","terminal":"no_reply"})
    );
    let restored: InflightTurnState = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(restored.full_response, "진행\nNO_REPLY");
    assert_eq!(restored.turn_start_offset, Some(1234));
    assert_eq!(restored.last_offset, 5678);
    assert_eq!(restored.response_sent_offset, 3);
    let mut legacy = raw;
    legacy
        .as_object_mut()
        .unwrap()
        .remove("routine_presentation");
    let legacy: InflightTurnState = serde_json::from_value(legacy).unwrap();
    assert_eq!(
        serde_json::to_value(legacy).unwrap()["routine_presentation"]["policy"],
        "legacy"
    );
}

#[tokio::test]
async fn i8117_no_visible_respects_existing_owner_and_raw_range() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    for other_owner in [false, true] {
        let body = "한글 진행문\nNO_REPLY";
        let mut driver =
            TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0).with_body(body.into());
        pin(&mut driver.inflight, "no_reply");
        driver.inflight.turn_start_offset = Some(120);
        driver.inflight.last_offset = 9876;
        driver.inflight.tmux_session_name = Some(DRIVER_TMUX_SESSION.into());
        crate::services::discord::inflight::save_inflight_state(&driver.inflight).unwrap();
        let channel = ChannelId::new(DRIVER_CHANNEL_ID);
        let key = bridge_delivery_lease_key_for_inflight(
            channel,
            driver.shared.restart.current_generation,
            &driver.inflight,
        );
        let cell = driver.shared.delivery_lease(channel);
        if other_owner {
            assert!(cell.try_acquire(
                key,
                crate::services::discord::LeaseHolder::Watcher { instance_id: 8117 },
                120,
                9876,
                crate::services::discord::lease_now_ms() + 60_000
            ));
        }
        let before = format!("{:?}", cell.read());
        let (mut ctx, state) = driver.parts();
        ctx.tmux_last_offset = Some(9876);
        let output = crate::test_support::wait::await_within(
            run_terminal_outcome_delivery(ctx, state),
            crate::test_support::wait::WAIT_CAP,
        )
        .await;
        assert_eq!(output.terminal_delivery_committed, !other_owner);
        assert_eq!(output.preserve_inflight_for_cleanup_retry, other_owner);
        assert_eq!(output.full_response, body);
        assert_eq!(output.inflight_state.turn_start_offset, Some(120));
        assert_eq!(output.inflight_state.last_offset, 9876);
        assert!(driver.published_bodies.lock().unwrap().is_empty());
        if other_owner {
            assert_eq!(
                format!("{:?}", cell.read()),
                before,
                "another holder's range stays unchanged"
            );
            assert_eq!(output.response_sent_offset, 0);
        } else {
            assert_eq!(
                output.response_sent_offset,
                body.len(),
                "consume original raw bytes"
            );
        }
    }
}
