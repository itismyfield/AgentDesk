use super::super::*;
use crate::services::discord::InflightRestartMode;
use crate::services::provider::{CancelToken, ProviderKind};
#[cfg(unix)]
use crate::services::tmux_diagnostics::record_tmux_exit_reason;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TmuxCleanupPolicy {
    PreserveSession,
    PreserveSessionAndInflight {
        restart_mode: InflightRestartMode,
    },
    CleanupSession {
        termination_reason_code: Option<&'static str>,
    },
}

impl TmuxCleanupPolicy {
    pub(crate) const fn preserves_inflight(self) -> Option<InflightRestartMode> {
        match self {
            Self::PreserveSessionAndInflight { restart_mode } => Some(restart_mode),
            Self::PreserveSession | Self::CleanupSession { .. } => None,
        }
    }

    pub(crate) const fn should_cleanup_tmux(self) -> bool {
        matches!(self, Self::CleanupSession { .. })
    }

    pub(crate) const fn should_clear_inflight(self) -> bool {
        !matches!(self, Self::PreserveSessionAndInflight { .. })
    }
}

/// Upper bound for how long we wait for the provider CLI to exit on its own
/// after the C-c / SIGINT-fallback interrupt was delivered. #2426: this used
/// to be an unconditional `tokio::sleep`; we now subscribe to the provider's
/// PID exit (kqueue on macOS, pidfd on Linux) and only fall back to the
/// upper-bound when the exit signal never fires. The constant is now a
/// *safety net*, not the primary timing source.
const PROVIDER_INTERRUPT_SETTLE: Duration = Duration::from_millis(750);
// #3021 / codex P2: confirmation delay before treating a claude pane as
// genuinely idle and skipping its SIGINT. Guards against a stale JSONL
// turn-state read in the sub-second window after a follow-up prompt is
// submitted but before claude flushes the new `user` envelope.
const PROVIDER_IDLE_CONFIRM_DELAY: Duration = Duration::from_millis(400);
/// Upper bound for the post-SIGINT grace period before we escalate to
/// SIGKILL. Same #2426 rationale as `PROVIDER_INTERRUPT_SETTLE`: when the
/// provider exits cleanly we observe its PID exit and proceed immediately.
const PROVIDER_HARD_STOP_GRACE: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProviderTurnInterruptPlan {
    keys: &'static [&'static str],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::services::discord) struct ProviderTurnInterruptOutcome {
    pub tmux_session: Option<String>,
    pub sent_keys: bool,
    pub fallback_sigint_pid: Option<u32>,
    pub missing_tmux_session: bool,
    /// #3029(A): set when the SIGINT-only interrupt path (empty key list —
    /// claude, whose pane C-c targets the wrapper) needed to deliver a SIGINT
    /// to an actively-generating turn but the provider PID lookup returned
    /// `None` (ps failure, command-name drift, or a just-spawned child not yet
    /// visible). On that path the interrupt is the *only* delivery mechanism,
    /// so a `None` PID is a silent no-op: the mailbox marks the turn [Stopped]
    /// but no signal reaches the provider. This flag converts that into an
    /// explicit failure the hard-stop path can escalate on, instead of
    /// reporting unconditional success.
    pub sigint_target_missing: bool,
}

/// #3029(A): does this provider/plan combination treat the direct SIGINT
/// fallback as its *only* interrupt delivery (i.e. there is no send-keys path
/// that could have reached the turn)? Claude uses an empty key list because a
/// pane C-c hits the wrapper and tears the session down (#1260) — so when its
/// SIGINT target is missing, the interrupt silently did nothing.
fn interrupt_is_sigint_only(provider: &ProviderKind, plan_keys_empty: bool) -> bool {
    plan_keys_empty && matches!(provider, ProviderKind::Claude)
}

/// #3029(A): the interrupt silently did nothing when the SIGINT-only path was
/// the only delivery mechanism, the turn was genuinely active
/// (`ready_for_input == false`), yet no SIGINT target PID could be resolved.
/// An idle pane (`ready_for_input == true`) intentionally resolves to no PID
/// and is NOT a missed interrupt (#3021).
fn interrupt_sigint_target_missing(
    provider: &ProviderKind,
    plan_keys_empty: bool,
    ready_for_input: bool,
    resolved_sigint_pid: Option<u32>,
) -> bool {
    interrupt_is_sigint_only(provider, plan_keys_empty)
        && !ready_for_input
        && resolved_sigint_pid.is_none()
}

fn provider_turn_interrupt_plan(provider: &ProviderKind) -> Option<ProviderTurnInterruptPlan> {
    match provider {
        // Claude runs as a child of `agentdesk tmux-wrapper`, with stdin
        // *piped* from the wrapper rather than wired to the PTY. A
        // `tmux send-keys C-c` on the pane therefore delivers SIGINT to the
        // wrapper (the PTY foreground), not to claude — and the wrapper has
        // no SIGINT handler, so it dies and tears the pane down with it
        // (#1260). We send SIGINT directly to claude's PID via the fallback
        // path instead; the empty key list signals "skip send-keys, go
        // straight to the SIGINT fallback".
        ProviderKind::Claude => Some(ProviderTurnInterruptPlan { keys: &[] }),
        ProviderKind::Codex => Some(ProviderTurnInterruptPlan { keys: &["Escape"] }),
        ProviderKind::Qwen => Some(ProviderTurnInterruptPlan { keys: &["C-c"] }),
        ProviderKind::Gemini | ProviderKind::OpenCode | ProviderKind::Unsupported(_) => None,
    }
}

fn fallback_sigint_pid_for_provider(
    provider: &ProviderKind,
    ready_for_input: bool,
    provider_pid: Option<u32>,
) -> Option<u32> {
    match provider {
        // #3021: Claude has NO send-keys interrupt path (empty key list — see
        // `provider_turn_interrupt_plan`), so the direct SIGINT is both its
        // only interrupt AND, on an idle pane, a process-kill that terminates
        // the TUI, kills the pane, and makes the watcher tear down the whole
        // tmux session as "dead after turn". Under `PreserveSession` (e.g. ⏳
        // reaction removal on a finished-but-still-"active" turn) that destroys
        // the session + context — the opposite of the policy intent. When the
        // pane is confirmed idle (`ready_for_input`, double-checked by the
        // caller's confirmation re-probe so a stale post-submit read cannot
        // pass) there is no generation to interrupt, so skip the SIGINT and
        // leave the live process alone. An actively streaming turn reports
        // `ready_for_input == false` and is still interrupted (#1260). The
        // hard-stop path already treats `ready_for_input` as "do not kill"
        // (`hard_stop_pid_for_unresponsive_provider`); this mirrors it.
        ProviderKind::Claude => {
            if ready_for_input {
                None
            } else {
                provider_pid
            }
        }
        // Codex/Qwen also send Escape/C-c, but those keys reach the wrapper
        // PTY rather than the separately-spawned provider child, so the direct
        // SIGINT fallback is what actually stops them. The readiness probe can
        // read a stale terminal/ready state in the sub-second window after a
        // follow-up submit, and (unlike Claude) these providers get no
        // confirmation re-probe — so do NOT gate their interrupt on
        // `ready_for_input`. Always deliver the fallback when we have the child
        // PID, matching base-branch behavior (#1260).
        ProviderKind::Codex | ProviderKind::Qwen => provider_pid,
        ProviderKind::Gemini | ProviderKind::OpenCode | ProviderKind::Unsupported(_) => None,
    }
}

fn tmux_ready_for_input_without_tui_pane(tmux_session_name: &str, provider: &ProviderKind) -> bool {
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name);
    let runtime_kind = binding
        .as_ref()
        .map(|binding| binding.runtime_kind)
        .or_else(|| {
            crate::services::tmux_common::resolve_tmux_runtime_kind_marker(tmux_session_name)
        });
    if let Some(ready) = binding
        .as_ref()
        .and_then(|binding| {
            crate::services::tui_turn_state::runtime_binding_ready_for_input(
                provider, binding, true,
            )
        })
        .map(crate::services::tui_turn_state::TuiReadyState::is_ready)
    {
        return ready;
    }
    if crate::services::tui_turn_state::pane_ready_fallback_allowed(provider, runtime_kind) {
        crate::services::provider::tmux_session_ready_for_input(tmux_session_name, provider)
    } else {
        false
    }
}

pub(in crate::services::discord) async fn interrupt_provider_cli_turn(
    provider: &ProviderKind,
    token: &Arc<CancelToken>,
    reason: &str,
) -> ProviderTurnInterruptOutcome {
    let tmux_session = token
        .tmux_session
        .lock()
        .ok()
        .and_then(|guard| guard.clone());
    let tracked_child_pid = token.child_pid.lock().ok().and_then(|guard| *guard);
    let Some(tmux_session_name) = tmux_session.as_deref() else {
        tracing::error!(
            "provider turn interrupt skipped: provider={} reason={} error=cancel_token_missing_tmux_session",
            provider.as_str(),
            reason
        );
        return ProviderTurnInterruptOutcome {
            tmux_session,
            sent_keys: false,
            fallback_sigint_pid: None,
            missing_tmux_session: true,
            sigint_target_missing: false,
        };
    };
    let Some(plan) = provider_turn_interrupt_plan(provider) else {
        return ProviderTurnInterruptOutcome {
            tmux_session,
            sent_keys: false,
            fallback_sigint_pid: None,
            missing_tmux_session: false,
            sigint_target_missing: false,
        };
    };

    // #1260: an empty key list means "no keys to send; go straight to the
    // SIGINT fallback". Used by claude, where C-c on the pane targets the
    // wrapper PID and would tear the session down. We treat the empty-key
    // path as an unconditional success so the SIGINT-fallback section below
    // still runs.
    let sent_keys = if plan.keys.is_empty() {
        true
    } else {
        let session_for_send = tmux_session_name.to_string();
        let keys = plan.keys.to_vec();
        let send_result = tokio::task::spawn_blocking(move || {
            crate::services::platform::tmux::send_keys(&session_for_send, &keys)
        })
        .await;

        match send_result {
            Ok(Ok(output)) if output.status.success() => true,
            Ok(Ok(output)) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                tracing::warn!(
                    "provider turn interrupt send-keys failed: provider={} session={} reason={} status={} stderr={}",
                    provider.as_str(),
                    tmux_session_name,
                    reason,
                    output.status,
                    stderr
                );
                false
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    "provider turn interrupt send-keys error: provider={} session={} reason={} error={}",
                    provider.as_str(),
                    tmux_session_name,
                    reason,
                    error
                );
                false
            }
            Err(error) => {
                tracing::warn!(
                    "provider turn interrupt send-keys join error: provider={} session={} reason={} error={}",
                    provider.as_str(),
                    tmux_session_name,
                    reason,
                    error
                );
                false
            }
        }
    };

    if !sent_keys {
        return ProviderTurnInterruptOutcome {
            tmux_session,
            sent_keys,
            fallback_sigint_pid: None,
            missing_tmux_session: false,
            sigint_target_missing: false,
        };
    }

    // #2426: instead of an unconditional `sleep(PROVIDER_INTERRUPT_SETTLE)`,
    // observe the provider PID's actual exit. We look up the provider PID
    // *before* waiting so we can subscribe to its exit signal (kqueue on
    // macOS, pidfd_open+poll on Linux). When the provider exits cleanly the
    // wait returns immediately; otherwise the 750ms upper bound matches the
    // pre-#2426 behavior as a safety net.
    let preinterrupt_session = tmux_session_name.to_string();
    let preinterrupt_provider = provider.clone();
    let early_provider_pid = tokio::task::spawn_blocking(move || {
        provider_cli_pid_in_tmux(
            &preinterrupt_session,
            &preinterrupt_provider,
            tracked_child_pid,
        )
    })
    .await
    .ok()
    .flatten();
    if let Some(pid) = early_provider_pid {
        wait_for_pid_exit(pid, PROVIDER_INTERRUPT_SETTLE).await;
    } else {
        // Fall back to the original wall-clock wait when we can't observe a
        // PID directly (e.g. provider not yet visible in `ps` output).
        tokio::time::sleep(PROVIDER_INTERRUPT_SETTLE).await;
    }

    let session_for_probe = tmux_session_name.to_string();
    let provider_for_probe = provider.clone();
    let probe = tokio::task::spawn_blocking(move || {
        let ready_for_input =
            tmux_ready_for_input_without_tui_pane(&session_for_probe, &provider_for_probe);
        let provider_pid =
            provider_cli_pid_in_tmux(&session_for_probe, &provider_for_probe, tracked_child_pid);
        (ready_for_input, provider_pid)
    })
    .await;

    let (ready_for_input, provider_pid) = match probe {
        Ok(values) => values,
        Err(error) => {
            tracing::warn!(
                "provider turn interrupt probe join error: provider={} session={} reason={} error={}",
                provider.as_str(),
                tmux_session_name,
                reason,
                error
            );
            (false, None)
        }
    };

    // #3021 / codex P2: the JSONL turn-state probe can briefly read a stale
    // terminal `result` in the sub-second window after a follow-up prompt is
    // submitted but before claude flushes the new `user` envelope to the
    // transcript — reporting ready_for_input=true for a turn that is actually
    // still generating. Claude has no send-keys interrupt path, so skipping
    // its SIGINT on that stale read would leave a cancelled turn running. The
    // 750ms settle above already lets the envelope land for a genuinely active
    // turn; require a second agreeing read before committing to "idle, skip
    // SIGINT" so a freshly-submitted turn (which flips to Busy on the confirm
    // read) is still interrupted, while a genuinely idle pane (#3021) stays
    // ready across both reads and is correctly left alone. Only claude needs
    // this — every other provider also has a send-keys/Escape interrupt, and
    // the extra delay is confined to the idle-stop path where nothing is being
    // interrupted anyway.
    let (ready_for_input, provider_pid) = if ready_for_input
        && matches!(provider, ProviderKind::Claude)
    {
        tokio::time::sleep(PROVIDER_IDLE_CONFIRM_DELAY).await;
        let session_for_confirm = tmux_session_name.to_string();
        let provider_for_confirm = provider.clone();
        let confirm = tokio::task::spawn_blocking(move || {
            let ready =
                tmux_ready_for_input_without_tui_pane(&session_for_confirm, &provider_for_confirm);
            let pid = provider_cli_pid_in_tmux(
                &session_for_confirm,
                &provider_for_confirm,
                tracked_child_pid,
            );
            (ready, pid)
        })
        .await;
        match confirm {
            // Stay "idle" only if the confirmation agrees; otherwise treat the
            // turn as active and interrupt with the freshly observed pid.
            //
            // codex review P2: use ONLY the confirm probe's freshly-observed
            // pid — never fall back to the pre-confirm `provider_pid`. If the
            // provider/pane exited during the 400ms confirm window the confirm
            // pid is `None`; reviving the stale pre-confirm pid here would
            // SIGINT a process that is no longer under the tmux pane (and could
            // signal an unrelated process after PID reuse). A `None` confirm
            // pid therefore correctly skips the SIGINT — there is nothing left
            // to interrupt.
            Ok((confirmed_ready, confirmed_pid)) => (confirmed_ready, confirmed_pid),
            Err(error) => {
                tracing::warn!(
                    "provider turn interrupt idle-confirm join error: provider={} session={} reason={} error={}",
                    provider.as_str(),
                    tmux_session_name,
                    reason,
                    error
                );
                // On a confirm-probe failure we cannot re-validate the pid, so
                // do NOT reuse the stale pre-confirm pid (same PID-reuse hazard
                // codex flagged). Skip the SIGINT fallback; the cooperative
                // cancel in `cancel_active_token` still runs, and the hard-stop
                // path re-probes for a live, pane-validated pid afterward.
                (false, None)
            }
        }
    } else {
        (ready_for_input, provider_pid)
    };

    let fallback_sigint_pid =
        fallback_sigint_pid_for_provider(provider, ready_for_input, provider_pid);

    // #3029(A): on the SIGINT-only path (claude — empty key list, so the
    // direct SIGINT is the *only* way to reach the turn), an active turn
    // (`ready_for_input == false`) that yields no SIGINT target is a silent
    // no-op: nothing actually interrupts the provider even though the caller
    // marks the turn [Stopped]. `ready_for_input == false` here means the
    // confirmation re-probe agreed the turn is genuinely generating (an idle
    // pane correctly resolves to `None` and is intentionally left alone, #3021).
    // Flag this so the hard-stop path escalates instead of trusting an
    // unconditional success.
    let sigint_target_missing = interrupt_sigint_target_missing(
        provider,
        plan.keys.is_empty(),
        ready_for_input,
        fallback_sigint_pid,
    );
    if sigint_target_missing {
        tracing::error!(
            "provider turn interrupt SIGINT target missing: provider={} session={} reason={} \
             detail=active_turn_without_provider_pid (PID lookup returned None; SIGINT not delivered) — escalating",
            provider.as_str(),
            tmux_session_name,
            reason
        );
    }

    if let Some(pid) = fallback_sigint_pid {
        if let Err(error) = send_sigint(pid) {
            tracing::warn!(
                "provider turn interrupt SIGINT fallback failed: provider={} session={} pid={} reason={} error={}",
                provider.as_str(),
                tmux_session_name,
                pid,
                reason,
                error
            );
        } else {
            tracing::info!(
                "provider turn interrupt SIGINT fallback sent: provider={} session={} pid={} reason={}",
                provider.as_str(),
                tmux_session_name,
                pid,
                reason
            );
        }
    }

    ProviderTurnInterruptOutcome {
        tmux_session,
        sent_keys,
        fallback_sigint_pid,
        missing_tmux_session: false,
        sigint_target_missing,
    }
}

pub(in crate::services::discord) fn cancel_token_has_tmux_session(token: &CancelToken) -> bool {
    token
        .tmux_session
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
        .is_some()
}

pub(in crate::services::discord) fn bind_cancel_token_tmux_runtime(
    provider: &ProviderKind,
    token: &Arc<CancelToken>,
    tmux_session_name: &str,
    reason: &str,
) -> Option<u32> {
    if let Ok(mut guard) = token.tmux_session.lock() {
        if guard.as_deref() != Some(tmux_session_name) {
            *guard = Some(tmux_session_name.to_string());
        }
    } else {
        tracing::error!(
            "cancel token tmux rebind failed: provider={} session={} reason={} error=tmux_session_lock_poisoned",
            provider.as_str(),
            tmux_session_name,
            reason
        );
    }

    let tracked_child_pid = token.child_pid.lock().ok().and_then(|guard| *guard);
    let provider_pid = provider_cli_pid_in_tmux(tmux_session_name, provider, tracked_child_pid);
    if let Some(pid) = provider_pid {
        if let Ok(mut guard) = token.child_pid.lock()
            && guard.is_none()
        {
            *guard = Some(pid);
        }
        tracing::info!(
            "cancel token tmux runtime rebound: provider={} session={} pid={} reason={}",
            provider.as_str(),
            tmux_session_name,
            pid,
            reason
        );
    } else {
        tracing::warn!(
            "cancel token tmux runtime rebound without provider pid: provider={} session={} reason={}",
            provider.as_str(),
            tmux_session_name,
            reason
        );
    }
    provider_pid
}

/// Standard turn-stop sequence: send the provider abort key (e.g. C-c) FIRST,
/// give the CLI ~750ms to settle / send SIGINT fallback, and THEN flip the
/// cooperative cancel flag + SIGKILL the wrapper PID.
///
/// #1218: When `cancel_active_token` runs first, `kill_pid_tree(child_pid)`
/// kills the agentdesk tmux-wrapper, which is the foreground process of the
/// tmux session. The session then dies, and the subsequent
/// `tmux send-keys -t =name C-c` fails with "can't find pane". For Claude
/// streaming this is masked because the session teardown also stops claude;
/// but for handoff/restart turns where `child_pid` is `None` (Codex/Qwen TUI,
/// resumed runs) the SIGKILL is a no-op and only the C-c can stop the CLI.
/// In that case the wrong order leaves the provider running and the user
/// sees stop "fail".
///
/// All user-initiated stop paths (⏳ reaction removal, `/stop`, `!stop`,
/// `/clear`, watchdog timeouts) MUST call this helper instead of pairing
/// the two primitives by hand.
pub(in crate::services::discord) async fn stop_active_turn(
    provider: &ProviderKind,
    token: &Arc<CancelToken>,
    cleanup_policy: TmuxCleanupPolicy,
    reason: &str,
) -> bool {
    let interrupt_outcome = interrupt_provider_cli_turn(provider, token, reason).await;
    let termination_recorded = cancel_active_token(token, cleanup_policy, reason);
    hard_stop_unresponsive_provider_cli_turn(
        provider,
        token,
        cleanup_policy,
        &interrupt_outcome,
        reason,
    )
    .await;
    termination_recorded
}

async fn hard_stop_unresponsive_provider_cli_turn(
    provider: &ProviderKind,
    token: &Arc<CancelToken>,
    cleanup_policy: TmuxCleanupPolicy,
    interrupt_outcome: &ProviderTurnInterruptOutcome,
    reason: &str,
) {
    if cleanup_policy.should_cleanup_tmux() {
        return;
    }

    let tmux_session_name = interrupt_outcome.tmux_session.clone().or_else(|| {
        token
            .tmux_session
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    });
    let Some(tmux_session_name) = tmux_session_name else {
        tracing::error!(
            "provider hard-stop skipped: provider={} reason={} error=cancel_token_missing_tmux_session interrupt_missing_tmux_session={}",
            provider.as_str(),
            reason,
            interrupt_outcome.missing_tmux_session
        );
        return;
    };

    // #2426: replace the unconditional `sleep(PROVIDER_HARD_STOP_GRACE)`
    // with PID-exit observation on the SIGINT target. If the SIGINT actually
    // worked the provider exits within milliseconds and we proceed to the
    // re-probe immediately; the 1.5s upper bound is now a safety net for
    // pathological providers that swallow SIGINT.
    //
    // Codex review HIGH: the wait_for_pid_exit return value must NOT be
    // discarded. If it returns `true` the SIGINT target genuinely exited —
    // continuing to pass that stale PID through `hard_stop_pid_for_unresponsive_provider`
    // → `kill_pid_tree` risks killing an unrelated process if the OS has
    // recycled the PID by then. Track the effective fallback locally so
    // escalation can only target a currently-observed provider PID from
    // the tmux pane probe when the original SIGINT target has exited.
    let effective_fallback_sigint_pid =
        if let Some(target_pid) = interrupt_outcome.fallback_sigint_pid {
            let exited = wait_for_pid_exit(target_pid, PROVIDER_HARD_STOP_GRACE).await;
            if exited { None } else { Some(target_pid) }
        } else {
            tokio::time::sleep(PROVIDER_HARD_STOP_GRACE).await;
            None
        };

    let tracked_child_pid = token.child_pid.lock().ok().and_then(|guard| *guard);
    let provider_for_probe = provider.clone();
    let session_for_probe = tmux_session_name.clone();
    let probe = tokio::task::spawn_blocking(move || {
        let pane_pid = crate::services::platform::tmux::pane_pid(&session_for_probe);
        let session_alive = pane_pid.is_some();
        let ready_for_input =
            tmux_ready_for_input_without_tui_pane(&session_for_probe, &provider_for_probe);
        let current_provider_pid =
            provider_cli_pid_in_tmux(&session_for_probe, &provider_for_probe, tracked_child_pid);
        (
            session_alive,
            ready_for_input,
            current_provider_pid,
            pane_pid,
        )
    })
    .await;

    let (session_alive, ready_for_input, current_provider_pid, pane_pid) = match probe {
        Ok(values) => values,
        Err(error) => {
            tracing::warn!(
                "provider hard-stop probe join error: provider={} session={} reason={} error={}",
                provider.as_str(),
                tmux_session_name,
                reason,
                error
            );
            (false, false, None, None)
        }
    };

    // #3029(A): the initial interrupt could not find a SIGINT target for an
    // active SIGINT-only (claude) turn, so no signal was delivered. The
    // post-grace re-probe runs against current `ps` state and may now resolve
    // the provider PID (the child became visible / `ps` recovered). Deliver
    // the SIGINT we previously missed — but only to a live provider PID that
    // is NOT the pane foreground, so we never SIGINT the wrapper/pane and tear
    // down a reusable session (PreserveSession intent stays intact; this is a
    // contained, reach-guaranteed escalation, not the #3018 mapping rework).
    if interrupt_outcome.sigint_target_missing && session_alive && !ready_for_input {
        match current_provider_pid {
            Some(pid) if Some(pid) != pane_pid => {
                if let Err(error) = send_sigint(pid) {
                    tracing::warn!(
                        "provider hard-stop SIGINT re-delivery failed: provider={} session={} pid={} reason={} error={}",
                        provider.as_str(),
                        tmux_session_name,
                        pid,
                        reason,
                        error
                    );
                } else {
                    tracing::info!(
                        "provider hard-stop SIGINT re-delivered after missed interrupt target: provider={} session={} pid={} reason={}",
                        provider.as_str(),
                        tmux_session_name,
                        pid,
                        reason
                    );
                }
            }
            _ => {
                tracing::error!(
                    "provider hard-stop could not escalate missed SIGINT: provider={} session={} reason={} \
                     detail=no_live_non_pane_provider_pid (interrupt did not reach the turn)",
                    provider.as_str(),
                    tmux_session_name,
                    reason
                );
            }
        }
    }

    let Some(pid) = hard_stop_pid_for_unresponsive_provider(
        provider,
        cleanup_policy,
        session_alive,
        ready_for_input,
        current_provider_pid,
        effective_fallback_sigint_pid,
        pane_pid,
    ) else {
        return;
    };

    tracing::warn!(
        "provider turn did not stop after interrupt; killing provider pid: provider={} session={} pid={} reason={}",
        provider.as_str(),
        tmux_session_name,
        pid,
        reason
    );
    crate::services::process::kill_pid_tree(pid);
}

fn hard_stop_pid_for_unresponsive_provider(
    provider: &ProviderKind,
    cleanup_policy: TmuxCleanupPolicy,
    session_alive: bool,
    ready_for_input: bool,
    current_provider_pid: Option<u32>,
    previous_provider_pid: Option<u32>,
    pane_pid: Option<u32>,
) -> Option<u32> {
    if cleanup_policy.should_cleanup_tmux() || !session_alive || ready_for_input {
        return None;
    }

    // #2965: PreserveSession means "do not tear down the reusable TUI".
    // Claude's CLI runs below the wrapper, so the candidate PID can be a
    // child rather than the pane foreground. Killing that child can still
    // make the wrapper exit and collapse the tmux session. Force=true keeps
    // the explicit cleanup path; preserve paths stop at cooperative SIGINT.
    if matches!(provider, ProviderKind::Claude) {
        return None;
    }

    let candidate = current_provider_pid.or(previous_provider_pid)?;

    // TUI mode regression guard: when the provider CLI is the tmux pane
    // foreground itself, hard-killing it tears down the pane — same blast
    // radius as `CleanupSession`, which `PreserveSession*` policies forbid.
    // If the provider CLI is still the pane foreground, killing it would
    // tear down a reusable TUI session. Skip the kill; either readiness
    // was missed by the structured/pane fallback probe or the next intake/recovery pass can
    // reconcile the preserved session.
    if Some(candidate) == pane_pid {
        return None;
    }

    Some(candidate)
}

pub(in crate::services::discord) fn cancel_active_token(
    token: &Arc<CancelToken>,
    cleanup_policy: TmuxCleanupPolicy,
    reason: &str,
) -> bool {
    token.cancelled.store(true, Ordering::Relaxed);
    token.set_restart_mode(cleanup_policy.preserves_inflight());
    let mut termination_recorded = false;

    let child_pid = token.child_pid.lock().ok().and_then(|guard| *guard);
    // `child_pid` is the wrapper PID — i.e. the foreground process of the
    // tmux pane. SIGKILL'ing it tears down the tmux session itself. For
    // `PreserveSession` / `PreserveSessionAndInflight` the caller has
    // already sent the provider abort key
    // (`interrupt_provider_cli_turn` C-c + SIGINT fallback in
    // `stop_active_turn`), so the provider is being asked to exit
    // cooperatively and we MUST NOT take down the tmux pane underneath it
    // — otherwise the next turn re-spawns the session, the capture file
    // rotates, and the watcher floods Discord with stale scrollback. Only
    // the tear-down policy kills the wrapper here.
    if cleanup_policy.should_cleanup_tmux()
        && let Some(pid) = child_pid
    {
        crate::services::process::kill_pid_tree(pid);
    }

    if let TmuxCleanupPolicy::CleanupSession {
        termination_reason_code,
    } = cleanup_policy
    {
        if child_pid.is_some() {
            if let Some(name) = token
                .tmux_session
                .lock()
                .ok()
                .and_then(|guard| guard.clone())
            {
                #[cfg(unix)]
                {
                    // #145: skip kill for unified-thread sessions with active runs
                    let is_unified =
                        crate::services::provider::parse_provider_and_channel_from_tmux_name(&name)
                            .map(|(_, ch)| {
                                crate::dispatch::is_unified_thread_channel_name_active(&ch)
                            })
                            .unwrap_or(false);
                    if !is_unified {
                        if let Some(reason_code) = termination_reason_code {
                            crate::services::termination_audit::record_termination_for_tmux(
                                &name,
                                None,
                                "turn_bridge",
                                reason_code,
                                Some(&format!("explicit cleanup via {reason}")),
                                None,
                            );
                            termination_recorded = true;
                        }
                        record_tmux_exit_reason(&name, &format!("explicit cleanup via {reason}"));
                        crate::services::platform::tmux::kill_session(
                            &name,
                            &format!("explicit cleanup via {reason}"),
                        );
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = &name;
                }
            }
        } else {
            #[cfg(unix)]
            if let Some(name) = token
                .tmux_session
                .lock()
                .ok()
                .and_then(|guard| guard.clone())
            {
                record_tmux_exit_reason(&name, &format!("explicit cleanup via {reason}"));
            }
            token.cancel_with_tmux_cleanup();
        }
    }

    termination_recorded
}

#[cfg(unix)]
#[derive(Debug, Clone, Eq, PartialEq)]
struct ProcessRow {
    pid: u32,
    ppid: u32,
    command: String,
}

#[cfg(unix)]
fn provider_cli_pid_in_tmux(
    tmux_session_name: &str,
    provider: &ProviderKind,
    tracked_child_pid: Option<u32>,
) -> Option<u32> {
    let pane_pid = crate::services::platform::tmux::pane_pid(tmux_session_name)?;
    let rows = process_table();
    select_provider_pid_in_pane(pane_pid, &rows, provider, tracked_child_pid)
}

// TUI mode regression: when the provider CLI itself is the tmux pane
// foreground (no wrapper in front — e.g. `claude --session-id …` running
// directly), `descendant_processes` excludes `pane_pid` and the search
// falls through to None. The interrupt path then silently no-ops: stop
// emoji marks the turn [Stopped] in the mailbox but no SIGINT ever
// reaches the claude TUI, so it keeps generating and the watcher keeps
// posting the response to Discord. Check `pane_pid` itself before walking
// descendants.
#[cfg(unix)]
fn select_provider_pid_in_pane(
    pane_pid: u32,
    rows: &[ProcessRow],
    provider: &ProviderKind,
    tracked_child_pid: Option<u32>,
) -> Option<u32> {
    if let Some(pane_row) = rows.iter().find(|row| row.pid == pane_pid)
        && !command_is_agentdesk_provider_wrapper(&pane_row.command)
        && command_matches_provider(&pane_row.command, provider)
    {
        return Some(pane_pid);
    }

    let descendants = descendant_processes(pane_pid, rows);

    if let Some(pid) = tracked_child_pid
        && descendants.iter().any(|row| {
            row.pid == pid
                && !command_is_agentdesk_provider_wrapper(&row.command)
                && command_matches_provider(&row.command, provider)
        })
    {
        return Some(pid);
    }

    descendants
        .into_iter()
        .filter(|row| !command_is_agentdesk_provider_wrapper(&row.command))
        .filter(|row| command_matches_provider(&row.command, provider))
        .max_by_key(|row| provider_command_match_score(&row.command, provider))
        .map(|row| row.pid)
}

#[cfg(not(unix))]
fn provider_cli_pid_in_tmux(
    _tmux_session_name: &str,
    _provider: &ProviderKind,
    _tracked_child_pid: Option<u32>,
) -> Option<u32> {
    None
}

#[cfg(unix)]
fn process_table() -> Vec<ProcessRow> {
    let output = match std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_process_row)
        .collect()
}

#[cfg(unix)]
fn parse_process_row(line: &str) -> Option<ProcessRow> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse::<u32>().ok()?;
    let ppid = parts.next()?.parse::<u32>().ok()?;
    let command = parts.collect::<Vec<_>>().join(" ");
    if command.is_empty() {
        return None;
    }
    Some(ProcessRow { pid, ppid, command })
}

#[cfg(unix)]
fn descendant_processes(root_pid: u32, rows: &[ProcessRow]) -> Vec<ProcessRow> {
    let mut descendants = Vec::new();
    let mut stack = vec![root_pid];
    while let Some(parent) = stack.pop() {
        for row in rows.iter().filter(|row| row.ppid == parent) {
            descendants.push(row.clone());
            stack.push(row.pid);
        }
    }
    descendants
}

#[cfg(unix)]
fn command_matches_provider(command: &str, provider: &ProviderKind) -> bool {
    provider_command_match_score(command, provider) > 0
}

#[cfg(unix)]
fn provider_command_match_score(command: &str, provider: &ProviderKind) -> u8 {
    let Some(binary) = provider_cli_binary_name(provider) else {
        return 0;
    };
    let lower = command.to_ascii_lowercase();
    let first = lower
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|ch| ch == '\'' || ch == '"');
    let first_basename = std::path::Path::new(first)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(first);

    if first_basename == binary {
        return 3;
    }
    if lower.contains(&format!("/{binary} "))
        || lower.ends_with(&format!("/{binary}"))
        || lower.contains(&format!(" {binary} "))
    {
        return 2;
    }
    if lower.contains(binary) { 1 } else { 0 }
}

#[cfg(unix)]
fn command_is_agentdesk_provider_wrapper(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.contains(" codex-tmux-wrapper")
        || lower.contains(" qwen-tmux-wrapper")
        || lower.contains(" tmux-wrapper")
}

#[cfg(unix)]
fn provider_cli_binary_name(provider: &ProviderKind) -> Option<&'static str> {
    match provider {
        ProviderKind::Claude => Some("claude"),
        ProviderKind::Codex => Some("codex"),
        ProviderKind::Qwen => Some("qwen"),
        ProviderKind::Gemini | ProviderKind::OpenCode | ProviderKind::Unsupported(_) => None,
    }
}

#[cfg(unix)]
fn send_sigint(pid: u32) -> Result<(), String> {
    #[allow(unsafe_code)]
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

#[cfg(not(unix))]
fn send_sigint(_pid: u32) -> Result<(), String> {
    Err("SIGINT fallback is only supported on Unix".to_string())
}

/// #2426: wait until the given PID exits, with an upper bound. Uses OS-level
/// exit signals (kqueue `EVFILT_PROC|NOTE_EXIT` on macOS, `pidfd_open` +
/// `poll` on Linux) so we never busy-poll. When the PID does not exist at
/// call time, returns immediately. When the OS signal API is unavailable or
/// fails (e.g. permission, pre-5.3 kernel), falls back to a single bounded
/// `tokio::sleep` so callers still observe a deterministic upper bound.
///
/// Returns `true` if the process exited within the deadline, `false` if the
/// upper bound elapsed first.
#[cfg(unix)]
pub(super) async fn wait_for_pid_exit(pid: u32, deadline: Duration) -> bool {
    if pid == 0 {
        // libc::kill(0, ...) targets the whole process group; treat as "no
        // PID to observe" and just honour the deadline.
        tokio::time::sleep(deadline).await;
        return false;
    }

    // Fast-path: process already gone (or never existed). `kill(pid, 0)`
    // delivers no signal but tells us whether the PID is reachable.
    #[allow(unsafe_code)]
    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if !alive {
        return true;
    }

    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    let join = tokio::task::spawn_blocking(move || {
        let exited = wait_for_pid_exit_blocking(pid, deadline);
        let _ = tx.send(exited);
    });

    match tokio::time::timeout(deadline, rx).await {
        Ok(Ok(exited)) => {
            // Allow the blocking task to settle. It already finished (rx
            // resolved), so this is a cheap join.
            let _ = join.await;
            exited
        }
        _ => {
            // Outer guard fired or the blocking task panicked / dropped tx.
            // Either way the upper bound has elapsed.
            false
        }
    }
}

#[cfg(not(unix))]
pub(super) async fn wait_for_pid_exit(_pid: u32, deadline: Duration) -> bool {
    tokio::time::sleep(deadline).await;
    false
}

#[cfg(target_os = "macos")]
fn wait_for_pid_exit_blocking(pid: u32, deadline: Duration) -> bool {
    // kqueue + EVFILT_PROC|NOTE_EXIT: kernel notifies us when the PID exits.
    // We arm a single ONESHOT kevent and block in `kevent()` until either
    // NOTE_EXIT fires or the supplied timespec deadline elapses.
    #[allow(unsafe_code)]
    unsafe {
        let kq = libc::kqueue();
        if kq < 0 {
            return wait_for_pid_exit_kill_fallback(pid, deadline);
        }
        let change = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // Register the watch. A zero timeout means "register only, don't
        // block". If the PID already exited or doesn't exist, `kevent` sets
        // EV_ERROR with ESRCH and we treat that as "exited".
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let reg = libc::kevent(
            kq,
            &change as *const _,
            1,
            std::ptr::null_mut(),
            0,
            &zero as *const _,
        );
        if reg < 0 {
            libc::close(kq);
            return wait_for_pid_exit_kill_fallback(pid, deadline);
        }

        let mut event: libc::kevent = std::mem::zeroed();
        let ts = libc::timespec {
            tv_sec: deadline.as_secs() as libc::time_t,
            tv_nsec: deadline.subsec_nanos() as libc::c_long,
        };
        let n = libc::kevent(
            kq,
            std::ptr::null(),
            0,
            &mut event as *mut _,
            1,
            &ts as *const _,
        );
        libc::close(kq);
        if n < 0 {
            return wait_for_pid_exit_kill_fallback(pid, deadline);
        }
        if n == 0 {
            // Timeout — deadline elapsed without NOTE_EXIT.
            return false;
        }
        // One event delivered. NOTE_EXIT fired or EV_ERROR/ESRCH (already
        // gone). Both mean "PID is no longer running".
        true
    }
}

#[cfg(all(unix, target_os = "linux"))]
fn wait_for_pid_exit_blocking(pid: u32, deadline: Duration) -> bool {
    // pidfd_open(pid, 0) returns an fd that becomes readable when the
    // process exits. We poll(POLLIN) it with the deadline. On kernels
    // without pidfd_open (<5.3) the syscall returns ENOSYS and we fall
    // back to a single-shot bounded wait.
    #[allow(unsafe_code)]
    unsafe {
        let fd = libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0_u32) as libc::c_int;
        if fd < 0 {
            return wait_for_pid_exit_kill_fallback(pid, deadline);
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms: libc::c_int = deadline.as_millis().try_into().unwrap_or(libc::c_int::MAX);
        let n = libc::poll(&mut pfd as *mut _, 1, timeout_ms);
        libc::close(fd);
        if n < 0 {
            return wait_for_pid_exit_kill_fallback(pid, deadline);
        }
        n > 0
    }
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn wait_for_pid_exit_blocking(pid: u32, deadline: Duration) -> bool {
    wait_for_pid_exit_kill_fallback(pid, deadline)
}

/// Last-resort fallback when the OS-native exit notifier is unavailable
/// (kqueue creation failure, pre-5.3 Linux kernel without `pidfd_open`,
/// other Unix). A single bounded sleep keeps the upper bound deterministic
/// without introducing a polling loop.
#[cfg(unix)]
fn wait_for_pid_exit_kill_fallback(_pid: u32, deadline: Duration) -> bool {
    std::thread::sleep(deadline);
    false
}

#[cfg(unix)]
pub(crate) fn tmux_runtime_paths(tmux_session_name: &str) -> (String, String) {
    use crate::services::tmux_common::session_temp_path;
    (
        session_temp_path(tmux_session_name, "jsonl"),
        session_temp_path(tmux_session_name, "input"),
    )
}

#[cfg(not(unix))]
pub(crate) fn tmux_runtime_paths(tmux_session_name: &str) -> (String, String) {
    let tmp = std::env::temp_dir();
    (
        tmp.join(format!("agentdesk-{}.jsonl", tmux_session_name))
            .display()
            .to_string(),
        tmp.join(format!("agentdesk-{}.input", tmux_session_name))
            .display()
            .to_string(),
    )
}

pub(in crate::services::discord) fn stale_inflight_message(saved_response: &str) -> String {
    let cleaned = crate::services::discord::response_sanitizer::strip_leading_tui_response_chrome(
        saved_response.trim(),
    );
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "⚠️ AgentDesk가 재시작되어 진행 중이던 응답을 이어붙이지 못했습니다.".to_string()
    } else {
        let formatted = format_for_discord(cleaned);
        format!("{formatted}\n\n[Interrupted by restart]")
    }
}

pub(in crate::services::discord) fn handoff_interrupted_message(
    restart_mode: InflightRestartMode,
    saved_response: &str,
) -> String {
    let cleaned = crate::services::discord::response_sanitizer::strip_leading_tui_response_chrome(
        saved_response.trim(),
    );
    let cleaned = cleaned.trim();
    match restart_mode {
        InflightRestartMode::DrainRestart => {
            if cleaned.is_empty() {
                "⚠️ dcserver 재시작 중이던 turn을 다시 붙이지 못했습니다. 다음 메시지부터 새 turn으로 이어갑니다.".to_string()
            } else {
                let formatted = format_for_discord(cleaned);
                format!("{formatted}\n\n[Restart handoff incomplete]")
            }
        }
        InflightRestartMode::HotSwapHandoff => {
            if cleaned.is_empty() {
                "⚠️ 런타임 handoff 중 세션 재연결이 완료되지 않았습니다. 다음 메시지부터 새 turn으로 이어갑니다.".to_string()
            } else {
                let formatted = format_for_discord(cleaned);
                format!("{formatted}\n\n[Runtime handoff incomplete]")
            }
        }
    }
}

pub(super) fn is_dcserver_restart_command(input: &str) -> bool {
    let lower = input.to_lowercase();

    if lower.contains("restart-dcserver") || lower.contains("restart_agentdesk.sh") {
        return true;
    }

    if lower.contains("agentdesk-discord-smoke.sh") && lower.contains("--deploy-live") {
        return true;
    }

    lower.contains("launchctl")
        && lower.contains("com.agentdesk.dcserver")
        && (lower.contains("kickstart") || lower.contains("bootstrap") || lower.contains("bootout"))
}

// #2426: tests for the PID-exit observation helper. These do not require the
// `legacy-sqlite-tests` feature because they exercise only the
// `wait_for_pid_exit` path and do not touch the SQLite test scaffolding.
#[cfg(all(test, unix))]
mod pid_exit_tests {
    use super::wait_for_pid_exit;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// A PID we are extremely unlikely to ever be reachable: well above the
    /// typical max PID and never spawned by this test process. The kernel
    /// reports ESRCH from `kill(pid, 0)` and `wait_for_pid_exit` should
    /// short-circuit to `true` without blocking for the deadline.
    #[tokio::test(flavor = "current_thread")]
    async fn wait_for_pid_exit_returns_immediately_for_nonexistent_pid() {
        let started = Instant::now();
        let exited = wait_for_pid_exit(4_000_000, Duration::from_secs(5)).await;
        let elapsed = started.elapsed();
        assert!(
            exited,
            "nonexistent PID must be reported as already-exited (kill(pid,0) -> ESRCH)"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "nonexistent PID short-circuit must not honour the full deadline (took {elapsed:?})"
        );
    }

    /// Spawn a real child, kill it, then call `wait_for_pid_exit`. The
    /// kqueue/pidfd path must signal exit well before the 2s upper bound.
    #[tokio::test(flavor = "current_thread")]
    async fn wait_for_pid_exit_observes_child_termination() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();

        // Schedule a kill 50ms in the future so the helper actually has to
        // wait for the exit signal rather than short-circuit via the
        // `kill(pid,0)` fast path.
        let killer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            // SIGKILL = unconditional, no handler chance.
            #[allow(unsafe_code)]
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        });

        let started = Instant::now();
        let exited = wait_for_pid_exit(pid, Duration::from_secs(2)).await;
        let elapsed = started.elapsed();

        // Reap the zombie regardless of test outcome so we don't leak.
        let _ = child.wait();
        let _ = killer.await;

        assert!(
            exited,
            "wait_for_pid_exit must report exit for killed child"
        );
        assert!(
            elapsed < Duration::from_millis(1500),
            "OS-level exit notification must beat the upper bound by a comfortable margin (took {elapsed:?})"
        );
    }

    /// A still-running child must drive `wait_for_pid_exit` to the upper
    /// bound and report `false`.
    #[tokio::test(flavor = "current_thread")]
    async fn wait_for_pid_exit_times_out_when_child_keeps_running() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();

        let started = Instant::now();
        let exited = wait_for_pid_exit(pid, Duration::from_millis(200)).await;
        let elapsed = started.elapsed();

        // Tear down the still-running child.
        #[allow(unsafe_code)]
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
        let _ = child.wait();

        assert!(
            !exited,
            "long-running child must not be reported as exited within the upper bound"
        );
        assert!(
            elapsed >= Duration::from_millis(180),
            "the upper bound must actually be honoured (took only {elapsed:?})"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "the upper bound must not be exceeded by more than scheduler jitter (took {elapsed:?})"
        );
    }
}

// #3029(A): the "missed SIGINT target" decision is a pure boolean and runs
// under the default `cargo test` invocation (the main suite is gated behind
// the `legacy-sqlite-tests` feature, which CI does not enable by default).
#[cfg(test)]
mod sigint_target_missing_tests {
    use super::interrupt_sigint_target_missing;
    use crate::services::provider::ProviderKind;

    #[test]
    fn active_claude_without_pid_is_a_missed_interrupt() {
        // SIGINT-only path (empty keys = claude), turn actively generating
        // (ready_for_input=false), and NO resolved PID → silent no-op that must
        // now be flagged for escalation instead of reporting success.
        assert!(
            interrupt_sigint_target_missing(&ProviderKind::Claude, true, false, None),
            "active claude with no resolvable PID must escalate (#3029 A), not silently succeed"
        );
    }

    #[test]
    fn active_claude_with_pid_is_not_missed() {
        assert!(
            !interrupt_sigint_target_missing(&ProviderKind::Claude, true, false, Some(42)),
            "a resolved PID means the SIGINT had a target — not a miss"
        );
    }

    #[test]
    fn idle_claude_without_pid_is_not_missed() {
        // #3021: an idle pane intentionally resolves to no PID and is left
        // alone; that is NOT a missed interrupt.
        assert!(
            !interrupt_sigint_target_missing(&ProviderKind::Claude, true, true, None),
            "idle claude (ready_for_input=true) is intentionally skipped, not a miss (#3021)"
        );
    }

    #[test]
    fn wrapped_providers_are_not_sigint_only() {
        // Codex/Qwen have a send-keys path, so a missing fallback PID is not a
        // SIGINT-only silent no-op (their send-keys still reaches the wrapper).
        assert!(!interrupt_sigint_target_missing(
            &ProviderKind::Codex,
            false,
            false,
            None
        ));
        assert!(!interrupt_sigint_target_missing(
            &ProviderKind::Qwen,
            false,
            false,
            None
        ));
    }
}
