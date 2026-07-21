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
    pub(crate) retry_pid_cleanup: bool,
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
        self.publish_cancel_if_source_absent(request.cancel_source.clone());

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
                CleanupOutcome {
                    authorization: KillAuthorizationState::Stale,
                    pid_killed: false,
                    retry_pid_cleanup: false,
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
        let mut retry_pid_cleanup = false;
        let mut tmux_killed = false;
        let mut termination_recorded = false;
        let mut pid_claimed = false;
        let mut name_claimed = false;

        if matches!(
            request.intent,
            TmuxCleanupIntent::PidOnly | TmuxCleanupIntent::CleanupSession
        ) {
            if let Some(target) = child.as_ref().or(request.hard_stop_target.as_ref()) {
                // A claim is consumed only when this request actually dispatches its
                // PID primitive. In particular, an early watchdog with no target must
                // not prevent a later CleanupSession from claiming a newly bound PID.
                let identity_allows_dispatch = target
                    .identity
                    .is_none_or(|identity| identity.matches(target.pid));
                if identity_allows_dispatch {
                    pid_claimed = self
                        .pid_kill_claim
                        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok();
                    if pid_claimed {
                        pid_killed = self.kill_pid_tree_guarded(target);
                        if !pid_killed {
                            self.pid_kill_claim.store(0, Ordering::Release);
                            pid_claimed = false;
                            retry_pid_cleanup = true;
                        }
                    }
                } else {
                    tracing::debug!(
                        pid = target.pid,
                        has_identity = target.identity.is_some(),
                        "skip cancellation PID kill because captured identity is absent or no longer matches"
                    );
                }
            }
        }

        if matches!(request.intent, TmuxCleanupIntent::CleanupSession) {
            if let Some(name) = binding.as_ref().map(TmuxBinding::name) {
                if !self.tmux_cleanup_is_suppressed(name) {
                    // As with the PID claim, only consume this claim immediately before
                    // the name primitive is dispatched. Preserve and suppressed/no-name
                    // requests leave a later CleanupSession eligible to clean up.
                    name_claimed = self
                        .name_kill_claim
                        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok();
                    if name_claimed {
                        tmux_killed = self.kill_tmux_session_guarded(
                            name,
                            request.termination_reason,
                            &request.cancel_source,
                            authorization,
                        );
                        if !tmux_killed {
                            // The slot guard serializes same-session cleanup, so this
                            // rollback cannot erase another request's successful claim.
                            // A transient tmux failure or a suppression transition must
                            // remain retryable rather than orphaning the live session.
                            self.name_kill_claim.store(0, Ordering::Release);
                            name_claimed = false;
                        }
                        termination_recorded = tmux_killed && request.termination_reason.is_some();
                    }
                }
            }
        }
        // The legacy timeout drain reads child_pid after this call to distinguish a
        // killed worker from a naturally completed one. Leave the PID published until
        // its worker clears it, after this chokepoint has delivered the signal.
        if tmux_killed {
            self.clear_binding_if_matches(binding.as_ref());
        }
        CleanupOutcome {
            authorization,
            pid_killed,
            retry_pid_cleanup,
            tmux_killed,
            duplicate: matches!(request.intent, TmuxCleanupIntent::CleanupSession)
                && !pid_claimed
                && !name_claimed,
            termination_recorded,
        }
    }

    fn clear_binding_if_matches(&self, binding: Option<&TmuxBinding>) {
        if let Some(binding) = binding {
            let mut current = self
                .tmux_binding
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if current.as_ref() == Some(binding) {
                *current = None;
            }
        }
    }

    fn kill_pid_tree_guarded(&self, target: &CapturedProcess) -> bool {
        #[cfg(test)]
        {
            let _ = target;
            PID_KILL_DISPATCHES.fetch_add(1, Ordering::Relaxed);
            return PID_KILL_SUCCEEDS.load(Ordering::Relaxed);
        }
        #[cfg(not(test))]
        match target.identity {
            Some(identity) => {
                crate::services::process::kill_pid_tree_if_identity_matches(target.pid, identity)
            }
            // A current-generation cleanup must retain legacy PID delivery when
            // registration could not capture a platform identity baseline. The
            // generation slot still fences another managed incarnation; this is
            // deliberately distinct from an identity mismatch, which is skipped.
            None => {
                crate::services::process::kill_pid_tree(target.pid);
                true
            }
        }
    }

    fn tmux_cleanup_is_suppressed(&self, name: &str) -> bool {
        #[cfg(unix)]
        {
            crate::services::provider::parse_provider_and_channel_from_tmux_name(name)
                .map(|(_, channel)| {
                    crate::dispatch::is_unified_thread_channel_name_active(&channel)
                })
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            let _ = name;
            false
        }
    }

    fn kill_tmux_session_guarded(
        &self,
        name: &str,
        termination_reason: Option<&str>,
        cancel_source: &str,
        authorization: KillAuthorizationState,
    ) -> bool {
        #[cfg(test)]
        {
            let _ = (name, termination_reason, cancel_source, authorization);
            TMUX_KILL_DISPATCHES.fetch_add(1, Ordering::Relaxed);
            return TMUX_KILL_SUCCEEDS.load(Ordering::Relaxed)
                && !SUPPRESS_TMUX_AFTER_CLAIM.load(Ordering::Relaxed);
        }
        #[cfg(all(unix, not(test)))]
        {
            if self.tmux_cleanup_is_suppressed(name) {
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
        #[cfg(all(not(unix), not(test)))]
        {
            let _ = (name, termination_reason, cancel_source, authorization);
            false
        }
    }
}

#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicUsize};
#[cfg(test)]
static PID_KILL_DISPATCHES: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TMUX_KILL_DISPATCHES: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static PID_KILL_SUCCEEDS: AtomicBool = AtomicBool::new(true);
#[cfg(test)]
static TMUX_KILL_SUCCEEDS: AtomicBool = AtomicBool::new(true);
#[cfg(test)]
static SUPPRESS_TMUX_AFTER_CLAIM: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
pub(crate) fn pid_kill_dispatches_for_test() -> usize {
    PID_KILL_DISPATCHES.load(Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn set_pid_kill_succeeds_for_test(succeeds: bool) {
    PID_KILL_SUCCEEDS.store(succeeds, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn with_executor_dispatch_seam(test: impl FnOnce()) {
    use std::sync::{Mutex, OnceLock};

    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _lock = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    PID_KILL_DISPATCHES.store(0, Ordering::Relaxed);
    TMUX_KILL_DISPATCHES.store(0, Ordering::Relaxed);
    PID_KILL_SUCCEEDS.store(true, Ordering::Relaxed);
    TMUX_KILL_SUCCEEDS.store(true, Ordering::Relaxed);
    SUPPRESS_TMUX_AFTER_CLAIM.store(false, Ordering::Relaxed);
    test();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::provider::ProviderKind;

    fn request(intent: TmuxCleanupIntent) -> CleanupRequest {
        CleanupRequest {
            cancel_source: "test".to_string(),
            intent,
            termination_reason: None,
            hard_stop_target: None,
        }
    }

    fn with_seam(test: impl FnOnce()) {
        with_executor_dispatch_seam(test);
    }

    fn bind(token: &CancelToken, name: &str) {
        *token.tmux_binding.lock().unwrap() = authority::publish(
            ProviderKind::Claude,
            name,
            token.claude_interrupt_generation,
        );
    }

    #[test]
    fn pid_only_without_target_does_not_consume_claim() {
        with_seam(|| {
            let token = CancelToken::new();
            token.request_cleanup(request(TmuxCleanupIntent::PidOnly));
            assert_eq!(token.pid_kill_claim.load(Ordering::Acquire), 0);
            assert_eq!(PID_KILL_DISPATCHES.load(Ordering::Relaxed), 0);
        });
    }

    #[test]
    fn pid_only_without_identity_preserves_fail_open_dispatch() {
        with_seam(|| {
            let token = CancelToken::new();
            token.store_child_pid_without_identity_for_test(42);

            let outcome = token.request_cleanup(request(TmuxCleanupIntent::PidOnly));

            assert!(outcome.pid_killed);
            assert!(!outcome.retry_pid_cleanup);
            assert_eq!(PID_KILL_DISPATCHES.load(Ordering::Relaxed), 1);
            assert_eq!(token.pid_kill_claim.load(Ordering::Acquire), 1);
        });
    }

    #[test]
    fn pid_only_claim_does_not_suppress_later_session_cleanup_name_kill() {
        with_seam(|| {
            let token = CancelToken::new();
            bind(&token, "AgentDesk-claude-executor-f1");
            token.store_child_pid(std::process::id());

            token.request_cleanup(request(TmuxCleanupIntent::PidOnly));
            let outcome = token.request_cleanup(request(TmuxCleanupIntent::CleanupSession));

            assert_eq!(PID_KILL_DISPATCHES.load(Ordering::Relaxed), 1);
            assert_eq!(TMUX_KILL_DISPATCHES.load(Ordering::Relaxed), 1);
            assert!(outcome.tmux_killed);
        });
    }

    #[test]
    fn preserve_does_not_clear_or_consume_cleanup_targets() {
        with_seam(|| {
            let token = CancelToken::new();
            bind(&token, "AgentDesk-claude-executor-preserve");
            token.store_child_pid(std::process::id());

            token.request_cleanup(request(TmuxCleanupIntent::PreserveSession));
            let outcome = token.request_cleanup(request(TmuxCleanupIntent::CleanupSession));

            assert_eq!(PID_KILL_DISPATCHES.load(Ordering::Relaxed), 1);
            assert_eq!(TMUX_KILL_DISPATCHES.load(Ordering::Relaxed), 1);
            assert!(outcome.tmux_killed);
        });
    }

    #[test]
    fn failed_pid_dispatch_rolls_back_claim_for_retry() {
        with_seam(|| {
            let token = CancelToken::new();
            token.store_child_pid(std::process::id());
            PID_KILL_SUCCEEDS.store(false, Ordering::Relaxed);

            token.request_cleanup(request(TmuxCleanupIntent::PidOnly));
            assert_eq!(PID_KILL_DISPATCHES.load(Ordering::Relaxed), 1);
            assert_eq!(token.pid_kill_claim.load(Ordering::Acquire), 0);

            PID_KILL_SUCCEEDS.store(true, Ordering::Relaxed);
            token.request_cleanup(request(TmuxCleanupIntent::PidOnly));
            assert_eq!(PID_KILL_DISPATCHES.load(Ordering::Relaxed), 2);
            assert_eq!(token.pid_kill_claim.load(Ordering::Acquire), 1);
        });
    }

    #[test]
    fn failed_tmux_dispatch_rolls_back_name_claim_for_retry() {
        with_seam(|| {
            let token = CancelToken::new();
            bind(&token, "AgentDesk-claude-executor-tmux-failure");
            TMUX_KILL_SUCCEEDS.store(false, Ordering::Relaxed);

            token.request_cleanup(request(TmuxCleanupIntent::CleanupSession));
            assert_eq!(TMUX_KILL_DISPATCHES.load(Ordering::Relaxed), 1);
            assert_eq!(token.name_kill_claim.load(Ordering::Acquire), 0);

            TMUX_KILL_SUCCEEDS.store(true, Ordering::Relaxed);
            let outcome = token.request_cleanup(request(TmuxCleanupIntent::CleanupSession));
            assert_eq!(TMUX_KILL_DISPATCHES.load(Ordering::Relaxed), 2);
            assert_eq!(token.name_kill_claim.load(Ordering::Acquire), 1);
            assert!(outcome.tmux_killed);
        });
    }

    #[test]
    fn suppression_after_name_claim_rolls_back_claim() {
        with_seam(|| {
            let token = CancelToken::new();
            bind(&token, "AgentDesk-claude-executor-suppression-transition");
            SUPPRESS_TMUX_AFTER_CLAIM.store(true, Ordering::Relaxed);

            token.request_cleanup(request(TmuxCleanupIntent::CleanupSession));
            assert_eq!(TMUX_KILL_DISPATCHES.load(Ordering::Relaxed), 1);
            assert_eq!(token.name_kill_claim.load(Ordering::Acquire), 0);
        });
    }

    #[test]
    fn unregistered_binding_dispatches_pid_and_tmux_cleanup() {
        with_seam(|| {
            let token = CancelToken::new();
            *token.tmux_binding.lock().unwrap() = Some(TmuxBinding::NameOnly {
                name: "AgentDesk-codex-executor-unregistered".to_string(),
            });
            token.store_child_pid(std::process::id());

            let outcome = token.request_cleanup(request(TmuxCleanupIntent::CleanupSession));

            assert_eq!(outcome.authorization, KillAuthorizationState::Unregistered);
            assert_eq!(PID_KILL_DISPATCHES.load(Ordering::Relaxed), 1);
            assert_eq!(TMUX_KILL_DISPATCHES.load(Ordering::Relaxed), 1);
        });
    }

    #[test]
    fn stale_binding_is_retained_and_dispatches_no_primitive() {
        with_seam(|| {
            let token = CancelToken::new();
            let name = "AgentDesk-claude-executor-stale";
            bind(&token, name);
            token.store_child_pid(std::process::id());
            authority::publish(
                ProviderKind::Claude,
                name,
                token.claude_interrupt_generation + 1,
            );

            let outcome = token.request_cleanup(request(TmuxCleanupIntent::CleanupSession));

            assert_eq!(outcome.authorization, KillAuthorizationState::Stale);
            assert_eq!(PID_KILL_DISPATCHES.load(Ordering::Relaxed), 0);
            assert_eq!(TMUX_KILL_DISPATCHES.load(Ordering::Relaxed), 0);
            assert!(token.tmux_binding.lock().unwrap().is_some());
            assert!(token.child_pid.lock().unwrap().is_some());
        });
    }
}
