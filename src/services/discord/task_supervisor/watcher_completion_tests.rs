use super::*;
use crate::services::discord::make_shared_data_for_tests;
fn spawn(
    cancel: Arc<AtomicBool>,
    future: impl Future<Output = ()> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    spawn_observed_tmux_watcher(
        "completion-test",
        make_shared_data_for_tests(),
        "completion-test".into(),
        cancel,
        future,
    )
}
#[tokio::test]
async fn whole_future_two_barriers_precede_completion() {
    let cancel = Arc::new(AtomicBool::new(false));
    let (first, rx1) = tokio::sync::oneshot::channel();
    let (second, rx2) = tokio::sync::oneshot::channel();
    let task = spawn(cancel.clone(), async {
        rx1.await.unwrap();
        rx2.await.unwrap();
    });
    let wait = observe(&cancel).unwrap().wait();
    tokio::pin!(wait);
    cancel.store(true, std::sync::atomic::Ordering::Release);
    assert!(futures::poll!(&mut wait).is_pending());
    first.send(()).unwrap();
    tokio::task::yield_now().await;
    assert!(futures::poll!(&mut wait).is_pending());
    second.send(()).unwrap();
    assert_eq!(wait.await, Outcome::Returned);
    task.await.unwrap();
    assert!(observe(&cancel).is_none());
}
#[tokio::test]
async fn panic_and_abort_are_distinct_observations() {
    for before_poll in [true, false] {
        let cancel = Arc::new(AtomicBool::new(false));
        let (entered, started) = tokio::sync::oneshot::channel();
        let task = spawn(cancel.clone(), async {
            let _ = entered.send(());
            std::future::pending::<()>().await;
        });
        let ticket = observe(&cancel).unwrap();
        if !before_poll {
            started.await.unwrap();
        }
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(ticket.wait().await, Outcome::Unknown);
        assert!(observe(&cancel).is_none());
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let task = spawn(cancel.clone(), async {
        panic!("completion probe");
    });
    let ticket = observe(&cancel).unwrap();
    task.await.unwrap();
    assert_eq!(ticket.wait().await, Outcome::Panicked);
}
#[tokio::test]
async fn duplicate_guard_does_not_remove_original_registration() {
    let cancel = Arc::new(AtomicBool::new(false));
    let original = Registration::new(cancel.clone());
    let ticket = observe(&cancel).unwrap();
    let duplicate = Registration::new(cancel.clone());
    assert_eq!(*duplicate.sender.borrow(), Outcome::Unknown);
    drop(duplicate);
    assert!(observe(&cancel).is_some());
    let duplicate = Registration::new(cancel.clone());
    let later_ticket = observe(&cancel).unwrap();
    original.finish(Outcome::Returned);
    // The second registration is still live: neither observer may see Returned.
    assert_eq!(ticket.wait().await, Outcome::Unknown);
    assert_eq!(later_ticket.wait().await, Outcome::Unknown);
    drop(duplicate);
    assert!(observe(&cancel).is_none());
}
#[tokio::test]
async fn old_completion_preserves_other_incarnation() {
    let old = Arc::new(AtomicBool::new(false));
    let new = Arc::new(AtomicBool::new(false));
    let old_registration = Registration::new(old.clone());
    let new_registration = Registration::new(new.clone());
    let new_ticket = observe(&new).unwrap();
    old_registration.finish(Outcome::Returned);
    assert!(observe(&old).is_none());
    assert!(observe(&new).is_some());
    new_registration.finish(Outcome::Returned);
    assert_eq!(new_ticket.wait().await, Outcome::Returned);
}

struct PausedReaderDrop {
    entered: Option<oneshot::Sender<()>>,
    release: std::sync::mpsc::Receiver<()>,
}
impl Future for PausedReaderDrop {
    type Output = ();
    fn poll(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        std::task::Poll::Pending
    }
}
impl Drop for PausedReaderDrop {
    fn drop(&mut self) {
        self.entered.take().unwrap().send(()).unwrap();
        self.release
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiesce_ack_waits_for_reader_drop_and_registry_cleanup() {
    use crate::services::discord::tmux_watcher_registry::lock_tmux_watcher_registry;
    use std::time::Duration;
    let cancel = Arc::new(AtomicBool::new(false));
    let (entered, dropping) = oneshot::channel();
    let (release, resume_drop) = std::sync::mpsc::channel();
    let task = spawn(
        cancel.clone(),
        PausedReaderDrop {
            entered: Some(entered),
            release: resume_drop,
        },
    );
    let (locked, ready) = oneshot::channel();
    let (unlock, release_lock) = std::sync::mpsc::channel();
    let cleanup = std::thread::spawn(move || {
        let _registry = lock_tmux_watcher_registry();
        locked.send(()).unwrap();
        release_lock.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    ready.await.unwrap();
    let request = quiesce(&cancel);
    tokio::pin!(request);
    assert!(futures::poll!(&mut request).is_pending());
    dropping.await.unwrap();
    let reader_early = tokio::time::timeout(Duration::from_millis(100), &mut request).await;
    release.send(()).unwrap();
    let cleanup_early = if reader_early.is_err() {
        tokio::time::timeout(Duration::from_millis(100), &mut request).await
    } else {
        reader_early
    };
    unlock.send(()).unwrap();
    cleanup.join().unwrap();
    assert!(
        cleanup_early.is_err(),
        "ACK preceded reader or cleanup Drop"
    );
    assert_eq!(request.await, Ok(()));
    task.await.unwrap();
    assert!(!cancel.load(std::sync::atomic::Ordering::Acquire));
    assert_eq!(quiesce(&cancel).await, Err(()));
}
#[tokio::test]
async fn quiesce_rejects_duplicates_even_after_original_retirement() {
    let cancel = Arc::new(AtomicBool::new(false));
    let original = Registration::new(cancel.clone());
    let duplicate = Registration::new(cancel.clone());
    assert_eq!(quiesce(&cancel).await, Err(()));
    original.finish(Outcome::Returned);
    assert!(observe(&cancel).is_none());
    let third = Registration::new(cancel.clone());
    let ticket = observe(&cancel).unwrap();
    assert_eq!(*ticket.receiver.borrow(), Outcome::Pending);
    assert_eq!(quiesce(&cancel).await, Err(()));
    drop(duplicate);
    assert_eq!(quiesce(&cancel).await, Err(()));
    third.finish(Outcome::Returned);
    assert_eq!(ticket.wait().await, Outcome::Returned);
    assert!(observe(&cancel).is_none());
    assert!(!RECORDS.lock().unwrap().contains_key(&key(&cancel)));
}
#[tokio::test]
async fn quiesce_rechecks_duplicates_and_rejects_missing_or_aborted_requests() {
    let cancel = Arc::new(AtomicBool::new(false));
    assert_eq!(quiesce(&cancel).await, Err(()));
    for duplicate in [false, true] {
        let mut registration = Registration::new(cancel.clone());
        let request = quiesce(&cancel);
        tokio::pin!(request);
        assert!(futures::poll!(&mut request).is_pending());
        assert_eq!(quiesce(&cancel).await, Err(()));
        if duplicate {
            registration.quiesce_requested().await;
            let duplicate = Registration::new(cancel.clone());
            registration.finish(Outcome::Unknown);
            assert_eq!(request.await, Err(()));
            drop(duplicate);
        } else {
            drop(registration);
            assert_eq!(request.await, Err(()));
        }
    }
}

#[tokio::test]
async fn aborted_later_observation_closes_with_a_duplicate_still_alive() {
    let cancel = Arc::new(AtomicBool::new(false));
    let original = Registration::new(cancel.clone());
    let duplicate = Registration::new(cancel.clone());
    original.finish(Outcome::Returned);
    let later = Registration::new(cancel.clone());
    let ticket = observe(&cancel).unwrap();
    drop(later);
    assert_eq!(ticket.wait().await, Outcome::Unknown);
    assert!(observe(&cancel).is_none());
    drop(duplicate);
}
