//! Session-level Claude turn-interrupt ownership.

use super::cancel_token_cleanup::authority::{self, KillAuthorization, SessionKillGuard};
use super::{CancelToken, ProviderKind};
use std::sync::atomic::Ordering;

pub(crate) struct ClaudeInterruptDeliveryGuard<'a> {
    token: &'a CancelToken,
    _session: SessionKillGuard,
}

impl ClaudeInterruptDeliveryGuard<'_> {
    pub(crate) fn commit_success<R, E>(self, outcome: Result<R, E>) -> Result<R, E> {
        if outcome.is_ok() {
            // The claim owner is the only caller that can hold this generation
            // guard. Commit with a plain store while the session lock is still
            // held, so no rollback/reclaim can interleave after provider I/O.
            self.token
                .claude_interrupt_claim
                .store(2, Ordering::Release);
            self.token.clear_claude_interrupt_submit_pending();
        }
        outcome
    }
}

pub(crate) fn submit_claude_wrapper_followup<Write>(
    token: Option<&CancelToken>,
    tmux_session_name: &str,
    write: Write,
) -> Result<(), String>
where
    Write: FnOnce() -> Result<(), String>,
{
    if let Some(token) = token {
        token.bind_claude_tmux_session(tmux_session_name);
    }
    write()?;
    if let Some(token) = token {
        token.mark_claude_interrupt_submit_pending();
    }
    Ok(())
}

pub(crate) fn observe_claude_wrapper_followup<R, Read>(
    token: Option<&CancelToken>,
    read: Read,
) -> Result<R, String>
where
    Read: FnOnce() -> Result<R, String>,
{
    let result = read();
    if let Some(token) = token {
        token.clear_claude_interrupt_submit_pending();
    }
    result
}

impl CancelToken {
    /// Publish this turn before its Claude pane can become reachable.
    ///
    /// The registry is monotonic per session: delayed recovery/rebind callers may
    /// refresh their token-local tmux name, but cannot replace a newer turn.
    pub(crate) fn bind_claude_tmux_session(&self, tmux_session_name: &str) {
        let tmux_session_name = tmux_session_name.trim();
        if tmux_session_name.is_empty() {
            return;
        }
        let Some(binding) = authority::publish(
            ProviderKind::Claude,
            tmux_session_name,
            self.claude_interrupt_generation,
        ) else {
            return;
        };
        *self
            .tmux_binding
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(binding);
    }

    /// Acquire the session-level generation fence for provider delivery.
    ///
    /// The returned guard holds the registry lock through the caller's provider
    /// write and synchronous claim commit. A newer turn cannot publish its
    /// generation between the check and the write.
    pub(crate) fn lock_current_claude_interrupt_session(
        &self,
        tmux_session_name: &str,
    ) -> Option<ClaudeInterruptDeliveryGuard<'_>> {
        let binding = self
            .tmux_binding
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let matches_requested_name = binding
            .as_ref()
            .is_some_and(|binding| binding.name() == tmux_session_name.trim());
        if !matches_requested_name {
            return None;
        }
        match authority::authorize(binding.as_ref()) {
            KillAuthorization::Current(session) => Some(ClaudeInterruptDeliveryGuard {
                token: self,
                _session: session,
            }),
            KillAuthorization::Unregistered | KillAuthorization::Stale { .. } => None,
        }
    }

    /// Store a tmux name without publishing a managed generation slot.
    pub(crate) fn bind_unmanaged_session_name(&self, tmux_session_name: &str) {
        let tmux_session_name = tmux_session_name.trim();
        if tmux_session_name.is_empty() {
            return;
        }
        *self
            .tmux_binding
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(
            super::cancel_token_cleanup::authority::TmuxBinding::NameOnly {
                name: tmux_session_name.to_string(),
            },
        );
    }

    pub(crate) fn tmux_session_name(&self) -> Option<String> {
        self.tmux_binding
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|binding| binding.name().to_string())
    }

    /// Record that a wrapper accepted this turn before JSONL confirms it.
    pub(crate) fn mark_claude_interrupt_submit_pending(&self) {
        self.claude_interrupt_submit_pending
            .store(true, Ordering::Release);
    }

    pub(crate) fn claude_interrupt_submit_pending(&self) -> bool {
        self.claude_interrupt_submit_pending.load(Ordering::Acquire)
    }

    /// Clear the handoff window once delivery commits or turn observation ends.
    pub(crate) fn clear_claude_interrupt_submit_pending(&self) {
        self.claude_interrupt_submit_pending
            .store(false, Ordering::Release);
    }

    /// Reserve the Claude interrupt-delivery right for this turn.
    pub(crate) fn claim_claude_interrupt(&self) -> bool {
        self.claude_interrupt_claim
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Release an undelivered reservation so a later stop can retry this turn.
    pub(crate) fn release_claude_interrupt_claim(&self) -> bool {
        self.claude_interrupt_claim
            .compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn claude_interrupt_generation(&self) -> u64 {
        self.claude_interrupt_generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn wrapper_followup_publishes_then_writes_then_marks_pending() {
        let session = "AgentDesk-claude-wrapper-followup-submit-order";
        let stale = CancelToken::new();
        let current = CancelToken::new();
        stale.bind_claude_tmux_session(session);
        let write_observed = AtomicUsize::new(0);

        submit_claude_wrapper_followup(Some(&current), session, || {
            assert!(
                stale
                    .lock_current_claude_interrupt_session(session)
                    .is_none(),
                "current generation must publish before FIFO write"
            );
            assert!(
                !current.claude_interrupt_submit_pending(),
                "submit-pending must remain false until FIFO flush succeeds"
            );
            write_observed.store(1, Ordering::Release);
            Ok(())
        })
        .unwrap();

        assert_eq!(write_observed.load(Ordering::Acquire), 1);
        assert!(current.claude_interrupt_submit_pending());
    }

    #[test]
    fn wrapper_followup_write_failure_does_not_mark_submit_pending() {
        let token = CancelToken::new();
        assert!(
            submit_claude_wrapper_followup(
                Some(&token),
                "AgentDesk-claude-wrapper-write-failure",
                || { Err("write failed".to_string()) }
            )
            .is_err()
        );
        assert!(!token.claude_interrupt_submit_pending());
    }

    #[test]
    fn wrapper_followup_read_end_clears_submit_pending() {
        for expected_ok in [true, false] {
            let token = CancelToken::new();
            token.mark_claude_interrupt_submit_pending();

            let observed = observe_claude_wrapper_followup(Some(&token), || {
                if expected_ok {
                    Ok("completed")
                } else {
                    Err("read failed".to_string())
                }
            });

            assert_eq!(observed.is_ok(), expected_ok);
            assert!(
                !token.claude_interrupt_submit_pending(),
                "terminal and error read exits must close the submitted window"
            );
        }
    }

    #[test]
    fn session_generation_advance_blocks_stale_stop_operation() {
        let session = "AgentDesk-claude-session-generation-advance";
        let stale = CancelToken::new();
        let current = CancelToken::new();
        stale.bind_claude_tmux_session(session);
        assert!(stale.claim_claude_interrupt());

        current.bind_claude_tmux_session(session);
        let writes = AtomicUsize::new(0);
        let guard = stale.lock_current_claude_interrupt_session(session);
        if let Some(guard) = guard {
            let outcome = guard.commit_success((|| {
                writes.fetch_add(1, Ordering::Relaxed);
                Ok::<(), ()>(())
            })());
            assert_eq!(outcome, Ok(()));
        }

        assert!(
            stale
                .lock_current_claude_interrupt_session(session)
                .is_none()
        );
        assert_eq!(writes.load(Ordering::Relaxed), 0);
        assert!(stale.release_claude_interrupt_claim());
    }

    #[test]
    fn stale_rebind_cannot_replace_a_newer_session_generation() {
        let session = "AgentDesk-claude-session-stale-rebind";
        let stale = CancelToken::new();
        let current = CancelToken::new();
        stale.bind_claude_tmux_session(session);
        current.bind_claude_tmux_session(session);

        stale.bind_claude_tmux_session(session);

        assert!(
            stale
                .lock_current_claude_interrupt_session(session)
                .is_none()
        );
        assert!(
            current
                .lock_current_claude_interrupt_session(session)
                .is_some()
        );
    }

    #[test]
    fn stale_pending_stop_is_rejected_after_next_generation_publishes() {
        let session = "AgentDesk-claude-session-pending-generation-advance";
        let stale = CancelToken::new();
        let current = CancelToken::new();
        stale.bind_claude_tmux_session(session);
        stale.mark_claude_interrupt_submit_pending();

        current.bind_claude_tmux_session(session);

        assert!(stale.claude_interrupt_submit_pending());
        assert!(
            stale
                .lock_current_claude_interrupt_session(session)
                .is_none(),
            "pending state must never bypass a newer generation publication"
        );
        assert!(
            current
                .lock_current_claude_interrupt_session(session)
                .is_some()
        );
    }

    #[test]
    fn unmanaged_binding_stores_name_without_registry_authority() {
        let token = CancelToken::new();
        token.bind_unmanaged_session_name("AgentDesk-codex-name-only-binding");

        assert_eq!(
            token.tmux_session_name().as_deref(),
            Some("AgentDesk-codex-name-only-binding")
        );
        let binding = token
            .tmux_binding
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        assert!(matches!(
            authority::authorize(binding.as_ref()),
            KillAuthorization::Unregistered
        ));
    }

    #[test]
    fn submitted_window_clears_when_delivery_commits() {
        let session = "AgentDesk-claude-session-pending-commit";
        let token = CancelToken::new();
        token.bind_claude_tmux_session(session);
        token.mark_claude_interrupt_submit_pending();
        assert!(token.claim_claude_interrupt());

        token
            .lock_current_claude_interrupt_session(session)
            .unwrap()
            .commit_success(Ok::<(), ()>(()))
            .unwrap();

        assert!(!token.claude_interrupt_submit_pending());
    }

    #[test]
    fn successful_operation_commits_before_returning() {
        let session = "AgentDesk-claude-session-atomic-commit";
        let token = CancelToken::new();
        token.bind_claude_tmux_session(session);
        assert!(token.claim_claude_interrupt());

        token
            .lock_current_claude_interrupt_session(session)
            .expect("current generation must acquire delivery guard")
            .commit_success(Ok::<(), ()>(()))
            .expect("current generation must deliver");

        assert!(!token.claim_claude_interrupt());
        assert!(!token.release_claude_interrupt_claim());
    }
}
