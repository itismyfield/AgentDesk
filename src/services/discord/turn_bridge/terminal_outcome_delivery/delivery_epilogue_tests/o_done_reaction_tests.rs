//! A turn-mode turn whose answer O posts marks its input done at its own end, queued first or not;
//! a stopped turn never shows done, and a Legacy channel keeps its own rule.
#![cfg(unix)]

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::discord::queue_reactions::QUEUE_STANDALONE_PENDING_REACTION;
use crate::services::discord::turn_view_reconciler::{
    note_intake_queue_marker_added_current, note_intake_turn_started_current,
};

/// A user message id the reaction view treats as a real Discord message.
const USER: u64 = 1_430_000_000_000_008_152;

/// Ends a path-less Herdr-style row with no input receipt (O: anchorless turn mode, else anchored
/// Legacy); returns the reactions left on the user's message and every emoji ever added to it.
async fn end_turn(o: bool, queued: bool, cancelled: bool) -> (Vec<char>, Vec<char>) {
    let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
    driver.inflight.runtime_kind = Some(ClaudeTui);
    driver.inflight.user_msg_id = USER;
    if o {
        driver.inflight.current_msg_id = 0;
    }
    assert_eq!(driver.inflight.output_path, None);
    inflight::save_inflight_state(&driver.inflight).expect("seed the Claude TUI row");
    let selected = [(DRIVER_CHANNEL_ID, ClaudeTui)];
    let selected = if o { &selected[..] } else { &[] };
    let _o = crate::services::tui_o::cutover::test_override::force_channels(selected);
    let _turn =
        o.then(|| crate::services::tui_o::turn_mode::TestConfirmation::new(DRIVER_CHANNEL_ID));
    let (channel, user) = (ChannelId::new(DRIVER_CHANNEL_ID), MessageId::new(USER));
    let http = Arc::new(serenity::http::Http::new("test-token"));
    if queued {
        let marker = QUEUE_STANDALONE_PENDING_REACTION;
        let shared = &driver.shared;
        assert!(
            note_intake_queue_marker_added_current(shared, &http, channel, user, marker, "q").await
        );
    }
    assert!(note_intake_turn_started_current(&driver.shared, &http, channel, user, "start").await);
    let (mut ctx, mut state) = driver.parts();
    ctx.cancelled = cancelled;
    ctx.user_msg_id = Some(user);
    if o {
        ctx.current_msg_id =
            crate::services::discord::turn_bridge::current_message_anchor::detached_current_msg_id_from_durable(0);
        state.inflight_state.current_msg_id = 0;
    }
    state.terminal_full_replay_cleanup_msg_ids.clear();
    tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
        .await
        .expect("terminal outcome delivery must not hang");
    let ops: Vec<_> = (driver.shared.turn_view_reconciler.ops().into_iter())
        .filter(|op| op.target.message_id == user)
        .collect();
    let mut visible = std::collections::BTreeSet::new();
    for op in &ops {
        if op.add {
            visible.insert(op.emoji);
        } else {
            visible.remove(&op.emoji);
        }
    }
    let added = ops.iter().filter(|op| op.add).map(|op| op.emoji).collect();
    (visible.into_iter().collect(), added)
}

#[tokio::test]
async fn an_o_turn_that_ends_on_its_own_marks_its_input_done() {
    for queued in [false, true] {
        let (visible, _) = end_turn(true, queued, false).await;
        assert_eq!(visible, ['✅'], "queued={queued}: done, no hourglass left");
    }
}

#[tokio::test]
async fn a_stopped_o_turn_never_marks_its_input_done() {
    for queued in [false, true] {
        let (visible, added) = end_turn(true, queued, true).await;
        assert!(!added.contains(&'✅'), "queued={queued}: {added:?}");
        assert!(!visible.contains(&'⏳'), "queued={queued}: {visible:?}");
    }
}

#[tokio::test]
async fn a_legacy_turn_without_an_input_receipt_stays_unmarked() {
    let (_, added) = end_turn(false, false, false).await;
    assert!(!added.contains(&'✅'), "{added:?}");
}
