//! One-shot probes at the Herdr termination boundaries.

use crate::services::dispatched_sessions::herdr_probe_tests::SettlementWindowProbe;

pub(super) type ProbeSlot = std::cell::RefCell<Option<SettlementWindowProbe>>;
thread_local! {
    pub(super) static SETTLEMENT_WINDOW: ProbeSlot = const { std::cell::RefCell::new(None) };
    pub(super) static AFTER_SETTLEMENT: ProbeSlot = const { std::cell::RefCell::new(None) };
    pub(super) static AFTER_CAPTURE: ProbeSlot = const { std::cell::RefCell::new(None) };
    pub(super) static AFTER_FENCE: ProbeSlot = const { std::cell::RefCell::new(None) };
    pub(super) static AFTER_CLOSE: ProbeSlot = const { std::cell::RefCell::new(None) };
    pub(super) static BEFORE_POSTLUDE: ProbeSlot = const { std::cell::RefCell::new(None) };
}
/// Runs once between the hold release and the captured-turn settlement on this thread.
pub(crate) fn probe_settlement_window(probe: SettlementWindowProbe) {
    SETTLEMENT_WINDOW.with(|slot| *slot.borrow_mut() = Some(probe));
}
/// Runs once after the captured-turn settlement, while the exclusion is still held.
pub(crate) fn probe_after_settlement(probe: SettlementWindowProbe) {
    AFTER_SETTLEMENT.with(|slot| *slot.borrow_mut() = Some(probe));
}
/// Runs once after the turn is captured and before the transition is taken.
pub(crate) fn probe_after_capture(probe: SettlementWindowProbe) {
    AFTER_CAPTURE.with(|slot| *slot.borrow_mut() = Some(probe));
}
/// Runs once right after the termination fence is taken, before any close.
pub(crate) fn probe_after_fence(probe: SettlementWindowProbe) {
    AFTER_FENCE.with(|slot| *slot.borrow_mut() = Some(probe));
}
/// Runs once after the close returned, or after the pane read as already ended.
pub(crate) fn probe_after_close(probe: SettlementWindowProbe) {
    AFTER_CLOSE.with(|slot| *slot.borrow_mut() = Some(probe));
}
/// Runs once when a scoped run hands its exclusion to an HTTP postlude, before the postlude.
pub(crate) fn probe_before_postlude(probe: SettlementWindowProbe) {
    BEFORE_POSTLUDE.with(|slot| *slot.borrow_mut() = Some(probe));
}
pub(super) async fn run_probe(slot: &'static std::thread::LocalKey<ProbeSlot>) {
    if let Some(probe) = slot.with(|slot| slot.borrow_mut().take()) {
        probe().await;
    }
}
