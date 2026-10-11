use super::super::{Gate, Mode};
use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct PollFixture {
    ready: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
    waker: Arc<std::sync::Mutex<Option<std::task::Waker>>>,
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: Option<Arc<std::sync::Barrier>>,
    cleanup: Option<tokio::sync::oneshot::Sender<()>>,
}
impl Future for PollFixture {
    type Output = ();
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        assert_eq!(self.active.fetch_add(1, Ordering::SeqCst), 0);
        assert!(current().is_some());
        assert_eq!(super::super::require_worker(), Ok(()));
        let ready = self.ready.load(Ordering::SeqCst);
        *self.waker.lock().unwrap() = Some(cx.waker().clone());
        if let Some(entered) = self.entered.take() {
            entered.send(()).unwrap();
        }
        if let Some(release) = self.release.take() {
            release.wait();
        }
        assert_eq!(self.active.fetch_sub(1, Ordering::SeqCst), 1);
        if ready {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    }
}
impl Drop for PollFixture {
    fn drop(&mut self) {
        assert_eq!(self.active.load(Ordering::SeqCst), 0);
        assert!(current().is_some());
        assert_eq!(super::super::require_worker(), Ok(()));
        let _ = self.cleanup.take().unwrap().send(());
    }
}

async fn wake_fixture(channel: u64, during_poll: bool, abort: bool, run_transport: bool) {
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _health = super::super::test_health::Clear::new(&gate);
    let ready = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicUsize::new(0));
    let waker = Arc::new(std::sync::Mutex::new(None));
    let release = during_poll.then(|| Arc::new(std::sync::Barrier::new(2)));
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (cleanup_tx, cleanup_rx) = tokio::sync::oneshot::channel();
    let fixture = PollFixture {
        ready: ready.clone(),
        active: active.clone(),
        waker: waker.clone(),
        entered: Some(entered_tx),
        release: release.clone(),
        cleanup: Some(cleanup_tx),
    };
    let permit = Some(gate.admit().unwrap());
    let task = tokio::spawn(async move {
        if run_transport {
            run(permit, fixture).await;
        } else {
            detached(permit, fixture).await;
        }
    });
    entered_rx.await.unwrap();
    let closing = gate.close().unwrap();
    let drain = closing.drain();
    tokio::pin!(drain);
    assert!(futures::poll!(drain.as_mut()).is_pending());
    if !during_poll {
        // A one-thread blocking pool makes this probe follow the Pending poll.
        tokio::task::spawn_blocking(|| ()).await.unwrap();
    }
    ready.store(true, Ordering::SeqCst);
    waker.lock().unwrap().as_ref().unwrap().wake_by_ref();
    if abort {
        task.abort();
    }
    if let Some(release) = release {
        release.wait();
    }
    let result = task.await;
    if abort {
        assert!(result.unwrap_err().is_cancelled());
    } else {
        result.unwrap();
    }
    cleanup_rx.await.unwrap();
    drain.await;
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn c1_outer_abort_before_first_poll_disposes_capture_on_worker() {
    let gate = Gate::protect(ProviderKind::Claude, 6_325_411).unwrap();
    let _health = super::super::test_health::Clear::new(&gate);
    let (cleanup_tx, cleanup_rx) = tokio::sync::oneshot::channel();
    let fixture = PollFixture {
        ready: Arc::new(AtomicBool::new(false)),
        active: Arc::new(AtomicUsize::new(0)),
        waker: Arc::new(std::sync::Mutex::new(None)),
        entered: None,
        release: None,
        cleanup: Some(cleanup_tx),
    };
    let future = detached(Some(gate.admit().unwrap()), fixture);
    let closing = gate.close().unwrap();
    let drain = closing.drain();
    tokio::pin!(drain);
    assert!(futures::poll!(drain.as_mut()).is_pending());
    drop(future);
    cleanup_rx.await.unwrap();
    drain.await;
}

#[tokio::test]
async fn c1_worker_wake_before_pending_is_not_lost() {
    wake_fixture(6_325_408, true, false, false).await;
}

#[test]
fn c1_worker_wake_after_pending_returns_pool_thread() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(wake_fixture(6_325_409, false, false, false));
}

#[tokio::test]
async fn c1_worker_wake_and_outer_abort_cleanup_after_inflight_poll() {
    wake_fixture(6_325_410, true, true, false).await;
}

#[tokio::test]
async fn c1b_run_cancel_before_first_poll_disposes_capture_on_worker() {
    let gate = Gate::protect(ProviderKind::Claude, 6_325_516).unwrap();
    let _health = super::super::test_health::Clear::new(&gate);
    let (cleanup_tx, cleanup_rx) = tokio::sync::oneshot::channel();
    let fixture = PollFixture {
        ready: Arc::new(AtomicBool::new(false)),
        active: Arc::new(AtomicUsize::new(0)),
        waker: Arc::new(std::sync::Mutex::new(None)),
        entered: None,
        release: None,
        cleanup: Some(cleanup_tx),
    };
    let future = run(Some(gate.admit().unwrap()), fixture);
    let closing = gate.close().unwrap();
    let drain = closing.drain();
    tokio::pin!(drain);
    assert!(futures::poll!(drain.as_mut()).is_pending());
    drop(future);
    cleanup_rx.await.unwrap();
    drain.await;
}

#[tokio::test]
async fn c1b_run_cancel_during_poll_holds_effect_until_worker_cleanup() {
    wake_fixture(6_325_517, true, true, true).await;
}

#[test]
fn c1b_run_cancel_pending_returns_pool_thread_and_cleans_on_worker() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(wake_fixture(6_325_518, false, true, true));
}

#[tokio::test]
async fn c1_detached_worker_holds_effect_until_cleanup_after_caller_returns() {
    let gate = Gate::protect(ProviderKind::Claude, 6_325_452).unwrap();
    let _health = super::super::test_health::Clear::new(&gate);
    let permit = gate.admit().unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (clean_tx, clean_rx) = tokio::sync::oneshot::channel();
    struct Cleanup(Option<tokio::sync::oneshot::Sender<()>>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            assert!(super::current().is_some());
            assert_eq!(super::super::require_worker(), Ok(()));
            let _ = self.0.take().unwrap().send(());
        }
    }
    let task = tokio::spawn(detached(Some(permit.clone()), async move {
        let _cleanup = Cleanup(Some(clean_tx));
        assert_eq!(super::super::require_worker(), Ok(()));
        entered_tx.send(()).unwrap();
        release_rx.await.unwrap();
    }));
    entered_rx.await.unwrap();
    drop(permit);
    let closing = gate.close().unwrap();
    let drain = closing.drain();
    tokio::pin!(drain);
    assert!(futures::poll!(drain.as_mut()).is_pending());
    release_tx.send(()).unwrap();
    clean_rx.await.unwrap();
    task.await.unwrap();
    drain.await;
    assert!(matches!(gate.admit(), Err(Failure::Mode(Mode::Closing))));
}

#[tokio::test]
async fn c1b_detached_cross_channel_uses_root_admission_without_lending_capability() {
    let source = Gate::protect(ProviderKind::Codex, 6_325_453).unwrap();
    let destination = Gate::protect(ProviderKind::Codex, 6_325_454).unwrap();
    let _source_health = super::super::test_health::Clear::new(&source);
    let _destination_health = super::super::test_health::Clear::new(&destination);
    let permit = source.admit().unwrap();
    let source_closing = source.close().unwrap();
    let destination_closing = destination.close().unwrap();
    tokio::spawn(detached(Some(permit), async move {
        assert_eq!(super::super::require_worker(), Ok(()));
        let inherited = admit(&ProviderKind::Codex, 6_325_453).unwrap().unwrap();
        inherited.validate(&ProviderKind::Codex, 6_325_453).unwrap();
        assert!(matches!(
            inherited.validate(&ProviderKind::Codex, 6_325_403),
            Err(Failure::StalePermit)
        ));
        assert!(admit(&ProviderKind::Codex, 6_325_403).unwrap().is_none());
        // Another provider meets the owner's protection on this channel, never an unprotected None.
        assert!(matches!(
            admit(&ProviderKind::Claude, 6_325_453),
            Err(Failure::Mode(Mode::Closing))
        ));
        assert!(matches!(
            admit(&ProviderKind::Codex, 6_325_454),
            Err(Failure::Mode(Mode::Closing))
        ));
        let drain = source_closing.drain();
        tokio::pin!(drain);
        assert!(futures::poll!(drain.as_mut()).is_pending());
        drop(inherited);
    }))
    .await
    .unwrap();
    destination_closing.drain().await;
}

#[test]
fn c1_worker_panic_restores_capability_and_blocking_backstop() {
    let gate = Gate::protect(ProviderKind::Claude, 6_325_404).unwrap();
    let _health = super::super::test_health::Clear::new(&gate);
    let result = std::panic::catch_unwind(|| {
        synchronous(Some(gate.admit().unwrap()), || {
            assert!(current().is_some());
            panic!("worker panic fixture");
        })
    });
    assert!(result.is_err());
    assert!(current().is_none());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            assert_eq!(super::super::require_worker(), Err(Failure::Busy));
            gate.close().unwrap().drain().await;
        });
}

#[tokio::test]
async fn c1_detached_panic_preserves_payload_and_effectful_drop_off_and_protected() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    struct Dispose {
        channel: u64,
        path: std::path::PathBuf,
        protected: bool,
    }
    impl Drop for Dispose {
        fn drop(&mut self) {
            assert_eq!(current().is_some(), self.protected);
            if self.protected {
                assert_eq!(super::super::require_worker(), Ok(()));
            }
            super::super::write(&ProviderKind::Claude, self.channel, || {
                std::fs::write(&self.path, b"panic cleanup").map_err(|error| error.to_string())
            })
            .unwrap();
        }
    }
    for protected in [false, true] {
        let channel = if protected { 6_325_429 } else { 6_325_428 };
        let gate = protected.then(|| Gate::protect(ProviderKind::Claude, channel).unwrap());
        let _health = gate.as_ref().map(super::super::test_health::Clear::new);
        let permit = gate.as_ref().map(|gate| gate.admit().unwrap());
        let path = root.path().join(channel.to_string());
        let dispose = Dispose {
            channel,
            path: path.clone(),
            protected,
        };
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(detached(permit, async move {
            let _dispose = dispose;
            entered_tx.send(()).unwrap();
            release_rx.await.unwrap();
            std::panic::panic_any(6325_u64);
        }));
        entered_rx.await.unwrap();
        let closing = gate.as_ref().map(|gate| gate.close().unwrap());
        if let Some(closing) = &closing {
            assert!(futures::FutureExt::now_or_never(closing.drain()).is_none());
        }
        release_tx.send(()).unwrap();
        let error = task.await.unwrap_err();
        assert!(error.is_panic());
        assert_eq!(*error.into_panic().downcast::<u64>().unwrap(), 6325);
        assert_eq!(std::fs::read(path).unwrap(), b"panic cleanup");
        if let Some(closing) = closing {
            closing.drain().await;
        }
        assert!(current().is_none());
    }
}

#[test]
fn c1_shutdown_disposes_queued_and_rejected_cleanup_with_preclose_effect() {
    use futures::FutureExt;
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    thread_local! {
        static THREAD_EXIT: RefCell<Option<std::sync::mpsc::Sender<(bool, bool, bool)>>> = const { RefCell::new(None) };
    }
    struct Dispose {
        channel: u64,
        path: std::path::PathBuf,
        report: std::sync::mpsc::Sender<(bool, bool, bool)>,
    }
    impl Drop for Dispose {
        fn drop(&mut self) {
            let inherited = current().is_some();
            let worker = super::super::require_worker().is_ok();
            let written = super::super::write(&ProviderKind::Claude, self.channel, || {
                std::fs::write(&self.path, b"shutdown cleanup").map_err(|e| e.to_string())
            })
            .is_ok();
            THREAD_EXIT.with(|slot| *slot.borrow_mut() = Some(self.report.clone()));
            let _ = self.report.send((inherited, worker, written));
        }
    }
    for rejected in [false, true] {
        let channel = if rejected { 6_325_438 } else { 6_325_437 };
        let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
        let _health = super::super::test_health::Clear::new(&gate);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let (blocked_tx, blocked_rx) = std::sync::mpsc::channel();
        let (unblock_tx, unblock_rx) = std::sync::mpsc::channel();
        handle.spawn_blocking(move || {
            blocked_tx.send(()).unwrap();
            unblock_rx.recv().unwrap();
        });
        blocked_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let (report_tx, report_rx) = std::sync::mpsc::channel();
        let path = root.path().join(channel.to_string());
        let dispose = Dispose {
            channel,
            path: path.clone(),
            report: report_tx,
        };
        let permit = gate.admit().unwrap();
        let closing = gate.close().unwrap();
        if rejected {
            runtime.shutdown_background();
            let _entered = handle.enter();
            drop(detached(Some(permit), async move {
                let _dispose = dispose;
                std::future::pending::<()>().await;
            }));
        } else {
            runtime.block_on(async {
                let task = tokio::spawn(detached(Some(permit), async move {
                    let _dispose = dispose;
                    std::future::pending::<()>().await;
                }));
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
                assert!(closing.drain().now_or_never().is_none());
                assert!(report_rx.try_recv().is_err());
            });
            runtime.shutdown_background();
        }
        unblock_tx.send(()).unwrap();
        assert_eq!(
            report_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap(),
            (true, true, true),
            "cleanup must retain its scope through Drop"
        );
        assert_eq!(std::fs::read(path).unwrap(), b"shutdown cleanup");
        let observer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        observer.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(10), closing.drain())
                .await
                .unwrap();
        });
        assert!(matches!(
            report_rx.recv_timeout(std::time::Duration::from_secs(10)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
    }
}

#[tokio::test]
async fn c1_async_accessory_worker_consumes_preclose_permit_and_keeps_scheduler_backstop() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let channel = 6_325_405;
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _health = super::super::test_health::Clear::new(&gate);
    scope(Some(gate.admit().unwrap()), async {
        let _closing = gate.close().unwrap();
        assert!(super::super::write(&ProviderKind::Claude, channel, || Ok(())).is_err());
        let path = root.path().join("written");
        let saved = path.clone();
        io(move || {
            super::super::write(&ProviderKind::Claude, channel, || {
                std::fs::write(path, b"existing effect").map_err(|e| e.to_string())
            })
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read(saved).unwrap(), b"existing effect");
    })
    .await;
}

#[tokio::test]
async fn c1_io_worker_preserves_original_panic_payload() {
    use futures::FutureExt;
    let panic = std::panic::AssertUnwindSafe(io(|| std::panic::panic_any(6325464u64)))
        .catch_unwind()
        .await
        .unwrap_err();
    assert_eq!(panic.downcast_ref::<u64>(), Some(&6325464));
}

struct IdentityDrop {
    channel: Option<u64>,
    dropped: Arc<AtomicBool>,
}
impl Future for IdentityDrop {
    type Output = ();
    fn poll(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        match self.channel {
            Some(channel) => assert!(current().unwrap().names(&ProviderKind::Codex, channel)),
            None => assert!(current().is_none()),
        }
        std::task::Poll::Pending
    }
}
impl Drop for IdentityDrop {
    fn drop(&mut self) {
        match self.channel {
            Some(channel) => assert!(current().unwrap().names(&ProviderKind::Codex, channel)),
            None => assert!(current().is_none()),
        }
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn c1b_nested_identity_scope_restores_worker_on_poll_and_cancel() {
    let source = Gate::protect(ProviderKind::Codex, 6_325_455).unwrap();
    let destination = Gate::protect(ProviderKind::Codex, 6_325_456).unwrap();
    let _source_health = super::super::test_health::Clear::new(&source);
    let _destination_health = super::super::test_health::Clear::new(&destination);
    let permit = source.admit().unwrap();
    let source_closing = source.close().unwrap();
    detached(Some(permit), async move {
        for channel in [None, Some(6_325_456)] {
            let permit =
                channel.map(|channel| admit(&ProviderKind::Codex, channel).unwrap().unwrap());
            let dropped = Arc::new(AtomicBool::new(false));
            let mut future = Box::pin(scope(
                permit,
                IdentityDrop {
                    channel,
                    dropped: dropped.clone(),
                },
            ));
            assert!(futures::poll!(future.as_mut()).is_pending());
            assert!(current().unwrap().names(&ProviderKind::Codex, 6_325_455));
            drop(future);
            assert!(dropped.load(Ordering::SeqCst));
            assert!(current().unwrap().names(&ProviderKind::Codex, 6_325_455));
        }
        scope(None, async {
            assert!(current().is_none());
            run(None, async {
                assert!(current().is_none());
                17
            })
            .await
        })
        .await;
        assert!(current().unwrap().names(&ProviderKind::Codex, 6_325_455));
    })
    .await;
    source_closing.drain().await;
    destination.close().unwrap().drain().await;
}

#[tokio::test]
async fn c1b_same_identity_stale_epoch_is_not_replaced_by_root_admission() {
    let gate = Gate::protect(ProviderKind::Codex, 6_325_536).unwrap();
    let _health = super::super::test_health::Clear::new(&gate);
    let permit = gate.admit().unwrap();
    gate.state.lock().unwrap().epoch += 1;
    scope(Some(permit), async {
        assert!(matches!(
            admit(&ProviderKind::Codex, 6_325_536),
            Err(Failure::StalePermit)
        ));
        assert_eq!(gate.state.lock().unwrap().effects, 1);
    })
    .await;
    gate.close().unwrap().drain().await;
}

#[tokio::test]
async fn c1b_off_root_run_does_not_create_registration_or_worker_transport() {
    assert!(current().is_none());
    assert!(super::super::lookup(&ProviderKind::Codex, 6_325_537).is_none());
    let permit = admit(&ProviderKind::Codex, 6_325_537).unwrap();
    assert!(permit.is_none());
    assert_eq!(
        run(permit, async {
            assert!(current().is_none());
            assert!(super::super::require_worker().is_err());
            17
        })
        .await,
        17
    );
    assert!(super::super::lookup(&ProviderKind::Codex, 6_325_537).is_none());
}

#[tokio::test]
async fn c1b_explicit_empty_worker_scope_masks_task_capability() {
    let gate = Gate::protect(ProviderKind::Codex, 6_325_457).unwrap();
    let _health = super::super::test_health::Clear::new(&gate);
    TASK.scope(Some(gate.admit().unwrap()), async {
        synchronous(None, || assert!(current().is_none()));
        assert!(current().unwrap().names(&ProviderKind::Codex, 6_325_457));
    })
    .await;
    assert!(current().is_none());
    gate.close().unwrap().drain().await;
}
