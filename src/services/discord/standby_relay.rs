//! Phase 5.3 of intake-node-routing (issue #2011): standalone JSONL → Discord
//! relay task for cluster-standby nodes.
//!
//! On the leader, the tmux watcher (`tmux_watcher.rs`) handles streaming
//! agent output to Discord. The watcher's relay path has many gateway-coupled
//! assumptions (cached cache, inflight reconciliation, monitor-auto-turn
//! claims, recent_stop suppression, paused/pause_epoch coordination, etc.)
//! that don't hold on cluster-standby nodes. Phase 5.2 made the watcher
//! *start* on standby via `serenity_http_or_token_fallback()`, but the watcher's
//! relay step still doesn't fire on standby in production (verified
//! 2026-05-10 with channel `1475086789696946196` outbox_id=2: response sat in
//! tmux indefinitely while the placeholder froze at "응답 처리 중").
//!
//! Phase 5.3 takes the simpler, more robust path: when on standby, skip the
//! watcher entirely and run this self-contained relay loop instead. It
//! polls the agent's JSONL output file for the `{"type":"result"}` event,
//! extracts the assistant response, and posts it to Discord via REST
//! (replacing the bridge-allocated placeholder when one is known, otherwise
//! sending a new channel message). No reliance on cached_serenity_ctx,
//! inflight reconciliation, or any of the watcher's leader-only state
//! machinery.
//!
//! Leader path is unchanged.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use serenity::model::id::{ChannelId, MessageId};

use super::SharedData;
use super::formatting::{self, ReplaceLongMessageOutcome};
use super::inflight::{
    GuardedClearOutcome, InflightSignal, InflightTurnIdentity, InflightTurnState,
};
use crate::services::provider::ProviderKind;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// #2448 graduation: the 900s (15min) cap was the heuristic stop signal —
/// "after this long the primary turn is presumed dead". Now that
/// `CompletionGuard` broadcasts `InflightSignal::Completed` explicitly,
/// the wall-clock deadline is demoted to a pure safety backstop: it only
/// fires when neither the broadcast (same-node) nor the on-disk inflight
/// poll (cross-node) ever observe completion. 30 min comfortably covers
/// any sane long-running turn.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(1800);
const MAX_FILE_BYTES_PER_TICK: u64 = 1_048_576; // 1 MiB safety cap
const INFLIGHT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub(in crate::services::discord) struct StandbyRelayTurnBinding {
    identity: InflightTurnIdentity,
    dispatch_id: Option<String>,
    session_key: Option<String>,
    turn_start_offset: Option<u64>,
}

impl StandbyRelayTurnBinding {
    pub(in crate::services::discord) fn from_state(state: &InflightTurnState) -> Self {
        Self {
            identity: InflightTurnIdentity::from_state(state),
            dispatch_id: state.dispatch_id.clone(),
            session_key: state.session_key.clone(),
            turn_start_offset: state.turn_start_offset,
        }
    }

    fn turn_id(&self, channel_id: ChannelId) -> Option<String> {
        if self.identity.user_msg_id == 0 {
            return None;
        }
        Some(format!(
            "discord:{}:{}",
            channel_id.get(),
            self.identity.user_msg_id
        ))
    }
}

/// Spawned per-turn on cluster-standby nodes. Returns when:
/// - `cancel` or `shared.shutting_down` flips to true,
/// - the JSONL emits a `{"type":"result"}` event and we deliver the response,
/// - or `timeout` elapses.
pub(super) async fn run_standby_relay(
    http: Arc<serenity::http::Http>,
    channel_id: ChannelId,
    placeholder_msg_id: Option<MessageId>,
    output_path: String,
    turn_binding: StandbyRelayTurnBinding,
    start_offset: u64,
    cancel: Arc<AtomicBool>,
    shared: Arc<SharedData>,
    provider: ProviderKind,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    let mut current_offset = start_offset;
    let mut last_inflight_heartbeat = Instant::now();
    // Buffer for incomplete trailing line across reads.
    let mut tail_buf = String::new();
    let mut tail_start_offset = start_offset;
    let mut pending_result_text: Option<String> = None;
    let mut pending_result_retry_offset: Option<u64> = None;
    // #2448: subscribe BEFORE the first poll tick so a `Completed` broadcast
    // emitted while we are setting up is queued instead of lost. Lag is
    // expected on heavy load — `RecvError::Lagged` is treated as "you may
    // have missed an exit-eligible signal" and triggers a force-poll +
    // state re-fetch on the next tick (matches the issue pitfalls section).
    let mut inflight_signals = shared.inflight_signals.subscribe();
    let ts_start = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts_start}] 👁 standby_relay started for channel {} from offset {} (placeholder={:?})",
        channel_id.get(),
        start_offset,
        placeholder_msg_id.map(|m| m.get())
    );

    loop {
        if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 standby_relay cancelled for channel {} (offset={})",
                channel_id.get(),
                current_offset
            );
            return;
        }
        if Instant::now() > deadline {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ standby_relay deadline reached for channel {} (offset={}, no completion signal or result event observed in {}s — safety backstop)",
                channel_id.get(),
                current_offset,
                timeout.as_secs()
            );
            return;
        }
        // #2448: drain the broadcast queue NON-blocking before each poll
        // tick. If we observe `Completed { channel_id: self }` here, the
        // primary turn_bridge has dropped its `CompletionGuard` and any
        // residual JSONL `result` event was either consumed by the bridge
        // (leader path) or will land on the next file-poll tick. Either
        // way the relay can exit promptly.
        loop {
            use tokio::sync::broadcast::error::TryRecvError;
            match inflight_signals.try_recv() {
                Ok(InflightSignal::Completed { channel_id: c })
                    if c == channel_id.get()
                        && standby_completed_signal_exits(pending_result_text.as_deref()) =>
                {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 👁 standby_relay exit on InflightSignal::Completed for channel {} (offset={})",
                        channel_id.get(),
                        current_offset
                    );
                    return;
                }
                Ok(InflightSignal::Completed { channel_id: c }) if c == channel_id.get() => {
                    continue;
                }
                Ok(_) => continue, // other channels — ignore
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Lagged(_)) => {
                    // Codex review HIGH: a bursty publisher can saturate the
                    // 256-slot broadcast and Lag us before we observe our
                    // own `Completed`. The previous `break` here meant we
                    // silently fell through to the 1800s backstop — the
                    // exact regression #2448 was meant to close. Recheck
                    // the on-disk inflight authoritatively: if terminal
                    // (file gone or pointing at a different output), the
                    // turn already completed and we should exit now.
                    if pending_result_text.is_none()
                        && super::inflight::load_inflight_state(&provider, channel_id.get())
                            .map(|state| {
                                !standby_inflight_matches(&state, &output_path, placeholder_msg_id)
                            })
                            .unwrap_or(true)
                    {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 👁 standby_relay exit on broadcast Lag + terminal inflight for channel {} (offset={})",
                            channel_id.get(),
                            current_offset
                        );
                        return;
                    }
                    break;
                }
                Err(TryRecvError::Closed) => break, // sender dropped — keep polling
            }
        }
        if last_inflight_heartbeat.elapsed() >= INFLIGHT_HEARTBEAT_INTERVAL {
            refresh_standby_inflight_heartbeat(
                &provider,
                channel_id,
                &output_path,
                placeholder_msg_id,
                &turn_binding,
                standby_heartbeat_offset(
                    current_offset,
                    pending_result_retry_offset,
                    (!tail_buf.is_empty()).then_some(tail_start_offset),
                ),
            );
            last_inflight_heartbeat = Instant::now();
        }

        if let Some(result_text) = pending_result_text.as_deref() {
            let delivered = deliver_response(
                &http,
                channel_id,
                placeholder_msg_id,
                &shared,
                &provider,
                result_text,
            )
            .await;
            if delivered {
                complete_standby_inflight_state(
                    &provider,
                    channel_id,
                    &output_path,
                    placeholder_msg_id,
                    &turn_binding,
                    result_text,
                    current_offset,
                );
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }

        let file_size = match std::fs::metadata(&output_path) {
            Ok(meta) => meta.len(),
            Err(_) => {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };
        if file_size <= current_offset {
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }

        let read_from = current_offset;
        let read_to = (read_from + MAX_FILE_BYTES_PER_TICK).min(file_size);
        let new_chunk = match read_file_range(&output_path, read_from, read_to) {
            Ok(s) => s,
            Err(_) => {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };
        current_offset = read_to;

        // Stitch any prior incomplete tail with the new chunk and process
        // line-by-line. Keep the trailing partial line (no '\n') for next tick.
        let stitched_start_offset = if tail_buf.is_empty() {
            read_from
        } else {
            tail_start_offset
        };
        let stitched = if tail_buf.is_empty() {
            new_chunk
        } else {
            let mut s = std::mem::take(&mut tail_buf);
            s.push_str(&new_chunk);
            s
        };
        let mut last_complete_end = 0usize;
        let bytes = stitched.as_bytes();
        let mut found_result_text = None;
        for (idx, b) in bytes.iter().enumerate() {
            if *b == b'\n' {
                let line_start = last_complete_end;
                let line = &stitched[line_start..idx];
                last_complete_end = idx + 1;
                if line.trim().is_empty() {
                    continue;
                }
                if let Some(result_text) = extract_result_text(line) {
                    pending_result_retry_offset =
                        Some(stitched_start_offset.saturating_add(line_start as u64));
                    found_result_text = Some(result_text);
                    break;
                }
            }
        }
        if let Some(result_text) = found_result_text {
            pending_result_text = Some(result_text);
            continue;
        }
        if last_complete_end < stitched.len() {
            tail_buf = stitched[last_complete_end..].to_string();
            tail_start_offset = stitched_start_offset.saturating_add(last_complete_end as u64);
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn read_file_range(path: &str, start: u64, end: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let len = end.saturating_sub(start) as usize;
    let mut buf = vec![0u8; len];
    let read = file.read(&mut buf)?;
    buf.truncate(read);
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn extract_result_text(line: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(line.trim()).ok()?;
    if parsed.get("type").and_then(Value::as_str) != Some("result") {
        return None;
    }
    let result_text = parsed.get("result").and_then(Value::as_str)?;
    let trimmed = result_text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(result_text.to_string())
}

fn standby_inflight_matches(
    state: &InflightTurnState,
    output_path: &str,
    placeholder_msg_id: Option<MessageId>,
) -> bool {
    if state.output_path.as_deref() != Some(output_path) {
        return false;
    }
    if let Some(msg_id) = placeholder_msg_id {
        state.current_msg_id == msg_id.get()
    } else {
        true
    }
}

fn standby_completed_signal_exits(pending_result_text: Option<&str>) -> bool {
    pending_result_text.is_none()
}

fn standby_heartbeat_offset(
    current_offset: u64,
    pending_result_retry_offset: Option<u64>,
    incomplete_tail_start_offset: Option<u64>,
) -> u64 {
    pending_result_retry_offset
        .or(incomplete_tail_start_offset)
        .unwrap_or(current_offset)
}

fn refresh_standby_inflight_heartbeat(
    provider: &ProviderKind,
    channel_id: ChannelId,
    output_path: &str,
    placeholder_msg_id: Option<MessageId>,
    turn_binding: &StandbyRelayTurnBinding,
    current_offset: u64,
) {
    let expected_current_msg_id = placeholder_msg_id.map(|msg| msg.get());
    let _ = super::inflight::refresh_inflight_last_offset_if_matches_identity(
        provider,
        channel_id.get(),
        &turn_binding.identity,
        turn_binding.turn_start_offset,
        output_path,
        expected_current_msg_id,
        current_offset,
    );
}

fn clear_outcome_label(outcome: GuardedClearOutcome) -> &'static str {
    match outcome {
        GuardedClearOutcome::Cleared => "cleared",
        GuardedClearOutcome::UserMsgMismatch => "user_msg_mismatch",
        GuardedClearOutcome::PlannedRestartSkipped => "planned_restart_skipped",
        GuardedClearOutcome::RebindOriginSkipped => "rebind_origin_skipped",
        GuardedClearOutcome::Missing => "missing",
        GuardedClearOutcome::IoError => "io_error",
    }
}

fn emit_standby_completion_event(
    provider: &ProviderKind,
    channel_id: ChannelId,
    output_path: &str,
    placeholder_msg_id: Option<MessageId>,
    turn_binding: &StandbyRelayTurnBinding,
    outcome_label: &str,
    response_text: &str,
    current_offset: u64,
    mirrored_response: bool,
) {
    let turn_id = turn_binding.turn_id(channel_id);
    crate::services::observability::emit_inflight_lifecycle_event(
        provider.as_str(),
        channel_id.get(),
        turn_binding.dispatch_id.as_deref(),
        turn_binding.session_key.as_deref(),
        turn_id.as_deref(),
        "cleared_by_standby_relay",
        serde_json::json!({
            "outcome": outcome_label,
            "expected_user_msg_id": turn_binding.identity.user_msg_id,
            "expected_started_at": turn_binding.identity.started_at.as_str(),
            "expected_tmux_session_name": turn_binding.identity.tmux_session_name.as_deref(),
            "expected_turn_start_offset": turn_binding.turn_start_offset,
            "placeholder_msg_id": placeholder_msg_id.map(|msg| msg.get()),
            "output_path": output_path,
            "current_offset": current_offset,
            "response_len": response_text.len(),
            "mirrored_response_before_clear": mirrored_response,
        }),
    );
}

fn complete_standby_inflight_state(
    provider: &ProviderKind,
    channel_id: ChannelId,
    output_path: &str,
    placeholder_msg_id: Option<MessageId>,
    turn_binding: &StandbyRelayTurnBinding,
    response_text: &str,
    current_offset: u64,
) -> GuardedClearOutcome {
    let Some(state) = super::inflight::load_inflight_state(provider, channel_id.get()) else {
        emit_standby_completion_event(
            provider,
            channel_id,
            output_path,
            placeholder_msg_id,
            turn_binding,
            clear_outcome_label(GuardedClearOutcome::Missing),
            response_text,
            current_offset,
            false,
        );
        return GuardedClearOutcome::Missing;
    };
    if !standby_inflight_matches(&state, output_path, placeholder_msg_id) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            provider = %provider.as_str(),
            channel = channel_id.get(),
            output_path = output_path,
            placeholder_msg_id = placeholder_msg_id.map(|msg| msg.get()),
            "[{ts}] ⚠ standby_relay skipped inflight cleanup because the on-disk row no longer matches this relay"
        );
        emit_standby_completion_event(
            provider,
            channel_id,
            output_path,
            placeholder_msg_id,
            turn_binding,
            "precheck_mismatch",
            response_text,
            current_offset,
            false,
        );
        return GuardedClearOutcome::UserMsgMismatch;
    }

    let user_msg_id = turn_binding.identity.user_msg_id;
    let (outcome, mirrored_response) =
        super::inflight::clear_inflight_state_if_matches_identity_after_delivery(
            provider,
            channel_id.get(),
            &turn_binding.identity,
            turn_binding.turn_start_offset,
            response_text,
            response_text.len(),
            current_offset,
        );
    let outcome_label = clear_outcome_label(outcome);
    emit_standby_completion_event(
        provider,
        channel_id,
        output_path,
        placeholder_msg_id,
        turn_binding,
        outcome_label,
        response_text,
        current_offset,
        mirrored_response,
    );

    match outcome {
        GuardedClearOutcome::Cleared | GuardedClearOutcome::Missing => {
            tracing::debug!(
                provider = %provider.as_str(),
                channel = channel_id.get(),
                outcome = outcome_label,
                "standby_relay completed delegated inflight cleanup"
            );
        }
        GuardedClearOutcome::UserMsgMismatch => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id.get(),
                user_msg_id = user_msg_id,
                "[{ts}] ⚠ standby_relay did not clear inflight because the guarded identity no longer matches"
            );
        }
        GuardedClearOutcome::PlannedRestartSkipped | GuardedClearOutcome::RebindOriginSkipped => {
            tracing::debug!(
                provider = %provider.as_str(),
                channel = channel_id.get(),
                outcome = outcome_label,
                "standby_relay preserved inflight row after delegated completion"
            );
        }
        GuardedClearOutcome::IoError => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id.get(),
                "standby_relay failed to clear inflight after delegated completion; sweeper will see mirrored response state"
            );
        }
    }
    outcome
}

async fn deliver_response(
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    placeholder_msg_id: Option<MessageId>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    response_text: &str,
) -> bool {
    let formatted = if shared.status_panel_v2_enabled {
        formatting::format_for_discord_with_status_panel(response_text, provider)
    } else {
        formatting::format_for_discord_with_provider(response_text, provider)
    };
    let chars = formatted.chars().count();

    match placeholder_msg_id {
        Some(msg_id) => {
            let outcome = formatting::replace_long_message_raw_with_outcome(
                http, channel_id, msg_id, &formatted, shared,
            )
            .await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            match outcome {
                Ok(ReplaceLongMessageOutcome::EditedOriginal)
                | Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { .. }) => {
                    // `Manage Messages` lives on the announce bot in this
                    // deployment; route the unpin there to avoid 403.
                    let unpin_http =
                        super::gateway::manage_messages_http(shared, http).await;
                    match channel_id.unpin(unpin_http.as_ref(), msg_id).await {
                        Ok(()) => shared
                            .placeholder_controller
                            .forget_placeholder_pin(provider, channel_id, msg_id),
                        Err(error) => {
                            tracing::warn!(
                                provider = provider.as_str(),
                                channel_id = channel_id.get(),
                                message_id = msg_id.get(),
                                error = %error,
                                "[standby_relay] placeholder unpin failed after terminal delivery; tracked cleanup will retry"
                            );
                        }
                    }
                    tracing::info!(
                        "  [{ts}] 👁 standby_relay ✓ delivered terminal response (edit) channel {} msg {} ({} chars)",
                        channel_id.get(),
                        msg_id.get(),
                        chars
                    );
                    true
                }
                Ok(ReplaceLongMessageOutcome::PartialContinuationFailure {
                    sent_chunks,
                    total_chunks,
                    failed_chunk_index,
                    sent_continuation_message_ids,
                    cleanup_errors,
                    error,
                }) => {
                    tracing::warn!(
                        "  [{ts}] ⚠ standby_relay partially delivered terminal response in channel {} msg {} (sent_chunks={}, total_chunks={}, failed_chunk_index={}, cleaned_continuations={}, cleanup_errors={}, error={})",
                        channel_id.get(),
                        msg_id.get(),
                        sent_chunks,
                        total_chunks,
                        failed_chunk_index,
                        sent_continuation_message_ids.len(),
                        cleanup_errors.len(),
                        error
                    );
                    false
                }
                Err(e) => {
                    tracing::warn!(
                        "  [{ts}] ⚠ standby_relay edit failed for channel {} msg {}: {e}",
                        channel_id.get(),
                        msg_id.get()
                    );
                    false
                }
            }
        }
        None => {
            let result =
                formatting::send_long_message_raw(http, channel_id, &formatted, shared).await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            match result {
                Ok(()) => {
                    tracing::info!(
                        "  [{ts}] 👁 standby_relay ✓ delivered terminal response (new message) channel {} ({} chars)",
                        channel_id.get(),
                        chars
                    );
                    true
                }
                Err(e) => {
                    tracing::warn!(
                        "  [{ts}] ⚠ standby_relay send failed for channel {}: {e}",
                        channel_id.get()
                    );
                    false
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_result_text_returns_none_for_non_result_lines() {
        let line = r#"{"type":"assistant","message":{"content":[{"text":"hi"}]}}"#;
        assert!(extract_result_text(line).is_none());
    }

    #[test]
    fn extract_result_text_returns_text_for_result_subtype_success() {
        let line = r#"{"type":"result","subtype":"success","result":"hello"}"#;
        assert_eq!(extract_result_text(line).as_deref(), Some("hello"));
    }

    #[test]
    fn extract_result_text_skips_empty_result() {
        let line = r#"{"type":"result","subtype":"success","result":"   "}"#;
        assert!(extract_result_text(line).is_none());
    }

    #[test]
    fn extract_result_text_handles_invalid_json() {
        assert!(extract_result_text("not json").is_none());
        assert!(extract_result_text("").is_none());
    }

    #[test]
    fn standby_inflight_match_requires_same_output_and_placeholder() {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            1234,
            None,
            42,
            100,
            5678,
            "test".to_string(),
            None,
            Some("tmux".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        );

        assert!(standby_inflight_matches(
            &state,
            "/tmp/out.jsonl",
            Some(MessageId::new(5678)),
        ));
        assert!(!standby_inflight_matches(
            &state,
            "/tmp/other.jsonl",
            Some(MessageId::new(5678)),
        ));
        assert!(!standby_inflight_matches(
            &state,
            "/tmp/out.jsonl",
            Some(MessageId::new(9999)),
        ));

        state.current_msg_id = 0;
        assert!(standby_inflight_matches(&state, "/tmp/out.jsonl", None));
    }

    /// #2448 acceptance — confirm the relay-side broadcast filter:
    /// `InflightSignal::Completed` for a NON-matching channel must be
    /// ignored, while one for the OWN channel must short-circuit. We
    /// exercise the filter shape inline because the live relay loop
    /// requires a `serenity::http::Http` fixture not available in this
    /// test scope.
    #[test]
    fn inflight_signal_filter_matches_own_channel_only() {
        use super::InflightSignal;
        let own = 11_111u64;
        let other = 22_222u64;

        let matches = |sig: &InflightSignal| match sig {
            InflightSignal::Completed { channel_id } => *channel_id == own,
        };

        assert!(matches(&InflightSignal::Completed { channel_id: own }));
        assert!(!matches(&InflightSignal::Completed { channel_id: other }));
    }

    #[test]
    fn completed_signal_does_not_exit_while_result_delivery_is_pending() {
        assert!(standby_completed_signal_exits(None));
        assert!(!standby_completed_signal_exits(Some("final response")));
    }

    #[test]
    fn heartbeat_offset_rewinds_to_pending_result_until_delivery_commits() {
        assert_eq!(standby_heartbeat_offset(250, None, None), 250);
        assert_eq!(standby_heartbeat_offset(250, Some(120), None), 120);
        assert_eq!(standby_heartbeat_offset(250, None, Some(180)), 180);
        assert_eq!(standby_heartbeat_offset(250, Some(120), Some(180)), 120);
    }

    fn with_isolated_runtime_root<F: FnOnce()>(f: F) {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("create temp runtime dir for standby relay test");
        unsafe {
            std::env::set_var(
                "AGENTDESK_ROOT_DIR",
                tmp.path().to_str().expect("temp path must be valid utf-8"),
            );
        }
        f();
        unsafe {
            std::env::remove_var("AGENTDESK_ROOT_DIR");
        }
    }

    #[test]
    fn standby_completion_clears_matching_inflight_with_identity_guard() {
        with_isolated_runtime_root(|| {
            let provider = ProviderKind::Codex;
            let channel_id = ChannelId::new(1234);
            let state = InflightTurnState::new(
                provider.clone(),
                channel_id.get(),
                None,
                42,
                100,
                5678,
                "test".to_string(),
                None,
                Some("tmux".to_string()),
                Some("/tmp/out.jsonl".to_string()),
                None,
                12,
            );
            let binding = StandbyRelayTurnBinding::from_state(&state);
            super::super::inflight::save_inflight_state(&state).expect("save inflight");

            let outcome = complete_standby_inflight_state(
                &provider,
                channel_id,
                "/tmp/out.jsonl",
                Some(MessageId::new(5678)),
                &binding,
                "done",
                88,
            );

            assert_eq!(outcome, GuardedClearOutcome::Cleared);
            assert!(
                super::super::inflight::load_inflight_state(&provider, channel_id.get()).is_none()
            );
        });
    }

    #[test]
    fn standby_completion_keeps_mismatched_placeholder_inflight() {
        with_isolated_runtime_root(|| {
            let provider = ProviderKind::Codex;
            let channel_id = ChannelId::new(1235);
            let state = InflightTurnState::new(
                provider.clone(),
                channel_id.get(),
                None,
                42,
                100,
                5678,
                "test".to_string(),
                None,
                Some("tmux".to_string()),
                Some("/tmp/out.jsonl".to_string()),
                None,
                12,
            );
            let binding = StandbyRelayTurnBinding::from_state(&state);
            super::super::inflight::save_inflight_state(&state).expect("save inflight");

            let outcome = complete_standby_inflight_state(
                &provider,
                channel_id,
                "/tmp/out.jsonl",
                Some(MessageId::new(9999)),
                &binding,
                "done",
                88,
            );

            assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);
            let loaded = super::super::inflight::load_inflight_state(&provider, channel_id.get())
                .expect("mismatched inflight should remain");
            assert_eq!(loaded.current_msg_id, 5678);
            assert!(loaded.full_response.is_empty());
        });
    }

    #[test]
    fn standby_completion_uses_captured_identity_when_fresh_turn_reuses_output() {
        with_isolated_runtime_root(|| {
            let provider = ProviderKind::Codex;
            let channel_id = ChannelId::new(1236);
            let mut old_state = InflightTurnState::new(
                provider.clone(),
                channel_id.get(),
                None,
                42,
                100,
                0,
                "old prompt".to_string(),
                None,
                Some("tmux".to_string()),
                Some("/tmp/out.jsonl".to_string()),
                None,
                12,
            );
            old_state.started_at = "2026-05-17 10:00:00".to_string();
            let binding = StandbyRelayTurnBinding::from_state(&old_state);
            super::super::inflight::save_inflight_state(&old_state).expect("save old inflight");

            let mut fresh_state = InflightTurnState::new(
                provider.clone(),
                channel_id.get(),
                None,
                42,
                101,
                0,
                "fresh prompt".to_string(),
                None,
                Some("tmux".to_string()),
                Some("/tmp/out.jsonl".to_string()),
                None,
                20,
            );
            fresh_state.started_at = "2026-05-17 10:00:05".to_string();
            super::super::inflight::save_inflight_state(&fresh_state)
                .expect("replace with fresh inflight");

            let outcome = complete_standby_inflight_state(
                &provider,
                channel_id,
                "/tmp/out.jsonl",
                None,
                &binding,
                "stale delivered response",
                88,
            );

            assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);
            let loaded = super::super::inflight::load_inflight_state(&provider, channel_id.get())
                .expect("fresh inflight should remain");
            assert_eq!(loaded.user_msg_id, 101);
            assert_eq!(loaded.started_at, "2026-05-17 10:00:05");
            assert!(loaded.full_response.is_empty());
            assert_eq!(loaded.response_sent_offset, 0);
        });
    }

    #[test]
    fn standby_heartbeat_uses_captured_identity_when_fresh_turn_reuses_output() {
        with_isolated_runtime_root(|| {
            let provider = ProviderKind::Codex;
            let channel_id = ChannelId::new(1237);
            let mut old_state = InflightTurnState::new(
                provider.clone(),
                channel_id.get(),
                None,
                42,
                100,
                0,
                "old prompt".to_string(),
                None,
                Some("tmux".to_string()),
                Some("/tmp/out.jsonl".to_string()),
                None,
                12,
            );
            old_state.started_at = "2026-05-17 10:00:00".to_string();
            let binding = StandbyRelayTurnBinding::from_state(&old_state);
            super::super::inflight::save_inflight_state(&old_state).expect("save old inflight");

            let mut fresh_state = InflightTurnState::new(
                provider.clone(),
                channel_id.get(),
                None,
                42,
                101,
                0,
                "fresh prompt".to_string(),
                None,
                Some("tmux".to_string()),
                Some("/tmp/out.jsonl".to_string()),
                None,
                20,
            );
            fresh_state.started_at = "2026-05-17 10:00:05".to_string();
            super::super::inflight::save_inflight_state(&fresh_state)
                .expect("replace with fresh inflight");

            refresh_standby_inflight_heartbeat(
                &provider,
                channel_id,
                "/tmp/out.jsonl",
                None,
                &binding,
                88,
            );

            let loaded = super::super::inflight::load_inflight_state(&provider, channel_id.get())
                .expect("fresh inflight should remain");
            assert_eq!(loaded.user_msg_id, 101);
            assert_eq!(loaded.last_offset, 20);
            assert_eq!(loaded.started_at, "2026-05-17 10:00:05");
        });
    }

    /// #2448 acceptance — `tokio::sync::broadcast` capacity 256 must
    /// deliver `Completed` to a subscribed receiver within one recv
    /// iteration. The relay's poll-tick observes the queued message via
    /// `try_recv` on the next iteration, so the broadcast latency is
    /// bounded by the relay's `POLL_INTERVAL` (500ms) ceiling.
    #[tokio::test]
    async fn inflight_signal_broadcast_delivers_to_subscriber() {
        use super::InflightSignal;
        let (tx, mut rx) = tokio::sync::broadcast::channel::<InflightSignal>(256);

        let send_result = tx.send(InflightSignal::Completed { channel_id: 42 });
        assert!(send_result.is_ok());

        let received = rx.recv().await.expect("broadcast delivered");
        match received {
            InflightSignal::Completed { channel_id } => assert_eq!(channel_id, 42),
        }
    }
}
