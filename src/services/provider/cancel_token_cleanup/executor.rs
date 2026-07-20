//! Authorized destructive cleanup for cancellation tokens.
//!
//! The session slot guard obtained here is deliberately retained across every
//! destructive primitive. A newer Claude incarnation must publish into the
//! same slot before it can become reachable, so it cannot appear between the
//! generation check and a kill.

use super::authority::{
    self, KillAuthorization, KillAuthorizationState, SessionKillGuard, TmuxBinding,
};
use super::target::CapturedProcess;
use crate::services::provider::CancelToken;
use std::sync::atomic::Ordering;

/// Cleanup intent for a managed tmux turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TmuxCleanupIntent {
    PreserveSession,
    /// A PID-only escalation; it must not consume the tmux-name claim.
    PidOnly,
    CleanupSession,
}

/// One authorized cleanup request. Callers must not compose PID and tmux kills.
#[derive(Clone, Debug)]
pub(crate) struct CleanupRequest {
    pub(crate) cancel_source: String,
    pub(crate) intent: TmuxCleanupIntent,
    pub(crate) termination_reason: Option<&'static str>,
    pub(crate) hard_stop_target: Option<CapturedProcess>,
}

/// Observable result of a cleanup request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CleanupOutcome {
    pub(crate) authorization: KillAuthorizationState,
    pub(crate) pid_killed: bool,
    pub(crate) tmux_killed: bool,
    pub(crate) duplicate: bool,
    pub(crate) termination_recorded: bool,
}

impl CleanupOutcome {
    pub(crate) fn termination_confirmed(self) -> bool {
        self.termination_recorded
    }
}

impl CancelToken {
    /// Execute all token-owned destructive cleanup behind one generation fence.
    pub(crate) fn request_cleanup(&self, request: CleanupRequest) -> CleanupOutcome {
        self.cancelled.store(true, Ordering::Relaxed);
        self.set_cancel_source(request.cancel_source.clone());

        let binding = self
            .tmux_binding
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let child = self
            .child_pid
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let authorization = authority::authorize(binding.as_ref());

        match authorization {
            KillAuthorization::Stale {
                token_generation,
                registry_generation,
            } => {
                tracing::warn!(
                    token_generation,
                    registry_generation,
                    cancel_source = request.cancel_source,
                    "skip cancellation cleanup for stale Claude generation"
                );
                self.clear_cleanup_targets(binding.as_ref(), child.as_ref());
                CleanupOutcome {
                    authorization: KillAuthorizationState::Stale,
                    pid_killed: false,
                    tmux_killed: false,
                    duplicate: false,
                    termination_recorded: false,
                }
            }
            KillAuthorization::Current(guard) => self.request_cleanup_authorized(
                request,
                binding,
                child,
                KillAuthorizationState::Current,
                Some(guard),
            ),
            KillAuthorization::Unregistered => {
                tracing::debug!(
                    cancel_source = request.cancel_source,
                    "cancellation cleanup has no managed generation; preserving fail-open behavior"
                );
                self.request_cleanup_authorized(
                    request,
                    binding,
                    child,
                    KillAuthorizationState::Unregistered,
                    None,
                )
            }
        }
    }

    fn request_cleanup_authorized(
        &self,
        request: CleanupRequest,
        binding: Option<TmuxBinding>,
        child: Option<CapturedProcess>,
        authorization: KillAuthorizationState,
        guard: Option<SessionKillGuard>,
    ) -> CleanupOutcome {
        // `guard` intentionally remains live through this function. Do not call a
        // public cleanup/bind API from here: those APIs can try to lock this slot.
        let _guard = guard;
        let mut pid_killed = false;
        let mut tmux_killed = false;
        let mut termination_recorded = false;
        let mut pid_claimed = false;
        let mut name_claimed = false;

        if matches!(
            request.intent,
            TmuxCleanupIntent::PidOnly | TmuxCleanupIntent::CleanupSession
        ) {
            pid_claimed = self
                .pid_kill_claim
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
            if pid_claimed {
                if let Some(target) = child.as_ref().or(request.hard_stop_target.as_ref()) {
                    if let Some(identity) = target.identity {
                        pid_killed = crate::services::process::kill_pid_tree_if_identity_matches(
                            target.pid, identity,
                        );
                    } else {
                        tracing::debug!(
                            pid = target.pid,
                            "skip cancellation PID kill without captured identity"
                        );
                    }
                }
            }
        }

        if matches!(request.intent, TmuxCleanupIntent::CleanupSession) {
            name_claimed = self
                .name_kill_claim
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
            if name_claimed {
                if let Some(name) = binding.as_ref().map(TmuxBinding::name) {
                    tmux_killed = self.kill_tmux_session_guarded(
                        name,
                        request.termination_reason,
                        &request.cancel_source,
                        authorization,
                    );
                    termination_recorded = tmux_killed && request.termination_reason.is_some();
                }
            }
        }

        if matches!(request.intent, TmuxCleanupIntent::CleanupSession) {
            self.clear_cleanup_targets(binding.as_ref(), child.as_ref());
        }
        CleanupOutcome {
            authorization,
            pid_killed,
            tmux_killed,
            duplicate: matches!(request.intent, TmuxCleanupIntent::CleanupSession)
                && !pid_claimed
                && !name_claimed,
            termination_recorded,
        }
    }

    fn kill_tmux_session_guarded(
        &self,
        name: &str,
        termination_reason: Option<&str>,
        cancel_source: &str,
        authorization: KillAuthorizationState,
    ) -> bool {
        #[cfg(unix)]
        {
            let unified =
                crate::services::provider::parse_provider_and_channel_from_tmux_name(name)
                    .map(|(_, channel)| {
                        crate::dispatch::is_unified_thread_channel_name_active(&channel)
                    })
                    .unwrap_or(false);
            if unified {
                tracing::debug!(
                    tmux_session = name,
                    "skip cleanup for active unified thread"
                );
                return false;
            }
            tracing::debug!(
                tmux_session = name,
                ?authorization,
                cancel_source,
                "dispatch authorized cancellation tmux cleanup"
            );
            let reason = format!("explicit cleanup via {cancel_source}");
            crate::services::tmux_diagnostics::record_tmux_exit_reason(name, &reason);
            let killed = crate::services::platform::tmux::kill_session(name, &reason);
            if killed {
                if let Some(reason_code) = termination_reason {
                    crate::services::termination_audit::record_termination_for_tmux(
                        name,
                        None,
                        "turn_bridge",
                        reason_code,
                        Some(&reason),
                        None,
                    );
                }
            }
            killed
        }
        #[cfg(not(unix))]
        {
            let _ = (name, termination_reason, cancel_source, authorization);
            false
        }
    }

    fn clear_cleanup_targets(
        &self,
        binding: Option<&TmuxBinding>,
        child: Option<&CapturedProcess>,
    ) {
        if let Some(binding) = binding {
            let mut current = self
                .tmux_binding
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if current.as_ref() == Some(binding) {
                *current = None;
            }
        }
        if let Some(child) = child {
            let mut current = self
                .child_pid
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if current
                .as_ref()
                .is_some_and(|current| current.pid == child.pid)
            {
                *current = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn request(intent: TmuxCleanupIntent) -> CleanupRequest {
        CleanupRequest {
            cancel_source: "test".to_string(),
            intent,
            termination_reason: None,
            hard_stop_target: None,
        }
    }

    #[test]
    fn pid_only_claim_does_not_suppress_later_session_cleanup_name_claim() {
        let token = CancelToken::new();
        token.request_cleanup(request(TmuxCleanupIntent::PidOnly));
        assert_eq!(token.pid_kill_claim.load(Ordering::Acquire), 1);
        assert_eq!(token.name_kill_claim.load(Ordering::Acquire), 0);

        token.request_cleanup(request(TmuxCleanupIntent::CleanupSession));
        assert_eq!(token.name_kill_claim.load(Ordering::Acquire), 1);
    }

    #[test]
    fn preserve_claims_no_destructive_primitive() {
        let token = CancelToken::new();
        token.request_cleanup(request(TmuxCleanupIntent::PreserveSession));
        assert_eq!(token.pid_kill_claim.load(Ordering::Acquire), 0);
        assert_eq!(token.name_kill_claim.load(Ordering::Acquire), 0);
    }
}
