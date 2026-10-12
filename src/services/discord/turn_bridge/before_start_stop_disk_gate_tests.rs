//! The receipt's disk-row gate on its own: a fresh row of the very same episode, nonce and
//! generation, differing only in its planned-restart mode, gets no receipt.
use super::*;
use crate::services::discord::InflightRestartMode;

/// The bytes of the row saved at the receipt's boundary.
type Written = std::rc::Rc<std::cell::RefCell<Option<Vec<u8>>>>;

/// A row of the same birth episode saved over the turn's row after its seal; only `mode` differs.
fn same_episode(close: &Close, mode: Option<InflightRestartMode>) -> Written {
    let mut fresh = close.row.clone();
    fresh.restart_mode = mode;
    let slot = Written::default();
    let written = slot.clone();
    test_hooks::observe(Some(Box::new(move |step, _| {
        if step == "before_receipt" && written.borrow().is_none() {
            discord::inflight::save_inflight_state(&fresh).unwrap();
            let path = discord::inflight::inflight_state_path(
                &discord::inflight::inflight_runtime_root().unwrap(),
                &ProviderKind::Claude,
                fresh.channel_id,
            );
            *written.borrow_mut() = std::fs::read(path).ok();
        }
    })));
    slot
}

/// A planned restart's row keeps its bytes, gets no receipt and shows nothing, though its pin,
/// nonce and generation all match; the same row without the mode settles.
#[tokio::test(flavor = "current_thread")]
async fn coldstop_receipt_refuses_a_planned_restart_row_of_the_same_episode() {
    let close = Close::open(926, |_| {}).await;
    let written = same_episode(&close, Some(InflightRestartMode::DrainRestart));
    let submitted = test_hooks::submitted();
    let polled = std::sync::atomic::AtomicBool::new(false);
    let settled = close.consume(recorded(&polled)).await;
    test_hooks::observe(None);
    let durable = close.durable().unwrap();
    assert_eq!(
        durable.restart_mode,
        Some(InflightRestartMode::DrainRestart)
    );
    assert_eq!(durable.turn_nonce, close.row.turn_nonce);
    assert!(written.borrow().is_some());
    assert_eq!(close.bytes(), *written.borrow(), "the row keeps its bytes");
    assert!(!settled);
    assert!(!polled.load(Ordering::Acquire), "nothing shown");
    assert_eq!(close.receipt_pending(), None, "no receipt admitted");
    assert_eq!(test_hooks::submitted(), submitted);
    assert!(close.held().await);
    drop(close);

    let close = Close::open(927, |_| {}).await;
    let written = same_episode(&close, None);
    let settled = close.consume(recorded(&polled)).await;
    test_hooks::observe(None);
    assert!(written.borrow().is_some());
    assert!(settled, "the same row without a planned restart settles");
    assert!(polled.load(Ordering::Acquire));
    assert!(close.durable().is_none());
    assert_eq!(test_hooks::submitted(), submitted + 1);
}
