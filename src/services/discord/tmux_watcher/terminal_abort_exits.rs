use super::*;
use std::sync::Arc;

use crate::services::discord::inflight::opt_message_id;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AbortExitOutcome {
    ContinueWatcherLoop,
    PreserveWatcher,
    Fallthrough,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalAbortKind {
    PromptTooLong,
    AuthError,
    ProviderOverload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalAbortDispatchAction {
    FailWithRetry,
    FailAuthExpired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalAbortPlan {
    kill_session: bool,
    preserve_watcher: bool,
    dispatch_action: Option<TerminalAbortDispatchAction>,
}

fn terminal_abort_plan(
    tmux_session_name: &str,
    channel_id: serenity::ChannelId,
    kind: TerminalAbortKind,
) -> TerminalAbortPlan {
    let main_orchestration_session =
        watcher_session_is_main_orchestration(tmux_session_name, channel_id);
    let dispatch_action = match kind {
        TerminalAbortKind::PromptTooLong => Some(TerminalAbortDispatchAction::FailWithRetry),
        TerminalAbortKind::AuthError => Some(TerminalAbortDispatchAction::FailAuthExpired),
        TerminalAbortKind::ProviderOverload if main_orchestration_session => {
            Some(TerminalAbortDispatchAction::FailWithRetry)
        }
        TerminalAbortKind::ProviderOverload => None,
    };
    TerminalAbortPlan {
        kill_session: !main_orchestration_session,
        preserve_watcher: main_orchestration_session,
        dispatch_action,
    }
}

async fn execute_terminal_abort_plan_with<F, Fut, G, GFut>(
    plan: TerminalAbortPlan,
    execute_dispatch: F,
    finalize_turn: G,
) -> AbortExitOutcome
where
    F: FnOnce(TerminalAbortDispatchAction) -> Fut,
    Fut: std::future::Future<Output = ()>,
    G: FnOnce() -> GFut,
    GFut: std::future::Future<Output = ()>,
{
    if let Some(action) = plan.dispatch_action {
        execute_dispatch(action).await;
    }
    finalize_turn().await;
    if plan.preserve_watcher {
        AbortExitOutcome::PreserveWatcher
    } else {
        AbortExitOutcome::ContinueWatcherLoop
    }
}

async fn execute_terminal_abort_plan<F, Fut>(
    plan: TerminalAbortPlan,
    api_port: u16,
    dispatch_id: Option<&str>,
    failure_text: &str,
    finalize_turn: F,
) -> AbortExitOutcome
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    execute_terminal_abort_plan_with(
        plan,
        |action| async move {
            match action {
                TerminalAbortDispatchAction::FailWithRetry => {
                    crate::services::discord::turn_bridge::fail_dispatch_with_retry(
                        api_port,
                        dispatch_id,
                        failure_text,
                    )
                    .await;
                }
                TerminalAbortDispatchAction::FailAuthExpired => {
                    crate::services::discord::turn_bridge::fail_dispatch_auth_expired(
                        api_port,
                        dispatch_id,
                        failure_text,
                    )
                    .await;
                }
            }
        },
        finalize_turn,
    )
    .await
}

fn should_run_post_stream_exit(outcome: AbortExitOutcome) -> bool {
    matches!(outcome, AbortExitOutcome::Fallthrough)
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
    let main_orchestration_session =
        watcher_session_is_main_orchestration(tmux_session_name, channel_id);
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

    // Handle prompt-too-long: kill disposable thread sessions so the next message
    // creates a fresh one; preserve the main orchestration session and watcher.
    if locals.is_prompt_too_long {
        clear_provider_overload_retry_state(channel_id);
        let ts = chrono::Local::now().format("%H:%M:%S");
        let plan = terminal_abort_plan(
            tmux_session_name,
            channel_id,
            TerminalAbortKind::PromptTooLong,
        );
        *state.prompt_too_long_killed = plan.kill_session;
        let inflight_state = crate::services::discord::inflight::load_inflight_state(
            watcher_provider,
            channel_id.get(),
        );
        let dispatch_id =
            resolve_watcher_dispatch_id(shared, channel_id, inflight_state.as_ref()).await;
        if main_orchestration_session {
            let fallback_session_id = crate::services::discord::inflight::load_inflight_state(
                watcher_provider,
                channel_id.get(),
            )
            .and_then(|inflight| inflight.session_id);
            clear_provider_session_for_retry(
                shared,
                channel_id,
                tmux_session_name,
                fallback_session_id.as_deref(),
            )
            .await;
            tracing::error!(
                tmux_session = %tmux_session_name,
                channel_id = channel_id.get(),
                current_offset = locals.current_offset,
                decision_reason = "prompt_too_long",
                "watcher blocked automatic kill of main orchestration session; cleared provider session and continued watcher"
            );
        }
        if plan.kill_session {
            tracing::info!(
                "  [{ts}] 👁 Prompt too long detected in watcher for {tmux_session_name}, killing session"
            );
            write_watcher_forced_kill_log(
                shared,
                channel_id,
                tmux_session_name,
                locals.current_offset,
                "prompt_too_long",
            );
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
        }

        let notice = if main_orchestration_session {
            "⚠️ 컨텍스트 한도 초과를 감지해 provider 대화 ID를 초기화했습니다. 메인 tmux 세션은 보존하며 다음 메시지는 새 대화로 시작됩니다."
        } else {
            "⚠️ 컨텍스트 한도 초과로 세션을 초기화했습니다. 다음 메시지부터 새 세션으로 처리됩니다."
        };
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
        let outcome = execute_terminal_abort_plan(
            plan,
            shared.api_port,
            dispatch_id.as_deref(),
            "prompt too long; provider session cleared for a fresh retry",
            || async {
                finalize_pinned_watcher_exit(
                    shared,
                    watcher_provider,
                    channel_id,
                    inflight_state.as_ref(),
                    "watcher_prompt_too_long_exit",
                )
                .await;
            },
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
        return outcome;
    }

    // Handle auth error: kill disposable sessions and notify the user; preserve
    // main orchestration sessions and their watcher.
    if locals.is_auth_error {
        clear_provider_overload_retry_state(channel_id);
        let plan = terminal_abort_plan(tmux_session_name, channel_id, TerminalAbortKind::AuthError);
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
        // This legacy flag suppresses the later pane-death handoff only when
        // this branch actually killed a disposable session.
        *state.prompt_too_long_killed = plan.kill_session;

        clear_provider_session_for_retry(
            shared,
            channel_id,
            tmux_session_name,
            fallback_session_id,
        )
        .await;
        if main_orchestration_session {
            tracing::error!(
                tmux_session = %tmux_session_name,
                channel_id = channel_id.get(),
                current_offset = locals.current_offset,
                decision_reason = "authentication_failed",
                "watcher blocked automatic kill of main orchestration session; continuing watcher"
            );
        }
        if plan.kill_session {
            write_watcher_forced_kill_log(
                shared,
                channel_id,
                tmux_session_name,
                locals.current_offset,
                "authentication_failed",
            );
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
        }

        let notice = if main_orchestration_session {
            format!(
                "⚠️ 인증이 만료되어 현재 dispatch를 실패 처리했습니다. 메인 오케스트레이션 세션은 보존하고 watcher 감시를 계속합니다.\n관리자가 CLI에서 재인증(`/login`)을 완료한 후 다시 디스패치해주세요.\n\n사유: {}",
                truncate_str(auth_detail, 300)
            )
        } else {
            format!(
                "⚠️ 인증이 만료되어 현재 dispatch를 실패 처리했습니다. 세션을 종료합니다.\n관리자가 CLI에서 재인증(`/login`)을 완료한 후 다시 디스패치해주세요.\n\n사유: {}",
                truncate_str(auth_detail, 300)
            )
        };
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
        let failure_text = format!(
            "authentication expired; re-authentication required: {}",
            truncate_str(auth_detail, 300)
        );
        let outcome = execute_terminal_abort_plan(
            plan,
            shared.api_port,
            dispatch_id.as_deref(),
            &failure_text,
            || async {
                finalize_pinned_watcher_exit(
                    shared,
                    watcher_provider,
                    channel_id,
                    inflight_state.as_ref(),
                    "watcher_auth_error_exit",
                )
                .await;
            },
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
        return outcome;
    }

    if locals.is_provider_overloaded {
        let plan = terminal_abort_plan(
            tmux_session_name,
            channel_id,
            TerminalAbortKind::ProviderOverload,
        );
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
        let retry_notice = if main_orchestration_session {
            format!(
                "⚠️ 모델 capacity 상태를 감지했지만 메인 오케스트레이션 세션은 보존했습니다. 현재 turn을 실패 처리하고 watcher 감시를 계속합니다.\n\n사유: {}",
                truncate_str(overload_message, 300)
            )
        } else {
            match &decision {
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
            }
        };

        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 👁 Provider overload detected in watcher for {}: {}",
            tmux_session_name,
            overload_message
        );
        *state.prompt_too_long_killed = plan.kill_session;

        clear_provider_session_for_retry(
            shared,
            channel_id,
            tmux_session_name,
            fallback_session_id,
        )
        .await;

        if main_orchestration_session {
            tracing::error!(
                tmux_session = %tmux_session_name,
                channel_id = channel_id.get(),
                current_offset = locals.current_offset,
                decision_reason = "structured_provider_overload",
                "watcher blocked automatic kill of main orchestration session; continuing watcher"
            );
        }
        if plan.kill_session {
            write_watcher_forced_kill_log(
                shared,
                channel_id,
                tmux_session_name,
                locals.current_offset,
                overload_message,
            );
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
        }

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
            if main_orchestration_session {
                tracing::warn!(
                    "  [{ts}] ⚠ watcher: provider overload notice failed; still failing the active dispatch before releasing the protected turn"
                );
            } else {
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
        let failure_text = format!(
            "provider overloaded; main orchestration session preserved: {}",
            truncate_str(overload_message, 300)
        );
        let outcome = if plan.dispatch_action.is_some() {
            let outcome = execute_terminal_abort_plan(
                plan,
                shared.api_port,
                dispatch_id.as_deref(),
                &failure_text,
                || async {
                    finalize_pinned_watcher_exit(
                        shared,
                        watcher_provider,
                        channel_id,
                        inflight_state.as_ref(),
                        "watcher_provider_overload_exit",
                    )
                    .await;
                },
            )
            .await;
            clear_provider_overload_retry_state(channel_id);
            outcome
        } else {
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
            AbortExitOutcome::ContinueWatcherLoop
        };
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
        return outcome;
    }

    AbortExitOutcome::Fallthrough
}

#[cfg(test)]
mod tests {
    use super::{
        AbortExitOutcome, TerminalAbortDispatchAction, TerminalAbortKind,
        execute_terminal_abort_plan_with, should_run_post_stream_exit, terminal_abort_plan,
    };
    use poise::serenity_prelude as serenity;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TestExecutionStep {
        Dispatch(TerminalAbortDispatchAction),
        Finalize,
    }

    #[tokio::test]
    async fn main_terminal_aborts_execute_one_authority_and_preserve_watcher() {
        let channel_id = serenity::ChannelId::new(42);
        for (kind, expected_action) in [
            (
                TerminalAbortKind::PromptTooLong,
                TerminalAbortDispatchAction::FailWithRetry,
            ),
            (
                TerminalAbortKind::AuthError,
                TerminalAbortDispatchAction::FailAuthExpired,
            ),
            (
                TerminalAbortKind::ProviderOverload,
                TerminalAbortDispatchAction::FailWithRetry,
            ),
        ] {
            let plan = terminal_abort_plan("AgentDesk-claude-adk-cc", channel_id, kind);
            assert!(
                !plan.kill_session,
                "main tmux must never be an abort kill target"
            );
            let calls = Arc::new(Mutex::new(Vec::new()));
            let dispatch_recorded = Arc::clone(&calls);
            let finalize_recorded = Arc::clone(&calls);
            let outcome = execute_terminal_abort_plan_with(
                plan,
                move |action| {
                    let recorded = Arc::clone(&dispatch_recorded);
                    async move {
                        recorded
                            .lock()
                            .expect("terminal execution log")
                            .push(TestExecutionStep::Dispatch(action));
                    }
                },
                move || {
                    let recorded = Arc::clone(&finalize_recorded);
                    async move {
                        recorded
                            .lock()
                            .expect("terminal execution log")
                            .push(TestExecutionStep::Finalize);
                    }
                },
            )
            .await;

            assert_eq!(
                calls.lock().expect("terminal execution log").as_slice(),
                &[
                    TestExecutionStep::Dispatch(expected_action),
                    TestExecutionStep::Finalize,
                ],
                "dispatch failure must execute exactly once before finalization can publish completion"
            );
            assert_eq!(
                outcome,
                AbortExitOutcome::PreserveWatcher,
                "protected tmux must retain its registry-owning watcher"
            );
            assert!(
                !should_run_post_stream_exit(outcome),
                "preserved watchers must never reach registry cleanup"
            );
        }
    }

    #[tokio::test]
    async fn disposable_terminal_aborts_keep_existing_kill_and_retry_policy() {
        let channel_id = serenity::ChannelId::new(42);
        let thread_session = "AgentDesk-claude-adk-cc-t42";
        let prompt =
            terminal_abort_plan(thread_session, channel_id, TerminalAbortKind::PromptTooLong);
        assert!(prompt.kill_session);
        let prompt_calls = Arc::new(Mutex::new(Vec::new()));
        let prompt_recorded = Arc::clone(&prompt_calls);
        let prompt_outcome = execute_terminal_abort_plan_with(
            prompt,
            move |action| {
                let prompt_recorded = Arc::clone(&prompt_recorded);
                async move {
                    prompt_recorded
                        .lock()
                        .expect("prompt dispatch log")
                        .push(action);
                }
            },
            || async {},
        )
        .await;
        assert_eq!(
            prompt_calls.lock().expect("prompt dispatch log").as_slice(),
            &[TerminalAbortDispatchAction::FailWithRetry]
        );
        assert_eq!(prompt_outcome, AbortExitOutcome::ContinueWatcherLoop);

        let auth = terminal_abort_plan(thread_session, channel_id, TerminalAbortKind::AuthError);
        assert!(auth.kill_session);
        let auth_calls = Arc::new(Mutex::new(Vec::new()));
        let auth_recorded = Arc::clone(&auth_calls);
        let auth_outcome = execute_terminal_abort_plan_with(
            auth,
            move |action| {
                let auth_recorded = Arc::clone(&auth_recorded);
                async move {
                    auth_recorded
                        .lock()
                        .expect("auth dispatch log")
                        .push(action);
                }
            },
            || async {},
        )
        .await;
        assert_eq!(
            auth_calls.lock().expect("auth dispatch log").as_slice(),
            &[TerminalAbortDispatchAction::FailAuthExpired]
        );
        assert_eq!(auth_outcome, AbortExitOutcome::ContinueWatcherLoop);

        let overload = terminal_abort_plan(
            thread_session,
            channel_id,
            TerminalAbortKind::ProviderOverload,
        );
        assert!(overload.kill_session);
        let overload_calls = Arc::new(Mutex::new(Vec::new()));
        let overload_recorded = Arc::clone(&overload_calls);
        let overload_outcome = execute_terminal_abort_plan_with(
            overload,
            move |action| {
                let overload_recorded = Arc::clone(&overload_recorded);
                async move {
                    overload_recorded
                        .lock()
                        .expect("overload dispatch log")
                        .push(action);
                }
            },
            || async {},
        )
        .await;
        assert!(
            overload_calls
                .lock()
                .expect("overload dispatch log")
                .is_empty()
        );
        assert_eq!(overload_outcome, AbortExitOutcome::ContinueWatcherLoop);
    }
}
