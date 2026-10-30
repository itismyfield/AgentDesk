//! A follow-up read that fails mid-run hands the live run back through the actual TmuxReady arm.

use super::*;
use crate::services::provider::FollowupResult;
use crate::services::session_backend::ReadOutputFailure;
use std::sync::atomic::Ordering;

/// The Codex/Qwen read-error fallback for a read from `start_offset` failing at `failed_at`, sent
/// as their `TmuxReady`; returns the claim that replaced the paused `incumbent`.
pub(in crate::services::discord) async fn hand_failed_followup_read_to_watcher_for_test(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &mut InflightTurnState,
    (tmux_session_name, output_path, input_fifo_path): (&str, &str, &str),
    (start_offset, failed_at): (u64, u64),
    incumbent_cancel: &Arc<AtomicBool>,
) -> WatcherClaimIncarnation {
    let channel = ChannelId::new(state.channel_id);
    let incumbent = shared
        .tmux_watchers
        .get(&channel)
        .expect("attached T0 watcher");
    assert!(Arc::ptr_eq(&incumbent.cancel, incumbent_cancel));
    assert!(
        incumbent.paused.load(Ordering::Acquire),
        "turn start paused it"
    );
    drop(incumbent);
    let failure = ReadOutputFailure::new("Failed to read output file", failed_at, false);
    let mut handed = None;
    let recovered = failure.recover_followup(|failure| {
        let fallback = crate::services::provider::tmux_followup_fallback_after_read_error(
            start_offset,
            failure.last_offset,
            std::fs::metadata(output_path).ok().map(|meta| meta.len()),
            crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_session_name),
            false,
            std::fs::metadata(output_path).is_ok(),
            std::fs::metadata(input_fifo_path).is_ok(),
        )
        .expect("a live session keeps the watcher fallback");
        assert!(!fallback.emit_synthetic_done, "the run is still going");
        handed = Some(fallback.last_offset);
        Ok(FollowupResult::Delivered)
    });
    assert_eq!(recovered, Ok(FollowupResult::Delivered));
    let last_offset = handed.expect("the fallback chose a resume offset");
    assert_eq!(last_offset, failed_at, "resumed where the read failed");
    let message = RuntimeHandoffLoopMessage::TmuxReady {
        output_path: output_path.to_string(),
        input_fifo_path: input_fifo_path.to_string(),
        tmux_session_name: tmux_session_name.to_string(),
        last_offset,
    };
    let mut dirty = false;
    let observed =
        dispatch_process_handoff(shared, provider, state, message, &mut dirty, false).await;
    assert_eq!(observed.outcome, Some(GuardedSaveOutcome::Saved));
    assert_eq!(
        observed.claim_outcome,
        WatcherHandoffClaimOutcome::Spawned,
        "a paused incumbent is replaced, never resumed into the adopted run"
    );
    assert!(observed.tmux_handed_off && observed.watcher_relay_available);
    assert!(
        incumbent_cancel.load(Ordering::Acquire),
        "the claim cancelled the T0 watcher"
    );
    let pin = observed
        .watcher_delivery_pin
        .expect("TmuxReady adopted its successor");
    assert!(!Arc::ptr_eq(&pin.cancel, incumbent_cancel));
    assert!(!pin.paused.load(Ordering::Acquire), "the successor runs");
    pin
}
