use super::*;

pub(super) fn refresh_stream_tick_expected_identity_after_handoff(
    expected: &mut crate::services::discord::inflight::InflightTurnIdentity,
    persisted_baseline: &mut InflightTurnState,
    inflight_state: &InflightTurnState,
    guarded_save_outcome: Option<crate::services::discord::inflight::GuardedSaveOutcome>,
) {
    if matches!(
        guarded_save_outcome,
        Some(crate::services::discord::inflight::GuardedSaveOutcome::Saved)
    ) {
        *expected =
            crate::services::discord::inflight::InflightTurnIdentity::from_state(inflight_state);
        if inflight_state.save_generation > persisted_baseline.save_generation {
            persisted_baseline.clone_from(inflight_state);
            return;
        }
        persisted_baseline.runtime_kind = inflight_state.runtime_kind;
        persisted_baseline
            .tmux_session_name
            .clone_from(&inflight_state.tmux_session_name);
        persisted_baseline
            .output_path
            .clone_from(&inflight_state.output_path);
        persisted_baseline
            .input_fifo_path
            .clone_from(&inflight_state.input_fifo_path);
        persisted_baseline
            .session_id
            .clone_from(&inflight_state.session_id);
        persisted_baseline.last_offset = inflight_state.last_offset;
        persisted_baseline.watcher_owner_channel_id = inflight_state.watcher_owner_channel_id;
        persisted_baseline.set_relay_owner_kind(inflight_state.effective_relay_owner_kind());
    }
}
