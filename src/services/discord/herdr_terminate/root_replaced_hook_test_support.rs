use super::super::terminate_hook_tests::ProbeSlot;
use std::cell::{Cell, RefCell};

thread_local! {
    pub(crate) static AFTER_FINAL_PROOF: ProbeSlot = const { RefCell::new(None) };
    pub(crate) static CAS_ATTEMPTS: Cell<usize> = const { Cell::new(0) };
}
