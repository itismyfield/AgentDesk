//! O posts the body: an anchored turn's completion and stop remove its own placeholder, and a
//! turn-mode turn, which has none, sends Discord nothing for it.
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;

async fn placeholder_deletes(anchored: bool, cancelled: bool) -> (usize, bool) {
    let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
    driver.inflight.runtime_kind = Some(ClaudeTui);
    if !anchored {
        driver.inflight.current_msg_id = 0;
    }
    crate::services::discord::inflight::save_inflight_state(&driver.inflight).unwrap();
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[(
        DRIVER_CHANNEL_ID,
        ClaudeTui,
    )]);
    let (mut ctx, mut state) = driver.parts();
    ctx.cancelled = cancelled;
    // Only the anchor's own delete is counted, not the driver's seeded replay cleanup.
    state.terminal_full_replay_cleanup_msg_ids.clear();
    if !anchored {
        ctx.current_msg_id =
            crate::services::discord::turn_bridge::current_message_anchor::detached_current_msg_id_from_durable(0);
        state.inflight_state.current_msg_id = 0;
    }
    let output = tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
        .await
        .expect("terminal outcome delivery must not hang");
    let deletes = driver
        .observations()
        .iter()
        .filter(|observed| observed.call == DriverCall::Delete)
        .count();
    let committed = output.terminal_delivery_committed || output.status_panel_terminal_committed;
    (deletes, committed)
}

#[tokio::test]
async fn an_anchorless_o_turn_completes_without_deleting_anything() {
    for cancelled in [false, true] {
        let (deletes, committed) = placeholder_deletes(false, cancelled).await;
        assert_eq!(deletes, 0, "cancelled={cancelled}: no message to delete");
        assert!(committed, "cancelled={cancelled}: the turn still settles");
    }
}

#[tokio::test]
async fn an_anchored_o_turn_still_deletes_its_own_placeholder() {
    for cancelled in [false, true] {
        let (deletes, committed) = placeholder_deletes(true, cancelled).await;
        assert_eq!(deletes, 1, "cancelled={cancelled}: the placeholder goes");
        assert!(committed, "cancelled={cancelled}");
    }
}

/// A notice on a turn-mode turn with no anchor writes nothing through the gateway, shows once as a
/// reply to the user's message, and settles the turn without a cleanup retry.
async fn anchorless_notice(prompt_too_long: bool) {
    let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
    driver.inflight.runtime_kind = Some(ClaudeTui);
    driver.inflight.current_msg_id = 0;
    crate::services::discord::inflight::save_inflight_state(&driver.inflight).unwrap();
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[(
        DRIVER_CHANNEL_ID,
        ClaudeTui,
    )]);
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(DRIVER_CHANNEL_ID);
    let discord =
        crate::services::discord::recovery_engine::o_cut_recorder::start(DRIVER_CHANNEL_ID).await;
    let _rest = crate::services::discord::shared_state::test_rest::install(discord.http.clone());
    let (mut ctx, mut state) = driver.parts();
    state.terminal_full_replay_cleanup_msg_ids.clear();
    ctx.current_msg_id =
        crate::services::discord::turn_bridge::current_message_anchor::detached_current_msg_id_from_durable(0);
    state.inflight_state.current_msg_id = 0;
    if prompt_too_long {
        state.full_response = "Error: prompt is too long".to_string();
        ctx.is_prompt_too_long = true;
    } else {
        // The rendered provider failure summary is what classifies a failed turn.
        let summary =
            crate::services::discord::turn_bridge::status_panel::provider_failure::PROVIDER_FAILURE_SUMMARY;
        state.full_response = format!("⚠️ {summary}\nthe transport closed");
        ctx.transport_error = true;
    }
    let case = if prompt_too_long {
        "prompt too long"
    } else {
        "provider failure"
    };
    let output = tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
        .await
        .expect("terminal outcome delivery must not hang");
    let writes: Vec<_> = driver
        .observations()
        .into_iter()
        .filter(|observed| {
            matches!(
                observed.call,
                DriverCall::Replace | DriverCall::Edit | DriverCall::Delete
            )
        })
        .collect();
    assert!(
        writes.is_empty(),
        "{case}: nothing aimed at a missing message: {writes:?}"
    );
    let calls = discord.calls();
    assert_eq!(calls.len(), 1, "{case}: one notice: {calls:?}");
    assert!(calls[0].route.starts_with("POST "), "{case}: {calls:?}");
    assert_eq!(
        calls[0].reply_to,
        Some(DRIVER_USER_MSG_ID.to_string()),
        "{case}: a reply to the user's message"
    );
    assert!(
        !output.preserve_inflight_for_cleanup_retry,
        "{case}: no cleanup retry for a message that never existed"
    );
    assert!(
        output.terminal_delivery_committed || output.status_panel_terminal_committed,
        "{case}: the shown notice settles the turn"
    );
}

#[tokio::test]
async fn an_anchorless_turn_mode_failure_notice_is_one_reply() {
    anchorless_notice(false).await;
}

#[tokio::test]
async fn an_anchorless_turn_mode_prompt_too_long_notice_is_one_reply() {
    anchorless_notice(true).await;
}

/// The live gateway sends no edit for a turn's in-memory absence: there is no message to edit.
#[tokio::test]
async fn the_live_gateway_sends_no_edit_for_the_sentinel() {
    let channel = 7_507_601;
    let discord = crate::services::discord::recovery_engine::o_cut_recorder::start(channel).await;
    let shared = crate::services::discord::make_shared_data_for_tests();
    let gateway = crate::services::discord::gateway::DiscordGateway::new(
        discord.http.clone(),
        shared,
        ProviderKind::Claude,
        None,
    );
    let sentinel =
        crate::services::discord::turn_bridge::current_message_anchor::detached_current_msg_id_from_durable(0);
    let edited = TurnGateway::edit_message(&gateway, ChannelId::new(channel), sentinel, "x").await;
    assert!(edited.is_err());
    assert!(discord.calls().is_empty(), "{:?}", discord.calls());
}
