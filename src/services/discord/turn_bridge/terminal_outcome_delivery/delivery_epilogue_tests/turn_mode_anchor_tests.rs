//! O posts the body: an anchored turn's completion and stop remove its own placeholder, a turn-mode
//! turn sends Discord nothing for it, and a TUI binding left out of the selection holds it.
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
    #[cfg(unix)]
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
    assert_eq!(
        edited,
        Err(crate::services::discord::gateway::SYNTHETIC_MESSAGE_EDIT_ERROR.to_string())
    );
    assert!(discord.calls().is_empty(), "{:?}", discord.calls());
}

const LISTED_CHANNEL_ID: u64 = DRIVER_CHANNEL_ID + 7;

/// A boot config binding the driver channel as a Claude TUI or a pipe runtime beside a second TUI
/// binding, under the `writer` selection.
fn bound(
    driver_tui: bool,
    writer: serde_json::Value,
) -> crate::services::tui_o::channel_policy::BootChannels {
    let runtime = if driver_tui { "tui" } else { "pipe" };
    let binding = |id: u64, runtime: &str| serde_json::json!({"claude": {"id": id.to_string(), "runtime": runtime}});
    let config: crate::config::Config = serde_json::from_value(serde_json::json!({
        "server": {},
        "agents": [
            {"id": "driver", "name": "Driver", "channels": binding(DRIVER_CHANNEL_ID, runtime)},
            {"id": "listed", "name": "Listed", "channels": binding(LISTED_CHANNEL_ID, "tui")},
        ],
        "tui_o": {"writer": writer, "turn": {"all_owned": false}},
    }))
    .unwrap();
    crate::services::tui_o::channel_policy::BootChannels::validate(&config).unwrap()
}

#[derive(Debug, PartialEq)]
struct Delivered {
    body_writes: usize,
    consumed: bool,
    committed: bool,
    row_kept: bool,
    headless_sent: bool,
    /// The channel's writer alarm reasons in force, as `tui_o:<kind>:<channel>`.
    alarms: Vec<String>,
}

/// Drives the driver channel's bridge tick (W14) and terminal (W10) on a direct gateway under
/// `boot`, then offers the same channel's headless body (W17) to its claim.
async fn deliver(
    boot: crate::services::tui_o::channel_policy::BootChannels,
    kind: crate::services::agent_protocol::RuntimeHandoffKind,
) -> Delivered {
    use crate::services::tui_o::cutover::{BodyClaim, BodySend, BodySite, claim_then_send_at};
    use crate::test_support::wait::{WAIT_CAP, await_within};
    let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
    driver.inflight.runtime_kind = Some(kind);
    crate::services::discord::inflight::save_inflight_state(&driver.inflight).unwrap();
    let _boot = crate::services::tui_o::cutover::test_override::force_boot(boot);
    let (offset, full_response, inflight) =
        drive_bridge_stream_tick(&driver, driver.inflight.clone()).await;
    let (ctx, mut state) = driver.parts();
    (
        state.response_sent_offset,
        state.full_response,
        state.inflight_state,
    ) = (offset, full_response, inflight);
    let output = await_within(run_terminal_outcome_delivery(ctx, state), WAIT_CAP).await;
    let body_writes = driver
        .observations()
        .iter()
        .filter(|o| {
            matches!(
                o.call,
                DriverCall::Replace | DriverCall::Send | DriverCall::Edit
            )
        })
        .count();
    let claim = BodyClaim::new(DRIVER_CHANNEL_ID, Some(kind)).direct(false);
    let headless = claim_then_send_at(BodySite::BridgeHeadless, Some(claim), || async {}).await;
    let suffix = format!(":{DRIVER_CHANNEL_ID}");
    let alarms = crate::services::tui_o::alarm::health_reasons().into_iter();
    Delivered {
        body_writes,
        consumed: offset == DRIVER_BODY.len(),
        committed: output.terminal_delivery_committed,
        row_kept: output.preserve_inflight_for_cleanup_retry
            && crate::services::discord::inflight::load_inflight_state(
                &ProviderKind::Claude,
                DRIVER_CHANNEL_ID,
            )
            .is_some(),
        headless_sent: matches!(headless, Ok(BodySend::Sent(()))),
        alarms: alarms.filter(|reason| reason.ends_with(&suffix)).collect(),
    }
}

/// A non-TUI channel beside the selection keeps Legacy's body on every gateway, without an alarm.
fn assert_legacy(case: &str, delivered: &Delivered) {
    assert!(
        delivered.body_writes > 0,
        "{case}: Legacy writes the body: {delivered:?}"
    );
    assert!(
        delivered.committed && delivered.headless_sent && delivered.alarms.is_empty(),
        "{case}: Legacy commits and enqueues the headless body: {delivered:?}"
    );
}

/// A TUI binding outside the selection sends no Legacy body at the tick, the terminal or the
/// headless claim; its row is kept for retry and the channel is alarmed.
fn assert_held(case: &str, delivered: &Delivered) {
    let held = Delivered {
        body_writes: 0,
        consumed: false,
        committed: false,
        row_kept: true,
        headless_sent: false,
        alarms: vec![format!("tui_o:halted:{DRIVER_CHANNEL_ID}")],
    };
    assert_eq!(delivered, &held, "{case}");
}

/// The W10 funnel: a selected TUI channel's body is O's on a direct gateway with no alarm, a TUI
/// binding the list leaves out is held, and a non-TUI channel in the same config stays Legacy's.
#[tokio::test]
async fn o_delegated_tui_body_is_cut_on_direct_gateways_but_not_headless() {
    use crate::services::agent_protocol::RuntimeHandoffKind::{ClaudeTui, ProcessBackend};
    use crate::services::tui_o::channel_policy::Adoption;
    // Alarms are process-wide, so the cases run in this order in a process of their own.
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::o_delegated_tui_body_is_cut_on_direct_gateways_but_not_headless"
    )) {
        return;
    }
    let taking = crate::services::tui_o::cutover::intake_route::test_probe::answer_with(|_| true);
    let all = serde_json::json!({});
    let selected = deliver(bound(true, all).adopted(Adoption::Committed), ClaudeTui).await;
    drop(taking);
    assert!(
        selected.body_writes == 0
            && selected.consumed
            && selected.committed
            && selected.alarms.is_empty(),
        "selected: O takes the body with no halted or released alarm: {selected:?}"
    );
    let listed = || serde_json::json!({"channels": [LISTED_CHANNEL_ID]});
    assert_legacy(
        "listed, pipe",
        &deliver(bound(false, listed()), ProcessBackend).await,
    );
    assert_held(
        "listed, tui",
        &deliver(bound(true, listed()), ClaudeTui).await,
    );
}

/// The same split when the writer is explicitly off and the selection is empty.
#[tokio::test]
async fn explicitly_off_writer_holds_tui_bodies_and_keeps_legacy_for_other_channels() {
    use crate::services::agent_protocol::RuntimeHandoffKind::{ClaudeTui, ProcessBackend};
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::explicitly_off_writer_holds_tui_bodies_and_keeps_legacy_for_other_channels"
    )) {
        return;
    }
    let off = || serde_json::json!({"all_tui": false});
    assert_legacy(
        "off, pipe",
        &deliver(bound(false, off()), ProcessBackend).await,
    );
    assert_held("off, tui", &deliver(bound(true, off()), ClaudeTui).await);
}
