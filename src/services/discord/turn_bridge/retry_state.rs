/// Retry-state management for turn_bridge.
///
/// Provides helpers to clear, reset, and manage the in-flight retry state
/// during Gemini/Qwen auto-retry boundaries and session recovery.
use super::super::*;
#[cfg(unix)]
use crate::services::tmux_diagnostics::record_tmux_exit_reason;

pub(super) fn clear_local_session_state(
    new_session_id: &mut Option<String>,
    new_raw_provider_session_id: &mut Option<String>,
    inflight_state: &mut InflightTurnState,
) {
    *new_session_id = None;
    *new_raw_provider_session_id = None;
    inflight_state.session_id = None;
}

pub(super) fn should_reset_gemini_retry_attempt_state(
    full_response: &str,
    current_tool_line: Option<&str>,
    any_tool_used: bool,
    has_post_tool_text: bool,
) -> bool {
    !full_response.trim().is_empty()
        || current_tool_line.is_some()
        || any_tool_used
        || has_post_tool_text
}

fn normalized_response_sent_offset(full_response: &str, response_sent_offset: usize) -> usize {
    let mut offset = response_sent_offset.min(full_response.len());
    while offset > 0 && !full_response.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

pub(super) fn sync_response_delivery_state(
    full_response: &str,
    response_sent_offset: &mut usize,
    inflight_state: &mut InflightTurnState,
) {
    *response_sent_offset = normalized_response_sent_offset(full_response, *response_sent_offset);
    inflight_state.full_response = full_response.to_string();
    inflight_state.response_sent_offset = *response_sent_offset;
}

pub(super) fn clear_response_delivery_state(
    full_response: &mut String,
    response_sent_offset: &mut usize,
    inflight_state: &mut InflightTurnState,
) {
    full_response.clear();
    *response_sent_offset = 0;
    inflight_state.full_response.clear();
    inflight_state.response_sent_offset = 0;
}

pub(super) fn reset_gemini_retry_attempt_state(
    full_response: &mut String,
    current_tool_line: &mut Option<String>,
    prev_tool_status: &mut Option<String>,
    last_tool_name: &mut Option<String>,
    last_tool_summary: &mut Option<String>,
    any_tool_used: &mut bool,
    has_post_tool_text: &mut bool,
    response_sent_offset: &mut usize,
    inflight_state: &mut InflightTurnState,
) {
    clear_response_delivery_state(full_response, response_sent_offset, inflight_state);
    *current_tool_line = None;
    *prev_tool_status = None;
    *last_tool_name = None;
    *last_tool_summary = None;
    *any_tool_used = false;
    *has_post_tool_text = false;
    inflight_state.current_tool_line = None;
    inflight_state.prev_tool_status = None;
    inflight_state.any_tool_used = false;
    inflight_state.has_post_tool_text = false;
}

pub(super) fn handle_gemini_retry_boundary(
    full_response: &mut String,
    current_tool_line: &mut Option<String>,
    prev_tool_status: &mut Option<String>,
    last_tool_name: &mut Option<String>,
    last_tool_summary: &mut Option<String>,
    any_tool_used: &mut bool,
    has_post_tool_text: &mut bool,
    response_sent_offset: &mut usize,
    last_edit_text: &mut String,
    new_session_id: &mut Option<String>,
    new_raw_provider_session_id: &mut Option<String>,
    inflight_state: &mut InflightTurnState,
) -> bool {
    let had_local_session = new_session_id.is_some() || inflight_state.session_id.is_some();
    let should_reset = should_reset_gemini_retry_attempt_state(
        full_response,
        current_tool_line.as_deref(),
        *any_tool_used,
        *has_post_tool_text,
    );

    if had_local_session {
        clear_local_session_state(new_session_id, new_raw_provider_session_id, inflight_state);
    }

    if should_reset {
        reset_gemini_retry_attempt_state(
            full_response,
            current_tool_line,
            prev_tool_status,
            last_tool_name,
            last_tool_summary,
            any_tool_used,
            has_post_tool_text,
            response_sent_offset,
            inflight_state,
        );
        last_edit_text.clear();
    }

    had_local_session || should_reset
}

pub(super) async fn reset_session_for_auto_retry(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    cancel_token: &Arc<CancelToken>,
    adk_session_key: Option<&str>,
    new_session_id: &mut Option<String>,
    new_raw_provider_session_id: &mut Option<String>,
    inflight_state: &mut InflightTurnState,
    reason: &str,
) {
    clear_local_session_state(new_session_id, new_raw_provider_session_id, inflight_state);
    let _ = save_inflight_state(inflight_state);

    let stale_sid = {
        let mut data = shared.core.lock().await;
        let old = data
            .sessions
            .get(&channel_id)
            .and_then(|s| s.session_id.clone());
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.clear_provider_session();
        }
        old
    };

    if let Some(key) = adk_session_key {
        super::super::adk_session::clear_provider_session_id(key, shared.api_port).await;
    }

    if let Some(ref sid) = stale_sid {
        let _ = super::super::internal_api::clear_stale_session_id(sid).await;
    }

    #[cfg(unix)]
    if let Some(name) = cancel_token
        .tmux_session
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ♻ auto-retry: killing tmux session {name} before retry ({reason})"
        );
        crate::services::termination_audit::record_termination_for_tmux(
            &name,
            None,
            "turn_bridge",
            "auto_retry_fresh_session",
            Some(&format!(
                "forcing fresh session before auto-retry: {reason}"
            )),
            None,
        );
        record_tmux_exit_reason(
            &name,
            &format!("forcing fresh session before auto-retry: {reason}"),
        );
        crate::services::platform::tmux::kill_session(&name);
    }
}
