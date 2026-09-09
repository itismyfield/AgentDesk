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
    original.finish(Outcome::Returned);
    assert_eq!(ticket.wait().await, Outcome::Returned);
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
