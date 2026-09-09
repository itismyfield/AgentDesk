//! #5191 A1 precursor P1: the monitor auto-turn synthetic-inflight upsert, moved
//! out of `tmux.rs`. Arm ordering, log lines and the persisted payload are all
//! unchanged; the new `Option` is a create receipt that every caller discards.
use super::*;
use crate::services::discord::{inflight, mailbox_snapshot};

#[allow(clippy::too_many_arguments)]
pub(super) async fn ensure_monitor_auto_turn_inflight(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    output_path: &str,
    input_fifo_path: &str,
    session_id: Option<&str>,
    turn_start_offset: u64,
    last_offset: u64,
) -> Option<inflight::InflightTurnIdentity> {
    if inflight::load_inflight_state(provider, channel_id.get()).is_some() {
        return None;
    }

    let channel_name = parse_provider_and_channel_from_tmux_name(tmux_session_name)
        .map(|(_, channel_name)| channel_name);
    let mut synthetic = inflight::InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        channel_name,
        0,
        0,
        0,
        "Monitor auto-turn".to_string(),
        session_id.map(str::to_string),
        Some(tmux_session_name.to_string()),
        Some(output_path.to_string()),
        Some(input_fifo_path.to_string()),
        last_offset,
    );
    synthetic.turn_nonce = mailbox_snapshot(shared, channel_id).await.active_turn_nonce;
    synthetic.turn_start_offset = Some(turn_start_offset);
    synthetic = build_monitor_triggered_inflight_state(synthetic);
    // #2285 audit trail: monitor pattern fired this turn without an
    // originating Discord message. The session-bound relay does NOT branch
    // on this — recorded for diagnostics only.
    // status-panel-v2: make this watcher-owned so the panel-eligibility
    // predicate (watcher_inflight_is_panel_eligible_for_session) recognises the
    // synthetic monitor/self-paced-loop turn and the watcher can create/update/
    // clean up a live status panel for it. The shared external-input predicate
    // (lease + ⏳ anchor lifecycle, #3164/#3174) stays untouched.

    match inflight::save_inflight_state_create_new(&synthetic) {
        Ok(()) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Registered synthetic inflight for monitor auto-turn in channel {}",
                channel_id.get()
            );
            Some(inflight::InflightTurnIdentity::from_state(&synthetic))
        }
        Err(inflight::CreateNewInflightError::AlreadyExists) => None,
        Err(inflight::CreateNewInflightError::Internal(error)) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ Failed to register synthetic monitor inflight for channel {}: {}",
                channel_id.get(),
                error
            );
            None
        }
    }
}
#[cfg(test)]
mod monitor_auto_turn_inflight_tests {
    include!("monitor_auto_turn_inflight_tests.rs");
}
