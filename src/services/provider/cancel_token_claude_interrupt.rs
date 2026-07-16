//! Session-level Claude turn-interrupt ownership.

use super::CancelToken;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{LazyLock, Mutex};

static ACTIVE_GENERATION_BY_TMUX: LazyLock<Mutex<HashMap<String, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) struct ClaudeInterruptDeliveryGuard<'a> {
    token: &'a CancelToken,
    _generations: std::sync::MutexGuard<'static, HashMap<String, u64>>,
}

impl ClaudeInterruptDeliveryGuard<'_> {
    pub(crate) fn commit_success<R, E>(self, outcome: Result<R, E>) -> Result<R, E> {
        if outcome.is_ok()
            && self
                .token
                .claude_interrupt_claim
                .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            tracing::error!(
                generation = self.token.claude_interrupt_generation,
                claim_state = self.token.claude_interrupt_claim.load(Ordering::Acquire),
                "Claude stop write succeeded but claim commit lost ownership"
            );
        }
        outcome
    }
}

impl CancelToken {
    /// Bind this turn token as the active Claude generation for a tmux session.
    pub(crate) fn bind_claude_tmux_session(&self, tmux_session_name: &str) {
        let tmux_session_name = tmux_session_name.trim();
        if tmux_session_name.is_empty() {
            return;
        }
        ACTIVE_GENERATION_BY_TMUX
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                tmux_session_name.to_string(),
                self.claude_interrupt_generation,
            );
        *self
            .tmux_session
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(tmux_session_name.to_string());
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
        let generations = ACTIVE_GENERATION_BY_TMUX
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let is_current = generations
            .get(tmux_session_name.trim())
            .is_some_and(|generation| *generation == self.claude_interrupt_generation);
        is_current.then_some(ClaudeInterruptDeliveryGuard {
            token: self,
            _generations: generations,
        })
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
    fn session_generation_advance_blocks_stale_stop_operation() {
        let session = "claude-session-generation-advance";
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
    fn successful_operation_commits_before_returning() {
        let session = "claude-session-atomic-commit";
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
