//! The current thread's deadline for synchronous native bridge ops.
//! The engine loader arms it around a bounded JS eval; bridge ops and the async bridge read it.

use std::cell::Cell;
use std::time::{Duration, Instant};

thread_local! {
    // The QuickJS interrupt handler cannot fire while Rust runs, so bridge ops bound themselves by this.
    static CURRENT: Cell<Option<Instant>> = const { Cell::new(None) };
    static WORK: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Installs a deadline on this thread and restores the previous (outer) one on drop.
pub struct ScopedBridgeDeadline {
    previous: Option<Instant>,
}

impl ScopedBridgeDeadline {
    pub fn install(deadline: Instant) -> Self {
        let previous = CURRENT.with(|cell| cell.replace(Some(deadline)));
        Self { previous }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn new(budget: Duration) -> Self {
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
pub fn current() -> Option<Instant> {
    current_work_deadline().or_else(|| CURRENT.with(Cell::get))
}

pub fn work_deadline(reserve: Duration) -> Option<Instant> {
    let hard = CURRENT.with(Cell::get)?;
    let limit = hard.checked_sub(reserve).unwrap_or_else(Instant::now);
    Some(current_work_deadline().map_or(limit, |outer| outer.min(limit)))
}

pub fn current_work_deadline() -> Option<Instant> {
    WORK.with(Cell::get)
}

pub struct ScopedWorkDeadline(Option<Instant>);

impl ScopedWorkDeadline {
    pub fn install(deadline: Instant) -> Self {
        Self(WORK.with(|cell| cell.replace(Some(deadline))))
    }
}

impl Drop for ScopedWorkDeadline {
    fn drop(&mut self) {
        WORK.with(|cell| cell.set(self.0));
    }
}

/// The remaining budget: `Some(Duration::ZERO)` once the deadline has passed, `None` when unarmed.
pub fn remaining() -> Option<Duration> {
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

    #[test]
    fn work_scope_shares_the_hard_deadline_and_restores_default_behavior() {
        let hard = Instant::now() + Duration::from_secs(5);
        let _hard = ScopedBridgeDeadline::install(hard);
        let reserve = Duration::from_millis(200);
        let work = hard - reserve;
        {
            let _work = ScopedWorkDeadline::install(work);
            assert_eq!(current(), Some(work));
            assert_eq!(work_deadline(Duration::ZERO), Some(work));
            assert_eq!(work_deadline(reserve), Some(work));
        }
        assert_eq!(current(), Some(hard));
        assert_eq!(current_work_deadline(), None);
    }
}
