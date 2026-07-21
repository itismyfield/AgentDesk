use std::sync::mpsc::Sender;

use crate::services::agent_protocol::StreamMessage;
use crate::services::claude::{
    ClaudeFollowupResult, classify_followup_result, claude_tui_turn_start_offset_after_timestamp,
    debug_log, emit_claude_tui_watcher_handoff, emit_followup_restart_suppressed_notice,
    fresh_claude_tui_session_resolution, log_producer_exit, read_claude_tui_transcript_until_done,
    read_output_result_kind, tui_delivered_zero_harvest,
};
use crate::services::provider::{CancelToken, ReadOutputResult};
use crate::services::tmux_diagnostics::record_tmux_exit_reason;

use super::followup_support::{
    ClaudeTuiStrandedPromptDraftState, ClaudeTuiWarmFollowupSubmitPlan,
    claude_tui_followup_busy_before_submit, claude_tui_followup_stranded_prompt_draft_state,
    claude_tui_prompt_remained_in_input_buffer,
    claude_tui_unknown_transcript_draft_recreate_allowed, claude_tui_warm_followup_submit_plan,
    claude_tui_zero_advance_input_buffer_error, clear_claude_tui_stranded_prompt_draft,
    emit_claude_tui_busy_followup_notice, emit_claude_tui_zero_harvest,
    gently_clear_claude_tui_prompt_draft,
};

/// Updated session resolution carried back to the orchestrator when a warm
/// follow-up could not be delivered and the hosted session must be recreated.
#[cfg(unix)]
pub(crate) struct ClaudeTuiRecreateState {
    pub(crate) resolved_session_id: String,
    pub(crate) transcript_path: std::path::PathBuf,
    pub(crate) transcript_path_string: String,
    pub(crate) resume: bool,
}

/// Outcome of stranded prompt-draft recovery; `Terminal` forwards original Ok/Err exits before submit side effects, and `Proceed` carries the fall-through quartet/flags.
#[cfg(unix)]
#[must_use]
pub(crate) enum ClaudeTuiDraftRecoveryOutcome {
    /// Continue to submit-plan computation with the possibly recreated quartet plus busy/recreate/draft-cleared flags.
    Proceed {
        state: ClaudeTuiRecreateState,
        busy_waited: bool,
        recreate_before_submit: bool,
        prompt_draft_cleared_before_submit: bool,
    },
    /// Recovery hit an original early return; forward the `Result` verbatim.
    Terminal(Result<(), String>),
}

/// Outcome of an attempted warm follow-up against a live Claude TUI session.
#[cfg(unix)]
pub(crate) enum ClaudeTuiWarmFollowupOutcome {
    /// Handled (delivered, aborted, or errored); return this without launch.
    Terminal(Result<(), String>),
    /// Could not proceed; fall through to fresh launch with this resolution.
    Recreate(ClaudeTuiRecreateState),
}

/// Verbatim extraction of the warm-followup submit-and-stream block; `Terminal` carries the original early-return `Result` unchanged.
/// `FallThroughRecreate` preserves `submit_existing_session == false` and `RecreateSession` kill-then-fresh-launch fall-through.
#[cfg(unix)]
#[must_use]
enum ClaudeTuiWarmFollowupSubmitOutcome {
    Terminal(Result<(), String>),
    FallThroughRecreate,
}

/// Recover from a stranded prompt draft left in the composer before submit.
/// Verbatim extraction: destructures state into the original mutable locals; cancellation returns `Ok(())`, fresh-resolution failures return `Err(..)`, and fall-through returns `Proceed` with quartet/flags.
#[cfg(unix)]
fn recover_claude_tui_stranded_prompt_draft(
    state: ClaudeTuiRecreateState,
    working_dir_path: &std::path::Path,
    cancel_token: &Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    report_channel_id: Option<u64>,
) -> ClaudeTuiDraftRecoveryOutcome {
    let ClaudeTuiRecreateState {
        mut resolved_session_id,
        mut transcript_path,
        mut transcript_path_string,
        mut resume,
    } = state;
    // #2416: a single busy_waited flag tells the offset-capture site below
    // that we need to re-read transcript length after the wait succeeded,
    // because the previous TUI turn may have appended bytes while we were
    // waiting (otherwise the follow-up reader would treat previous-turn
    // bytes as new-turn output).
    let mut busy_waited = false;
    let mut recreate_before_submit = false;
    let mut prompt_draft_cleared_before_submit = false;
    if let Some(snapshot) =
        claude_tui_followup_busy_before_submit(tmux_session_name, Some(&transcript_path))
    {
        if let Some(draft_state) =
            claude_tui_followup_stranded_prompt_draft_state(&snapshot, &transcript_path)
        {
            let allow_recreate = matches!(
                draft_state,
                ClaudeTuiStrandedPromptDraftState::IdleTranscript
            );
            tracing::warn!(
                tmux_session_name,
                transcript_path = %transcript_path_string,
                transcript_turn_state = draft_state.as_str(),
                prompt_marker_detected = snapshot.prompt_marker_detected,
                prompt_draft_detected = snapshot.prompt_draft_detected,
                capture_available = snapshot.capture_available,
                pane_tail = %snapshot.pane_tail,
                "claude_tui follow-up found non-busy transcript with stranded composer draft; attempting draft clear"
            );
            debug_log(&format!(
                "Claude TUI follow-up: {} transcript has stranded prompt draft, attempting clear (session={}, transcript={})",
                draft_state.as_str(),
                tmux_session_name,
                transcript_path_string
            ));
            // F1: route the stranded-draft clear through the SAME composer
            // mutation lock `/compact` steering holds, so this clear and a
            // busy-pane auto `/compact` can never interleave their key sends
            // (the race that let a draft-clear mistake a just-typed `/compact`
            // literal for a stranded draft and soak it up). This runs on the
            // warm-followup recovery path, OUTSIDE any composer critical section
            // (the submit lock is acquired later, inside
            // `send_followup_prompt_or_idle_transcript`), so it is the outermost
            // composer acquisition here — no re-entry, no deadlock.
            let clear_result = crate::services::claude_tui::input::with_composer_cleanup_lock(
                tmux_session_name,
                || {
                    if allow_recreate {
                        clear_claude_tui_stranded_prompt_draft(
                            tmux_session_name,
                            cancel_token.as_deref(),
                        )
                    } else {
                        gently_clear_claude_tui_prompt_draft(
                            tmux_session_name,
                            cancel_token.as_deref(),
                        )
                    }
                },
            );
            match clear_result {
                Ok(post_clear_snapshot)
                    if post_clear_snapshot.tmux_pane_alive
                        && !post_clear_snapshot.prompt_draft_detected =>
                {
                    busy_waited = true;
                    prompt_draft_cleared_before_submit = true;
                    tracing::info!(
                        tmux_session_name,
                        transcript_turn_state = draft_state.as_str(),
                        prompt_marker_detected = post_clear_snapshot.prompt_marker_detected,
                        capture_available = post_clear_snapshot.capture_available,
                        "claude_tui stranded prompt draft cleared before follow-up submit"
                    );
                    debug_log(&format!(
                        "Claude TUI follow-up: stranded prompt draft cleared (session={} prompt_marker_detected={} capture_available={})",
                        tmux_session_name,
                        post_clear_snapshot.prompt_marker_detected,
                        post_clear_snapshot.capture_available
                    ));
                }
                Ok(post_clear_snapshot) => {
                    let reason = if post_clear_snapshot.tmux_pane_alive {
                        "stranded claude tui prompt draft persisted after clear attempts"
                    } else {
                        "claude tui pane died while clearing stranded prompt draft"
                    };
                    let recreate_after_persistent_draft = allow_recreate
                        || (matches!(
                            draft_state,
                            ClaudeTuiStrandedPromptDraftState::UnknownTranscript
                        ) && claude_tui_unknown_transcript_draft_recreate_allowed(
                            &post_clear_snapshot,
                        ));
                    if !recreate_after_persistent_draft {
                        tracing::warn!(
                            tmux_session_name,
                            transcript_turn_state = draft_state.as_str(),
                            prompt_marker_detected = post_clear_snapshot.prompt_marker_detected,
                            prompt_draft_detected = post_clear_snapshot.prompt_draft_detected,
                            tmux_pane_alive = post_clear_snapshot.tmux_pane_alive,
                            capture_available = post_clear_snapshot.capture_available,
                            pane_tail = %post_clear_snapshot.pane_tail,
                            "claude_tui unknown-transcript draft recovery did not clear draft; falling back to busy wait"
                        );
                        debug_log(&format!(
                            "Claude TUI follow-up: {} under unknown transcript; falling back to busy wait (session={})",
                            reason, tmux_session_name
                        ));
                    } else {
                        tracing::warn!(
                            tmux_session_name,
                            prompt_marker_detected = post_clear_snapshot.prompt_marker_detected,
                            prompt_draft_detected = post_clear_snapshot.prompt_draft_detected,
                            tmux_pane_alive = post_clear_snapshot.tmux_pane_alive,
                            capture_available = post_clear_snapshot.capture_available,
                            pane_tail = %post_clear_snapshot.pane_tail,
                            "claude_tui stranded prompt draft recovery will recreate hosted session"
                        );
                        debug_log(&format!(
                            "Claude TUI follow-up: {} (session={})",
                            reason, tmux_session_name
                        ));
                        crate::services::termination_audit::record_termination_for_tmux(
                            tmux_session_name,
                            None,
                            "claude_tui_provider",
                            "stranded_prompt_draft_recreate",
                            Some(reason),
                            None,
                        );
                        record_tmux_exit_reason(tmux_session_name, reason);
                        crate::services::platform::tmux::kill_session(tmux_session_name, reason);
                        let fresh_resolution =
                            match fresh_claude_tui_session_resolution(working_dir_path, None) {
                                Ok(resolution) => resolution,
                                Err(error) => {
                                    return ClaudeTuiDraftRecoveryOutcome::Terminal(Err(error));
                                }
                            };
                        resolved_session_id = fresh_resolution.session_id;
                        transcript_path = fresh_resolution.transcript_path;
                        transcript_path_string = transcript_path.display().to_string();
                        resume = fresh_resolution.resume;
                        recreate_before_submit = true;
                    }
                }
                Err(error)
                    if crate::services::claude_tui::input::is_prompt_ready_cancelled_error(
                        &error,
                    ) =>
                {
                    debug_log(&format!(
                        "Claude TUI follow-up: cancellation observed while clearing stranded prompt draft (session={})",
                        tmux_session_name
                    ));
                    log_producer_exit(
                        "tui_warm_followup_cancelled_during_draft_clear",
                        Some(&resolved_session_id),
                        report_channel_id,
                        0,
                        serde_json::json!({
                            "tmux_session_name": tmux_session_name,
                            "transcript_path": transcript_path_string,
                        }),
                    );
                    return ClaudeTuiDraftRecoveryOutcome::Terminal(Ok(()));
                }
                Err(error) => {
                    let recreate_after_clear_error = allow_recreate
                        || (matches!(
                            draft_state,
                            ClaudeTuiStrandedPromptDraftState::UnknownTranscript
                        ) && claude_tui_unknown_transcript_draft_recreate_allowed(&snapshot));
                    if !recreate_after_clear_error {
                        tracing::warn!(
                            tmux_session_name,
                            error = %error,
                            "claude_tui unknown-transcript draft clear failed; falling back to busy wait"
                        );
                        debug_log(&format!(
                            "Claude TUI follow-up: unknown-transcript draft clear failed, falling back to busy wait (session={} error={})",
                            tmux_session_name, error
                        ));
                    } else {
                        tracing::warn!(
                            tmux_session_name,
                            error = %error,
                            "claude_tui stranded prompt draft clear failed; recreating hosted session"
                        );
                        crate::services::termination_audit::record_termination_for_tmux(
                            tmux_session_name,
                            None,
                            "claude_tui_provider",
                            "stranded_prompt_draft_clear_failed_recreate",
                            Some(&format!(
                                "claude tui stranded prompt draft clear failed: {}",
                                error
                            )),
                            None,
                        );
                        record_tmux_exit_reason(
                            tmux_session_name,
                            &format!("claude tui stranded prompt draft clear failed: {}", error),
                        );
                        crate::services::platform::tmux::kill_session(
                            tmux_session_name,
                            &format!("claude tui stranded prompt draft clear failed: {}", error),
                        );
                        let fresh_resolution =
                            match fresh_claude_tui_session_resolution(working_dir_path, None) {
                                Ok(resolution) => resolution,
                                Err(error) => {
                                    return ClaudeTuiDraftRecoveryOutcome::Terminal(Err(error));
                                }
                            };
                        resolved_session_id = fresh_resolution.session_id;
                        transcript_path = fresh_resolution.transcript_path;
                        transcript_path_string = transcript_path.display().to_string();
                        resume = fresh_resolution.resume;
                        recreate_before_submit = true;
                    }
                }
            }
        }
    }
    ClaudeTuiDraftRecoveryOutcome::Proceed {
        state: ClaudeTuiRecreateState {
            resolved_session_id,
            transcript_path,
            transcript_path_string,
            resume,
        },
        busy_waited,
        recreate_before_submit,
        prompt_draft_cleared_before_submit,
    }
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn run_claude_tui_warm_followup_submit_and_stream(
    submit_plan: ClaudeTuiWarmFollowupSubmitPlan,
    mut busy_waited: bool,
    tmux_session_name: &str,
    resolved_session_id: &str,
    transcript_path: &std::path::Path,
    transcript_path_string: &str,
    prompt: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    report_channel_id: Option<u64>,
    hook_rx: tokio::sync::broadcast::Receiver<crate::services::claude_tui::hook_server::HookEvent>,
) -> ClaudeTuiWarmFollowupSubmitOutcome {
    if submit_plan.recheck_busy_before_submit
        && let Some(snapshot) =
            claude_tui_followup_busy_before_submit(tmux_session_name, Some(&transcript_path))
    {
        // #2416: instead of dropping the user's message when the TUI is busy,
        // wait for the next prompt-ready window. The transcript-idle
        // fallback covers Claude TUI redraws where the JSONL terminal
        // envelope is authoritative but the prompt glyph is not visible.
        match crate::services::claude_tui::input::wait_for_prompt_ready_or_idle_transcript(
            tmux_session_name,
            crate::services::claude_tui::input::PromptReadinessKind::ProvenWarmFollowup,
            cancel_token.as_deref(),
            &transcript_path,
        ) {
            Ok(()) => {
                busy_waited = true;
                debug_log(&format!(
                    "Claude TUI follow-up: busy at first check, became ready after wait (session={})",
                    tmux_session_name
                ));
            }
            Err(err) => {
                if crate::services::claude_tui::input::is_prompt_ready_cancelled_error(&err) {
                    debug_log(&format!(
                        "Claude TUI follow-up: cancellation observed during busy wait, aborting injection (session={})",
                        tmux_session_name
                    ));
                    log_producer_exit(
                        "tui_warm_followup_cancelled_during_busy_wait",
                        Some(&resolved_session_id),
                        report_channel_id,
                        0,
                        serde_json::json!({
                            "tmux_session_name": tmux_session_name,
                            "transcript_path": transcript_path_string,
                        }),
                    );
                    return ClaudeTuiWarmFollowupSubmitOutcome::Terminal(Ok(()));
                }
                let timed_out =
                    crate::services::claude_tui::input::is_prompt_ready_timeout_error(&err);
                debug_log(&format!(
                    "Claude TUI follow-up wait failed after busy snapshot (session={}, timed_out={}): {}",
                    tmux_session_name, timed_out, err
                ));
                let post_wait_snapshot =
                    crate::services::claude_tui::input::prompt_readiness_snapshot(
                        tmux_session_name,
                    );
                let requeue_for_retry = claude_tui_followup_wait_error_requeue_for_retry(&err);
                emit_claude_tui_busy_followup_notice(
                    &sender,
                    tmux_session_name,
                    &post_wait_snapshot,
                    requeue_for_retry,
                );
                log_producer_exit(
                    "tui_warm_followup_busy_pre_submit",
                    Some(&resolved_session_id),
                    report_channel_id,
                    0,
                    serde_json::json!({
                        "tmux_session_name": tmux_session_name,
                        "transcript_path": transcript_path_string,
                        "prompt_marker_detected": post_wait_snapshot.prompt_marker_detected,
                        "prompt_draft_detected": post_wait_snapshot.prompt_draft_detected,
                        "prompt_draft_blocks_submission": post_wait_snapshot.tmux_pane_alive && post_wait_snapshot.prompt_draft_detected,
                        "tmux_pane_alive": post_wait_snapshot.tmux_pane_alive,
                        "capture_available": post_wait_snapshot.capture_available,
                        "initial_busy_snapshot_prompt_marker_detected": snapshot.prompt_marker_detected,
                        "initial_busy_snapshot_prompt_draft_detected": snapshot.prompt_draft_detected,
                        "wait_outcome": if timed_out { "timeout" } else { "error" },
                        "wait_error": err.clone(),
                        "requeue_for_retry": requeue_for_retry,
                    }),
                );
                // The busy-timeout is PRE-submit: the prompt was never sent to
                // the pane, so the message can be retried with no double-send.
                // When requeue is enabled, surface it as a retryable error so the
                // turn bridge re-queues the inflight message (it holds owner/
                // msg_id/text); otherwise keep the legacy drop-with-busy-notice.
                if requeue_for_retry {
                    return ClaudeTuiWarmFollowupSubmitOutcome::Terminal(Err(err));
                }
                return ClaudeTuiWarmFollowupSubmitOutcome::Terminal(Ok(()));
            }
        }
    }
    // #2416: capture the transcript offset AFTER the optional busy wait so
    // that any bytes the previous TUI turn appended while we were waiting
    // are not replayed as part of this follow-up's output window. This
    // closes a Codex-flagged HIGH (stale offset → duplicate / early-done
    // delivery accounting).
    let fallback_start_offset = std::fs::metadata(&transcript_path)
        .map(|meta| meta.len())
        .unwrap_or(0);
    // #2416: also honour cancellation that may have flipped during the
    // up-to-45s busy wait. Without this, a user stop reaction / watchdog
    // cancellation arriving mid-wait would still inject the prompt as
    // soon as the TUI returns to ready. Closes a Codex-flagged HIGH.
    if busy_waited && crate::services::provider::cancel_requested(cancel_token.as_deref()) {
        debug_log(&format!(
            "Claude TUI follow-up: cancellation observed after busy wait, aborting injection (session={})",
            tmux_session_name
        ));
        log_producer_exit(
            "tui_warm_followup_cancelled_after_busy_wait",
            Some(&resolved_session_id),
            report_channel_id,
            0,
            serde_json::json!({
                "tmux_session_name": tmux_session_name,
                "transcript_path": transcript_path_string,
            }),
        );
        return ClaudeTuiWarmFollowupSubmitOutcome::Terminal(Ok(()));
    }
    let turn_started_at = chrono::Utc::now();
    if let Err(error) = crate::services::claude_tui::input::send_followup_prompt_or_idle_transcript(
        tmux_session_name,
        prompt,
        cancel_token.as_deref(),
        &transcript_path,
    ) {
        if crate::services::claude_tui::input::is_prompt_ready_cancelled_error(&error) {
            debug_log(&format!(
                "Claude TUI follow-up: cancellation observed during prompt submission, aborting injection (session={})",
                tmux_session_name
            ));
            log_producer_exit(
                "tui_warm_followup_cancelled_during_prompt_submit",
                Some(&resolved_session_id),
                report_channel_id,
                0,
                serde_json::json!({
                    "tmux_session_name": tmux_session_name,
                    "transcript_path": transcript_path_string,
                }),
            );
            return ClaudeTuiWarmFollowupSubmitOutcome::Terminal(Ok(()));
        }
        return ClaudeTuiWarmFollowupSubmitOutcome::Terminal(Err(error));
    }
    let hook_events_after = chrono::Utc::now();
    let start_offset = claude_tui_turn_start_offset_after_timestamp(
        &transcript_path,
        turn_started_at,
        fallback_start_offset,
    );
    let (read_result, harvest) = match read_claude_tui_transcript_until_done(
        &transcript_path_string,
        start_offset,
        sender.clone(),
        cancel_token.clone(),
        tmux_session_name,
        &resolved_session_id,
        hook_rx,
        hook_events_after,
    ) {
        Ok(result) => result,
        Err(error) => {
            return ClaudeTuiWarmFollowupSubmitOutcome::Terminal(Err(error));
        }
    };
    // #3281: capture BEFORE `classify_followup_result` consumes the read
    // result. The zero-harvest gate is `Completed`-only: classify maps
    // `Cancelled` to `Delivered` too, and a cancelled turn legitimately
    // forwards nothing.
    let read_result_kind = read_output_result_kind(&read_result);
    let delivered_zero_harvest = tui_delivered_zero_harvest(&read_result, &harvest);
    let zero_advance_terminal_result = match &read_result {
        ReadOutputResult::Completed { offset } | ReadOutputResult::Cancelled { offset } => {
            *offset <= start_offset
        }
        ReadOutputResult::SessionDied { .. } => false,
    };
    if zero_advance_terminal_result {
        let snapshot =
            crate::services::claude_tui::input::prompt_readiness_snapshot(tmux_session_name);
        if claude_tui_prompt_remained_in_input_buffer(&snapshot, prompt) {
            return ClaudeTuiWarmFollowupSubmitOutcome::Terminal(Err(
                claude_tui_zero_advance_input_buffer_error(
                    tmux_session_name,
                    &transcript_path_string,
                    start_offset,
                    &snapshot,
                ),
            ));
        }
    }
    match classify_followup_result(
        read_result,
        start_offset,
        "claude tui session died during follow-up output reading",
    ) {
        ClaudeFollowupResult::Delivered => {
            emit_claude_tui_watcher_handoff(
                &sender,
                &transcript_path_string,
                tmux_session_name,
                &transcript_path,
            );
            let transcript_len = std::fs::metadata(&transcript_path)
                .map(|meta| meta.len())
                .unwrap_or(0);
            if delivered_zero_harvest {
                emit_claude_tui_zero_harvest(
                    "claude_tui_zero_harvest_warm_followup_delivered",
                    report_channel_id,
                    tmux_session_name,
                    &transcript_path_string,
                    start_offset,
                    transcript_len,
                );
            }
            log_producer_exit(
                "tui_warm_followup_delivered",
                Some(&resolved_session_id),
                report_channel_id,
                // #3281: real forwarded-message count (was a hardcoded 0).
                usize::try_from(harvest.forwarded_messages).unwrap_or(usize::MAX),
                serde_json::json!({
                    "tmux_session_name": tmux_session_name,
                    "transcript_path": transcript_path_string,
                    "assistant_text_bytes": harvest.assistant_text_bytes,
                    "start_offset": start_offset,
                    "transcript_len": transcript_len,
                    "read_result_kind": read_result_kind,
                }),
            );
            return ClaudeTuiWarmFollowupSubmitOutcome::Terminal(Ok(()));
        }
        ClaudeFollowupResult::RecreateSession { error } => {
            debug_log(&format!(
                "Claude TUI follow-up failed, recreating session: {}",
                error
            ));
            crate::services::termination_audit::record_termination_for_tmux(
                tmux_session_name,
                None,
                "claude_tui_provider",
                "followup_failed_recreate",
                Some(&format!(
                    "claude tui follow-up failed, recreating: {}",
                    error
                )),
                None,
            );
            record_tmux_exit_reason(
                tmux_session_name,
                &format!("claude tui follow-up failed, recreating: {}", error),
            );
            crate::services::platform::tmux::kill_session(
                tmux_session_name,
                &format!("claude tui follow-up failed, recreating: {}", error),
            );
        }
        ClaudeFollowupResult::FinalizeWithNotice { error, notice } => {
            debug_log(&format!(
                "Claude TUI follow-up streamed partial output before session death — suppressing replay: {}",
                error
            ));
            crate::services::termination_audit::record_termination_for_tmux(
                tmux_session_name,
                None,
                "claude_tui_provider",
                "followup_partial_output_no_replay",
                Some(&format!(
                    "claude tui partial follow-up output delivered: {}",
                    error
                )),
                None,
            );
            record_tmux_exit_reason(
                tmux_session_name,
                &format!("claude tui partial follow-up output delivered: {}", error),
            );
            crate::services::platform::tmux::kill_session(
                tmux_session_name,
                &format!("claude tui partial follow-up output delivered: {}", error),
            );
            emit_followup_restart_suppressed_notice(&sender, &notice);
            return ClaudeTuiWarmFollowupSubmitOutcome::Terminal(Ok(()));
        }
    }
    ClaudeTuiWarmFollowupSubmitOutcome::FallThroughRecreate
}

fn claude_tui_followup_wait_error_requeue_for_retry(error: &str) -> bool {
    crate::services::claude_tui::input::is_prompt_ready_timeout_error(error)
        && crate::services::claude::claude_tui_followup_requeue_enabled()
}

#[cfg(test)]
mod followup_wait_error_requeue_tests {
    use super::*;

    const FOLLOWUP_REQUEUE_ENV: &str = "AGENTDESK_CLAUDE_TUI_FOLLOWUP_REQUEUE";

    struct EnvRestore {
        previous: Option<String>,
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.previous.as_deref() {
                Some(value) => unsafe { std::env::set_var(FOLLOWUP_REQUEUE_ENV, value) },
                None => unsafe { std::env::remove_var(FOLLOWUP_REQUEUE_ENV) },
            }
        }
    }

    fn with_followup_requeue_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .expect("shared test env lock poisoned");
        let _restore = EnvRestore {
            previous: std::env::var(FOLLOWUP_REQUEUE_ENV).ok(),
        };
        match value {
            Some(value) => unsafe { std::env::set_var(FOLLOWUP_REQUEUE_ENV, value) },
            None => unsafe { std::env::remove_var(FOLLOWUP_REQUEUE_ENV) },
        }
        f()
    }

    #[test]
    fn followup_wait_requeue_requires_timeout_error() {
        with_followup_requeue_env(None, || {
            assert!(claude_tui_followup_wait_error_requeue_for_retry(
                "timeout waiting for claude tui follow-up prompt input readiness after 45s"
            ));
            assert!(!claude_tui_followup_wait_error_requeue_for_retry(
                "unexpected prompt wait error"
            ));
            assert!(!claude_tui_followup_wait_error_requeue_for_retry(
                crate::services::claude_tui::input::PROMPT_READY_CANCELLED_ERROR
            ));
        });
    }

    #[test]
    fn followup_wait_requeue_respects_emergency_opt_out() {
        with_followup_requeue_env(Some("0"), || {
            assert!(!claude_tui_followup_wait_error_requeue_for_retry(
                "timeout waiting for claude tui follow-up prompt input readiness after 45s"
            ));
        });
    }
}

/// Attempt a warm follow-up against an existing live Claude TUI tmux session.
/// Mirrors the pre-refactor live-session branch: recovery, busy wait, submit/read/classify; `Terminal` returns before fresh-launch side effects, `Recreate` carries fall-through resolution.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_claude_tui_warm_followup(
    mut resolved_session_id: String,
    mut transcript_path: std::path::PathBuf,
    mut transcript_path_string: String,
    mut resume: bool,
    working_dir_path: &std::path::Path,
    prompt: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    report_channel_id: Option<u64>,
) -> ClaudeTuiWarmFollowupOutcome {
    debug_log("Existing Claude TUI tmux session found — sending follow-up");
    if let Some(ref token) = cancel_token {
        token.bind_claude_tmux_session(tmux_session_name);
    }
    let hook_rx = crate::services::claude_tui::hook_server::subscribe_hook_events();
    let (busy_waited, recreate_before_submit, prompt_draft_cleared_before_submit) =
        match recover_claude_tui_stranded_prompt_draft(
            ClaudeTuiRecreateState {
                resolved_session_id,
                transcript_path,
                transcript_path_string,
                resume,
            },
            working_dir_path,
            &cancel_token,
            tmux_session_name,
            report_channel_id,
        ) {
            ClaudeTuiDraftRecoveryOutcome::Proceed {
                state,
                busy_waited,
                recreate_before_submit,
                prompt_draft_cleared_before_submit,
            } => {
                resolved_session_id = state.resolved_session_id;
                transcript_path = state.transcript_path;
                transcript_path_string = state.transcript_path_string;
                resume = state.resume;
                (
                    busy_waited,
                    recreate_before_submit,
                    prompt_draft_cleared_before_submit,
                )
            }
            ClaudeTuiDraftRecoveryOutcome::Terminal(r) => {
                return ClaudeTuiWarmFollowupOutcome::Terminal(r);
            }
        };
    let submit_plan = claude_tui_warm_followup_submit_plan(
        recreate_before_submit,
        prompt_draft_cleared_before_submit,
    );
    if submit_plan.submit_existing_session {
        match run_claude_tui_warm_followup_submit_and_stream(
            submit_plan,
            busy_waited,
            tmux_session_name,
            &resolved_session_id,
            &transcript_path,
            &transcript_path_string,
            prompt,
            sender,
            cancel_token,
            report_channel_id,
            hook_rx,
        ) {
            ClaudeTuiWarmFollowupSubmitOutcome::Terminal(r) => {
                return ClaudeTuiWarmFollowupOutcome::Terminal(r);
            }
            ClaudeTuiWarmFollowupSubmitOutcome::FallThroughRecreate => {}
        }
    }

    ClaudeTuiWarmFollowupOutcome::Recreate(ClaudeTuiRecreateState {
        resolved_session_id,
        transcript_path,
        transcript_path_string,
        resume,
    })
}
