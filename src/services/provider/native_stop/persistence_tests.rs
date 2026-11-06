use super::*;
use std::sync::atomic::AtomicUsize;

fn pin() -> NativeUserPin {
    NativeUserPin {
        uuid: "user".into(),
        source_file: (1, 2),
        range: (0, 10),
        receipt_start: 0,
        row_digest: "row".into(),
        frame_digest: "frame".into(),
        session_id: "sid".into(),
        output_path: "transcript".into(),
        tmux_session: "session".into(),
        spawn_nonce: Some("pane".into()),
        direct: true,
    }
}

#[test]
fn native_pin_persistence_backoff_keeps_a_later_save_opportunity() {
    let sink = ReceiptSink::default();
    sink.adopt(pin());
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = calls.clone();
    sink.on_pin(Arc::new(move |_| {
        if callback_calls.fetch_add(1, Ordering::SeqCst) < 2 {
            PinSaveOutcome::Retry
        } else {
            PinSaveOutcome::Saved
        }
    }));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(sink.persistence.lock().unwrap().retry_after.is_some());
    sink.persistence.lock().unwrap().retry_after = Some(Instant::now() + Duration::from_secs(3600));
    for _ in 0..1000 {
        assert!(sink.observe().unwrap().is_ok());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    sink.retry_persistence();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(sink.persistence.lock().unwrap().retry_after.is_some());
    sink.persistence.lock().unwrap().retry_after = Some(Instant::now() - Duration::from_secs(1));
    assert!(sink.observe().unwrap().is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    sink.retry_persistence();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[test]
fn native_pin_persistence_never_is_terminal_even_for_a_direct_retry() {
    let sink = ReceiptSink::default();
    sink.adopt(pin());
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = calls.clone();
    sink.on_pin(Arc::new(move |_| {
        callback_calls.fetch_add(1, Ordering::SeqCst);
        PinSaveOutcome::Never
    }));
    for _ in 0..1000 {
        assert!(sink.observe().unwrap().is_ok());
        sink.retry_persistence();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
