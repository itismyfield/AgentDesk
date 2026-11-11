use super::*;
use crate::services::discord::make_shared_data_for_tests;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test(flavor = "current_thread")]
async fn worker_held_recovery_never_runs_top_level_intake_or_barrier() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = make_shared_data_for_tests();
    assert!(shared.serenity_http_or_token_fallback().is_none());
    let intake = AtomicUsize::new(0);
    let barrier = AtomicUsize::new(0);
    let gate = restore_worker_then(&shared, &shared.provider, || async {
        intake.fetch_add(1, Ordering::SeqCst);
        barrier.fetch_add(1, Ordering::SeqCst);
    })
    .await;
    assert_eq!(gate, RecoveryGate::Held);
    assert_eq!(
        (
            intake.load(Ordering::SeqCst),
            barrier.load(Ordering::SeqCst)
        ),
        (0, 0)
    );
    assert!(!shared.restart.reconcile_done.load(Ordering::Acquire));
}

#[tokio::test(flavor = "current_thread")]
async fn worker_catalog_held_result_reaches_the_top_level_gate() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = make_shared_data_for_tests();
    shared
        .http
        .cached_bot_token
        .set("Bot fixture-token".into())
        .unwrap();
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join(shared.provider.as_str())
        .join(&shared.token_hash);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("8057304.json");
    std::fs::write(&path, b"unreadable queue").unwrap();
    let continued = AtomicUsize::new(0);
    let gate = restore_worker_then(&shared, &shared.provider, || async {
        continued.fetch_add(1, Ordering::SeqCst);
    })
    .await;
    assert_eq!(gate, RecoveryGate::Held);
    assert_eq!(continued.load(Ordering::SeqCst), 0);
    assert!(!shared.restart.reconcile_done.load(Ordering::Acquire));
    assert_eq!(std::fs::read(path).unwrap(), b"unreadable queue");
}

#[tokio::test(flavor = "current_thread")]
async fn worker_complete_gate_marks_before_top_level_intake_and_barrier() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = make_shared_data_for_tests();
    shared
        .http
        .cached_bot_token
        .set("Bot fixture-token".into())
        .unwrap();
    let continued = AtomicUsize::new(0);
    let gate = restore_worker_then(&shared, &shared.provider, || async {
        assert!(shared.restart.reconcile_done.load(Ordering::Acquire));
        continued.fetch_add(1, Ordering::SeqCst);
    })
    .await;
    assert_eq!(gate, RecoveryGate::Complete);
    assert_eq!(continued.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn active_completion_observes_catalog_before_mark_and_keeps_failed_bytes() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = make_shared_data_for_tests();
    let pass = RecoveryPass::new();
    let early = complete_active_reconcile_in_pass(&shared, &pass, RecoveryGate::Held).await;
    assert_eq!(early, RecoveryGate::Held);
    assert!(!shared.restart.reconcile_done.load(Ordering::Acquire));
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join(shared.provider.as_str())
        .join(&shared.token_hash);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("8057302.json");
    std::fs::write(&path, b"unreadable queue").unwrap();
    let held = complete_active_reconcile_in_pass(&shared, &pass, RecoveryGate::Complete).await;
    assert_eq!(held, RecoveryGate::Held);
    assert!(!shared.restart.reconcile_done.load(Ordering::Acquire));
    assert_eq!(std::fs::read(&path).unwrap(), b"unreadable queue");
    std::fs::write(&path, b"[]").unwrap();
    let recovered = complete_active_reconcile_in_pass(&shared, &pass, RecoveryGate::Complete).await;
    assert_eq!(recovered, RecoveryGate::Complete);
    assert!(shared.restart.reconcile_done.load(Ordering::Acquire));
}

#[tokio::test(flavor = "current_thread")]
async fn gateway_held_recovery_skips_success_log_second_kickoff_barrier_and_flush() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = make_shared_data_for_tests();
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join(shared.provider.as_str())
        .join(&shared.token_hash);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("8057303.json");
    std::fs::write(&path, b"unreadable queue").unwrap();
    let logs = CapturedLogs::default();
    let output = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || output.clone())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let http = Arc::new(serenity::Http::new("Bot fixture-token"));
    let (_task, finished) = gateway::run(&http, &shared, &shared.provider);
    crate::test_support::wait::await_within(finished, crate::test_support::wait::WAIT_CAP)
        .await
        .unwrap();
    assert_eq!(gateway::counts(), (1, 0, 0));
    assert!(!shared.restart.reconcile_done.load(Ordering::Acquire));
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(!text.contains("Reconcile complete — intake open"), "{text}");
    assert!(
        text.contains("reconcile held; intake remains closed"),
        "{text}"
    );
    assert_eq!(std::fs::read(path).unwrap(), b"unreadable queue");
}

#[derive(Clone, Default)]
struct CapturedLogs(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLogs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

mod gateway {
    use super::*;
    use crate::services::discord::{health, turn_bridge};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    static KICKOFFS: AtomicUsize = AtomicUsize::new(0);
    static BARRIERS: AtomicUsize = AtomicUsize::new(0);
    static FLUSHES: AtomicUsize = AtomicUsize::new(0);
    static DONE: Mutex<Option<::tokio::sync::oneshot::Sender<()>>> = Mutex::new(None);
    static TASK: Mutex<Option<::tokio::task::JoinHandle<()>>> = Mutex::new(None);
    static STARTUP_THREAD_MAP_VALIDATION_STARTED: AtomicBool = AtomicBool::new(false);
    const RESTART_REPORT_FLUSH_INTERVAL: Duration = Duration::from_secs(60);

    pub(super) struct TaskGuard;
    impl Drop for TaskGuard {
        fn drop(&mut self) {
            if let Some(task) = TASK.lock().unwrap().take() {
                task.abort();
            }
        }
    }

    pub(super) fn counts() -> (usize, usize, usize) {
        (
            KICKOFFS.load(Ordering::SeqCst),
            BARRIERS.load(Ordering::SeqCst),
            FLUSHES.load(Ordering::SeqCst),
        )
    }

    mod serenity {
        pub(super) use poise::serenity_prelude::Http;
        #[derive(Clone)]
        pub(super) struct Context {
            pub(super) http: Arc<Http>,
        }
        use std::sync::Arc;
    }

    mod tokio {
        pub(super) fn spawn<F>(future: F)
        where
            F: std::future::Future<Output = ()> + Send + 'static,
        {
            let task = ::tokio::spawn(async move {
                future.await;
                let done = super::DONE.lock().unwrap().take();
                if let Some(done) = done {
                    let _ = done.send(());
                }
            });
            *super::TASK.lock().unwrap() = Some(task);
        }
        pub(crate) mod time {
            pub(crate) async fn sleep(_: std::time::Duration) {
                let done = super::super::DONE.lock().unwrap().take();
                if let Some(done) = done {
                    let _ = done.send(());
                }
                std::future::pending::<()>().await;
            }
        }
    }

    mod recovery_flush {
        include!("recovery_flush.rs");
    }

    pub(super) fn run(
        http: &Arc<serenity::Http>,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
    ) -> (TaskGuard, ::tokio::sync::oneshot::Receiver<()>) {
        KICKOFFS.store(0, Ordering::SeqCst);
        BARRIERS.store(0, Ordering::SeqCst);
        FLUSHES.store(0, Ordering::SeqCst);
        let (done, finished) = ::tokio::sync::oneshot::channel();
        assert!(DONE.lock().unwrap().replace(done).is_none());
        recovery_flush::run_bot_spawn_recovery_and_flush_restart_reports(
            &serenity::Context { http: http.clone() },
            shared,
            "fixture-token",
            provider,
            &Arc::new(AtomicUsize::new(1)),
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(health::HealthRegistry::new()),
            0,
        );
        (TaskGuard, finished)
    }

    async fn catch_up_missed_messages(
        _: &Arc<serenity::Http>,
        _: &Arc<SharedData>,
        _: &ProviderKind,
    ) {
    }
    async fn gc_stale_fixed_working_sessions(_: &Arc<SharedData>) {}
    async fn cleanup_orphan_tmux_sessions(_: &Arc<SharedData>) {}
    fn purge_legacy_durable_handoffs() {}
    async fn recover_orphan_pending_dispatches(_: &Arc<SharedData>) {}
    async fn kickoff_idle_queues(
        _: &serenity::Context,
        _: &Arc<SharedData>,
        _: &str,
        _: &ProviderKind,
    ) {
        KICKOFFS.fetch_add(1, Ordering::SeqCst);
    }
    async fn delete_stale_queued_placeholder_cards(
        _: &Arc<serenity::Http>,
        _: &Arc<SharedData>,
        _: &[(ChannelId, MessageId, MessageId)],
    ) {
    }
    fn mark_reconcile_complete(shared: &Arc<SharedData>) {
        crate::services::discord::mark_reconcile_complete(shared);
    }
    fn spawn_startup_thread_map_validation(_: Option<sqlx::PgPool>, _: String) {}
    async fn run_startup_diagnostic_after_reconcile_barrier_for_provider(
        _: &ProviderKind,
        _: Arc<AtomicUsize>,
        _: Arc<AtomicBool>,
        _: Arc<health::HealthRegistry>,
        _: u16,
    ) {
        BARRIERS.fetch_add(1, Ordering::SeqCst);
    }
    async fn flush_restart_reports(_: &Arc<serenity::Http>, _: &Arc<SharedData>, _: &ProviderKind) {
        FLUSHES.fetch_add(1, Ordering::SeqCst);
    }
    async fn restore_tmux_watchers(_: &Arc<serenity::Http>, _: &Arc<SharedData>) {}

    mod queued_recovery {
        use super::*;
        pub(super) async fn restore_queued_and_inflight_in_pass(
            http: &Arc<serenity::Http>,
            shared: &Arc<SharedData>,
            provider: &ProviderKind,
            pass: &RecoveryPass,
        ) -> (Vec<(ChannelId, MessageId, MessageId)>, RecoveryGate) {
            crate::services::discord::runtime_bootstrap::queued_recovery::restore_queued_and_inflight_in_pass(
                http, shared, provider, pass,
            ).await
        }
    }
    mod tmux {
        use super::*;
        pub(super) async fn store_recovery_handled_channels(_: &Arc<SharedData>, _: &[u64]) {}
        pub(super) async fn clear_recovery_handled_channels(_: &Arc<SharedData>) {}
    }
}
