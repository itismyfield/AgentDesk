//! Pre-payload fence for managed Claude prompts on the provider execution thread.
//! The owner of the inflight row installs it; input only asks it before the first payload.

use std::cell::RefCell;
use std::sync::Arc;

/// The durable "may have submitted" mark could not be persisted, so no payload was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreSubmitPersistenceRefused {
    pub(crate) reason: String,
}

impl std::fmt::Display for PreSubmitPersistenceRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "claude tui prompt held before submit: {}", self.reason)
    }
}

/// Durable submission boundary of one turn attempt.
pub(crate) trait SubmissionFence: Send + Sync {
    /// Persists the boundary before the first payload; idempotent once it succeeded.
    fn before_first_payload(&self) -> Result<(), PreSubmitPersistenceRefused>;
    /// The refusal that closed this attempt's input, if any.
    fn refusal(&self) -> Option<PreSubmitPersistenceRefused>;
}

thread_local! {
    static SCOPE: RefCell<Option<Arc<dyn SubmissionFence>>> = const { RefCell::new(None) };
}

struct ScopeReset(Option<Arc<dyn SubmissionFence>>);

impl Drop for ScopeReset {
    fn drop(&mut self) {
        let previous = self.0.take();
        SCOPE.with(|slot| *slot.borrow_mut() = previous);
    }
}

/// Runs `run` with `fence` as this thread's submission boundary; the previous one returns after.
pub(crate) fn with_scope<R>(fence: Option<Arc<dyn SubmissionFence>>, run: impl FnOnce() -> R) -> R {
    let previous = SCOPE.with(|slot| std::mem::replace(&mut *slot.borrow_mut(), fence));
    let _reset = ScopeReset(previous);
    run()
}

fn current() -> Option<Arc<dyn SubmissionFence>> {
    SCOPE.with(|slot| slot.borrow().clone())
}

/// Input calls this after its final readiness checks; an unmanaged prompt passes unchanged.
pub(crate) fn before_first_payload() -> Result<(), String> {
    match current() {
        Some(fence) => fence
            .before_first_payload()
            .map_err(|refused| refused.to_string()),
        None => Ok(()),
    }
}

/// The typed refusal of this thread's attempt, read before any teardown or retry decision.
pub(crate) fn refusal() -> Option<PreSubmitPersistenceRefused> {
    current().and_then(|fence| fence.refusal())
}

/// A fence whose write always fails, for callers that branch on the typed refusal.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{PreSubmitPersistenceRefused, SubmissionFence};

    #[derive(Default)]
    pub(crate) struct RefusingFence {
        asked: AtomicBool,
    }

    impl RefusingFence {
        fn refused() -> PreSubmitPersistenceRefused {
            PreSubmitPersistenceRefused {
                reason: "injected fence write failure".into(),
            }
        }
    }

    impl SubmissionFence for RefusingFence {
        fn before_first_payload(&self) -> Result<(), PreSubmitPersistenceRefused> {
            self.asked.store(true, Ordering::SeqCst);
            Err(Self::refused())
        }

        fn refusal(&self) -> Option<PreSubmitPersistenceRefused> {
            self.asked.load(Ordering::SeqCst).then(Self::refused)
        }
    }
}
