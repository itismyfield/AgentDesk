//! An O channel's Claude TUI turn from the after-Done watcher handoff to its completed panel,
//! and the real bridge's terminal owner for each order the producer's frames can arrive in.

use super::*;
use crate::services::discord::status_panel_singleton_store as singleton;
use crate::services::discord::turn_bridge::runtime_handoff_loop::{
    RuntimeHandoffLoopContext, RuntimeHandoffLoopMessage, RuntimeHandoffLoopState,
    handle_runtime_handoff_loop_message,
};
use crate::services::discord::turn_bridge::{
    output_lifecycle::classify_bridge_output_owner,
    watcher_handoff::{o_body_needs_bridge_terminal, should_delegate_bridge_relay_to_watcher},
};
use crate::services::tui_o::{cutover::test_override, writer::deliver};
use RuntimeHandoffKind::ClaudeTui;

const PANEL: u64 = 4_000_000;
const LATE_BODY: u64 = 4_500_000;
const REST_BASE: u64 = 4_600_000;

struct SeparatePanel;

impl Drop for SeparatePanel {
    fn drop(&mut self) {
        crate::services::discord::turn_bridge::single_message_footer::SEPARATE_PANEL_FOR_TESTS
            .set(false);
    }
}

/// The real handoff after Done, then the bridge decision it leaves: (delegated, claim outcome).
async fn hand_off_after_done(
    driver: &TerminalDeliveryDriver,
    state: &mut TerminalOutcomeDeliveryState,
) -> (bool, WatcherHandoffClaimOutcome) {
    let channel_id = ChannelId::new(DRIVER_CHANNEL_ID);
    let transcript = driver
        ._temp
        .path()
        .join("driver.jsonl")
        .display()
        .to_string();
    let message = RuntimeHandoffLoopMessage::RuntimeReady {
        handoff: crate::services::agent_protocol::RuntimeHandoff::ClaudeTui {
            transcript_path: transcript,
            tmux_session_name: DRIVER_TMUX_SESSION.to_string(),
            last_offset: 64,
        },
    };
    let (mut ready, mut tmux_last_offset, mut owner) = (false, None, channel_id);
    let (mut standby, mut available, mut pin) = (false, false, None);
    let (mut claim, mut handed_off, mut owns) = (WatcherHandoffClaimOutcome::None, false, false);
    let mut adopted_after_done = false;
    let (mut dirty, mut drain, mut heartbeat) = (false, None, None);
    let _ = handle_runtime_handoff_loop_message(
        message,
        RuntimeHandoffLoopContext {
            shared_owned: &driver.shared,
            provider: &ProviderKind::Claude,
            channel_id,
            done: true,
            adk_session_name: &None,
        },
        RuntimeHandoffLoopState {
            terminal_control_ready_observed: &mut ready,
            tmux_last_offset: &mut tmux_last_offset,
            inflight_state: &mut state.inflight_state,
            watcher_owner_channel_id: &mut owner,
            standby_relay_owns_output: &mut standby,
            watcher_relay_available_for_turn: &mut available,
            watcher_delivery_pin: &mut pin,
            watcher_handoff_claim_outcome: &mut claim,
            tmux_handed_off: &mut handed_off,
            watcher_owns_assistant_relay: &mut owns,
            watcher_adopted_after_done: &mut adopted_after_done,
            state_dirty: &mut dirty,
            terminal_control_drain_until: &mut drain,
            last_activity_heartbeat_at: &mut heartbeat,
        },
    )
    .await;
    assert!(owns, "the live watcher takes the session for later input");
    let pending = false;
    let direct = state.gateway.can_deliver_directly();
    let delegated = !o_body_needs_bridge_terminal(
        adopted_after_done,
        &state.full_response,
        channel_id,
        &state.inflight_state,
        direct,
    ) && should_delegate_bridge_relay_to_watcher(
        owns, available, pending, false, false, false, false,
    );
    (delegated, claim)
}

/// One turn whose body O consumed and posts after completion: the final panel and the REST log.
async fn finish_turn(headless: bool) -> (TerminalDeliveryDriver, u64, Option<Vec<(String, u64)>>) {
    crate::services::discord::turn_bridge::single_message_footer::SEPARATE_PANEL_FOR_TESTS
        .set(true);
    let _separate = SeparatePanel;
    let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
    let ui = &mut Arc::get_mut(&mut driver.shared).expect("fresh driver").ui;
    (ui.status_panel_v2_enabled, ui.two_message_panel_enabled) = (true, true);
    driver.inflight.runtime_kind = Some(ClaudeTui);
    driver.inflight.status_message_id = Some(PANEL);
    driver.inflight.full_response = driver.body.clone();
    inflight::save_inflight_state(&driver.inflight).expect("seed the two-message row");
    let (token, channel) = (driver.shared.token_hash.clone(), DRIVER_CHANNEL_ID);
    singleton::bind_if_owned(&ProviderKind::Claude, &token, channel, PANEL, None).unwrap();
    let _mailbox = driver.shared.mailbox(ChannelId::new(channel));
    let _o = test_override::force_channels(&[(channel, ClaudeTui)]);
    let _posted = deliver::forget_posted_for_tests(channel);
    let rest = if headless {
        Some(
            crate::services::discord::shared_state::test_rest::recording_mock(REST_BASE, channel)
                .await,
        )
    } else {
        None
    };

    let (mut ctx, mut state) = driver.parts();
    state.response_sent_offset = state.full_response.len();
    if headless {
        state.gateway = Arc::new(crate::services::discord::gateway::HeadlessGateway);
    }
    let (delegated, claim) = hand_off_after_done(&driver, &mut state).await;
    let owner = classify_bridge_output_owner(false, delegated);
    (
        ctx.bridge_relay_delegated_to_watcher,
        ctx.bridge_output_owner,
    ) = (delegated, owner);
    ctx.watcher_handoff_claim_outcome = claim;
    let output = run(ctx, state).await;
    assert!(
        output.terminal_delivery_committed,
        "the bridge ends the turn"
    );
    run_postlude_for_owner(&driver, output, false, false, owner).await;
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let path = inflight::inflight_state_path(&root, &ProviderKind::Claude, channel);
    let _ = std::fs::remove_file(path);
    deliver::note_posted_for_tests(channel, LATE_BODY);

    let panel =
        || singleton::load(&ProviderKind::Claude, &token, channel).map(|b| b.panel_message_id);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while panel() == Some(PANEL) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let moved = panel().expect("a singleton panel");
    // Let the follow's window end while this test still holds the runtime root.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let log = rest.map(|(log, _guard)| log.lock().unwrap().clone());
    (driver, moved, log)
}

/// A direct gateway: the completed panel ends below O's later body and the bridge posts no body.
#[tokio::test]
async fn o_turn_after_done_handoff_completes_below_o_body_on_a_direct_gateway() {
    let (driver, moved, _) = finish_turn(false).await;
    assert!(
        moved > LATE_BODY,
        "panel {moved} stays above O's body {LATE_BODY}"
    );
    let posted = driver.published_bodies.lock().unwrap().clone();
    assert!(
        !posted.is_empty() && posted.iter().all(|text| !text.contains(DRIVER_BODY)),
        "{posted:?}"
    );
}

/// An API-injected turn: the completed panel is edited, re-posted below O's body and the old one
/// deleted over bot REST, and the channel's panel is that real message.
#[tokio::test]
async fn o_turn_after_done_handoff_completes_below_o_body_on_a_headless_gateway() {
    let (_driver, moved, log) = finish_turn(true).await;
    let log = log.expect("REST log");
    assert!(
        moved > LATE_BODY,
        "panel {moved} stays above O's body {LATE_BODY}"
    );
    assert!(
        !crate::services::discord::is_synthetic_headless_message_id_raw(moved),
        "{moved}"
    );
    let posts: Vec<u64> = log
        .iter()
        .filter(|(m, _)| m == "POST")
        .map(|r| r.1)
        .collect();
    assert_eq!(posts, vec![moved], "{log:?}");
    assert!(log.contains(&("PATCH".into(), PANEL)), "{log:?}");
    assert!(log.contains(&("DELETE".into(), PANEL)), "{log:?}");
}

const ORDER_BODY: &str = "[E2E:E1:OK]";

#[derive(Clone, Copy, Debug, PartialEq)]
enum Frame {
    Text,
    Done,
    Ready,
}

/// Runs the real bridge over `drains`, each sent once the previous one is saved (its text, else its
/// handoff), and returns the bridge's completion signal and the bodies it posted.
async fn bridge_turn(o_owns: bool, drains: &[&[Frame]]) -> (BridgeCompletionSignal, Vec<String>) {
    use crate::services::discord::turn_bridge::{TurnBridgeContext, spawn_turn_bridge};
    let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
    let channel_id = ChannelId::new(DRIVER_CHANNEL_ID);
    driver.inflight.runtime_kind = Some(ClaudeTui);
    inflight::save_inflight_state(&driver.inflight).expect("seed the Claude TUI row");
    let selected = [(DRIVER_CHANNEL_ID, ClaudeTui)];
    let _o = test_override::force_channels(if o_owns { &selected[..] } else { &[] });
    // A connected bot, so the adopted watcher's ownership is what the handoff persists.
    let _gateway =
        crate::services::discord::turn_bridge::runtime_handoff_loop::test_gateway::connect();
    let _rest = crate::services::discord::shared_state::test_rest::recording_mock(
        REST_BASE,
        DRIVER_CHANNEL_ID,
    )
    .await;
    let transcript = driver
        ._temp
        .path()
        .join("driver.jsonl")
        .display()
        .to_string();
    let frame = |frame: Frame| match frame {
        Frame::Text => StreamMessage::Text {
            content: ORDER_BODY.to_string(),
        },
        Frame::Done => StreamMessage::Done {
            result: String::new(),
            session_id: None,
        },
        Frame::Ready => StreamMessage::RuntimeReady {
            handoff: crate::services::agent_protocol::RuntimeHandoff::ClaudeTui {
                transcript_path: transcript.clone(),
                tmux_session_name: DRIVER_TMUX_SESSION.to_string(),
                last_offset: 64,
            },
        },
    };
    let cancel = Arc::new(CancelToken::new());
    let user_msg = MessageId::new(DRIVER_USER_MSG_ID);
    assert!(
        crate::services::discord::mailbox_try_start_turn(
            &driver.shared,
            channel_id,
            cancel.clone(),
            UserId::new(1),
            user_msg,
        )
        .await
    );
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    let bridge = TurnBridgeContext {
        provider: ProviderKind::Claude,
        gateway: driver.gateway.clone(),
        channel_id,
        user_msg_id: Some(user_msg),
        user_text_owned: "driver prompt".to_string(),
        request_owner_name: String::new(),
        role_binding: None,
        adk_session_key: None,
        adk_session_name: None,
        adk_session_info: None,
        adk_cwd: None,
        dispatch_id: None,
        dispatch_kind: None,
        memory_recall_usage: TokenUsage::default(),
        context_window_tokens: 0,
        context_compact_percent: 0,
        current_msg_id: Some(MessageId::new(DRIVER_CURRENT_MSG_ID)),
        response_sent_offset: 0,
        full_response: String::new(),
        tmux_last_offset: None,
        new_session_id: None,
        defer_watcher_resume: false,
        reuse_status_panel_message: false,
        completion_tx: Some(completion_tx),
        is_external_input_tui_direct: false,
        inflight_state: driver.inflight.clone(),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    for message in drains[0] {
        tx.send(frame(*message)).unwrap();
    }
    spawn_turn_bridge(driver.shared.clone(), cancel, rx, bridge);
    for (previous, drain) in drains.iter().zip(&drains[1..]) {
        let saved = |row: InflightTurnState| {
            if previous.contains(&Frame::Text) {
                row.full_response == ORDER_BODY
            } else {
                row.effective_relay_owner_kind() == inflight::RelayOwnerKind::Watcher
            }
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !inflight::load_inflight_state(&ProviderKind::Claude, DRIVER_CHANNEL_ID)
            .is_some_and(saved)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "{previous:?} never saved"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        for message in *drain {
            tx.send(frame(*message)).unwrap();
        }
    }
    drop(tx);
    let signal = tokio::time::timeout(std::time::Duration::from_secs(10), completion_rx)
        .await
        .expect("the bridge ends")
        .expect("completion signal");
    let posted = driver.published_bodies.lock().unwrap().clone();
    (signal, posted)
}

/// On O's channel a watcher adopted after Done leaves the bridge the terminal and O the body,
/// whether the text drains alone or with Done; Legacy posts its own body as before.
#[tokio::test]
async fn o_turn_keeps_the_bridge_terminal_in_either_frame_order() {
    use Frame::{Done, Ready, Text};
    let posts_body = |posted: &[String]| posted.iter().any(|text| text.contains(ORDER_BODY));
    for (order, drains) in [
        ("live", &[&[Text][..], &[Done, Ready][..]][..]),
        ("batch", &[&[Text, Done, Ready][..]][..]),
    ] {
        let (signal, posted) = bridge_turn(true, drains).await;
        assert_eq!(signal, BridgeCompletionSignal::Finalized, "{order}");
        assert!(!posts_body(&posted), "{order}: {posted:?}");
    }
    let (signal, posted) = bridge_turn(false, &[&[Text], &[Done, Ready]]).await;
    assert_eq!(signal, BridgeCompletionSignal::Finalized, "legacy");
    assert!(posts_body(&posted), "legacy: {posted:?}");
}

/// On O's channel a watcher that already relayed the turn before Done still ends it, and so does
/// the watcher of a turn with no text, which has no O body behind it.
#[tokio::test]
async fn o_turn_without_an_after_done_body_stays_delegated() {
    use Frame::{Done, Ready, Text};
    for (case, drains) in [
        ("relayed", &[&[Ready][..], &[Text, Done, Ready][..]][..]),
        ("empty", &[&[Done, Ready][..]][..]),
    ] {
        let (signal, posted) = bridge_turn(true, drains).await;
        assert_eq!(signal, BridgeCompletionSignal::Unresolved, "{case}");
        assert!(
            posted.iter().all(|text| !text.contains(ORDER_BODY)),
            "{case}: {posted:?}"
        );
    }
}
