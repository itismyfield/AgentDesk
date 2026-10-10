use super::completion::receive;
use super::*;

#[tokio::test]
async fn worker_panic_is_not_default_success() {
    let roster = BootRoster::new(vec![BootBot {
        slot: "bot".into(),
        provider: "codex".into(),
        utility: false,
        selection: BootSelection {
            runtime_kind: "codex_tui".into(),
            turn_channels: [7].into(),
        },
    }])
    .unwrap();
    let c = BootCohort::install_in(&OnceLock::new(), roster).unwrap();
    let slot = BootSlot::begin(&c, "bot").unwrap();
    let outcome = BootWorkOnce::<usize>::default()
        .run_once(&slot, || panic!("worker failed"))
        .await;
    assert!(matches!(outcome, Err(BootWorkFailure::Worker(_))));
    slot.fail(outcome.err().unwrap());
    assert!(!c.try_start_confirmation(|_, _| Ok(())));
    assert_eq!(c.snapshot().failed, 1);
    assert!(!c.snapshot().supervisors_released);
    assert!(c.wait_released().await.is_err());
}

#[tokio::test]
async fn completion_closed_without_result_is_failure() {
    let (sender, receiver) = tokio::sync::watch::channel(None);
    drop(sender);
    assert!(matches!(
        receive::<usize>(receiver).await,
        Err(BootWorkFailure::Closed)
    ));
}

#[tokio::test]
async fn completed_value_survives_sender_close_and_late_waiter() {
    let once = BootWorkOnce::<usize>::default();
    let receiver = once.start(1, "codex", || 9);
    let late = receiver.clone();
    let value = receive(receiver).await.unwrap();
    drop(once);
    let observed = receive(late).await.unwrap();
    assert_eq!(*observed.value(), 9);
    assert!(Arc::ptr_eq(&value, &observed));
}

#[tokio::test]
async fn completion_requires_normal_join_after_worker_latch() {
    let once = BootWorkOnce::<usize>::default();
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let receiver = once.start(1, "codex", move || {
        entered_tx.send(()).unwrap();
        resume_rx.recv().unwrap();
        9
    });
    entered_rx.recv().await.unwrap();
    assert!(receiver.borrow().is_none());
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            receive(receiver.clone())
        )
        .await
        .is_err()
    );
    resume_tx.send(()).unwrap();
    assert_eq!(*receive(receiver).await.unwrap().value(), 9);
}

#[tokio::test]
async fn dropping_requester_does_not_restart_worker() {
    let once = BootWorkOnce::<usize>::default();
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let first = once.start(1, "codex", move || {
        entered_tx.send(()).unwrap();
        resume_rx.recv().unwrap();
        9
    });
    entered_rx.recv().await.unwrap();
    drop(first);
    let late = once.start(1, "codex", || 999);
    resume_tx.send(()).unwrap();
    assert_eq!(*receive(late).await.unwrap().value(), 9);
}
