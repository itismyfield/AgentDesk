//! An O channel's Claude TUI turn from the after-Done watcher handoff to its completed panel.

use super::*;
use crate::services::discord::status_panel_singleton_store as singleton;
use crate::services::discord::turn_bridge::runtime_handoff_loop::{
    RuntimeHandoffLoopContext, RuntimeHandoffLoopMessage, RuntimeHandoffLoopState,
    handle_runtime_handoff_loop_message,
};
use crate::services::discord::turn_bridge::{
    output_lifecycle::classify_bridge_output_owner,
    watcher_handoff::should_delegate_bridge_relay_to_watcher,
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
            state_dirty: &mut dirty,
            terminal_control_drain_until: &mut drain,
            last_activity_heartbeat_at: &mut heartbeat,
        },
    )
    .await;
    assert!(owns, "the live watcher takes the session for later input");
    let pending = false;
    let delegated = should_delegate_bridge_relay_to_watcher(
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
