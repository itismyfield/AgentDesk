use super::completion::{join_work, receive};
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
    let c = BootCohort::<usize>::install_in(&OnceLock::new(), roster).unwrap();
    let slot = BootSlot::begin(&c, "bot").unwrap();
    let outcome = slot
        .work_once()
        .unwrap()
        .run_once(|| panic!("worker failed"))
        .await;
    if let Ok(done) = &outcome {
        slot.arrive_reaped(done).unwrap();
    } else {
        slot.fail(outcome.as_ref().err().unwrap().clone());
    }
    let epoch = c.epoch();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sink = calls.clone();
    let started = c.try_start_confirmation(move |p, publication| {
        publication.publish_with(epoch, p, 7, || {
            sink.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
    });
    if started {
        c.wait_released().await.unwrap();
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(!c.snapshot().supervisors_released);
    assert!(matches!(outcome, Err(BootWorkFailure::Worker(_))));
    assert_eq!(c.snapshot().failed, 1);
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
    let once = BootWorkOnce::<usize>::new(1, "codex");
    let receiver = once.start(|| 9);
    let late = receiver.clone();
    let value = receive(receiver).await.unwrap();
    drop(once);
    let observed = receive(late).await.unwrap();
    assert_eq!(*observed.value(), 9);
    assert!(Arc::ptr_eq(&value, &observed));
}

#[tokio::test]
async fn completion_requires_normal_join_after_worker_latch() {
    let once = Arc::new(BootWorkOnce::<usize>::new(1, "codex"));
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let worker = once.clone();
    let job = tokio::spawn(async move {
        worker
            .run_once(move || {
                entered_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
                9
            })
            .await
    });
    entered_rx.recv().await.unwrap();
    let finished = job.is_finished();
    resume_tx.send(()).unwrap();
    assert!(!finished, "Completed must not exist before the worker join");
    assert_eq!(*job.await.unwrap().unwrap().value(), 9);
}

#[tokio::test]
async fn dropping_requester_does_not_restart_worker() {
    let once = BootWorkOnce::<usize>::new(1, "codex");
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let first = once.start(move || {
        entered_tx.send(()).unwrap();
        resume_rx.recv().unwrap();
        9
    });
    entered_rx.recv().await.unwrap();
    drop(first);
    let late = once.start(|| 999);
    resume_tx.send(()).unwrap();
    assert_eq!(*receive(late).await.unwrap().value(), 9);
}

#[test]
fn cancelled_worker_join_is_failure() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            entered_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let queued = tokio::task::spawn_blocking(|| 9_usize);
        queued.abort();
        resume_tx.send(()).unwrap();
        blocker.await.unwrap();
        let result = join_work((1, "codex".into()), queued).await;
        assert!(
            matches!(result, Err(BootWorkFailure::Worker(ref error)) if error.contains("cancelled"))
        );
    });
}
