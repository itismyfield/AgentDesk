//! The current thread's deadline for synchronous native bridge ops.
//! The engine loader arms it around a bounded JS eval; bridge ops and the async bridge read it.

use std::cell::Cell;
use std::time::{Duration, Instant};

thread_local! {
    // The QuickJS interrupt handler cannot fire while Rust runs, so bridge ops bound themselves by this.
    static CURRENT: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Installs a deadline on this thread and restores the previous (outer) one on drop.
pub(crate) struct ScopedBridgeDeadline {
    previous: Option<Instant>,
}

impl ScopedBridgeDeadline {
    pub(crate) fn install(deadline: Instant) -> Self {
        let previous = CURRENT.with(|cell| cell.replace(Some(deadline)));
        Self { previous }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn new(budget: Duration) -> Self {
        Self::install(Instant::now() + budget)
    }
}

impl Drop for ScopedBridgeDeadline {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CURRENT.with(|cell| cell.set(previous));
    }
}

/// The armed deadline, or `None` when no bounded eval is in flight and callers keep their defaults.
pub(crate) fn current() -> Option<Instant> {
    CURRENT.with(|cell| cell.get())
}

/// The remaining budget: `Some(Duration::ZERO)` once the deadline has passed, `None` when unarmed.
pub(crate) fn remaining() -> Option<Duration> {
    current().map(|deadline| {
        deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_scope_restores_the_outer_deadline_then_clears() {
        let outer = Instant::now() + Duration::from_secs(60);
        let inner = Instant::now() + Duration::from_secs(5);
        {
            let _outer = ScopedBridgeDeadline::install(outer);
            {
                let _inner = ScopedBridgeDeadline::install(inner);
                assert_eq!(current(), Some(inner));
            }
            assert_eq!(current(), Some(outer));
        }
        assert_eq!(current(), None);
    }
}
