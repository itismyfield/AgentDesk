//! ProcessBackend cancellation for provider turns without a tmux runtime.
//!
//! Pipe-mode sessions register only the wrapper child PID on the cancel token.
//! Keep the tmux-specific interrupt path in the parent module, but make this
//! no-tmux path explicit so a Discord stop cannot mark a turn stopped while the
//! underlying wrapper keeps generating.

use super::interrupt_policy::{
    ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON, ProviderTurnInterruptOutcome,
};
use super::pid_exit::wait_for_pid_exit;
use super::process_table::{
    provider_cli_pid_for_process_backend, send_sigint, send_sigint_to_process_group_or_pid,
};
use crate::services::provider::{CancelToken, ProviderKind};
use std::sync::Arc;

pub(super) fn interrupt_process_backend_turn(
    provider: &ProviderKind,
    child_pid: Option<u32>,
    reason: &str,
) -> ProviderTurnInterruptOutcome {
    if reason == ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON {
        tracing::info!(
            "process backend interrupt suppressed: provider={} reason={} detail=anonymous_turn_bridge_teardown",
            provider.as_str(),
            reason
        );
        return ProviderTurnInterruptOutcome {
            tmux_session: None,
            sent_keys: false,
            fallback_sigint_pid: None,
            missing_tmux_session: true,
            sigint_target_missing: false,
        };
    }

    let Some(child_pid) = child_pid else {
        tracing::error!(
            "provider turn interrupt skipped: provider={} reason={} error=cancel_token_missing_runtime_target",
            provider.as_str(),
            reason
        );
        return ProviderTurnInterruptOutcome {
            tmux_session: None,
            sent_keys: false,
            fallback_sigint_pid: None,
            missing_tmux_session: true,
            sigint_target_missing: true,
        };
    };

    let stopped_sessions =
        crate::services::session_backend::mark_process_sessions_stopped_by_pid(child_pid);
    if stopped_sessions.is_empty() {
        tracing::warn!(
            "process backend interrupt skipped SIGINT: provider={} pid={} reason={} error=registry_entry_missing_or_identity_mismatch",
            provider.as_str(),
            child_pid,
            reason
        );
        return ProviderTurnInterruptOutcome {
            tmux_session: None,
            sent_keys: false,
            fallback_sigint_pid: None,
            missing_tmux_session: true,
            sigint_target_missing: true,
        };
    }

    let signal_target = provider_cli_pid_for_process_backend(child_pid, provider);
    let (target_pid, target_kind) = match signal_target {
        Some(pid) => (pid, "provider_cli"),
        None => {
            tracing::warn!(
                "process backend interrupt could not resolve provider CLI pid; falling back to verified wrapper signal: provider={} wrapper_pid={} reason={} stopped_sessions={:?}",
                provider.as_str(),
                child_pid,
                reason,
                stopped_sessions
            );
            (child_pid, "wrapper")
        }
    };

    let signal_result = if signal_target.is_some() {
        send_sigint_to_process_group_or_pid(target_pid)
    } else {
        send_sigint(target_pid)
    };

    if let Err(error) = signal_result {
        tracing::warn!(
            "process backend interrupt SIGINT failed: provider={} target_kind={} pid={} wrapper_pid={} reason={} error={}",
            provider.as_str(),
            target_kind,
            target_pid,
            child_pid,
            reason,
            error
        );
    } else {
        tracing::info!(
            "process backend interrupt SIGINT sent: provider={} target_kind={} pid={} wrapper_pid={} reason={} stopped_sessions={:?}",
            provider.as_str(),
            target_kind,
            target_pid,
            child_pid,
            reason,
            stopped_sessions
        );
    }

    ProviderTurnInterruptOutcome {
        tmux_session: None,
        sent_keys: false,
        fallback_sigint_pid: Some(target_pid),
        missing_tmux_session: true,
        sigint_target_missing: false,
    }
}

pub(super) async fn hard_stop_unresponsive_process_backend_turn(
    provider: &ProviderKind,
    token: &Arc<CancelToken>,
    interrupt_outcome: &ProviderTurnInterruptOutcome,
    reason: &str,
) {
    let Some(target_pid) = interrupt_outcome.fallback_sigint_pid else {
        tracing::error!(
            "provider hard-stop skipped: provider={} reason={} error=no_verified_process_backend_target interrupt_missing_tmux_session={} sigint_target_missing={}",
            provider.as_str(),
            reason,
            interrupt_outcome.missing_tmux_session,
            interrupt_outcome.sigint_target_missing
        );
        return;
    };
    let wrapper_pid = token.child_pid_value();

    if wait_for_pid_exit(target_pid, super::PROVIDER_HARD_STOP_GRACE).await {
        return;
    }

    tracing::warn!(
        "process backend turn did not stop after SIGINT; killing process tree: provider={} pid={} wrapper_pid={:?} reason={}",
        provider.as_str(),
        target_pid,
        wrapper_pid,
        reason
    );
    if let Some(wrapper_pid) = wrapper_pid {
        crate::services::session_backend::mark_process_sessions_stopped_by_pid(wrapper_pid);
    }
    crate::services::process::kill_pid_tree(target_pid);
}
