use super::{CodexWrapperTurnState, RotatingJsonlWriter, emit_result_error};

pub(super) fn record(state: &mut CodexWrapperTurnState, event: &serde_json::Value) {
    if !state.saw_turn_completed {
        // Reconnection errors are provisional until the stream ends without completion.
        state.pending_error = Some(
            event
                .get("message")
                .and_then(|value| value.as_str())
                .unwrap_or("Unknown Codex error")
                .to_string(),
        );
    }
}

pub(super) fn emit_at_eof(output: &mut RotatingJsonlWriter, state: &mut CodexWrapperTurnState) {
    if state.saw_turn_completed {
        return;
    }
    if let Some(message) = state.pending_error.take() {
        emit_result_error(output, &message);
        state.saw_turn_completed = true;
    }
}
