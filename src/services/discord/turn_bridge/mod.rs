mod completion_guard;
mod context_window;
mod recovery_text;
mod retry_state;
mod stale_resume;
mod tmux_runtime;

#[cfg(test)]
mod tests;

use super::handoff::{HandoffRecord, save_handoff};
use super::restart_report::{RestartCompletionReport, clear_restart_report, save_restart_report};
use super::*;
use crate::services::memory::{
    CaptureRequest, ReflectRequest, SessionEndReason, TokenUsage, build_resolved_memory_backend,
    resolve_memory_role_id, resolve_memory_session_id,
};
use crate::services::provider::cancel_requested;
#[cfg(unix)]
use crate::services::tmux_diagnostics::record_tmux_exit_reason;
use crate::utils::format::tail_with_ellipsis;

// Re-exports for pub(super) items used by sibling modules in the discord package
pub(super) use completion_guard::guard_review_dispatch_completion;
pub(super) use completion_guard::runtime_db_fallback_complete;
pub(super) use stale_resume::result_event_has_stale_resume_error;
pub(super) use tmux_runtime::cancel_active_token;
pub(super) use tmux_runtime::stale_inflight_message;

// Re-export pub(crate) items
pub(crate) use tmux_runtime::tmux_runtime_paths;

// Items used by spawn_turn_bridge from submodules
use completion_guard::{complete_work_dispatch_on_turn_end, fail_dispatch_with_retry};
use context_window::{persisted_context_tokens, resolve_done_response};
use recovery_text::auto_retry_with_history;
use retry_state::{
    clear_local_session_state, handle_gemini_retry_boundary, reset_session_for_auto_retry,
};
use stale_resume::{
    output_file_has_stale_resume_error_after_offset, stream_error_has_stale_resume_error,
    stream_error_requires_terminal_session_reset,
};
use tmux_runtime::{is_dcserver_restart_command, should_resume_watcher_after_turn};

pub(super) fn spawn_memory_capture_task(
    channel_id: ChannelId,
    capture_memory_settings: settings::ResolvedMemorySettings,
    capture_request: CaptureRequest,
) -> tokio::task::JoinHandle<crate::services::memory::CaptureResult> {
    tokio::spawn(async move {
        let backend = build_resolved_memory_backend(&capture_memory_settings);
        let result = backend.capture(capture_request).await;
        for warning in &result.warnings {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!(
                "  [{ts}] [memory] capture warning for channel {}: {}",
                channel_id.get(),
                warning
            );
        }
        result
    })
}

pub(super) fn spawn_memory_reflect_task(
    channel_id: ChannelId,
    reflect_memory_settings: settings::ResolvedMemorySettings,
    reflect_request: ReflectRequest,
) -> tokio::task::JoinHandle<crate::services::memory::CaptureResult> {
    tokio::spawn(async move {
        let backend = build_resolved_memory_backend(&reflect_memory_settings);
        let reason = reflect_request.reason.as_str().to_string();
        let result = backend.reflect(reflect_request).await;
        for warning in &result.warnings {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!(
                "  [{ts}] [memory] reflect warning for channel {} ({}): {}",
                channel_id.get(),
                reason,
                warning
            );
        }
        result
    })
}

fn build_memento_transcript(history: &[HistoryItem]) -> String {
    history
        .iter()
        .filter_map(|item| {
            let content = item.content.trim();
            if content.is_empty() {
                return None;
            }

            let label = match item.item_type {
                HistoryType::User => "User",
                HistoryType::Assistant => "Assistant",
                HistoryType::Error => "Error",
                HistoryType::System => "System",
                HistoryType::ToolUse => "ToolUse",
                HistoryType::ToolResult => "ToolResult",
            };

            Some(format!("[{label}]: {content}"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn take_memento_reflect_request(
    session: &mut DiscordSession,
    memory_settings: &settings::ResolvedMemorySettings,
    provider: &ProviderKind,
    role_binding: Option<&RoleBinding>,
    channel_id: u64,
    reason: SessionEndReason,
) -> Option<ReflectRequest> {
    if memory_settings.backend != settings::MemoryBackendKind::Memento
        || !session.memento_context_loaded
        || session.memento_reflected
    {
        return None;
    }

    let session_id = session
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let transcript = build_memento_transcript(&session.history);
    if transcript.trim().is_empty() {
        return None;
    }

    session.memento_reflected = true;
    Some(ReflectRequest {
        provider: provider.clone(),
        role_id: resolve_memory_role_id(role_binding),
        channel_id,
        session_id,
        reason,
        transcript,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TurnEndMemoryPlan {
    pub(super) reflect_reason: Option<SessionEndReason>,
    pub(super) clear_provider_session: bool,
    pub(super) persist_transcript: bool,
    pub(super) spawn_capture: bool,
}

pub(super) fn plan_turn_end_memory(
    session: &DiscordSession,
    backend: settings::MemoryBackendKind,
    is_prompt_too_long: bool,
    resume_failure_detected: bool,
    terminal_session_reset_required: bool,
    should_record_final_turn: bool,
) -> Option<TurnEndMemoryPlan> {
    if session.cleared || is_prompt_too_long {
        return None;
    }

    let persist_transcript = should_record_final_turn;
    let reflect_reason = if terminal_session_reset_required {
        Some(SessionEndReason::LocalSessionReset)
    } else {
        None
    };
    let clear_provider_session = resume_failure_detected || terminal_session_reset_required;

    Some(TurnEndMemoryPlan {
        reflect_reason,
        clear_provider_session,
        persist_transcript,
        spawn_capture: persist_transcript && backend != settings::MemoryBackendKind::Memento,
    })
}

pub(super) struct TurnBridgeContext {
    pub(super) provider: ProviderKind,
    pub(super) channel_id: ChannelId,
    pub(super) user_msg_id: MessageId,
    pub(super) user_text_owned: String,
    pub(super) request_owner_name: String,
    pub(super) request_owner: Option<UserId>,
    pub(super) serenity_ctx: Option<serenity::Context>,
    pub(super) token: Option<String>,
    pub(super) role_binding: Option<RoleBinding>,
    pub(super) adk_session_key: Option<String>,
    pub(super) adk_session_name: Option<String>,
    pub(super) adk_session_info: Option<String>,
    pub(super) adk_cwd: Option<String>,
    pub(super) dispatch_id: Option<String>,
    pub(super) memory_recall_usage: TokenUsage,
    pub(super) current_msg_id: MessageId,
    pub(super) response_sent_offset: usize,
    pub(super) full_response: String,
    pub(super) tmux_last_offset: Option<u64>,
    pub(super) new_session_id: Option<String>,
    pub(super) defer_watcher_resume: bool,
    pub(super) completion_tx: Option<tokio::sync::oneshot::Sender<()>>,
    pub(super) inflight_state: InflightTurnState,
}

pub(super) fn optional_metric_token_fields(usage: TokenUsage) -> (Option<u64>, Option<u64>) {
    if usage.is_zero() {
        return (None, None);
    }
    (
        if usage.input_tokens > 0 {
            Some(usage.input_tokens)
        } else {
            None
        },
        if usage.output_tokens > 0 {
            Some(usage.output_tokens)
        } else {
            None
        },
    )
}

fn extract_skill_id_from_tool_use(name: &str, input: &str) -> Option<String> {
    if name != "Skill" {
        return None;
    }

    serde_json::from_str::<serde_json::Value>(input)
        .ok()
        .and_then(|value| {
            value
                .get("skill")
                .and_then(|skill| skill.as_str())
                .map(str::trim)
                .filter(|skill| !skill.is_empty())
                .map(ToString::to_string)
        })
}

fn resolve_skill_usage_agent_id(
    conn: &rusqlite::Connection,
    session_key: Option<&str>,
    role_binding: Option<&RoleBinding>,
) -> Option<String> {
    session_key
        .and_then(|key| {
            conn.query_row(
                "SELECT agent_id FROM sessions WHERE session_key = ?1",
                [key],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
        })
        .or_else(|| {
            role_binding
                .map(|binding| binding.role_id.trim().to_string())
                .filter(|role_id| !role_id.is_empty())
        })
}

fn record_skill_usage(
    db: &crate::db::Db,
    skill_id: &str,
    session_key: Option<&str>,
    role_binding: Option<&RoleBinding>,
) -> Result<(), String> {
    let conn = db.lock().map_err(|e| format!("db lock failed: {e}"))?;
    let agent_id = resolve_skill_usage_agent_id(&conn, session_key, role_binding);
    conn.execute(
        "INSERT INTO skill_usage (skill_id, agent_id, session_key) VALUES (?1, ?2, ?3)",
        rusqlite::params![skill_id, agent_id, session_key],
    )
    .map_err(|e| format!("insert skill_usage failed: {e}"))?;
    Ok(())
}

pub(super) fn spawn_turn_bridge(
    http: Arc<serenity::Http>,
    shared_owned: Arc<SharedData>,
    cancel_token: Arc<CancelToken>,
    rx: mpsc::Receiver<StreamMessage>,
    bridge: TurnBridgeContext,
) {
    tokio::spawn(async move {
        const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let channel_id = bridge.channel_id;
        let provider = bridge.provider.clone();
        let user_msg_id = bridge.user_msg_id;
        let user_text_owned = bridge.user_text_owned.clone();
        let request_owner_name = bridge.request_owner_name.clone();
        let request_owner = bridge.request_owner;
        let serenity_ctx = bridge.serenity_ctx.clone();
        let token = bridge.token.clone();
        let role_binding = bridge.role_binding.clone();
        let adk_session_key = bridge.adk_session_key.clone();
        let adk_session_name = bridge.adk_session_name.clone();
        let adk_session_info = bridge.adk_session_info.clone();
        let adk_cwd = bridge.adk_cwd.clone();
        let dispatch_id = bridge.dispatch_id.clone();

        let mut full_response = bridge.full_response.clone();
        let mut last_edit_text = String::new();
        let mut done = false;
        let mut cancelled = false;
        let mut rx_disconnected = false;
        let mut current_tool_line: Option<String> = bridge.inflight_state.current_tool_line.clone();
        let mut prev_tool_status: Option<String> = bridge.inflight_state.prev_tool_status.clone();
        let mut last_tool_name: Option<String> = None;
        let mut last_tool_summary: Option<String> = None;
        let mut accumulated_input_tokens: u64 = 0;
        let mut accumulated_output_tokens: u64 = 0;
        let mut accumulated_memory_input_tokens: u64 = bridge.memory_recall_usage.input_tokens;
        let mut accumulated_memory_output_tokens: u64 = bridge.memory_recall_usage.output_tokens;
        let mut spin_idx: usize = 0;
        let mut restart_followup_pending = false;
        let mut any_tool_used = bridge.inflight_state.any_tool_used;
        let mut has_post_tool_text = bridge.inflight_state.has_post_tool_text;
        let mut tmux_handed_off = false;
        let mut transport_error = false;
        let mut resume_failure_detected = false;
        let mut terminal_session_reset_required = false;
        let mut restart_recovery_handoff = false;
        let mut recovery_retry = false;
        let mut last_adk_heartbeat = std::time::Instant::now();
        let current_msg_id = bridge.current_msg_id;
        let mut response_sent_offset = bridge.response_sent_offset;
        let mut tmux_last_offset = bridge.tmux_last_offset;
        let mut new_session_id = bridge.new_session_id.clone();
        let defer_watcher_resume = bridge.defer_watcher_resume;
        let completion_tx = bridge.completion_tx;
        // Guard: ensure completion_tx fires even if the task panics or
        // exits early, preventing the parent from hanging on completion_rx.
        struct CompletionGuard(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for CompletionGuard {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }
        let _completion_guard = CompletionGuard(completion_tx);

        // Guard: ensure inflight state file is cleaned up even if the task
        // panics or exits early.  On the normal path we defuse the guard
        // after the explicit clear_inflight_state() call.
        struct InflightCleanupGuard {
            provider: Option<ProviderKind>,
            channel_id: u64,
        }
        impl Drop for InflightCleanupGuard {
            fn drop(&mut self) {
                if let Some(ref provider) = self.provider {
                    clear_inflight_state(provider, self.channel_id);
                }
            }
        }
        let mut inflight_guard = InflightCleanupGuard {
            provider: Some(provider.clone()),
            channel_id: channel_id.get(),
        };

        let mut inflight_state = bridge.inflight_state.clone();
        let mut last_status_edit = tokio::time::Instant::now();
        let status_interval = super::status_update_interval();
        let narrate_progress = super::settings::load_narrate_progress(shared_owned.db.as_ref());
        let turn_start = std::time::Instant::now();

        let _ = save_inflight_state(&inflight_state);

        while !done {
            let mut state_dirty = false;

            if cancel_requested(Some(cancel_token.as_ref())) {
                cancelled = true;
                break;
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

            if cancel_requested(Some(cancel_token.as_ref())) {
                cancelled = true;
                break;
            }

            loop {
                match rx.try_recv() {
                    Ok(msg) => match msg {
                        StreamMessage::RetryBoundary => {
                            if provider == ProviderKind::Gemini
                                && handle_gemini_retry_boundary(
                                    &mut full_response,
                                    &mut current_tool_line,
                                    &mut prev_tool_status,
                                    &mut last_tool_name,
                                    &mut last_tool_summary,
                                    &mut any_tool_used,
                                    &mut has_post_tool_text,
                                    &mut response_sent_offset,
                                    &mut last_edit_text,
                                    &mut new_session_id,
                                    &mut inflight_state,
                                )
                            {
                                state_dirty = true;
                            }
                        }
                        StreamMessage::Init { session_id: sid } => {
                            new_session_id = Some(sid.clone());
                            inflight_state.session_id = Some(sid);
                            state_dirty = true;
                        }
                        StreamMessage::Text { content } => {
                            full_response.push_str(&content);
                            if any_tool_used {
                                has_post_tool_text = true;
                                inflight_state.has_post_tool_text = true;
                            }
                            super::formatting::preserve_previous_tool_status(
                                &mut prev_tool_status,
                                current_tool_line.as_deref(),
                                None,
                            );
                            current_tool_line = None;
                            last_tool_name = None;
                            last_tool_summary = None;
                            inflight_state.full_response = full_response.clone();
                            state_dirty = true;
                        }
                        StreamMessage::Thinking { summary } => {
                            let display = if let Some(ref s) = summary {
                                format!("💭 {s}")
                            } else {
                                "💭 Thinking...".to_string()
                            };
                            super::formatting::preserve_previous_tool_status(
                                &mut prev_tool_status,
                                current_tool_line.as_deref(),
                                Some(display.as_str()),
                            );
                            current_tool_line = Some(display);
                            last_tool_name = None;
                            last_tool_summary = None;
                        }
                        StreamMessage::ToolUse { name, input } => {
                            any_tool_used = true;
                            has_post_tool_text = false;
                            inflight_state.any_tool_used = true;
                            inflight_state.has_post_tool_text = false;
                            if let Some(skill_id) = extract_skill_id_from_tool_use(&name, &input) {
                                if let Some(db) = shared_owned.db.as_ref() {
                                    if let Err(e) = record_skill_usage(
                                        db,
                                        &skill_id,
                                        adk_session_key.as_deref(),
                                        role_binding.as_ref(),
                                    ) {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        eprintln!(
                                            "  [{ts}] ⚠ Failed to record skill usage for {}: {}",
                                            skill_id, e
                                        );
                                    }
                                }
                            }
                            let summary = format_tool_input(&name, &input);
                            let display_summary = if summary.trim().is_empty() {
                                "…".to_string()
                            } else {
                                truncate_str(&summary, 120).to_string()
                            };
                            let display = format!("⚙ {}: {}", name, display_summary);
                            super::formatting::preserve_previous_tool_status(
                                &mut prev_tool_status,
                                current_tool_line.as_deref(),
                                Some(display.as_str()),
                            );
                            current_tool_line = Some(display);
                            last_tool_name = Some(name.clone());
                            last_tool_summary = Some(display_summary);
                            if !restart_followup_pending && is_dcserver_restart_command(&input) {
                                let mut report = RestartCompletionReport::new(
                                    provider.clone(),
                                    channel_id.get(),
                                    "pending",
                                    format!(
                                        "dcserver restart requested by `{}`; 새 프로세스가 후속 보고를 이어받을 예정입니다.",
                                        request_owner_name
                                    ),
                                );
                                report.current_msg_id = Some(current_msg_id.get());
                                report.channel_name = adk_session_name.clone();
                                if save_restart_report(&report).is_ok() {
                                    restart_followup_pending = true;

                                    // Save durable handoff for post-restart follow-up
                                    let handoff = HandoffRecord::new(
                                        &provider,
                                        channel_id.get(),
                                        adk_session_name.clone(),
                                        "재시작 후 수정 내용 확인 및 후속 작업 이어서 진행",
                                        format!(
                                            "재시작 전 사용자 요청: {}\n\n이전 턴의 응답 요약: {}",
                                            user_text_owned,
                                            tail_with_ellipsis(&full_response, 500),
                                        ),
                                        adk_cwd.clone(),
                                        Some(user_msg_id.get()),
                                    );
                                    if let Err(e) = save_handoff(&handoff) {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        println!("  [{ts}] ⚠ failed to save handoff: {e}");
                                    }

                                    let handoff_text = "♻️ dcserver 재시작 중...\n\n새 dcserver가 이 메시지를 이어받는 중입니다.";
                                    rate_limit_wait(&shared_owned, channel_id).await;
                                    let _ = channel_id
                                        .edit_message(
                                            &http,
                                            current_msg_id,
                                            EditMessage::new().content(handoff_text),
                                        )
                                        .await;
                                    last_edit_text = handoff_text.to_string();
                                    inflight_state.current_msg_id = current_msg_id.get();
                                    inflight_state.current_msg_len = handoff_text.len();
                                    state_dirty = true;
                                }
                            }
                            if !full_response.is_empty() {
                                let trimmed = full_response.trim_end();
                                full_response.truncate(trimmed.len());
                                full_response.push_str("\n\n");
                                inflight_state.full_response = full_response.clone();
                                state_dirty = true;
                            }
                        }
                        StreamMessage::ToolResult { content, is_error } => {
                            if let Some(ref tn) = last_tool_name {
                                let status = if is_error { "✗" } else { "✓" };
                                let detail = last_tool_summary
                                    .as_deref()
                                    .filter(|s| !s.is_empty() && *s != "…")
                                    .map(|s| format!("{} {}: {}", status, tn, s))
                                    .unwrap_or_else(|| format!("{} {}", status, tn));
                                super::formatting::preserve_previous_tool_status(
                                    &mut prev_tool_status,
                                    current_tool_line.as_deref(),
                                    Some(detail.as_str()),
                                );
                                current_tool_line = Some(detail);
                            }
                            let _ = content;
                        }
                        StreamMessage::TaskNotification { summary, .. } => {
                            if !summary.is_empty() {
                                full_response.push_str(&format!("\n[Task: {}]\n", summary));
                                inflight_state.full_response = full_response.clone();
                                state_dirty = true;
                            }
                        }
                        StreamMessage::Done {
                            result,
                            session_id: sid,
                        } => {
                            if result == super::recovery::RESTART_SESSION_DIED_HANDOFF_SENTINEL {
                                restart_recovery_handoff = true;
                            } else if result == "__session_died_retry__" {
                                // Legacy fallback: older recovery reporters used
                                // auto-retry instead of the internal handoff
                                // sentinel. Keep this branch for mixed-runtime
                                // rollouts until every caller emits the new
                                // sentinel consistently.
                                recovery_retry = true;
                            }
                            if let Some(resolved) = resolve_done_response(
                                &full_response,
                                &result,
                                any_tool_used,
                                has_post_tool_text,
                            ) {
                                full_response = resolved;
                                inflight_state.full_response = full_response.clone();
                            }
                            if let Some(s) = sid {
                                new_session_id = Some(s.clone());
                                inflight_state.session_id = Some(s);
                            }
                            state_dirty = true;
                            done = true;
                        }
                        StreamMessage::Error {
                            message, stderr, ..
                        } => {
                            let is_stale_resume =
                                stream_error_has_stale_resume_error(&message, &stderr);
                            let session_reset_required =
                                stream_error_requires_terminal_session_reset(&message, &stderr);
                            transport_error = true;
                            let combined = format!("{} {}", message, stderr).to_lowercase();
                            if combined.contains("prompt is too long")
                                || combined.contains("prompt too long")
                                || combined.contains("context_length_exceeded")
                                || combined.contains("max_tokens")
                                || combined.contains("context window")
                                || combined.contains("token limit")
                            {
                                // Prompt too long is not a terminal failure — user can retry
                                // with a shorter message or /compact. Don't mark as transport error.
                                transport_error = false;
                                full_response = "⚠️ __prompt too long__".to_string();
                            } else if is_stale_resume {
                                // Recoverable stale resume: auto-retry with a fresh provider
                                // session instead of failing the current dispatch/turn.
                                transport_error = false;
                                resume_failure_detected = true;
                                if !stderr.is_empty() {
                                    full_response = format!(
                                        "Error: {}\nstderr: {}",
                                        message,
                                        truncate_str(&stderr, 500)
                                    );
                                } else {
                                    full_response = format!("Error: {}", message);
                                }
                            } else if !stderr.is_empty() {
                                full_response = format!(
                                    "Error: {}\nstderr: {}",
                                    message,
                                    truncate_str(&stderr, 500)
                                );
                            } else {
                                full_response = format!("Error: {}", message);
                            }
                            if session_reset_required {
                                terminal_session_reset_required = true;
                                clear_local_session_state(&mut new_session_id, &mut inflight_state);
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                eprintln!(
                                    "  [{ts}] ⚠ Clearing stored provider session after terminal {} session failure (channel {})",
                                    provider.as_str(),
                                    channel_id,
                                );
                            }
                            inflight_state.full_response = full_response.clone();
                            state_dirty = true;
                            done = true;
                        }
                        StreamMessage::StatusUpdate {
                            input_tokens,
                            output_tokens,
                            ..
                        } => {
                            // Use latest value (not cumulative) — each StatusUpdate
                            // from claude.rs already includes cumulative cache tokens,
                            // representing the current context window occupancy.
                            if let Some(it) = input_tokens {
                                accumulated_input_tokens = it;
                            }
                            if let Some(ot) = output_tokens {
                                accumulated_output_tokens = ot;
                            }
                        }
                        StreamMessage::TmuxReady {
                            output_path,
                            input_fifo_path,
                            tmux_session_name,
                            last_offset,
                        } => {
                            tmux_handed_off = true;
                            tmux_last_offset = Some(last_offset);
                            inflight_state.tmux_session_name = Some(tmux_session_name.clone());
                            inflight_state.output_path = Some(output_path.clone());
                            inflight_state.input_fifo_path = Some(input_fifo_path);
                            inflight_state.last_offset = last_offset;

                            // #226: Atomic claim via try_claim_watcher
                            let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let paused = Arc::new(std::sync::atomic::AtomicBool::new(true));
                            let resume_offset = Arc::new(std::sync::Mutex::new(None::<u64>));
                            let pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(1));
                            let turn_delivered =
                                Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let handle = TmuxWatcherHandle {
                                paused: paused.clone(),
                                resume_offset: resume_offset.clone(),
                                cancel: cancel.clone(),
                                pause_epoch: pause_epoch.clone(),
                                turn_delivered: turn_delivered.clone(),
                            };
                            let watcher_claimed = {
                                #[cfg(unix)]
                                {
                                    // #243: Use claim_or_replace to avoid races where
                                    // a stale watcher blocks the new turn's watcher.
                                    super::tmux::claim_or_replace_watcher(
                                        &shared_owned.tmux_watchers,
                                        channel_id,
                                        handle,
                                    );
                                    true
                                }
                                #[cfg(not(unix))]
                                {
                                    let _ = handle;
                                    false
                                }
                            };
                            if watcher_claimed {
                                #[cfg(unix)]
                                {
                                    let http_bg = http.clone();
                                    let shared_bg = shared_owned.clone();
                                    tokio::spawn(tmux_output_watcher(
                                        channel_id,
                                        http_bg,
                                        shared_bg,
                                        output_path,
                                        tmux_session_name,
                                        last_offset,
                                        cancel,
                                        paused,
                                        resume_offset,
                                        pause_epoch,
                                        turn_delivered,
                                    ));
                                }
                            }
                            state_dirty = true;
                        }
                        StreamMessage::ProcessReady {
                            output_path,
                            session_name,
                            last_offset,
                        } => {
                            // ProcessBackend completed first turn.
                            // No tmux watcher needed — process sessions are monitored
                            // inline via SessionProbe::process during read_output_file_until_result.
                            // Do NOT set tmux_handed_off: ProcessBackend has no watcher,
                            // so the handoff cleanup path would delete the placeholder
                            // with no one to send the final response.
                            tmux_last_offset = Some(last_offset);
                            inflight_state.tmux_session_name = Some(session_name);
                            inflight_state.output_path = Some(output_path);
                            inflight_state.last_offset = last_offset;
                            state_dirty = true;
                        }
                        StreamMessage::OutputOffset { offset } => {
                            tmux_last_offset = Some(offset);
                            inflight_state.last_offset = offset;
                            state_dirty = true;
                        }
                    },
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        rx_disconnected = true;
                        done = true;
                        break;
                    }
                }
            }

            let indicator = SPINNER[spin_idx % SPINNER.len()];
            spin_idx += 1;

            let current_portion = if response_sent_offset < full_response.len() {
                &full_response[response_sent_offset..]
            } else {
                ""
            };
            let status_block = super::formatting::build_placeholder_status_block(
                indicator,
                prev_tool_status.as_deref(),
                current_tool_line.as_deref(),
                &full_response,
                narrate_progress,
            );
            let footer = format!("\n\n{status_block}");
            let body_budget = DISCORD_MSG_LIMIT.saturating_sub(footer.len() + 10);
            let normalized = normalize_empty_lines(current_portion);
            let stable_display_text = if current_portion.is_empty() {
                status_block.clone()
            } else {
                let body = tail_with_ellipsis(&normalized, body_budget.max(1));
                format!("{}{}", body, footer)
            };

            if stable_display_text != last_edit_text
                && !done
                && last_status_edit.elapsed() >= status_interval
            {
                rate_limit_wait(&shared_owned, channel_id).await;
                let _ = channel_id
                    .edit_message(
                        &http,
                        current_msg_id,
                        EditMessage::new().content(&stable_display_text),
                    )
                    .await;
                last_edit_text = stable_display_text;
                last_status_edit = tokio::time::Instant::now();
                inflight_state.current_msg_id = current_msg_id.get();
                inflight_state.current_msg_len = last_edit_text.len();
                inflight_state.response_sent_offset = response_sent_offset;
                inflight_state.full_response = full_response.clone();
                state_dirty = true;
            }

            if state_dirty
                || inflight_state.current_tool_line != current_tool_line
                || inflight_state.prev_tool_status != prev_tool_status
            {
                inflight_state.current_tool_line = current_tool_line.clone();
                inflight_state.prev_tool_status = prev_tool_status.clone();
                let _ = save_inflight_state(&inflight_state);
            }

            if last_adk_heartbeat.elapsed() >= std::time::Duration::from_secs(30) {
                post_adk_session_status(
                    adk_session_key.as_deref(),
                    adk_session_name.as_deref(),
                    Some(provider.as_str()),
                    "working",
                    &provider,
                    adk_session_info.as_deref(),
                    None,
                    adk_cwd.as_deref(),
                    dispatch_id.as_deref(),
                    adk_session_name.as_deref().and_then(
                        crate::services::discord::adk_session::parse_thread_channel_id_from_name,
                    ),
                    shared_owned.api_port,
                )
                .await;
                last_adk_heartbeat = std::time::Instant::now();
            }
        }

        let is_prompt_too_long = full_response.contains("__prompt too long__");
        let review_dispatch_warning = if !cancelled && !is_prompt_too_long {
            guard_review_dispatch_completion(
                shared_owned.api_port,
                dispatch_id.as_deref(),
                &full_response,
                provider.as_str(),
            )
            .await
        } else {
            None
        };

        // Explicitly complete implementation/rework dispatches before sending idle.
        // These types are NOT auto-completed by the session idle hook — they require
        // this explicit PATCH call so the pipeline can advance.
        // Skip if: cancelled, prompt too long, or transport error.
        // transport_error is set by StreamMessage::Error — not substring matching.
        if !cancelled && !is_prompt_too_long && !transport_error {
            complete_work_dispatch_on_turn_end(
                &shared_owned,
                dispatch_id.as_deref(),
                adk_cwd.as_deref(),
                Some(&full_response),
            )
            .await;
        } else if transport_error && !cancelled {
            // Transport error — fail the dispatch instead of completing
            fail_dispatch_with_retry(
                shared_owned.api_port,
                dispatch_id.as_deref(),
                &full_response,
            )
            .await;
        }

        post_adk_session_status(
            adk_session_key.as_deref(),
            adk_session_name.as_deref(),
            Some(provider.as_str()),
            "idle",
            &provider,
            adk_session_info.as_deref(),
            persisted_context_tokens(accumulated_input_tokens, accumulated_output_tokens),
            adk_cwd.as_deref(),
            dispatch_id.as_deref(),
            adk_session_name
                .as_deref()
                .and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name),
            shared_owned.api_port,
        )
        .await;

        let can_chain_locally =
            serenity_ctx.is_some() && request_owner.is_some() && token.is_some();
        // Mark this turn as finalizing — deferred restart must wait until we finish
        // sending the Discord response and cleaning up state.
        shared_owned
            .finalizing_turns
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        shared_owned
            .global_finalizing
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let finish = super::mailbox_finish_turn(&shared_owned, &provider, channel_id).await;
        if let Some(removed_token) = finish.removed_token {
            // Mark the token as cancelled so any lingering watchdog timer exits cleanly
            // instead of mistakenly firing on a newer turn's token.
            removed_token
                .cancelled
                .store(true, std::sync::atomic::Ordering::Relaxed);
            shared_owned
                .global_active
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
        // Clean up any pending watchdog deadline override for this channel
        super::clear_watchdog_deadline_override(channel_id.get());
        // Clean up dispatch-thread parent mapping when the thread turn ends.
        // Iterate and remove entries whose thread matches this channel_id.
        shared_owned
            .dispatch_thread_parents
            .retain(|_, thread| *thread != channel_id);
        // Keep the override while queued turns remain so review/reused-thread routing
        // survives restart-preserve and same-runtime dequeue paths.
        if !finish.has_pending {
            shared_owned.dispatch_role_overrides.remove(&channel_id);
        }
        let has_queued_turns = finish.has_pending;

        // Remove ⏳ only if NOT handing off to tmux watcher.
        // When tmux watcher is handling the response, it will do ⏳→✅ after delivery.
        let tmux_handoff_path = rx_disconnected && tmux_handed_off && full_response.is_empty();
        if !tmux_handoff_path {
            remove_reaction_raw(&http, channel_id, user_msg_id, '⏳').await;
        }

        // Recovery auto-retry: session died during restart recovery
        if recovery_retry {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!(
                "  [{ts}] ↻ Recovery session died — triggering auto-retry with history (channel {})",
                channel_id
            );
            reset_session_for_auto_retry(
                &shared_owned,
                channel_id,
                &cancel_token,
                adk_session_key.as_deref(),
                &mut new_session_id,
                &mut inflight_state,
                "recovery session died",
            )
            .await;
            // Auto-retry with Discord history
            let http_c = http.clone();
            let retry_text = user_text_owned.clone();
            let retry_port = shared_owned.api_port;
            tokio::spawn(async move {
                auto_retry_with_history(&http_c, channel_id, &retry_text, retry_port).await;
            });
            // Replace placeholder with recovery notice (don't delete — avoids visual gap)
            let _ = channel_id
                .edit_message(
                    &http,
                    current_msg_id,
                    serenity::EditMessage::new()
                        .content("↻ 세션 복구 중... 잠시 후 자동으로 이어갑니다."),
                )
                .await;
            full_response = String::new();
        }

        if restart_recovery_handoff {
            #[cfg(unix)]
            {
                let best_response =
                    if full_response == super::recovery::RESTART_SESSION_DIED_HANDOFF_SENTINEL {
                        String::new()
                    } else {
                        full_response.clone()
                    };
                let handed_off = super::tmux::start_restart_handoff_from_state(
                    channel_id,
                    &http,
                    &shared_owned,
                    &provider,
                    inflight_state.clone(),
                    &best_response,
                )
                .await;
                if handed_off {
                    full_response = String::new();
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    eprintln!(
                        "  [{ts}] ↻ Recovery session died — queued internal handoff instead of Discord auto-retry (channel {})",
                        channel_id
                    );
                } else {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    eprintln!(
                        "  [{ts}] ⚠ Recovery session died — internal handoff failed, falling back to auto-retry (channel {})",
                        channel_id
                    );
                    reset_session_for_auto_retry(
                        &shared_owned,
                        channel_id,
                        &cancel_token,
                        adk_session_key.as_deref(),
                        &mut new_session_id,
                        &mut inflight_state,
                        "restart recovery handoff failed",
                    )
                    .await;
                    let http_c = http.clone();
                    let retry_text = user_text_owned.clone();
                    let retry_port = shared_owned.api_port;
                    tokio::spawn(async move {
                        auto_retry_with_history(&http_c, channel_id, &retry_text, retry_port).await;
                    });
                    let _ = channel_id
                        .edit_message(
                            &http,
                            current_msg_id,
                            serenity::EditMessage::new()
                                .content("↻ 세션 복구 중... 잠시 후 자동으로 이어갑니다."),
                        )
                        .await;
                    full_response = String::new();
                }
            }
        } else if cancelled {
            if let Ok(guard) = cancel_token.child_pid.lock() {
                if let Some(pid) = *guard {
                    crate::services::process::kill_pid_tree(pid);
                }
            }

            full_response = if full_response.trim().is_empty() {
                "[Stopped]".to_string()
            } else {
                let formatted =
                    super::formatting::format_for_discord_with_provider(&full_response, &provider);
                format!("{}\n\n[Stopped]", formatted)
            };

            rate_limit_wait(&shared_owned, channel_id).await;
            let _ = super::formatting::replace_long_message_raw(
                &http,
                channel_id,
                current_msg_id,
                &full_response,
                &shared_owned,
            )
            .await;

            add_reaction_raw(&http, channel_id, user_msg_id, '🛑').await;

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ■ Stopped");
        } else if is_prompt_too_long {
            let mention = request_owner
                .map(|uid| format!("<@{}>", uid.get()))
                .unwrap_or_default();
            full_response = format!(
                "{} ⚠️ 프롬프트가 너무 깁니다. 대화 컨텍스트가 모델 한도를 초과했습니다.\n\n\
                 다음 메시지를 보내면 자동으로 새 턴이 시작됩니다.\n\
                 컨텍스트를 줄이려면 `/compact` 또는 `/clear`를 사용해 주세요.",
                mention
            );
            rate_limit_wait(&shared_owned, channel_id).await;
            let _ = super::formatting::replace_long_message_raw(
                &http,
                channel_id,
                current_msg_id,
                &full_response,
                &shared_owned,
            )
            .await;

            add_reaction_raw(&http, channel_id, user_msg_id, '⚠').await;

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ⚠ Prompt too long (channel {})", channel_id);
        } else if rx_disconnected && tmux_handed_off && full_response.is_empty() {
            // Tmux watcher is handling response delivery — this is normal.
            // Don't delete placeholder — update it so the user sees the turn is still active.
            // The tmux watcher will replace this content when output arrives.
            let _ = channel_id
                .edit_message(
                    &http,
                    current_msg_id,
                    serenity::builder::EditMessage::new().content("⏳ 처리 중..."),
                )
                .await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!(
                "  [{ts}] ✓ tmux handoff complete, placeholder cleaned up, watcher handles response (channel {})",
                channel_id
            );
        } else {
            // Check for stale resume failure BEFORE any other response handling.
            // This path is driven by explicit error/result events, not assistant text.
            if resume_failure_detected {
                let ts = chrono::Local::now().format("%H:%M:%S");
                eprintln!(
                    "  [{ts}] ⚠ Resume failed (error in response), clearing session_id (channel {})",
                    channel_id
                );
                reset_session_for_auto_retry(
                    &shared_owned,
                    channel_id,
                    &cancel_token,
                    adk_session_key.as_deref(),
                    &mut new_session_id,
                    &mut inflight_state,
                    "resume failed in response output",
                )
                .await;
                // Auto-retry with Discord history context
                let http_c = http.clone();
                let retry_text = user_text_owned.clone();
                let retry_port = shared_owned.api_port;
                tokio::spawn(async move {
                    auto_retry_with_history(&http_c, channel_id, &retry_text, retry_port).await;
                });
                full_response = String::new(); // Suppress error message to user
            } else if full_response.is_empty() {
                // Fallback: try to extract response from tmux output file
                if let Some(ref path) = inflight_state.output_path {
                    let recovered = super::recovery::extract_response_from_output_pub(
                        path,
                        inflight_state.last_offset,
                    );
                    if !recovered.trim().is_empty() {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        eprintln!(
                            "  [{ts}] ↻ Recovered {} chars from output file for channel {}",
                            recovered.len(),
                            channel_id
                        );
                        full_response = recovered;
                    }
                }

                // Check for stale resume failure in recovered output
                let stale_resume_in_output = inflight_state
                    .output_path
                    .as_deref()
                    .map(|path| {
                        output_file_has_stale_resume_error_after_offset(
                            path,
                            inflight_state.last_offset,
                        )
                    })
                    .unwrap_or(false);
                if stale_resume_in_output {
                    resume_failure_detected = true;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    eprintln!(
                        "  [{ts}] ⚠ Resume failed (stale session_id in recovered output), auto-retrying (channel {})",
                        channel_id
                    );
                    reset_session_for_auto_retry(
                        &shared_owned,
                        channel_id,
                        &cancel_token,
                        adk_session_key.as_deref(),
                        &mut new_session_id,
                        &mut inflight_state,
                        "stale session_id in recovered output",
                    )
                    .await;
                    let http_c = http.clone();
                    let retry_text = user_text_owned.clone();
                    let retry_port = shared_owned.api_port;
                    tokio::spawn(async move {
                        auto_retry_with_history(&http_c, channel_id, &retry_text, retry_port).await;
                    });
                    full_response = String::new();
                } else {
                    // Check for resume failure via other methods
                    let mut resume_failed = false;
                    let quick_exit = turn_start.elapsed().as_secs() < 10;
                    // Method 1: check tmux output file
                    if let Some(ref path) = inflight_state.output_path {
                        if output_file_has_stale_resume_error_after_offset(
                            path,
                            inflight_state.last_offset,
                        ) {
                            resume_failed = true;
                            resume_failure_detected = true;
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            eprintln!(
                                "  [{ts}] ⚠ Resume failed (stale session_id in output file), auto-retrying (channel {})",
                                channel_id
                            );
                            reset_session_for_auto_retry(
                                &shared_owned,
                                channel_id,
                                &cancel_token,
                                adk_session_key.as_deref(),
                                &mut new_session_id,
                                &mut inflight_state,
                                "stale session_id in output file",
                            )
                            .await;
                            let http_c = http.clone();
                            let retry_text = user_text_owned.clone();
                            let retry_port = shared_owned.api_port;
                            tokio::spawn(async move {
                                auto_retry_with_history(
                                    &http_c,
                                    channel_id,
                                    &retry_text,
                                    retry_port,
                                )
                                .await;
                            });
                            full_response = String::new();
                        }
                    }
                    // Method 2: quick exit (<10s) + empty response + had a session_id to resume
                    if !resume_failed && quick_exit && rx_disconnected {
                        let attempted_resume = {
                            let data = shared_owned.core.lock().await;
                            data.sessions
                                .get(&channel_id)
                                .and_then(|s| s.session_id.as_ref())
                                .is_some()
                        };
                        if attempted_resume {
                            resume_failed = true;
                            resume_failure_detected = true;
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            eprintln!(
                                "  [{ts}] ⚠ Quick exit with empty response — auto-retrying with fresh session (channel {})",
                                channel_id
                            );
                            reset_session_for_auto_retry(
                                &shared_owned,
                                channel_id,
                                &cancel_token,
                                adk_session_key.as_deref(),
                                &mut new_session_id,
                                &mut inflight_state,
                                "quick exit with empty response",
                            )
                            .await;
                            let http_c = http.clone();
                            let retry_text = user_text_owned.clone();
                            let retry_port = shared_owned.api_port;
                            tokio::spawn(async move {
                                auto_retry_with_history(
                                    &http_c,
                                    channel_id,
                                    &retry_text,
                                    retry_port,
                                )
                                .await;
                            });
                            full_response = String::new();
                        }
                    }
                    if !resume_failed {
                        if rx_disconnected {
                            full_response =
                                "(No response — 프로세스가 응답 없이 종료됨)".to_string();
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            eprintln!(
                                "  [{ts}] ⚠ Empty response: rx disconnected before any text \
                                 (channel {}, output_path={:?}, last_offset={})",
                                channel_id, inflight_state.output_path, inflight_state.last_offset
                            );
                        } else {
                            full_response = "(No response)".to_string();
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            eprintln!(
                                "  [{ts}] ⚠ Empty response: done without text (channel {})",
                                channel_id
                            );
                        }
                    }
                }
            }

            // If response is empty (e.g. auto-retry on stale session), show
            // recovery notice instead of deleting — avoids visual gap.
            if full_response.trim().is_empty() {
                let _ = channel_id
                    .edit_message(
                        &http,
                        current_msg_id,
                        serenity::EditMessage::new()
                            .content("↻ 세션 복구 중... 잠시 후 자동으로 이어갑니다."),
                    )
                    .await;
            } else {
                full_response =
                    super::formatting::format_for_discord_with_provider(&full_response, &provider);
                let _ = super::formatting::replace_long_message_raw(
                    &http,
                    channel_id,
                    current_msg_id,
                    &full_response,
                    &shared_owned,
                )
                .await;
            }

            // Signal the watcher that this turn's response was already delivered.
            // Prevents the watcher from relaying the same response when it resumes.
            if let Some(watcher) = shared_owned.tmux_watchers.get(&channel_id) {
                watcher.turn_delivered.store(true, Ordering::Relaxed);
            }

            if !full_response.trim().is_empty() {
                add_reaction_raw(&http, channel_id, user_msg_id, '✅').await;
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ▶ Response sent");
            if let Ok(mut last) = shared_owned.last_turn_at.lock() {
                *last = Some(chrono::Local::now().to_rfc3339());
            }

            if let Some(warning) = review_dispatch_warning.as_deref() {
                // Send via announce bot so the agent sees this as an external
                // message and re-triggers a turn to handle the pending review.
                // Using the provider bot (claude/codex) would be ignored as
                // the agent treats its own bot's messages as self-messages.
                let _ = reqwest::Client::new()
                    .post(crate::config::local_api_url(
                        shared_owned.api_port,
                        "/api/send",
                    ))
                    .json(&serde_json::json!({
                        "target": format!("channel:{}", channel_id),
                        "content": warning,
                        "source": "pipeline",
                        "bot": "announce",
                    }))
                    .send()
                    .await;
            }
        }

        if should_resume_watcher_after_turn(
            defer_watcher_resume,
            has_queued_turns,
            can_chain_locally,
        ) {
            if let Some(offset) = tmux_last_offset {
                if let Some(watcher) = shared_owned.tmux_watchers.get(&channel_id) {
                    if let Ok(mut guard) = watcher.resume_offset.lock() {
                        *guard = Some(offset);
                    }
                    // NOTE: turn_delivered is NOT cleared here — the watcher clears it
                    // when it consumes resume_offset, ensuring the flag stays active
                    // until the watcher actually starts reading from the new offset.
                    watcher.paused.store(false, Ordering::Relaxed);
                }
            }
        }

        let should_record_final_turn = !is_prompt_too_long
            && !resume_failure_detected
            && !recovery_retry
            && !restart_recovery_handoff
            && !(rx_disconnected && tmux_handed_off && full_response.is_empty())
            && !full_response.trim().is_empty();

        // Update in-memory session under lock.
        let mut should_persist_transcript = false;
        let mut should_spawn_memory_capture = false;
        let mut reflect_request = None;
        let capture_memory_settings = settings::memory_settings_for_binding(role_binding.as_ref());
        let session_id_to_persist = {
            let mut data = shared_owned.core.lock().await;
            if let Some(session) = data.sessions.get_mut(&channel_id) {
                if let Some(memory_plan) = plan_turn_end_memory(
                    session,
                    capture_memory_settings.backend,
                    is_prompt_too_long,
                    resume_failure_detected,
                    terminal_session_reset_required,
                    should_record_final_turn,
                ) {
                    if let Some(reason) = memory_plan.reflect_reason {
                        reflect_request = take_memento_reflect_request(
                            session,
                            &capture_memory_settings,
                            &provider,
                            role_binding.as_ref(),
                            channel_id.get(),
                            reason,
                        );
                    }
                    if memory_plan.clear_provider_session {
                        session.clear_provider_session();
                    } else if let Some(sid) = new_session_id.as_ref() {
                        session.restore_provider_session(Some(sid.clone()));
                    }
                    if memory_plan.persist_transcript {
                        session.history.push(HistoryItem {
                            item_type: HistoryType::User,
                            content: user_text_owned.clone(),
                        });
                        session.history.push(HistoryItem {
                            item_type: HistoryType::Assistant,
                            content: full_response.clone(),
                        });
                        should_persist_transcript = true;
                    }
                    should_spawn_memory_capture = memory_plan.spawn_capture;
                    session.session_id.clone()
                } else {
                    None
                }
            } else {
                None
            }
        };

        // Persist provider session_id to DB so it survives dcserver restarts.
        if !resume_failure_detected && !terminal_session_reset_required {
            if let (Some(session_key), Some(persisted_sid)) =
                (adk_session_key.as_deref(), session_id_to_persist.as_deref())
            {
                super::adk_session::save_provider_session_id(
                    session_key,
                    persisted_sid,
                    &provider,
                    shared_owned.api_port,
                )
                .await;
            }
        } else if terminal_session_reset_required {
            if let Some(ref session_key) = adk_session_key {
                super::adk_session::clear_provider_session_id(session_key, shared_owned.api_port)
                    .await;
            }
        }

        if should_persist_transcript {
            if let Some(db) = shared_owned.db.as_ref() {
                let turn_id = format!("discord:{}:{}", channel_id.get(), user_msg_id.get());
                let channel_id_text = channel_id.get().to_string();
                if let Err(e) = crate::db::session_transcripts::persist_turn(
                    db,
                    crate::db::session_transcripts::PersistSessionTranscript {
                        turn_id: &turn_id,
                        session_key: adk_session_key.as_deref(),
                        channel_id: Some(channel_id_text.as_str()),
                        agent_id: None,
                        provider: Some(provider.as_str()),
                        dispatch_id: dispatch_id.as_deref(),
                        user_message: &user_text_owned,
                        assistant_message: &full_response,
                    },
                ) {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    eprintln!("  [{ts}] ⚠ failed to persist session transcript: {e}");
                }
            }
        }

        let mut background_memory_task = None;
        if let Some(reflect_request) = reflect_request {
            background_memory_task = Some(spawn_memory_reflect_task(
                channel_id,
                capture_memory_settings.clone(),
                reflect_request,
            ));
        }
        if should_spawn_memory_capture {
            let capture_request = CaptureRequest {
                provider: provider.clone(),
                role_id: resolve_memory_role_id(role_binding.as_ref()),
                channel_id: channel_id.get(),
                session_id: resolve_memory_session_id(
                    session_id_to_persist.as_deref(),
                    channel_id.get(),
                ),
                dispatch_id: dispatch_id.clone(),
                user_text: user_text_owned.clone(),
                assistant_text: full_response.clone(),
            };
            background_memory_task = Some(spawn_memory_capture_task(
                channel_id,
                capture_memory_settings,
                capture_request,
            ));
        }

        if let Some(memory_task) = background_memory_task {
            match memory_task.await {
                Ok(result) => {
                    accumulated_memory_input_tokens = accumulated_memory_input_tokens
                        .saturating_add(result.token_usage.input_tokens);
                    accumulated_memory_output_tokens = accumulated_memory_output_tokens
                        .saturating_add(result.token_usage.output_tokens);
                }
                Err(err) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    eprintln!(
                        "  [{ts}] [memory] background task join failed for channel {}: {}",
                        channel_id.get(),
                        err
                    );
                }
            }
        }

        {
            let duration = shared_owned
                .turn_start_times
                .remove(&channel_id)
                .map(|(_, start)| start.elapsed().as_secs_f64())
                .unwrap_or(0.0);
            let memory_usage = TokenUsage {
                input_tokens: accumulated_memory_input_tokens,
                output_tokens: accumulated_memory_output_tokens,
            };
            let (memory_input_tokens, memory_output_tokens) =
                optional_metric_token_fields(memory_usage);
            let provider_name = {
                let settings = shared_owned.settings.read().await;
                settings.provider.as_str().to_string()
            };
            super::metrics::record_turn(&super::metrics::TurnMetric {
                channel_id: channel_id.get(),
                provider: provider_name,
                timestamp: chrono::Local::now().to_rfc3339(),
                duration_secs: duration,
                model: None, // model info from StatusUpdate not yet accumulated in turn_bridge
                input_tokens: if accumulated_input_tokens > 0 {
                    Some(accumulated_input_tokens)
                } else {
                    None
                },
                output_tokens: if accumulated_output_tokens > 0 {
                    Some(accumulated_output_tokens)
                } else {
                    None
                },
                memory_input_tokens,
                memory_output_tokens,
            });
        }

        // Clear restart report BEFORE clearing inflight state (which removes
        // the cancel token) to prevent the flush loop from processing the
        // report in the gap between cancel token removal and report deletion.
        if restart_followup_pending {
            clear_restart_report(&provider, channel_id.get());
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ✓ Cleared restart report for channel {} (turn completed normally)",
                channel_id
            );
        }

        clear_inflight_state(&provider, channel_id.get());
        // Defuse the guard — cleanup already done above.
        inflight_guard.provider.take();
        shared_owned.recovering_channels.remove(&channel_id);

        // For dispatch-based turns (threads), kill the tmux session after
        // finalization. Thread sessions are one-shot — keeping claude alive
        // in "Ready for input" blocks idle detection and the auto-complete pipeline.
        //
        // Exception (#145): unified-thread auto-queue runs reuse the same thread
        // session across multiple entries. Skip kill if the run is still active.
        #[cfg(unix)]
        if dispatch_id.is_some() {
            let should_kill = if let Some(ref did) = dispatch_id {
                !crate::dispatch::is_unified_thread_active(did)
            } else {
                true
            };
            if should_kill {
                if let Some(ref name) = cancel_token
                    .tmux_session
                    .lock()
                    .ok()
                    .and_then(|g| g.clone())
                {
                    record_tmux_exit_reason(
                        name,
                        "dispatch turn completed — killing thread session",
                    );
                    let name_c = name.to_string();
                    let kill_result = tokio::task::spawn_blocking(move || {
                        crate::services::platform::tmux::kill_session_output(&name_c)
                    })
                    .await;
                    let kill_ok = matches!(&kill_result, Ok(Ok(o)) if o.status.success());
                    if !kill_ok {
                        match &kill_result {
                            Ok(Ok(o)) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                eprintln!(
                                    "  [{ts}] ⚠ tmux kill-session failed for {}: {}",
                                    name,
                                    String::from_utf8_lossy(&o.stderr)
                                );
                            }
                            Ok(Err(e)) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                eprintln!("  [{ts}] ⚠ tmux kill-session error for {name}: {e}");
                            }
                            Err(e) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                eprintln!(
                                    "  [{ts}] ⚠ tmux kill-session spawn error for {name}: {e}"
                                );
                            }
                        }
                    }

                    // Only delete the DB session row if tmux kill succeeded.
                    // If kill failed, leave the row so the periodic reaper can retry.
                    if kill_ok {
                        if let Some(session_key) = super::adk_session::build_adk_session_key(
                            &shared_owned,
                            channel_id,
                            &provider,
                        )
                        .await
                        {
                            super::adk_session::delete_adk_session(
                                &session_key,
                                shared_owned.api_port,
                            )
                            .await;
                        }
                    }
                }
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] ♻ Skipping tmux kill for unified-thread dispatch {:?} — run still active",
                    dispatch_id
                );
            }
        }

        // Finalization complete — decrement counters
        shared_owned
            .finalizing_turns
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        shared_owned
            .global_finalizing
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        // Note: deferred restart exit is handled by the 5-second poll loop in mod.rs,
        // which saves pending queues before calling check_deferred_restart.
        // Calling it here would risk exiting before other providers save their queues.

        if has_queued_turns {
            // Drain mode: if restart is pending, don't start new turns from queue.
            // The queued messages will be saved to disk and processed after restart.
            if shared_owned
                .restart_pending
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] ⏸ DRAIN: skipping queued turn dequeue for channel {} (restart pending)",
                    channel_id
                );
            } else if let (Some(ctx), Some(owner), Some(tok)) =
                (serenity_ctx.as_ref(), request_owner, token.as_deref())
            {
                let bot_owner_provider = super::resolve_discord_bot_provider(tok);
                let settings_snapshot = shared_owned.settings.read().await.clone();
                if let Err(reason) = super::validate_live_channel_routing(
                    ctx,
                    &bot_owner_provider,
                    &settings_snapshot,
                    channel_id,
                )
                .await
                {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] ⚠ QUEUE-GUARD: preserving queued command(s) for channel {} (reason={})",
                        channel_id, reason
                    );
                } else {
                    let next_intervention = super::mailbox_take_next_soft_intervention(
                        &shared_owned,
                        &bot_owner_provider,
                        channel_id,
                    )
                    .await;

                    if let Some((intervention, has_more_queued_turns)) = next_intervention {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts}] 📋 Processing next queued command");
                        // Remove 📬 (queued) reaction before processing
                        remove_reaction_raw(&http, channel_id, intervention.message_id, '📬').await;
                        if let Err(e) = handle_text_message(
                            ctx,
                            channel_id,
                            intervention.message_id,
                            owner,
                            &request_owner_name,
                            &intervention.text,
                            &shared_owned,
                            tok,
                            true,
                            has_more_queued_turns,
                            true,
                            None,
                        )
                        .await
                        {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            println!("  [{ts}]   ⚠ queued command failed: {e}");
                            super::mailbox_requeue_intervention_front(
                                &shared_owned,
                                &bot_owner_provider,
                                channel_id,
                                intervention,
                            )
                            .await;
                        }
                    }
                }
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] 📦 preserving queued command(s): missing live Discord context — scheduling deferred drain"
                );
                if let Some(offset) = tmux_last_offset {
                    if let Some(watcher) = shared_owned.tmux_watchers.get(&channel_id) {
                        if let Ok(mut guard) = watcher.resume_offset.lock() {
                            *guard = Some(offset);
                        }
                        watcher.paused.store(false, Ordering::Relaxed);
                    }
                }
                super::schedule_deferred_idle_queue_kickoff(
                    shared_owned.clone(),
                    provider.clone(),
                    channel_id,
                    "turn bridge queued backlog",
                );
            }
        }

        // completion_tx is sent automatically by CompletionGuard on drop
    });
}
