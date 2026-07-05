//! ProcessBackend cancellation for provider turns without a tmux runtime.
//!
//! Pipe-mode sessions register only the wrapper child PID on the cancel token.
//! Keep the tmux-specific interrupt path in the parent module, but make this
//! no-tmux path explicit so a Discord stop cannot mark a turn stopped while the
//! underlying wrapper keeps generating.

use super::interrupt_policy::ProviderTurnInterruptOutcome;
use super::pid_exit::wait_for_pid_exit;
use super::process_table::send_sigint;
use crate::services::provider::{CancelToken, ProviderKind};
use std::sync::Arc;

pub(super) fn interrupt_process_backend_turn(
    provider: &ProviderKind,
    child_pid: Option<u32>,
    reason: &str,
) -> ProviderTurnInterruptOutcome {
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
            "process backend interrupt found no registry entry for pid: provider={} pid={} reason={}",
            provider.as_str(),
            child_pid,
            reason
        );
    }

    if let Err(error) = send_sigint(child_pid) {
        tracing::warn!(
            "process backend interrupt SIGINT failed: provider={} pid={} reason={} error={}",
            provider.as_str(),
            child_pid,
            reason,
            error
        );
    } else {
        tracing::info!(
            "process backend interrupt SIGINT sent: provider={} pid={} reason={} stopped_sessions={:?}",
            provider.as_str(),
            child_pid,
            reason,
            stopped_sessions
        );
    }

    ProviderTurnInterruptOutcome {
        tmux_session: None,
        sent_keys: false,
        fallback_sigint_pid: Some(child_pid),
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
    let child_pid = interrupt_outcome
        .fallback_sigint_pid
        .or_else(|| token.child_pid.lock().ok().and_then(|guard| *guard));
    let Some(child_pid) = child_pid else {
        tracing::error!(
            "provider hard-stop skipped: provider={} reason={} error=cancel_token_missing_runtime_target interrupt_missing_tmux_session={}",
            provider.as_str(),
            reason,
            interrupt_outcome.missing_tmux_session
        );
        return;
    };

    if wait_for_pid_exit(child_pid, super::PROVIDER_HARD_STOP_GRACE).await {
        return;
    }

    tracing::warn!(
        "process backend turn did not stop after SIGINT; killing process tree: provider={} pid={} reason={}",
        provider.as_str(),
        child_pid,
        reason
    );
    crate::services::session_backend::mark_process_sessions_stopped_by_pid(child_pid);
    crate::services::process::kill_pid_tree(child_pid);
}
