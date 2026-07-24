use super::*;

pub(super) async fn resolve_watcher_dispatch_id(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    inflight_state: Option<&super::super::inflight::InflightTurnState>,
) -> Option<String> {
    inflight_state
        .and_then(|state| state.dispatch_id.clone())
        .or_else(|| {
            inflight_state
                .and_then(|state| super::super::adk_session::parse_dispatch_id(&state.user_text))
        })
        .or(
            super::super::adk_session::lookup_pending_dispatch_for_thread(
                shared.api_port,
                channel_id.get(),
            )
            .await,
        )
        .or_else(|| {
            resolve_dispatched_thread_dispatch_from_db(shared.pg_pool.as_ref(), channel_id.get())
        })
}

pub(super) fn should_suppress_terminal_output_after_recent_stop(
    has_assistant_response: bool,
    inflight_missing: bool,
    recent_turn_stop: bool,
) -> bool {
    should_suppress_streaming_placeholder_after_recent_stop(
        has_assistant_response,
        inflight_missing,
        recent_turn_stop,
    )
}

pub(super) fn should_suppress_streaming_placeholder_after_recent_stop(
    has_assistant_response: bool,
    inflight_missing: bool,
    recent_turn_stop: bool,
) -> bool {
    has_assistant_response && inflight_missing && recent_turn_stop
}

pub(super) fn should_skip_streaming_placeholder_without_inflight(
    inflight_missing: bool,
    pane_actively_streaming: bool,
) -> bool {
    // #3107: a live agentic TUI turn can lose its inflight mid-turn (a momentary
    // idle observation between tool calls commits and clears it). When the pane
    // is still actively producing assistant output, the missing inflight is a
    // self-heal opportunity, NOT a signal to suppress — dropping the edit here
    // is exactly the relay-degradation bug. Only suppress when inflight is
    // missing AND the pane looks finished/idle (genuine post-finish ghost noise
    // like provider-selector chrome).
    inflight_missing && !pane_actively_streaming
}

#[allow(clippy::too_many_arguments)]
pub(super) fn should_suppress_post_terminal_output_without_inflight(
    terminal_success_seen: bool,
    inflight_missing: bool,
    ssh_direct_prompt_pending: bool,
    external_input_lease_present: bool,
    assistant_continuation_present: bool,
    pane_actively_streaming: bool,
    pending_synthetic_start_present: bool,
) -> bool {
    // SSH-direct prompts never create an inflight (they bypass the Discord
    // message path), so the (terminal + no-inflight) shape alone is not enough
    // to call new output "ghost noise" — a pending prompt anchor or
    // ExternalInput relay lease signals a legitimate user turn whose response
    // we must still relay even when notification/anchor creation failed.
    // Likewise, another assistant event after an early terminal relay means
    // the provider turn continued with tool calls or final text; do not drop it.
    // #3107: and if the pane is still actively producing assistant output, the
    // turn is live and merely lost its inflight — relay (and re-acquire) rather
    // than suppress.
    // #3154: while a deferred synthetic turn-start is pending for this channel,
    // the worker has not yet saved the matching inflight. Suppressing here (or
    // advancing the confirmed offset) would EAT the wait window and drop the
    // wakeup turn's response batch. Keep the bytes buffered until the worker
    // claims (its inflight save then takes over the relay).
    terminal_success_seen
        && inflight_missing
        && !ssh_direct_prompt_pending
        && !external_input_lease_present
        && !assistant_continuation_present
        && !pane_actively_streaming
        && !pending_synthetic_start_present
}

#[cfg(test)]
mod post_terminal_output_tests {
    use super::{
        should_skip_streaming_placeholder_without_inflight,
        should_suppress_post_terminal_output_without_inflight,
    };

    #[test]
    fn post_terminal_output_without_inflight_is_suppressed() {
        assert!(should_suppress_post_terminal_output_without_inflight(
            true, true, false, false, false, false, false
        ));
        assert!(
            !should_suppress_post_terminal_output_without_inflight(
                false, true, false, false, false, false, false
            ),
            "pre-terminal output still belongs to the active watcher turn"
        );
        assert!(
            !should_suppress_post_terminal_output_without_inflight(
                true, false, false, false, false, false, false
            ),
            "a newly active inflight owns subsequent output"
        );
        assert!(
            !should_suppress_post_terminal_output_without_inflight(
                true, true, true, false, false, false, false
            ),
            "SSH-direct prompt anchor present: output is a real direct-input response"
        );
        assert!(
            !should_suppress_post_terminal_output_without_inflight(
                true, true, false, true, false, false, false
            ),
            "ExternalInput lease present: notification failure must not suppress response output"
        );
        assert!(
            !should_suppress_post_terminal_output_without_inflight(
                true, true, false, false, true, false, false
            ),
            "assistant continuation after early terminal relay still belongs to the provider turn"
        );
    }

    #[test]
    fn post_terminal_hard_result_after_committed_turn_requires_direct_input_evidence() {
        assert!(
            should_suppress_post_terminal_output_without_inflight(
                true, true, false, false, false, false, false
            ),
            "a late hard_result envelope after a committed Discord turn must not relay again"
        );
        assert!(
            !should_suppress_post_terminal_output_without_inflight(
                true, true, true, false, false, false, false
            ),
            "a pending SSH-direct prompt is explicit evidence of a fresh direct-input turn"
        );
        assert!(
            !should_suppress_post_terminal_output_without_inflight(
                true, true, false, true, false, false, false
            ),
            "an ExternalInput lease is explicit evidence of a fresh direct-input turn"
        );
        assert!(
            should_suppress_post_terminal_output_without_inflight(
                true, true, false, false, false, false, false
            ),
            "a result-only duplicate without assistant continuation stays suppressed"
        );
    }

    #[test]
    fn post_terminal_output_with_actively_streaming_pane_is_not_suppressed() {
        // #3107: the (terminal + no-inflight) shape that would otherwise be
        // suppressed must still relay when the pane is actively producing —
        // the turn is live and merely lost its inflight.
        assert!(
            !should_suppress_post_terminal_output_without_inflight(
                true, true, false, false, false, true, false
            ),
            "an actively-streaming pane means the turn is live: relay, do not suppress"
        );
        // Asymmetry: with a finished/idle pane the same shape is still genuine
        // post-finish ghost noise and stays suppressed.
        assert!(
            should_suppress_post_terminal_output_without_inflight(
                true, true, false, false, false, false, false
            ),
            "a finished pane with missing inflight is real ghost noise: still suppressed"
        );
    }

    #[test]
    fn post_terminal_output_with_pending_synthetic_start_is_not_suppressed() {
        // #3154: while a deferred synthetic turn-start is pending for this
        // channel (the per-channel worker has not yet saved the matching
        // inflight), the (terminal + no-inflight) shape that would otherwise be
        // suppressed must keep its bytes buffered — suppressing here would EAT
        // the wait window and drop the wakeup turn's response batch.
        assert!(
            !should_suppress_post_terminal_output_without_inflight(
                true, true, false, false, false, false, true
            ),
            "a pending synthetic turn-start must keep post-terminal bytes buffered, not suppress"
        );
        // Without the pending start the same shape is genuine ghost noise.
        assert!(
            should_suppress_post_terminal_output_without_inflight(
                true, true, false, false, false, false, false
            ),
            "no pending synthetic start: real ghost noise stays suppressed"
        );
    }

    #[test]
    fn streaming_placeholder_without_inflight_is_skipped() {
        // Genuine ghost noise: inflight missing AND pane idle/finished.
        assert!(should_skip_streaming_placeholder_without_inflight(
            true, false
        ));
        assert!(!should_skip_streaming_placeholder_without_inflight(
            false, false
        ));
        // #3107 asymmetry: inflight missing but pane actively streaming → the
        // live turn lost its inflight; do NOT skip the streaming edit.
        assert!(
            !should_skip_streaming_placeholder_without_inflight(true, true),
            "an actively-streaming pane with missing inflight is a live turn: relay"
        );
        // A present inflight is never skipped regardless of pane state.
        assert!(!should_skip_streaming_placeholder_without_inflight(
            false, true
        ));
    }
}
