use crate::services::discord::inflight::InflightTurnState;
use std::cell::{Cell, RefCell};

type Observer = Box<dyn FnMut(&'static str, &InflightTurnState)>;

thread_local! {
    static OBSERVER: RefCell<Option<Observer>> = const { RefCell::new(None) };
    static SUBMITTED: Cell<usize> = const { Cell::new(0) };
}

pub(in crate::services::discord) const STOP_AT: &str = "ADK_COLDSTOP_B_STOP_AT";

#[cfg(unix)]
pub(in crate::services::discord) fn observe(observer: Option<Observer>) {
    OBSERVER.with(|slot| *slot.borrow_mut() = observer);
}

/// Policy submissions `consume` made on this thread.
#[cfg(unix)]
pub(in crate::services::discord) fn submitted() -> usize {
    SUBMITTED.with(Cell::get)
}

/// Counts one call to the finalizer's policy submit API, right before it is made.
pub(super) fn submitting() {
    SUBMITTED.with(|count| count.set(count.get() + 1));
}

pub(super) fn boundary(step: &'static str, row: &InflightTurnState) {
    if std::env::var(STOP_AT).ok().as_deref() == Some(step) {
        println!(
            "COLDSTOP_B_STOP {step} channel={} pid={}",
            row.channel_id,
            std::process::id()
        );
        std::process::exit(0);
    }
    OBSERVER.with(|slot| {
        if let Some(observer) = slot.borrow_mut().as_mut() {
            observer(step, row);
        }
    });
}
