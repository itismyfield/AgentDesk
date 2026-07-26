use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AbortExitOutcome {
    ContinueWatcherLoop,
    Fallthrough,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalAbortDiagnosis {
    PromptTooLong,
    AuthenticationFailed,
    ProviderOverload,
}

impl TerminalAbortDiagnosis {
    fn public_reason(self) -> &'static str {
        match self {
            Self::PromptTooLong => "prompt_too_long",
            Self::AuthenticationFailed => "provider_authentication_failed",
            Self::ProviderOverload => "provider_capacity_response",
        }
    }

    fn dispatch_action(self) -> TerminalAbortDispatchAction {
        match self {
            Self::AuthenticationFailed => TerminalAbortDispatchAction::FailAuthExpired,
            Self::PromptTooLong | Self::ProviderOverload => {
                TerminalAbortDispatchAction::FailWithRetry
            }
        }
    }

    fn termination_reason(self) -> &'static str {
        match self {
            Self::PromptTooLong => "prompt_too_long",
            Self::AuthenticationFailed => "auth_error",
            Self::ProviderOverload => "provider_overload",
        }
    }

    fn notice(self, killed: bool) -> &'static str {
        match (self, killed) {
            (Self::PromptTooLong, true) => {
                "⚠️ 컨텍스트 한도 초과로 현재 thread 세션을 초기화했습니다. 다음 메시지부터 새 세션으로 처리됩니다."
            }
            (Self::PromptTooLong, false) => {
                "⚠️ 컨텍스트 한도 초과로 현재 turn을 종료했습니다. 보호된 tmux 세션과 watcher는 유지됩니다."
            }
            (Self::AuthenticationFailed, true) => {
                "⚠️ 인증이 만료되어 현재 dispatch를 실패 처리하고 thread 세션을 종료했습니다. 관리자가 CLI에서 재인증(`/login`)한 후 다시 디스패치해주세요."
            }
            (Self::AuthenticationFailed, false) => {
                "⚠️ 인증이 만료되어 현재 dispatch를 실패 처리했습니다. 보호된 tmux 세션과 watcher는 유지됩니다. 관리자가 CLI에서 재인증(`/login`)한 후 다시 디스패치해주세요."
            }
            (Self::ProviderOverload, true) => {
                "⚠️ provider capacity 오류로 현재 dispatch를 실패 처리하고 thread 세션을 종료했습니다. 잠시 후 다시 시도해주세요."
            }
            (Self::ProviderOverload, false) => {
                "⚠️ provider capacity 오류로 현재 dispatch를 실패 처리했습니다. 보호된 tmux 세션과 watcher는 유지됩니다. 잠시 후 다시 시도해주세요."
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalAbortDispatchAction {
    FailWithRetry,
    FailAuthExpired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatcherSessionRole {
    DisposableCurrentThread,
    Protected,
}

fn watcher_session_role(
    tmux_session_name: &str,
    watcher_provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> WatcherSessionRole {
    let expected_suffix = format!("-t{}", channel_id.get());
    let current_thread =
        crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_session_name)
            .filter(|(provider, _)| provider == watcher_provider)
            .map(|(_, channel_name)| channel_name)
            .is_some_and(|channel_name| {
                channel_name.ends_with(&expected_suffix)
                    && channel_name.len() > expected_suffix.len()
                    && !channel_name[..channel_name.len() - expected_suffix.len()].ends_with("-t")
            });
    if current_thread {
        WatcherSessionRole::DisposableCurrentThread
    } else {
        WatcherSessionRole::Protected
    }
}

#[derive(Debug, Clone)]
struct PinnedTerminalAbort {
    key: crate::services::discord::turn_finalizer::TurnKey,
    claim_snapshot: crate::services::discord::turn_finalizer::SyntheticClaimSnapshot,
    dispatch_id: Option<String>,
}

fn pinned_terminal_abort(
    shared: &SharedData,
    channel_id: serenity::ChannelId,
    tmux_session_name: &str,
    pinned: Option<&crate::services::discord::inflight::InflightTurnState>,
    terminal_evidence_offset: Option<u64>,
    consumed_offset: u64,
    current: Option<&crate::services::discord::inflight::InflightTurnState>,
) -> Option<PinnedTerminalAbort> {
    let pinned = pinned?;
    let finalizer_turn_id = pinned.effective_finalizer_turn_id();
    let evidence_offset = terminal_evidence_offset?;
    let turn_start_offset = pinned.turn_start_offset.unwrap_or(pinned.last_offset);
    let pinned_identity =
        crate::services::discord::inflight::InflightTurnIdentity::from_state(pinned);
    let current_identity_matches = current
        .map(|row| pinned_identity.matches_state(row))
        .unwrap_or(true);
    let same_session =
        pinned.tmux_session_name.as_deref().map(str::trim) == Some(tmux_session_name.trim());
    if finalizer_turn_id == 0
        || !same_session
        || !current_identity_matches
        || evidence_offset < turn_start_offset
        || evidence_offset >= consumed_offset
    {
        return None;
    }

    Some(PinnedTerminalAbort {
        key: crate::services::discord::turn_finalizer::TurnKey::new(
            channel_id,
            finalizer_turn_id,
            shared.restart.current_generation,
        ),
        claim_snapshot: crate::services::discord::turn_finalizer::SyntheticClaimSnapshot::from_row(
            pinned,
        ),
        dispatch_id: pinned.dispatch_id.clone().or_else(|| {
            crate::services::discord::adk_session::parse_dispatch_id(&pinned.user_text)
        }),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalAbortPlan {
    diagnosis: TerminalAbortDiagnosis,
    session_role: WatcherSessionRole,
}

impl TerminalAbortPlan {
    fn kills_session(self) -> bool {
        self.session_role == WatcherSessionRole::DisposableCurrentThread
    }
}

async fn execute_terminal_abort_plan_with<D, DFut, K, KFut, F, FFut, P, PFut, S, SFut, E>(
    plan: TerminalAbortPlan,
    dispatch: D,
    kill: K,
    finalize: F,
    project: P,
    settle_projection: S,
) -> AbortExitOutcome
where
    D: FnOnce(TerminalAbortDispatchAction, &'static str) -> DFut,
    DFut: std::future::Future<Output = ()>,
    K: FnOnce(bool, TerminalAbortDiagnosis) -> KFut,
    KFut: std::future::Future<Output = ()>,
    F: FnOnce() -> FFut,
    FFut: std::future::Future<Output = ()>,
    P: FnOnce() -> PFut,
    PFut: std::future::Future<Output = Result<(), E>>,
    S: FnOnce() -> SFut,
    SFut: std::future::Future<Output = ()>,
    E: std::fmt::Display,
{
    dispatch(
        plan.diagnosis.dispatch_action(),
        plan.diagnosis.public_reason(),
    )
    .await;
    kill(plan.kills_session(), plan.diagnosis).await;
    finalize().await;
    if let Err(error) = project().await {
        let error = crate::utils::redact::redact_known_secrets(&error.to_string());
        let error = crate::services::discord::formatting::redact_sensitive_for_placeholder(&error);
        tracing::warn!(
            diagnosis = plan.diagnosis.public_reason(),
            error,
            "watcher terminal notification failed after lifecycle finalization"
        );
    }
    settle_projection().await;
    AbortExitOutcome::ContinueWatcherLoop
}

#[derive(Debug, serde::Serialize)]
struct WatcherForcedKillLog<'a> {
    timestamp: String,
    session: &'a str,
    pane_id: Option<String>,
    pane_pid: Option<u32>,
    terminal_evidence_offset: Option<u64>,
    decision_reason: &'a str,
    live_background_workers: Vec<String>,
}

fn tmux_active_pane_id(tmux_session_name: &str) -> Option<String> {
    let target = format!("={tmux_session_name}");
    let output = std::process::Command::new("tmux")
        .args(["display-message", "-p", "-t", &target, "#{pane_id}"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let pane_id = String::from_utf8(output.stdout).ok()?;
    let pane_id = pane_id.trim();
    (!pane_id.is_empty()).then(|| pane_id.to_string())
}

fn write_watcher_forced_kill_log(
    shared: &SharedData,
    channel_id: serenity::ChannelId,
    tmux_session_name: &str,
    terminal_evidence_offset: Option<u64>,
    decision_reason: &str,
) {
    let record = WatcherForcedKillLog {
        timestamp: chrono::Utc::now().to_rfc3339(),
        session: tmux_session_name,
        pane_id: tmux_active_pane_id(tmux_session_name),
        pane_pid: crate::services::platform::tmux::pane_pid(tmux_session_name),
        terminal_evidence_offset,
        decision_reason,
        live_background_workers: shared
            .ui
            .placeholder_live_events
            .render_block(channel_id)
            .map(|block| {
                block
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .take(32)
                    .map(|line| truncate_str(line, 240).to_string())
                    .collect()
            })
            .unwrap_or_default(),
    };
    let path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "forced_kill_log");
    let serialized = match serde_json::to_string(&record) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                tmux_session = tmux_session_name,
                error = %error,
                "failed to serialize watcher forced-kill log"
            );
            return;
        }
    };
    if let Err(error) = std::fs::write(&path, format!("{serialized}\n")) {
        tracing::error!(
            tmux_session = tmux_session_name,
            path = %path,
            error = %error,
            "failed to persist watcher forced-kill log"
        );
    }
}

async fn execute_authorized_terminal_kill(
    should_kill: bool,
    diagnosis: TerminalAbortDiagnosis,
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    tmux_session_name: &str,
    terminal_evidence_offset: Option<u64>,
) {
    if !should_kill {
        tracing::warn!(
            tmux_session = tmux_session_name,
            channel_id = channel_id.get(),
            diagnosis = diagnosis.public_reason(),
            "watcher preserved protected tmux session after terminal provider error"
        );
        return;
    }

    write_watcher_forced_kill_log(
        shared,
        channel_id,
        tmux_session_name,
        terminal_evidence_offset,
        diagnosis.public_reason(),
    );
    let session = tmux_session_name.to_string();
    let detail = format!("watcher cleanup: {}", diagnosis.public_reason());
    let termination_reason = diagnosis.termination_reason();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            crate::services::termination_audit::record_termination_for_tmux(
                &session,
                None,
                "tmux_watcher",
                termination_reason,
                Some(&detail),
                None,
            );
            record_tmux_exit_reason(&session, &detail);
            crate::services::platform::tmux::kill_session(&session, &detail);
        }),
    )
    .await;
}

async fn project_terminal_abort_notice(
    http: &serenity::Http,
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    placeholder_msg_id: Option<serenity::MessageId>,
    notice: &str,
) -> serenity::Result<()> {
    match placeholder_msg_id {
        Some(msg_id) => {
            rate_limit_wait(shared, channel_id).await;
            crate::services::discord::http::edit_channel_message(http, channel_id, msg_id, notice)
                .await
                .map(|_| ())
        }
        None => crate::services::discord::http::send_channel_message(http, channel_id, notice)
            .await
            .map(|_| ()),
    }
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
    pub(super) startup_inflight_snapshot:
        Option<crate::services::discord::inflight::InflightTurnState>,
    pub(super) terminal_evidence_offset: Option<u64>,
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
    let paused_now = context.paused.load(Ordering::Relaxed);
    let epoch_changed_now = context.pause_epoch.load(Ordering::Relaxed) != locals.epoch_snapshot;
    let deferred_monitor_ready =
        *state.monitor_auto_turn_claimed && locals.monitor_auto_turn_deferred && !paused_now;
    if (locals.was_paused || paused_now || epoch_changed_now) && !deferred_monitor_ready {
        if let Some(msg_id) = locals.placeholder_msg_id {
            if watcher_should_delete_suppressed_placeholder(
                *state.placeholder_from_restored_inflight,
            ) {
                let inflight_before_cleanup =
                    crate::services::discord::inflight::load_inflight_state(
                        context.watcher_provider,
                        context.channel_id.get(),
                    );
                let _ = delete_nonterminal_placeholder_unless_delivered(
                    context.http,
                    context.channel_id,
                    context.shared,
                    context.watcher_provider,
                    context.tmux_session_name,
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
            context.shared,
            context.watcher_provider,
            context.channel_id,
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

    let diagnosis = if locals.is_provider_overloaded {
        Some(TerminalAbortDiagnosis::ProviderOverload)
    } else if locals.is_auth_error {
        Some(TerminalAbortDiagnosis::AuthenticationFailed)
    } else if locals.is_prompt_too_long {
        Some(TerminalAbortDiagnosis::PromptTooLong)
    } else {
        None
    };
    let Some(diagnosis) = diagnosis else {
        return AbortExitOutcome::Fallthrough;
    };

    let current = crate::services::discord::inflight::load_inflight_state(
        context.watcher_provider,
        context.channel_id.get(),
    );
    let Some(pinned) = pinned_terminal_abort(
        context.shared,
        context.channel_id,
        context.tmux_session_name,
        locals.startup_inflight_snapshot.as_ref(),
        locals.terminal_evidence_offset,
        locals.current_offset,
        current.as_ref(),
    ) else {
        tracing::warn!(
            provider = context.watcher_provider.as_str(),
            channel_id = context.channel_id.get(),
            tmux_session = context.tmux_session_name,
            terminal_evidence_offset = locals.terminal_evidence_offset.unwrap_or_default(),
            diagnosis = diagnosis.public_reason(),
            "watcher ignored terminal abort whose pinned turn identity no longer owns the evidence"
        );
        return AbortExitOutcome::ContinueWatcherLoop;
    };

    let plan = TerminalAbortPlan {
        diagnosis,
        session_role: watcher_session_role(
            context.tmux_session_name,
            context.watcher_provider,
            context.channel_id,
        ),
    };
    *state.prompt_too_long_killed = plan.kills_session();
    let notice = diagnosis.notice(plan.kills_session());
    let dispatch_id = pinned.dispatch_id.clone();
    let key = pinned.key;
    let claim_snapshot = pinned.claim_snapshot;

    let outcome = execute_terminal_abort_plan_with(
        plan,
        |action, reason| async move {
            match action {
                TerminalAbortDispatchAction::FailWithRetry => {
                    crate::services::discord::turn_bridge::fail_dispatch_with_retry(
                        context.shared.api_port,
                        dispatch_id.as_deref(),
                        reason,
                    )
                    .await;
                }
                TerminalAbortDispatchAction::FailAuthExpired => {
                    crate::services::discord::turn_bridge::fail_dispatch_auth_expired(
                        context.shared.api_port,
                        dispatch_id.as_deref(),
                        reason,
                    )
                    .await;
                }
            }
        },
        |should_kill, diagnosis| async move {
            execute_authorized_terminal_kill(
                should_kill,
                diagnosis,
                context.shared,
                context.channel_id,
                context.tmux_session_name,
                locals.terminal_evidence_offset,
            )
            .await;
        },
        || async move {
            let _ = context
                .shared
                .turn_finalizer
                .submit_terminal_with_claim_snapshot(
                    key,
                    context.watcher_provider.clone(),
                    crate::services::discord::turn_finalizer::TerminalEvent::Cancel,
                    crate::services::discord::turn_finalizer::FinalizeContext::stale_busy_mailbox(),
                    Some(claim_snapshot),
                    context.shared.clone(),
                )
                .await;
        },
        || async move {
            project_terminal_abort_notice(
                context.http,
                context.shared,
                context.channel_id,
                locals.placeholder_msg_id,
                notice,
            )
            .await
        },
        || async move {
            context
                .shared
                .turn_finalizer
                .note_terminal_projection_settled(key, true, context.shared.clone());
        },
    )
    .await;

    if *state.monitor_auto_turn_claimed {
        *state.monitor_auto_turn_claimed = false;
        *state.monitor_auto_turn_finished = true;
        *state.monitor_auto_turn_synthetic_msg_id = None;
        *state.monitor_auto_turn_ledger_generation = None;
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    use std::sync::{Arc, Mutex};

    fn test_state(
        channel_id: ChannelId,
        message_id: u64,
        session: &str,
        turn_start_offset: u64,
    ) -> crate::services::discord::inflight::InflightTurnState {
        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            ProviderKind::Claude,
            channel_id.get(),
            None,
            1,
            message_id,
            message_id,
            "turn".to_string(),
            Some("session".to_string()),
            Some(session.to_string()),
            Some("/tmp/terminal-abort.jsonl".to_string()),
            None,
            turn_start_offset,
        );
        state.turn_start_offset = Some(turn_start_offset);
        state
    }

    #[test]
    fn diagnosis_by_session_role_kill_matrix_is_fail_closed() {
        let channel_id = ChannelId::new(42);
        let sessions = [
            ("AgentDesk-claude-adk-cc-t42", true),
            ("AgentDesk-codex-worker-t42", false),
            ("AgentDesk-claude-adk-cc", false),
            ("AgentDesk-claude-adk-cc-t41", false),
            ("AgentDesk-claude-adk-cc-t42-extra", false),
            ("malformed-t42", false),
        ];
        for diagnosis in [
            TerminalAbortDiagnosis::PromptTooLong,
            TerminalAbortDiagnosis::AuthenticationFailed,
            TerminalAbortDiagnosis::ProviderOverload,
        ] {
            for (session, expected_kill) in sessions {
                let plan = TerminalAbortPlan {
                    diagnosis,
                    session_role: watcher_session_role(session, &ProviderKind::Claude, channel_id),
                };
                let kill_spy = Arc::new(Mutex::new(Vec::new()));
                let recorded = Arc::clone(&kill_spy);
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                runtime.block_on(execute_terminal_abort_plan_with(
                    plan,
                    |_action, _reason| async {},
                    move |kill, diagnosis| {
                        let recorded = Arc::clone(&recorded);
                        async move {
                            recorded.lock().expect("kill spy").push((kill, diagnosis));
                        }
                    },
                    || async {},
                    || async { Ok::<(), &str>(()) },
                    || async {},
                ));
                assert_eq!(
                    kill_spy.lock().expect("kill spy").as_slice(),
                    &[(expected_kill, diagnosis)],
                    "session={session} diagnosis={diagnosis:?}"
                );
            }
        }
    }

    #[test]
    fn pinned_identity_rejects_stale_evidence_and_newer_row() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let shared =
            runtime.block_on(async { crate::services::discord::make_shared_data_for_tests() });
        let channel_id = ChannelId::new(4_895_001);
        let session = "AgentDesk-claude-worker-t4895001";
        let old = test_state(channel_id, 4_895_101, session, 100);
        let newer = test_state(channel_id, 4_895_202, session, 200);

        assert!(
            pinned_terminal_abort(
                &shared,
                channel_id,
                session,
                Some(&old),
                Some(150),
                180,
                Some(&old),
            )
            .is_some()
        );
        assert!(
            pinned_terminal_abort(
                &shared,
                channel_id,
                session,
                Some(&old),
                Some(99),
                180,
                Some(&old),
            )
            .is_none(),
            "terminal evidence before the pinned turn must not be admitted"
        );
        assert!(
            pinned_terminal_abort(
                &shared,
                channel_id,
                session,
                Some(&old),
                Some(150),
                180,
                Some(&newer),
            )
            .is_none(),
            "a newer durable identity must survive a stale watcher terminal"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn executor_finalizes_once_and_settles_after_projection_failure() {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Step {
            Dispatch,
            Kill,
            Finalize,
            Project,
            Settle,
        }
        let steps = Arc::new(Mutex::new(Vec::new()));
        let dispatch = Arc::clone(&steps);
        let kill = Arc::clone(&steps);
        let finalize = Arc::clone(&steps);
        let project = Arc::clone(&steps);
        let settle = Arc::clone(&steps);
        let outcome = execute_terminal_abort_plan_with(
            TerminalAbortPlan {
                diagnosis: TerminalAbortDiagnosis::ProviderOverload,
                session_role: WatcherSessionRole::Protected,
            },
            move |_action, _reason| {
                let steps = Arc::clone(&dispatch);
                async move { steps.lock().expect("steps").push(Step::Dispatch) }
            },
            move |_should_kill, _diagnosis| {
                let steps = Arc::clone(&kill);
                async move { steps.lock().expect("steps").push(Step::Kill) }
            },
            move || {
                let steps = Arc::clone(&finalize);
                async move { steps.lock().expect("steps").push(Step::Finalize) }
            },
            move || {
                let steps = Arc::clone(&project);
                async move {
                    steps.lock().expect("steps").push(Step::Project);
                    Err::<(), _>("projection failed")
                }
            },
            move || {
                let steps = Arc::clone(&settle);
                async move { steps.lock().expect("steps").push(Step::Settle) }
            },
        )
        .await;

        assert_eq!(outcome, AbortExitOutcome::ContinueWatcherLoop);
        assert_eq!(
            steps.lock().expect("steps").as_slice(),
            &[
                Step::Dispatch,
                Step::Kill,
                Step::Finalize,
                Step::Project,
                Step::Settle,
            ]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplicate_terminal_submission_is_exactly_once() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::tempdir().expect("runtime root");
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };

        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_895_002);
        let message_id = 4_895_102;
        let session = "AgentDesk-claude-worker-t4895002";
        let state = test_state(channel_id, message_id, session, 100);
        crate::services::discord::inflight::save_inflight_state(&state).expect("save inflight");
        let token = Arc::new(crate::services::provider::CancelToken::new());
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                shared.as_ref(),
                channel_id,
                token.clone(),
                UserId::new(1),
                MessageId::new(message_id),
            )
            .await
        );
        shared.restart.global_active.store(1, Ordering::Relaxed);
        let key = crate::services::discord::turn_finalizer::TurnKey::new(
            channel_id,
            state.effective_finalizer_turn_id(),
            shared.restart.current_generation,
        );
        shared.turn_finalizer.register_start(
            key,
            ProviderKind::Claude,
            crate::services::discord::inflight::RelayOwnerKind::Watcher,
            &shared,
        );

        let first = shared
            .turn_finalizer
            .submit_terminal_with_claim_snapshot(
                key,
                ProviderKind::Claude,
                crate::services::discord::turn_finalizer::TerminalEvent::Cancel,
                crate::services::discord::turn_finalizer::FinalizeContext::stale_busy_mailbox(),
                Some(
                    crate::services::discord::turn_finalizer::SyntheticClaimSnapshot::from_row(
                        &state,
                    ),
                ),
                shared.clone(),
            )
            .await;
        let second = shared
            .turn_finalizer
            .submit_terminal_with_claim_snapshot(
                key,
                ProviderKind::Claude,
                crate::services::discord::turn_finalizer::TerminalEvent::Cancel,
                crate::services::discord::turn_finalizer::FinalizeContext::stale_busy_mailbox(),
                Some(
                    crate::services::discord::turn_finalizer::SyntheticClaimSnapshot::from_row(
                        &state,
                    ),
                ),
                shared.clone(),
            )
            .await;

        assert!(matches!(
            first,
            crate::services::discord::turn_finalizer::FinalizeOutcome::Finalized { .. }
        ));
        assert!(matches!(
            second,
            crate::services::discord::turn_finalizer::FinalizeOutcome::AlreadyFinalized
        ));
        assert!(token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);

        match previous {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }
}
