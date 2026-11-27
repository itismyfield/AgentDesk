//! Test probes the forced Herdr branches run before picking an execution and before releasing.

/// A one-shot async probe a test installs on this thread.
pub(crate) type SettlementWindowProbe =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send>;
type ProbeSlot = std::cell::RefCell<Option<SettlementWindowProbe>>;
thread_local! {
    static BEFORE_SELECTION: ProbeSlot = const { std::cell::RefCell::new(None) };
    static BEFORE_RELEASED: ProbeSlot = const { std::cell::RefCell::new(None) };
}
/// Runs once when a forced HTTP branch is about to pick its execution, after the request's own
/// session read.
pub(crate) fn probe_before_selection(probe: SettlementWindowProbe) {
    BEFORE_SELECTION.with(|slot| *slot.borrow_mut() = Some(probe));
}
pub(crate) async fn run_before_selection_probe() {
    run_probe(&BEFORE_SELECTION).await;
}
/// Runs once when a postlude's released step starts, after the exclusion was dropped.
pub(crate) fn probe_before_released(probe: SettlementWindowProbe) {
    BEFORE_RELEASED.with(|slot| *slot.borrow_mut() = Some(probe));
}
pub(crate) async fn run_before_released_probe() {
    run_probe(&BEFORE_RELEASED).await;
}
async fn run_probe(slot: &'static std::thread::LocalKey<ProbeSlot>) {
    if let Some(probe) = slot.with(|slot| slot.borrow_mut().take()) {
        probe().await;
    }
}
