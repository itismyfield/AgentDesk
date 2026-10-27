use std::future::Future;

// Acquire fixture serialization before creating its executor so contention cannot block runtime progress.
// Reject the entire runtime context, including spawn_blocking threads, before acquiring the lock.
pub(crate) fn run<G, F: Future>(acquire: impl FnOnce() -> G, future: F) -> F::Output {
    assert!(
        tokio::runtime::Handle::try_current().is_err(),
        "serialized async fixtures must start outside a Tokio runtime"
    );
    let _guard = acquire();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(future)
}

#[test]
fn serialization_is_held_through_suspension_and_released_on_return() {
    let lock = std::sync::Arc::new(std::sync::Mutex::new(()));
    let competing_lock = lock.clone();
    let (attempted, waiting) = tokio::sync::oneshot::channel();
    let (entered, events) = std::sync::mpsc::channel();
    let contender = run(|| lock.lock().unwrap(), async {
        let contender = std::thread::spawn(move || {
            run(
                || {
                    attempted.send(()).unwrap();
                    competing_lock.lock().unwrap()
                },
                async { entered.send(()).unwrap() },
            );
        });
        waiting.await.unwrap();
        tokio::task::yield_now().await;
        assert!(matches!(
            lock.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
        assert!(matches!(
            events.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        contender
    });
    contender.join().unwrap();
    events.recv().unwrap();
    assert!(lock.try_lock().is_ok());
}

#[test]
fn nested_runtime_is_rejected_before_trying_the_fixture_lock() {
    let acquired = std::cell::Cell::new(false);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async {
            run(|| acquired.set(true), async {});
        });
    }));
    assert!(result.is_err());
    assert!(!acquired.get(), "nested fixtures must fail before locking");
}
