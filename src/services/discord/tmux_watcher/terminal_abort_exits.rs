use super::*;
use std::sync::Arc;

use crate::services::discord::inflight::opt_message_id;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AbortExitOutcome {
    ContinueWatcherLoop,
    Fallthrough,
}

pub(super) struct TerminalAbortExitContext<'a> {
    pub(super) http: &'a Arc<serenity::Http>,
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) channel_id: serenity::ChannelId,
    pub(super) watcher_provider: &'a ProviderKind,
    pub(super) tmux_session_name: &'a String,
    pub(super) paused: &'a Arc<AtomicBool>,
    pub(super) pause_epoch: &'a Arc<AtomicU64>,
}

pub(super) struct TerminalAbortExitLocals<'a> {
    pub(super) was_paused: bool,
    pub(super) epoch_snapshot: u64,
    pub(super) monitor_auto_turn_deferred: bool,
    pub(super) placeholder_msg_id: Option<serenity::MessageId>,
    pub(super) turn_data_start_offset: u64,
    pub(super) current_offset: u64,
    pub(super) response_sent_offset: usize,
    pub(super) is_prompt_too_long: bool,
    pub(super) is_auth_error: bool,
    pub(super) auth_error_message: &'a Option<String>,
    pub(super) is_provider_overloaded: bool,
    pub(super) provider_overload_message: &'a Option<String>,
}

pub(super) struct TerminalAbortExitState<'a> {
    pub(super) placeholder_from_restored_inflight: &'a mut bool,
    pub(super) last_edit_text: &'a mut String,
    pub(super) monitor_auto_turn_claimed: &'a mut bool,
    pub(super) monitor_auto_turn_finished: &'a mut bool,
    pub(super) monitor_auto_turn_synthetic_msg_id: &'a mut Option<serenity::MessageId>,
    pub(super) monitor_auto_turn_ledger_generation: &'a mut Option<u64>,
    pub(super) all_data: &'a mut String,
    pub(super) all_data_start_offset: &'a mut u64,
    pub(super) all_data_fully_mirrored_to_session_relay: &'a mut bool,
    pub(super) all_data_session_bound_relay_ack: &'a mut Option<SessionBoundRelayAckTarget>,
    pub(super) all_data_first_forwarded_relay_sequence: &'a mut Option<u64>,
    pub(super) prompt_too_long_killed: &'a mut bool,
}

pub(super) async fn handle_terminal_abort_exits(
    context: &TerminalAbortExitContext<'_>,
    locals: TerminalAbortExitLocals<'_>,
    state: &mut TerminalAbortExitState<'_>,
) -> AbortExitOutcome {
    let http = context.http;
    let shared = context.shared;
    let channel_id = context.channel_id;
    let watcher_provider = context.watcher_provider;
    let tmux_session_name = context.tmux_session_name;
    let paused = context.paused;
    let pause_epoch = context.pause_epoch;

    // Discard partial data if paused while reading (even if now unpaused), or if the epoch
    // changed (a Discord turn claimed this data even when paused is now false).
    let paused_now = paused.load(Ordering::Relaxed);
    let epoch_changed_now = pause_epoch.load(Ordering::Relaxed) != locals.epoch_snapshot;
    let deferred_monitor_ready =
        *state.monitor_auto_turn_claimed && locals.monitor_auto_turn_deferred && !paused_now;
    if (locals.was_paused || paused_now || epoch_changed_now) && !deferred_monitor_ready {
        if let Some(msg_id) = locals.placeholder_msg_id {
            if watcher_should_delete_suppressed_placeholder(
                *state.placeholder_from_restored_inflight,
            ) {
                let inflight_before_cleanup =
                    crate::services::discord::inflight::load_inflight_state(
                        watcher_provider,
                        channel_id.get(),
                    );
                let _ = delete_nonterminal_placeholder_unless_delivered(
                    http,
                    channel_id,
                    shared,
                    watcher_provider,
                    tmux_session_name,
                    msg_id,
                    inflight_before_cleanup.as_ref(),
                    Some((
                        locals.turn_data_start_offset,
                        terminal_event_consumed_offset(locals.current_offset, &*state.all_data),
                    )),
                    locals.response_sent_offset,
                    state.last_edit_text.as_str(),
                    "watcher_pause_epoch_placeholder_cleanup",
                )
                .await;
            } else {
                *state.placeholder_from_restored_inflight = false;
                state.last_edit_text.clear();
            }
        }
        finish_monitor_auto_turn_if_claimed(
            shared,
            watcher_provider,
            channel_id,
            &mut *state.monitor_auto_turn_claimed,
            &mut *state.monitor_auto_turn_finished,
            &mut *state.monitor_auto_turn_synthetic_msg_id,
            &mut *state.monitor_auto_turn_ledger_generation,
        )
        .await;
        state.all_data.clear();
        *state.all_data_start_offset = locals.current_offset;
        *state.all_data_fully_mirrored_to_session_relay = true;
        *state.all_data_session_bound_relay_ack = None;
        *state.all_data_first_forwarded_relay_sequence = None;
        return AbortExitOutcome::ContinueWatcherLoop;
    }

    // Handle prompt-too-long: kill session so next message creates a fresh one
    if locals.is_prompt_too_long {
        clear_provider_overload_retry_state(channel_id);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 👁 Prompt too long detected in watcher for {tmux_session_name}, killing session"
        );
        *state.prompt_too_long_killed = true;

        let sess = (*tmux_session_name).clone();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking(move || {
                crate::services::termination_audit::record_termination_for_tmux(
                    &sess,
                    None,
                    "tmux_watcher",
                    "prompt_too_long",
                    Some("watcher cleanup: prompt too long"),
                    None,
                );
                record_tmux_exit_reason(&sess, "watcher cleanup: prompt too long");
                crate::services::platform::tmux::kill_session(
                    &sess,
                    "watcher cleanup: prompt too long",
                );
            }),
        )
        .await;

        let notice = "⚠️ 컨텍스트 한도 초과로 세션을 초기화했습니다. 다음 메시지부터 새 세션으로 처리됩니다.";
        match locals.placeholder_msg_id {
            Some(msg_id) => {
                rate_limit_wait(shared, channel_id).await;
                let _ = crate::services::discord::http::edit_channel_message(
                    http, channel_id, msg_id, notice,
                )
                .await;
            }
            None => {
                let _ =
                    crate::services::discord::http::send_channel_message(http, channel_id, notice)
                        .await;
            }
        }
        // Don't break — let the watcher exit naturally when session-alive check fails
        finish_monitor_auto_turn_if_claimed(
            shared,
            watcher_provider,
            channel_id,
            &mut *state.monitor_auto_turn_claimed,
            &mut *state.monitor_auto_turn_finished,
            &mut *state.monitor_auto_turn_synthetic_msg_id,
            &mut *state.monitor_auto_turn_ledger_generation,
        )
        .await;
        return AbortExitOutcome::ContinueWatcherLoop;
    }

    // Handle auth error: kill session and notify user to re-authenticate
    if locals.is_auth_error {
        clear_provider_overload_retry_state(channel_id);
        let inflight_state = crate::services::discord::inflight::load_inflight_state(
            watcher_provider,
            channel_id.get(),
        );
        let fallback_session_id = inflight_state
            .as_ref()
            .and_then(|state| state.session_id.as_deref());
        let dispatch_id =
            resolve_watcher_dispatch_id(shared, channel_id, inflight_state.as_ref()).await;
        let auth_detail = locals
            .auth_error_message
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("authentication expired");
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 👁 Auth error detected in watcher for {tmux_session_name}: {}",
            truncate_str(auth_detail, 300)
        );
        *state.prompt_too_long_killed = true; // reuse flag to suppress duplicate "session ended" message

        clear_provider_session_for_retry(
            shared,
            channel_id,
            tmux_session_name,
            fallback_session_id,
        )
        .await;

        let sess = (*tmux_session_name).clone();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking(move || {
                crate::services::termination_audit::record_termination_for_tmux(
                    &sess,
                    None,
                    "tmux_watcher",
                    "auth_error",
                    Some("watcher cleanup: authentication failed"),
                    None,
                );
                record_tmux_exit_reason(&sess, "watcher cleanup: authentication failed");
                crate::services::platform::tmux::kill_session(
                    &sess,
                    "watcher cleanup: authentication failed",
                );
            }),
        )
        .await;

        let notice = format!(
            "⚠️ 인증이 만료되어 현재 dispatch를 실패 처리했습니다. 세션을 종료합니다.\n관리자가 CLI에서 재인증(`/login`)을 완료한 후 다시 디스패치해주세요.\n\n사유: {}",
            truncate_str(auth_detail, 300)
        );
        let notice_ok = match locals.placeholder_msg_id {
            Some(msg_id) => {
                rate_limit_wait(shared, channel_id).await;
                crate::services::discord::http::edit_channel_message(
                    http, channel_id, msg_id, &notice,
                )
                .await
                .is_ok()
            }
            None => crate::services::discord::http::send_channel_message(http, channel_id, &notice)
                .await
                .is_ok(),
        };
        if !notice_ok {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: auth error notice failed before dispatch failure — preserving inflight for retry"
            );
            finish_monitor_auto_turn_if_claimed(
                shared,
                watcher_provider,
                channel_id,
                &mut *state.monitor_auto_turn_claimed,
                &mut *state.monitor_auto_turn_finished,
                &mut *state.monitor_auto_turn_synthetic_msg_id,
                &mut *state.monitor_auto_turn_ledger_generation,
            )
            .await;
            return AbortExitOutcome::ContinueWatcherLoop;
        }
        // #897 round-3 Medium: skip reaction work for `rebind_origin`
        // inflights — their `user_msg_id=0` identifies no real Discord
        // message so issuing reactions against it just produces API
        // errors. The synthetic state was created by
        // `/api/inflight/rebind` to adopt a live tmux session. The same
        // holds for any user_msg_id == 0 (e.g. a TUI-direct turn) — there
        // is no message to react against and `MessageId::new(0)` panics.
        if let Some(state) = inflight_state.as_ref().filter(|s| !s.rebind_origin)
            && let Some(user_msg_id) = opt_message_id(state.user_msg_id)
        {
            crate::services::discord::turn_view_reconciler::note_intake_turn_failed(
                shared,
                http,
                channel_id,
                user_msg_id,
                state.born_generation,
                "tmux_watcher_auth_expired",
            )
            .await;
        }
        finalize_pinned_watcher_exit(
            shared,
            watcher_provider,
            channel_id,
            inflight_state.as_ref(),
            "watcher_auth_error_exit",
        )
        .await;
        let failure_text = format!(
            "authentication expired; re-authentication required: {}",
            truncate_str(auth_detail, 300)
        );
        crate::services::discord::turn_bridge::fail_dispatch_auth_expired(
            shared.api_port,
            dispatch_id.as_deref(),
            &failure_text,
        )
        .await;
        finish_monitor_auto_turn_if_claimed(
            shared,
            watcher_provider,
            channel_id,
            &mut *state.monitor_auto_turn_claimed,
            &mut *state.monitor_auto_turn_finished,
            &mut *state.monitor_auto_turn_synthetic_msg_id,
            &mut *state.monitor_auto_turn_ledger_generation,
        )
        .await;
        return AbortExitOutcome::ContinueWatcherLoop;
    }

    if locals.is_provider_overloaded {
        let overload_message = locals
            .provider_overload_message
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("provider overload detected");
        let inflight_state = crate::services::discord::inflight::load_inflight_state(
            watcher_provider,
            channel_id.get(),
        );
        let retry_text = inflight_state
            .as_ref()
            .map(|state| state.user_text.clone())
            .filter(|text| !text.trim().is_empty());
        let fallback_session_id = inflight_state
            .as_ref()
            .and_then(|state| state.session_id.as_deref());
        let dispatch_id =
            resolve_watcher_dispatch_id(shared, channel_id, inflight_state.as_ref()).await;

        let decision = retry_text
            .as_deref()
            .map(|text| record_provider_overload_retry(channel_id, text))
            .unwrap_or(ProviderOverloadDecision::Exhausted);
        let retry_notice = match &decision {
            ProviderOverloadDecision::Retry { attempt, delay, .. } => format!(
                "⚠️ 모델 capacity 상태를 감지해 세션을 정리했습니다. {}분 후 자동 재시도합니다. ({}/{})",
                delay.as_secs() / 60,
                attempt,
                PROVIDER_OVERLOAD_MAX_RETRIES
            ),
            ProviderOverloadDecision::Exhausted => format!(
                "⚠️ 모델 capacity 상태가 계속되어 자동 재시도를 중단했습니다. 잠시 후 다시 시도해 주세요.\n\n사유: {}",
                truncate_str(overload_message, 300)
            ),
        };

        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 👁 Provider overload detected in watcher for {}: {}",
            tmux_session_name,
            overload_message
        );
        *state.prompt_too_long_killed = true;

        clear_provider_session_for_retry(
            shared,
            channel_id,
            tmux_session_name,
            fallback_session_id,
        )
        .await;

        let sess = (*tmux_session_name).clone();
        let termination_reason = match &decision {
            ProviderOverloadDecision::Retry { .. } => "provider_overload_retry",
            ProviderOverloadDecision::Exhausted => "provider_overload_exhausted",
        };
        let termination_detail = format!("watcher cleanup: {overload_message}");
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking(move || {
                crate::services::termination_audit::record_termination_for_tmux(
                    &sess,
                    None,
                    "tmux_watcher",
                    termination_reason,
                    Some(&termination_detail),
                    None,
                );
                record_tmux_exit_reason(&sess, &termination_detail);
                crate::services::platform::tmux::kill_session(&sess, &termination_detail);
            }),
        )
        .await;

        let notice_ok = match locals.placeholder_msg_id {
            Some(msg_id) => {
                rate_limit_wait(shared, channel_id).await;
                crate::services::discord::http::edit_channel_message(
                    http,
                    channel_id,
                    msg_id,
                    &retry_notice,
                )
                .await
                .is_ok()
            }
            None => crate::services::discord::http::send_channel_message(
                http,
                channel_id,
                &retry_notice,
            )
            .await
            .is_ok(),
        };
        if !notice_ok {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: provider overload notice failed before retry/failure handling — preserving inflight for retry"
            );
            finish_monitor_auto_turn_if_claimed(
                shared,
                watcher_provider,
                channel_id,
                &mut *state.monitor_auto_turn_claimed,
                &mut *state.monitor_auto_turn_finished,
                &mut *state.monitor_auto_turn_synthetic_msg_id,
                &mut *state.monitor_auto_turn_ledger_generation,
            )
            .await;
            return AbortExitOutcome::ContinueWatcherLoop;
        }

        // #897 round-3 Medium: skip reaction + retry scheduling for
        // `rebind_origin` inflights — they have no real user message
        // to react against and no real user text to re-prompt. The same
        // holds for user_msg_id == 0 (e.g. a TUI-direct turn): no message
        // to react against, and `MessageId::new(0)` would panic.
        if let Some(state) = inflight_state.as_ref().filter(|s| !s.rebind_origin)
            && let Some(user_msg_id) = opt_message_id(state.user_msg_id)
        {
            if matches!(&decision, ProviderOverloadDecision::Exhausted) {
                crate::services::discord::turn_view_reconciler::note_intake_turn_failed(
                    shared,
                    http,
                    channel_id,
                    user_msg_id,
                    state.born_generation,
                    "tmux_watcher_overload_exhausted",
                )
                .await;
            } else {
                crate::services::discord::turn_view_reconciler::note_intake_turn_cleared(
                    shared,
                    http,
                    channel_id,
                    user_msg_id,
                    state.born_generation,
                    "tmux_watcher_overload_retry",
                )
                .await;
            }
        }
        finalize_pinned_watcher_exit(
            shared,
            watcher_provider,
            channel_id,
            inflight_state.as_ref(),
            "watcher_provider_overload_exit",
        )
        .await;

        match decision {
            ProviderOverloadDecision::Retry {
                attempt,
                delay,
                fingerprint,
            } => {
                if let Some(retry_text) = retry_text {
                    // A turn with no anchored user message (rebind_origin or
                    // user_msg_id == 0, e.g. a TUI-direct turn) has no
                    // message to re-prompt against; clear retry state
                    // instead of building `MessageId::new(0)` (panics).
                    if let Some(state) = inflight_state.as_ref().filter(|s| !s.rebind_origin)
                        && let Some(user_msg_id) = opt_message_id(state.user_msg_id)
                    {
                        schedule_provider_overload_retry(
                            Arc::clone(shared),
                            Arc::clone(http),
                            watcher_provider.clone(),
                            channel_id,
                            user_msg_id,
                            retry_text,
                            attempt,
                            delay,
                            fingerprint,
                        );
                    } else {
                        clear_provider_overload_retry_state(channel_id);
                    }
                } else {
                    clear_provider_overload_retry_state(channel_id);
                }
            }
            ProviderOverloadDecision::Exhausted => {
                let failure_text = format!(
                    "provider overloaded after {} auto-retries: {}",
                    PROVIDER_OVERLOAD_MAX_RETRIES,
                    truncate_str(overload_message, 300)
                );
                crate::services::discord::turn_bridge::fail_dispatch_with_retry(
                    shared.api_port,
                    dispatch_id.as_deref(),
                    &failure_text,
                )
                .await;
            }
        }
        finish_monitor_auto_turn_if_claimed(
            shared,
            watcher_provider,
            channel_id,
            &mut *state.monitor_auto_turn_claimed,
            &mut *state.monitor_auto_turn_finished,
            &mut *state.monitor_auto_turn_synthetic_msg_id,
            &mut *state.monitor_auto_turn_ledger_generation,
        )
        .await;
        return AbortExitOutcome::ContinueWatcherLoop;
    }

    AbortExitOutcome::Fallthrough
}
