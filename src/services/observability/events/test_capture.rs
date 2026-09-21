use super::StructuredEvent;
use std::cell::RefCell;
use std::future::Future;

tokio::task_local! { static CAPTURE: RefCell<Vec<StructuredEvent>>; }

pub(crate) fn capture_sync<R>(action: impl FnOnce() -> R) -> (R, Vec<StructuredEvent>) {
    CAPTURE.sync_scope(RefCell::new(Vec::new()), || {
        let result = action();
        (result, CAPTURE.with(|rows| rows.take()))
    })
}

pub(crate) async fn capture_async<R>(action: impl Future<Output = R>) -> (R, Vec<StructuredEvent>) {
    CAPTURE
        .scope(RefCell::new(Vec::new()), async {
            let result = action.await;
            (result, CAPTURE.with(|rows| rows.take()))
        })
        .await
}

pub(crate) fn snapshot() -> Vec<StructuredEvent> {
    CAPTURE.with(|rows| rows.borrow().clone())
}

pub(crate) fn one(event_type: &str) -> StructuredEvent {
    let mut rows: Vec<_> = snapshot()
        .into_iter()
        .filter(|row| row.event_type == event_type)
        .collect();
    assert_eq!(rows.len(), 1, "expected exactly one owned {event_type}");
    rows.remove(0)
}

pub(super) fn record(event: &StructuredEvent) {
    let _ = CAPTURE.try_with(|rows| rows.borrow_mut().push(event.clone()));
}
