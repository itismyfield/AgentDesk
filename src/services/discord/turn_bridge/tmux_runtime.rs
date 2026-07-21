use super::super::*;
use crate::services::discord::InflightRestartMode;
use crate::services::provider::cancel_token_cleanup::executor::{
    CleanupRequest, TmuxCleanupIntent,
};
use crate::services::provider::{CancelToken, ProviderKind};
use std::time::Duration;

// #3479: behavior-preserving decomposition of this giant module. The pure
// interrupt-policy decisions, the `ps`-backed process-table discovery, and the
// OS-level PID-exit observation moved verbatim into sibling leaf modules; the
// async orchestration + session-teardown logic stays here and reaches the
// moved items by their original bare names via these glob/explicit re-imports.
mod claude_stop_delivery;
mod interrupt_policy;
mod pid_exit;
mod process_backend_cancel;
mod process_table;

use claude_stop_delivery::interrupt_claude_turn_session_preserving;
use interrupt_policy::*;
use pid_exit::wait_for_pid_exit;
use process_backend_cancel::{
    hard_stop_unresponsive_process_backend_turn, interrupt_process_backend_turn,
};
use process_table::{provider_cli_pid_in_tmux, send_sigint};

// #3169: `mod.rs`'s cancel epilogue records this sentinel via the
// `tmux_runtime::ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON` path, so re-export it
// from the module root at its original visibility now that it lives in the
// `interrupt_policy` leaf module.
pub(in crate::services::discord) use interrupt_policy::ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON;

#[cfg(unix)]
pub(in crate::services::discord) fn tmux_generation_file_mtime_ns(tmux_session_name: &str) -> i64 {
    super::super::tmux::read_generation_file_mtime_ns(tmux_session_name)
}

#[cfg(not(unix))]
pub(in crate::services::discord) fn tmux_generation_file_mtime_ns(_tmux_session_name: &str) -> i64 {
    0
}

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
    crate::services::provider::tmux_session_fallback_ready_for_input(
        tmux_session_name,
        provider,
        runtime_kind,
    )
    .is_some_and(crate::services::pane_readiness::FallbackPaneReadiness::is_ready)
}

pub(in crate::services::discord) async fn interrupt_provider_cli_turn(
    provider: &ProviderKind,
    token: &Arc<CancelToken>,
    reason: &str,
) -> ProviderTurnInterruptOutcome {
    let tmux_session = token.tmux_session_name();
    let tracked_child_pid = token.child_pid_value();
    if tmux_session.is_none() {
        return interrupt_process_backend_turn(provider, tracked_child_pid, reason);
    }
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

    // #3207 (part 1): claude's legacy interrupt was a direct SIGINT to the CLI,
    // which exits it, collapses the pane to bash, and makes the watcher tear the
    // whole tmux session down ("dead session after turn") — destroying the
    // reusable session + context on a mere turn-stop. Replace it with a
    // session-preserving turn cancel: ESC to the interactive TUI, or a
    // stream-json `control_request{interrupt}` to the wrapper FIFO. SIGINT /
    // session teardown stays exclusively in `cancel_active_token` for the
    // `CleanupSession` policy (genuine "terminate the session" intent), so a
    // `PreserveSession` user stop (`!stop` / ⏳ removal / watchdog) cancels only
    // the turn and the next message warm-resumes instead of cold-starting.
    if matches!(provider, ProviderKind::Claude) {
        let _ = plan; // claude takes the dedicated session-preserving path below
        return interrupt_claude_turn_session_preserving(token, tmux_session, reason).await;
    }

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

    let resolved_sigint_pid =
        fallback_sigint_pid_for_provider(provider, ready_for_input, provider_pid);

    // #3169 (death #3): on an anonymous/internal `PreserveSession` teardown
    // (reason == `turn_bridge_cancelled`, i.e. no user `cancel_source`), claude
    // must NOT receive the teardown SIGINT — for a busy claude that SIGINT is a
    // process kill that tears down the reusable session (the warm-followup
    // self-collision). Drop the fallback target on that path and leave the live
    // turn for the watcher to reconcile. User-explicit stops and non-claude
    // providers are unaffected (see `claude_teardown_sigint_suppressed`).
    let suppress_claude_teardown_sigint = claude_teardown_sigint_suppressed(provider, reason);
    if suppress_claude_teardown_sigint && (resolved_sigint_pid.is_some() || !ready_for_input) {
        tracing::warn!(
            "provider turn interrupt SIGINT suppressed: provider={} session={} reason={} \
             detail=anonymous_preserve_session_teardown (claude SIGINT==process-kill on a busy TUI; \
             leaving live turn for watcher reconcile to avoid #3169 warm-followup session kill)",
            provider.as_str(),
            tmux_session_name,
            reason
        );
    }
    let fallback_sigint_pid = if suppress_claude_teardown_sigint {
        None
    } else {
        resolved_sigint_pid
    };

    // #3029(A): on the SIGINT-only path (claude — empty key list, so the
    // direct SIGINT is the *only* way to reach the turn), an active turn
    // (`ready_for_input == false`) that yields no SIGINT target is a silent
    // no-op: nothing actually interrupts the provider even though the caller
    // marks the turn [Stopped]. `ready_for_input == false` here means the
    // confirmation re-probe agreed the turn is genuinely generating (an idle
    // pane correctly resolves to `None` and is intentionally left alone, #3021).
    // Flag this so the hard-stop path escalates instead of trusting an
    // unconditional success.
    //
    // #3169: when we deliberately suppressed claude's teardown SIGINT above,
    // the absent target is INTENTIONAL — not a missed interrupt. Force
    // `sigint_target_missing = false` so the hard-stop path does not "escalate"
    // by re-delivering the very SIGINT we just suppressed (which would re-kill
    // the session we are trying to preserve).
    let sigint_target_missing = if suppress_claude_teardown_sigint {
        false
    } else {
        interrupt_sigint_target_missing(
            provider,
            plan.keys.is_empty(),
            ready_for_input,
            fallback_sigint_pid,
        )
    };
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
    token.tmux_session_name().is_some()
}

pub(in crate::services::discord) fn bind_cancel_token_tmux_runtime(
    provider: &ProviderKind,
    token: &Arc<CancelToken>,
    tmux_session_name: &str,
    reason: &str,
) -> Option<u32> {
    if matches!(provider, ProviderKind::Claude) {
        token.bind_claude_tmux_session(tmux_session_name);
    } else {
        token.bind_unmanaged_session_name(tmux_session_name);
    }

    let tracked_child_pid = token.child_pid_value();
    let provider_pid = provider_cli_pid_in_tmux(tmux_session_name, provider, tracked_child_pid);
    if let Some(pid) = provider_pid {
        token.store_child_pid_if_empty(pid);
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

    let tmux_session_name = interrupt_outcome
        .tmux_session
        .clone()
        .or_else(|| token.tmux_session_name());
    let Some(tmux_session_name) = tmux_session_name else {
        hard_stop_unresponsive_process_backend_turn(provider, token, interrupt_outcome, reason)
            .await;
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

    let tracked_child_pid = token.child_pid_value();
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
    token.set_restart_mode(cleanup_policy.preserves_inflight());
    let child_pid = token.child_pid_value();
    let has_tmux_session = token.tmux_session_name().is_some();
    if !has_tmux_session
        && cleanup_policy.should_cleanup_tmux()
        && let Some(pid) = child_pid
    {
        crate::services::session_backend::mark_process_sessions_stopped_by_pid(pid);
    }

    let (intent, termination_reason) = match cleanup_policy {
        TmuxCleanupPolicy::CleanupSession {
            termination_reason_code,
        } => (TmuxCleanupIntent::CleanupSession, termination_reason_code),
        TmuxCleanupPolicy::PreserveSession
        | TmuxCleanupPolicy::PreserveSessionAndInflight { .. } => {
            (TmuxCleanupIntent::PreserveSession, None)
        }
    };
    token
        .request_cleanup(CleanupRequest {
            cancel_source: reason.to_string(),
            intent,
            termination_reason,
            hard_stop_target: None,
        })
        .termination_confirmed()
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
    let cleaned = restart_saved_response_for_discord(saved_response);
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
    let cleaned = restart_saved_response_for_discord(saved_response);
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

fn restart_saved_response_for_discord(saved_response: &str) -> String {
    crate::services::discord::response_sanitizer::sanitize_hidden_context_and_strip_chrome(
        saved_response.trim(),
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};
    use std::time::Duration;

    const RAW_SUBAGENT: &str = r#"<subagent_notification>
{"agent_path":"/tmp/private-agent","status":{"completed":"Read-only review complete.\n\n1. Check relay path."}}
</subagent_notification>"#;
    const CHROME_RAW_SUBAGENT: &str = "No response requested.\n<subagent_notification>{\"agent_path\":\"/tmp/private-agent\",\"status\":{\"completed\":\"Read-only review complete.\"}}</subagent_notification>";
    static SIGINT_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn stale_inflight_message_hides_raw_subagent_notification() {
        let message = stale_inflight_message(RAW_SUBAGENT);

        assert!(message.contains("Subagent completed"));
        assert!(message.contains("Read-only review complete."));
        assert!(message.contains("[Interrupted by restart]"));
        assert!(!message.contains("<subagent_notification>"));
        assert!(!message.contains("agent_path"));
        assert!(!message.contains("/tmp/private-agent"));
        assert!(!message.contains("{\""));
    }

    #[test]
    fn handoff_interrupted_message_hides_raw_subagent_notification() {
        let message = handoff_interrupted_message(InflightRestartMode::DrainRestart, RAW_SUBAGENT);

        assert!(message.contains("Subagent completed"));
        assert!(message.contains("Read-only review complete."));
        assert!(message.contains("[Restart handoff incomplete]"));
        assert!(!message.contains("<subagent_notification>"));
        assert!(!message.contains("agent_path"));
        assert!(!message.contains("/tmp/private-agent"));
        assert!(!message.contains("{\""));
    }

    #[test]
    fn restart_messages_sanitize_subagent_after_tui_chrome_strip() {
        let stale = stale_inflight_message(CHROME_RAW_SUBAGENT);
        assert!(stale.contains("Subagent completed"));
        assert!(stale.contains("Read-only review complete."));
        assert!(!stale.contains("No response requested."));
        assert!(!stale.contains("<subagent_notification>"));
        assert!(!stale.contains("agent_path"));

        let handoff =
            handoff_interrupted_message(InflightRestartMode::DrainRestart, CHROME_RAW_SUBAGENT);
        assert!(handoff.contains("Subagent completed"));
        assert!(!handoff.contains("<subagent_notification>"));
        assert!(!handoff.contains("agent_path"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn issue_4112_pipe_interrupt_sends_sigint_and_marks_stopped() {
        let _guard = SIGINT_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _ = process_table::take_sigint_test_events();
        let session_name = format!("pipe-cancel-{}", uuid::Uuid::new_v4());
        let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        crate::services::session_backend::insert_process_session(
            session_name.clone(),
            crate::services::session_backend::SessionHandle::TestProcess { pid: 4112, alive },
        );
        let token = std::sync::Arc::new(CancelToken::new());
        token.store_child_pid(4112);

        let outcome =
            interrupt_provider_cli_turn(&ProviderKind::Claude, &token, "explicit_stop").await;

        assert_eq!(process_table::take_sigint_test_events(), vec![4112]);
        assert_eq!(outcome.tmux_session, None);
        assert_eq!(outcome.fallback_sigint_pid, Some(4112));
        assert!(outcome.missing_tmux_session);
        assert!(!outcome.sigint_target_missing);
        assert!(crate::services::session_backend::process_session_was_stopped(&session_name));
        assert!(!crate::services::session_backend::process_session_is_alive(
            &session_name
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn issue_3169_process_backend_anonymous_teardown_is_noop() {
        let _guard = SIGINT_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _ = process_table::take_sigint_test_events();
        let session_name = format!("pipe-anonymous-teardown-{}", uuid::Uuid::new_v4());
        let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        crate::services::session_backend::insert_process_session(
            session_name.clone(),
            crate::services::session_backend::SessionHandle::TestProcess {
                pid: 43169,
                alive: alive.clone(),
            },
        );
        let token = std::sync::Arc::new(CancelToken::new());
        token.store_child_pid(43169);

        let outcome = interrupt_provider_cli_turn(
            &ProviderKind::Claude,
            &token,
            ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON,
        )
        .await;

        assert!(process_table::take_sigint_test_events().is_empty());
        assert_eq!(outcome.tmux_session, None);
        assert_eq!(outcome.fallback_sigint_pid, None);
        assert!(!outcome.sigint_target_missing);
        assert!(!crate::services::session_backend::process_session_was_stopped(&session_name));
        assert!(crate::services::session_backend::process_session_is_alive(
            &session_name
        ));

        if let Some(handle) =
            crate::services::session_backend::remove_process_session(&session_name)
        {
            crate::services::session_backend::terminate_process_handle(handle);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn issue_4112_process_backend_registry_miss_skips_sigint() {
        let _guard = SIGINT_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _ = process_table::take_sigint_test_events();
        let token = std::sync::Arc::new(CancelToken::new());
        token.store_child_pid(499_112);

        let outcome =
            interrupt_provider_cli_turn(&ProviderKind::Claude, &token, "explicit_stop").await;

        assert!(process_table::take_sigint_test_events().is_empty());
        assert_eq!(outcome.fallback_sigint_pid, None);
        assert!(outcome.sigint_target_missing);
    }

    #[test]
    fn issue_4112_preserve_session_cancel_does_not_mark_process_backend_stopped() {
        let session_name = format!("pipe-preserve-mark-only-{}", uuid::Uuid::new_v4());
        let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        crate::services::session_backend::insert_process_session(
            session_name.clone(),
            crate::services::session_backend::SessionHandle::TestProcess {
                pid: 44_112,
                alive: alive.clone(),
            },
        );
        let token = std::sync::Arc::new(CancelToken::new());
        token.store_child_pid(44_112);

        let termination_recorded =
            cancel_active_token(&token, TmuxCleanupPolicy::PreserveSession, "auto_heal");

        assert!(!termination_recorded);
        assert!(token.cancelled.load(Ordering::Relaxed));
        assert!(!crate::services::session_backend::process_session_was_stopped(&session_name));
        assert!(crate::services::session_backend::process_session_is_alive(
            &session_name
        ));

        if let Some(handle) =
            crate::services::session_backend::remove_process_session(&session_name)
        {
            crate::services::session_backend::terminate_process_handle(handle);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn issue_4112_process_backend_stop_hard_stops_sigint_ignoring_provider() {
        let _guard = SIGINT_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _ = process_table::take_sigint_test_events();
        let session_name = format!("pipe-hard-stop-{}", uuid::Uuid::new_v4());
        let pid_path =
            std::env::temp_dir().join(format!("agentdesk-provider-pid-{}", uuid::Uuid::new_v4()));
        let pid_path_arg = crate::services::process::shell_escape(&pid_path.display().to_string());
        let wrapper_script = format!(
            "bash -c 'trap \"\" INT; while :; do read -t 1 _ || true; done' & echo $! > {pid_path_arg}; wait $!"
        );
        let mut child = std::process::Command::new("bash")
            .arg("-c")
            .arg(wrapper_script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn process-backend wrapper fixture");
        let wrapper_pid = child.id();
        let child_stdin = child.stdin.take().expect("capture wrapper stdin");

        let mut provider_pid = None;
        for _ in 0..50 {
            if let Ok(raw) = std::fs::read_to_string(&pid_path)
                && let Ok(pid) = raw.trim().parse::<u32>()
            {
                provider_pid = Some(pid);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let provider_pid = provider_pid.expect("provider fixture pid should be visible");
        process_table::set_process_backend_provider_pid_for_test(wrapper_pid, provider_pid);

        crate::services::session_backend::insert_process_session(
            session_name.clone(),
            crate::services::session_backend::SessionHandle::Process {
                child_stdin: std::sync::Arc::new(std::sync::Mutex::new(Some(child_stdin))),
                child: std::sync::Arc::new(std::sync::Mutex::new(Some(child))),
                pid: wrapper_pid,
            },
        );
        let token = std::sync::Arc::new(CancelToken::new());
        token.store_child_pid(wrapper_pid);

        let termination_recorded = stop_active_turn(
            &ProviderKind::Claude,
            &token,
            TmuxCleanupPolicy::PreserveSession,
            "explicit_stop",
        )
        .await;

        assert!(!termination_recorded);
        assert_eq!(process_table::take_sigint_test_events(), vec![provider_pid]);
        assert!(crate::services::session_backend::process_session_was_stopped(&session_name));
        assert!(wait_for_pid_exit(provider_pid, Duration::from_secs(2)).await);
        assert!(wait_for_pid_exit(wrapper_pid, Duration::from_secs(2)).await);
        process_table::clear_process_backend_provider_pid_for_test(wrapper_pid);
        let _ = std::fs::remove_file(pid_path);
    }

    #[test]
    fn issue_4112_tmux_hard_stop_policy_preserves_existing_provider_behavior() {
        assert_eq!(
            hard_stop_pid_for_unresponsive_provider(
                &ProviderKind::Codex,
                TmuxCleanupPolicy::PreserveSession,
                true,
                false,
                Some(41),
                None,
                Some(7),
            ),
            Some(41),
            "non-pane provider PID remains the tmux hard-stop candidate"
        );
        assert_eq!(
            hard_stop_pid_for_unresponsive_provider(
                &ProviderKind::Codex,
                TmuxCleanupPolicy::PreserveSession,
                true,
                true,
                Some(41),
                None,
                Some(7),
            ),
            None,
            "ready tmux panes are still not hard-killed"
        );
        assert_eq!(
            hard_stop_pid_for_unresponsive_provider(
                &ProviderKind::Claude,
                TmuxCleanupPolicy::PreserveSession,
                true,
                false,
                Some(41),
                None,
                Some(7),
            ),
            None,
            "claude preserve-session tmux turns still avoid hard-killing the CLI"
        );
    }
}
