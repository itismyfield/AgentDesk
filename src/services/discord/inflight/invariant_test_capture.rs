//! Thread-local test witness for inflight invariant emissions.
//!
//! This deliberately observes the typed severity selected by the inflight
//! save path rather than using formatted tracing as a correctness witness.
//! Parallel subscriber/callsite state made that witness flaky in #4422's macOS
//! CI runs. The module is compiled only for tests.

use std::cell::RefCell;

use super::ObsSeverity;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CapturedInvariant {
    pub(super) invariant: &'static str,
    pub(super) severity: ObsSeverity,
}

thread_local! {
    static CAPTURE: RefCell<Option<Vec<CapturedInvariant>>> = const { RefCell::new(None) };
}

struct CaptureGuard {
    active: bool,
}

impl CaptureGuard {
    fn begin() -> Self {
        CAPTURE.with(|slot| {
            let previous = slot.borrow_mut().replace(Vec::new());
            assert!(previous.is_none(), "nested inflight invariant test capture");
        });
        Self { active: true }
    }

    fn finish(mut self) -> Vec<CapturedInvariant> {
        self.active = false;
        CAPTURE.with(|slot| slot.borrow_mut().take().unwrap_or_default())
    }
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        if self.active {
            CAPTURE.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
}

pub(super) fn record(invariant: &'static str, severity: ObsSeverity) {
    CAPTURE.with(|slot| {
        if let Some(events) = slot.borrow_mut().as_mut() {
            events.push(CapturedInvariant {
                invariant,
                severity,
            });
        }
    });
}

pub(super) fn capture<T>(run: impl FnOnce() -> T) -> (T, Vec<CapturedInvariant>) {
    let guard = CaptureGuard::begin();
    let result = run();
    (result, guard.finish())
}
