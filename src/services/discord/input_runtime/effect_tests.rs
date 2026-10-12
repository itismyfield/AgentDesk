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

#[test]
fn c1_detached_panic_preserves_payload_and_effectful_drop_off_and_protected() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
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
                        std::fs::write(&self.path, b"panic cleanup")
                            .map_err(|error| error.to_string())
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
        });
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

#[test]
fn c1_async_accessory_worker_consumes_preclose_permit_and_keeps_scheduler_backstop() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
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
        });
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

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod g1b_off {
    type OutboxRow = (String, String, String, String, String, String, Option<bool>);
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::discord::input_runtime::{fence, supervisor};
    use crate::services::discord::router::message_handler::IntakeDeps;
    use crate::services::discord::router::{IntakeOrigin, dispatch_skill_intake};
    use crate::services::provider::ProviderKind;
    use crate::services::tui_input::ledger::{Ledger, OPENS, Presence};
    use poise::serenity_prelude::{ChannelId, Http, MessageId, UserId};
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    const TEST: &str = "g1b_off_entrypoint_preserves_legacy_trace_pg";
    const CHILD: &str = "ADK_G1B_OFF_CHILD";
    const TMUX_LOG: &str = "ADK_G1B_OFF_TMUX_LOG";
    const CHANNEL: u64 = 6_325_710;
    const MESSAGE: u64 = 6_325_711;
    const OWNER: &str = "g1b-off-foreign-owner";
    const TEXT: &str = "off intake keeps the existing foreign route";

    fn child() -> bool {
        if std::env::var(CHILD).as_deref() == Ok(TEST) {
            return true;
        }
        let sandbox = tempfile::tempdir().unwrap();
        let runtime = sandbox.path().join("runtime");
        let temporary = sandbox.path().join("tmp");
        let bin = sandbox.path().join("bin");
        for path in [&runtime, &temporary, &bin] {
            std::fs::create_dir(path).unwrap();
        }
        let stub = bin.join("tmux");
        std::fs::write(
            &stub,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$ADK_G1B_OFF_TMUX_LOG\"\nexit 97\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
        let qualified = format!("{}::{TEST}", module_path!().split_once("::").unwrap().1);
        let mut process = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &qualified, "--nocapture"])
            .env(CHILD, TEST)
            .env("AGENTDESK_ROOT_DIR", &runtime)
            .env("AGENTDESK_CONFIG", sandbox.path().join("unused-config"))
            .env("TMPDIR", &temporary)
            .env(TMUX_LOG, sandbox.path().join("tmux-calls"))
            .env("ADK_INTAKE_ROUTING_MODE", "enforce")
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("ALL_PROXY", "http://127.0.0.1:9")
            .env("NO_PROXY", "")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(120);
        while process.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                process.kill().unwrap();
                let _ = process.wait();
                panic!("off intake child exceeded its deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = process.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
        assert!(
            stdout
                .lines()
                .any(|line| line.starts_with("G1B_OFF_TRACE="))
        );
        print!("{stdout}");
        false
    }

    async fn seed(pool: &sqlx::PgPool) {
        let channel = CHANNEL.to_string();
        let agent = format!("g1b-off-{CHANNEL}");
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id,
             preferred_intake_node_labels) VALUES ($1, 'Off input', 'claude', $2, '[]'::jsonb)",
        )
        .bind(&agent)
        .bind(&channel)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO worker_nodes (instance_id, status, role, effective_role,
             labels, capabilities, last_heartbeat_at, started_at, updated_at)
             VALUES ($1, 'online', 'worker', 'worker', '[]'::jsonb,
             '{\"intake_worker\":{\"enabled\":true,\"providers\":[\"claude\"],
             \"features\":[\"preserve_on_cancel_v1\"]}}'::jsonb, NOW(), NOW(), NOW())",
        )
        .bind(OWNER)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sessions (session_key, agent_id, provider, channel_id,
             instance_id, status, last_heartbeat)
             VALUES ($1, $2, 'claude', $3, $4, 'idle', NOW())",
        )
        .bind(format!("claude-{CHANNEL}"))
        .bind(&agent)
        .bind(&channel)
        .bind(OWNER)
        .execute(pool)
        .await
        .unwrap();
    }

    fn resources(root: &std::path::Path, workers: &AtomicUsize) {
        assert!(!supervisor::REGISTRY.used());
        assert_eq!(supervisor::REGISTRY.population_for_test(), (0, 0));
        assert_eq!(supervisor::ordering::handback_count_for_test(), 0);
        assert_eq!(supervisor::ordering::handback_callback_count_for_test(), 0);
        assert!(super::super::super::GATES.get().is_none());
        assert!(fence::lookup(&ProviderKind::Claude, CHANNEL).is_none());
        assert!(!supervisor::ordering::order_barrier(
            &ProviderKind::Claude,
            CHANNEL
        ));
        assert_eq!(OPENS.with(|opens| opens.get()), 0);
        assert_eq!(workers.load(Ordering::SeqCst), 0);
        assert_eq!(Ledger::probe(root, CHANNEL), Presence::Absent);
        assert!(std::fs::read_dir(root).unwrap().next().is_none());
        assert!(super::current().is_none());
        assert!(fence::require_worker().is_err());
    }

    #[test]
    fn g1b_off_entrypoint_preserves_legacy_trace_pg() {
        if !child() {
            return;
        }
        let config = serde_json::from_value(serde_json::json!({
            "server": {},
            "cluster": {"enabled": true, "instance_id": "g1b-off-gateway"},
        }))
        .unwrap();
        crate::config_live_reload::install(config);
        let setup = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let database = setup.block_on(async {
            let database = TestPostgresDb::create().await;
            let pool = database.connect_and_migrate().await;
            seed(&pool).await;
            pool.close().await;
            database
        });
        drop(setup);
        let workers = Arc::new(AtomicUsize::new(0));
        let started = workers.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .on_thread_start(move || {
                started.fetch_add(1, Ordering::SeqCst);
            })
            .build()
            .unwrap();
        runtime.block_on(async {
            let root = crate::config::runtime_root().unwrap();
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(4)
                .connect(&database.database_url)
                .await
                .unwrap();
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
            let http = Arc::new(Http::new("Bot off-intake-test"));
            let deps = IntakeDeps { http: &http, cache: None, ctx_for_chained_dispatch: None, shared: &shared, token: "Bot off-intake-test" };
            resources(&root, &workers);
            dispatch_skill_intake(
                &deps,
                ProviderKind::Claude,
                ChannelId::new(CHANNEL),
                MessageId::new(MESSAGE),
                UserId::new(6325),
                "off input owner".to_owned(),
                TEXT.to_owned(),
                IntakeOrigin::LiveMessage,
                Vec::new(),
                None,
            ).await.unwrap();
            let rows: Vec<OutboxRow> = sqlx::query_as(
                "SELECT target_instance_id, provider, channel_id, user_msg_id, user_text, status,
                 preserve_on_cancel FROM intake_outbox WHERE channel_id = $1 ORDER BY id",
            ).bind(CHANNEL.to_string()).fetch_all(&pool).await.unwrap();
            assert_eq!(rows, vec![(OWNER.to_owned(), "claude".to_owned(), CHANNEL.to_string(), MESSAGE.to_string(), TEXT.to_owned(), "pending".to_owned(), Some(false))]);
            assert!(shared.core.lock().await.sessions.is_empty());
            assert!(shared.mailbox(ChannelId::new(CHANNEL)).snapshot().await.intervention_queue.is_empty());
            resources(&root, &workers);
            assert!(!std::path::PathBuf::from(std::env::var_os(TMUX_LOG).unwrap()).exists());
            println!("G1B_OFF_TRACE={}", serde_json::json!({"processed": 1, "legacy_effects": rows.len(), "outbox": rows, "input_workers": workers.load(Ordering::SeqCst), "ledger_opens": OPENS.with(|opens| opens.get())}));
            pool.close().await;
        });
        drop(runtime);
        let cleanup = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        cleanup.block_on(database.drop());
    }
}
