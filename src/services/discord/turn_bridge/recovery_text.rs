use super::super::*;
use crate::config::local_api_url;
#[cfg(unix)]
use crate::services::tmux_diagnostics::record_tmux_exit_reason;

/// Auto-retry a failed resume by fetching recent Discord history,
/// storing it in kv_meta for the router to inject into the LLM prompt,
/// and re-sending the original message via announce bot.
/// Discord only sees a short notice — the full history is LLM-only.
pub(super) async fn auto_retry_with_history(
    http: &serenity::Http,
    channel_id: ChannelId,
    user_text: &str,
    api_port: u16,
) {
    let ts = chrono::Local::now().format("%H:%M:%S");

    // Dedup guard: use a static set to prevent turn_bridge + watcher from
    // both firing auto-retry for the same channel simultaneously.
    use std::sync::LazyLock;
    static RETRY_PENDING: LazyLock<dashmap::DashSet<u64>> =
        LazyLock::new(|| dashmap::DashSet::new());
    if !RETRY_PENDING.insert(channel_id.get()) {
        eprintln!("  [{ts}] ⏭ auto-retry: skipped (dedup) for channel {channel_id}");
        return;
    }
    // Clean up guard after 30 seconds (allow future retries)
    let ch_id = channel_id.get();
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
        RETRY_PENDING.remove(&ch_id);
    });

    eprintln!("  [{ts}] ↻ auto-retry: fetching last 10 messages for channel {channel_id}");

    // Fetch last 10 messages from Discord
    let history = match channel_id
        .messages(http, serenity::builder::GetMessages::new().limit(10))
        .await
    {
        Ok(msgs) => {
            let mut lines = Vec::new();
            for msg in msgs.iter().rev() {
                let author = &msg.author.name;
                let content = msg.content.chars().take(300).collect::<String>();
                if !content.trim().is_empty() {
                    lines.push(format!("{}: {}", author, content));
                }
            }
            if lines.is_empty() {
                None
            } else {
                Some(lines.join("\n"))
            }
        }
        Err(e) => {
            eprintln!("  [{ts}] ⚠ auto-retry: failed to fetch history: {e}");
            None
        }
    };

    // Store history in kv_meta for the router to inject into LLM prompt.
    // Key: session_retry_context:{channel_id} — consumed on next turn start.
    if let Some(ref hist) = history {
        let _ = reqwest::Client::new()
            .post(local_api_url(api_port, "/api/kv"))
            .json(&serde_json::json!({
                "key": format!("session_retry_context:{}", channel_id),
                "value": hist,
            }))
            .send()
            .await;
    }

    // Discord message: short notice only — history stays LLM-side
    let retry_content = format!(
        "[이전 대화 복원 — 세션이 만료되어 최근 대화를 컨텍스트로 제공합니다]\n\n{}",
        user_text
    );
    let retry_ch = channel_id.get().to_string();

    let _ = reqwest::Client::new()
        .post(local_api_url(api_port, "/api/send"))
        .json(&serde_json::json!({
            "target": format!("channel:{retry_ch}"),
            "content": retry_content,
            "source": "pipeline",
            "bot": "announce",
        }))
        .send()
        .await;
}

pub(super) fn clear_local_session_state(
    new_session_id: &mut Option<String>,
    inflight_state: &mut InflightTurnState,
) {
    *new_session_id = None;
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

pub(super) fn reset_gemini_retry_attempt_state(
    full_response: &mut String,
    current_tool_line: &mut Option<String>,
    last_tool_name: &mut Option<String>,
    last_tool_summary: &mut Option<String>,
    any_tool_used: &mut bool,
    has_post_tool_text: &mut bool,
    response_sent_offset: &mut usize,
    inflight_state: &mut InflightTurnState,
) {
    full_response.clear();
    *current_tool_line = None;
    *last_tool_name = None;
    *last_tool_summary = None;
    *any_tool_used = false;
    *has_post_tool_text = false;
    *response_sent_offset = 0;
    inflight_state.full_response.clear();
    inflight_state.current_tool_line = None;
    inflight_state.any_tool_used = false;
    inflight_state.has_post_tool_text = false;
    inflight_state.response_sent_offset = 0;
}

pub(super) fn handle_gemini_retry_boundary(
    full_response: &mut String,
    current_tool_line: &mut Option<String>,
    last_tool_name: &mut Option<String>,
    last_tool_summary: &mut Option<String>,
    any_tool_used: &mut bool,
    has_post_tool_text: &mut bool,
    response_sent_offset: &mut usize,
    last_edit_text: &mut String,
    new_session_id: &mut Option<String>,
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
        clear_local_session_state(new_session_id, inflight_state);
    }

    if should_reset {
        reset_gemini_retry_attempt_state(
            full_response,
            current_tool_line,
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
    inflight_state: &mut InflightTurnState,
    reason: &str,
) {
    clear_local_session_state(new_session_id, inflight_state);
    let _ = save_inflight_state(inflight_state);

    let stale_sid = {
        let mut data = shared.core.lock().await;
        let old = data
            .sessions
            .get(&channel_id)
            .and_then(|s| s.session_id.clone());
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.session_id = None;
        }
        old
    };

    if let Some(key) = adk_session_key {
        super::super::adk_session::clear_provider_session_id(key, shared.api_port).await;
    }

    if let Some(ref sid) = stale_sid {
        let port = shared.api_port;
        let sid_c = sid.clone();
        let _ = reqwest::Client::new()
            .post(crate::config::local_api_url(
                port,
                "/api/dispatched-sessions/clear-stale-session-id",
            ))
            .json(&serde_json::json!({"claude_session_id": sid_c}))
            .send()
            .await;
    }

    #[cfg(unix)]
    if let Some(name) = cancel_token
        .tmux_session
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        eprintln!("  [{ts}] ♻ auto-retry: killing tmux session {name} before retry ({reason})");
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

pub(super) fn contains_stale_resume_error_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("no conversation found") || lower.contains("error: no conversation")
}

pub(in crate::services::discord) fn result_event_has_stale_resume_error(
    value: &serde_json::Value,
) -> bool {
    if value.get("type").and_then(|v| v.as_str()) != Some("result") {
        return false;
    }

    let subtype = value.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
    let is_error = value
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || subtype.starts_with("error");
    if !is_error {
        return false;
    }

    if value
        .get("result")
        .and_then(|v| v.as_str())
        .map(contains_stale_resume_error_text)
        .unwrap_or(false)
    {
        return true;
    }

    value
        .get("errors")
        .and_then(|v| v.as_array())
        .map(|errors| {
            errors.iter().any(|err| {
                err.as_str()
                    .map(contains_stale_resume_error_text)
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

pub(super) fn output_file_has_stale_resume_error_after_offset(
    output_path: &str,
    start_offset: u64,
) -> bool {
    let Ok(bytes) = std::fs::read(output_path) else {
        return false;
    };
    let start = usize::try_from(start_offset)
        .ok()
        .map(|offset| offset.min(bytes.len()))
        .unwrap_or(bytes.len());

    String::from_utf8_lossy(&bytes[start..])
        .lines()
        .any(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return false;
            }
            serde_json::from_str::<serde_json::Value>(trimmed)
                .ok()
                .map(|value| result_event_has_stale_resume_error(&value))
                .unwrap_or(false)
        })
}

pub(super) fn stream_error_has_stale_resume_error(message: &str, stderr: &str) -> bool {
    contains_stale_resume_error_text(message) || contains_stale_resume_error_text(stderr)
}

pub(super) fn stream_error_requires_terminal_session_reset(message: &str, stderr: &str) -> bool {
    let lower = format!("{} {}", message, stderr).to_ascii_lowercase();
    lower.contains("gemini session could not be recovered after retry")
        || lower.contains("gemini stream ended without a terminal result")
        || lower.contains("invalidargument: gemini resume selector must be")
        || lower.contains("qwen session could not be recovered after retry")
        || lower.contains("qwen stream ended without a terminal result")
}

/// Decide the final response text when a Done event arrives.
///
/// Returns the text that should be used as `full_response`.
/// - If streaming accumulated post-tool text, keep the streamed `full_response`.
/// - If streaming only accumulated pre-tool narration (tools used, no post-tool
///   text), replace with the authoritative `result` from the Done event.
/// - If streaming produced nothing, use `result` directly.
pub(super) fn resolve_done_response(
    full_response: &str,
    result: &str,
    any_tool_used: bool,
    has_post_tool_text: bool,
) -> Option<String> {
    if result.is_empty() {
        return None;
    }
    if full_response.trim().is_empty() {
        return Some(result.to_string());
    }
    if any_tool_used && !has_post_tool_text {
        return Some(result.to_string());
    }
    None
}
