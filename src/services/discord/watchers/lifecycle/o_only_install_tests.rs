use super::*;
use crate::services::discord::health::{HealthRegistry, legacy_supervision::RetiredForTest};
use crate::services::discord::task_supervisor::watcher_completion::{self, Outcome};
use crate::services::discord::{
    OOnlyInstallOutcome, OOnlyInstallReason, OOnlyInstallRequest, install_retired_o_watcher,
};
use std::collections::HashMap;
use tokio::sync::Notify;

pub(in crate::services::discord) struct InstallFixture {
    pub shared: Arc<SharedData>,
    pub registry: Arc<HealthRegistry>,
    pub http: Arc<serenity::Http>,
    pub provider: ProviderKind,
    pub channel: ChannelId,
    pub session: String,
    pub output: String,
}

impl InstallFixture {
    pub async fn new(provider: ProviderKind, channel: u64) -> Self {
        Self::new_with_pool(provider, channel, None).await
    }
    async fn new_with_pool(
        provider: ProviderKind,
        channel: u64,
        pool: Option<sqlx::PgPool>,
    ) -> Self {
        let channel = ChannelId::new(channel);
        let registry = Arc::new(HealthRegistry::new());
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared).unwrap().health_registry = Arc::downgrade(&registry);
        Arc::get_mut(&mut shared).unwrap().provider = provider.clone();
        Arc::get_mut(&mut shared).unwrap().pg_pool = pool;
        {
            let mut settings = shared.settings.write().await;
            settings.provider = provider.clone();
            settings.allowed_channel_ids = vec![channel.get()];
        }
        registry
            .register(provider.as_str().to_owned(), shared.clone())
            .await;
        let session = provider.build_tmux_session_name(&format!(
            "n4d-{}-{}",
            channel.get(),
            if provider == ProviderKind::Codex {
                "cdx"
            } else {
                "cc"
            }
        ));
        let output = tc::session_temp_path(&session, "jsonl");
        std::fs::create_dir_all(Path::new(&output).parent().unwrap()).unwrap();
        std::fs::write(&output, []).unwrap();
        std::fs::write(
            tc::tmux_owner_path(&session),
            tc::current_tmux_owner_marker(),
        )
        .unwrap();
        tc::write_tmux_channel_binding(&session, channel.get()).unwrap();
        tc::write_tmux_runtime_kind_marker(
            &session,
            match provider {
                ProviderKind::Codex => RuntimeHandoffKind::CodexTui,
                ProviderKind::Claude => RuntimeHandoffKind::ClaudeTui,
                _ => panic!("fixture requires TUI provider"),
            },
        )
        .unwrap();
        Self {
            shared,
            registry,
            http: Arc::new(serenity::Http::new("n4d-fixture")),
            provider,
            channel,
            session,
            output,
        }
    }

    pub fn request(&self) -> OOnlyInstallRequest {
        OOnlyInstallRequest {
            runtime: self.shared.clone(),
            http: self.http.clone(),
            provider: self.provider.clone(),
            channel_id: self.channel,
            session_name: self.session.clone(),
        }
    }
    pub async fn install(&self) -> OOnlyInstallOutcome {
        install_retired_o_watcher(self.request()).await
    }
    pub fn handle(&self) -> TmuxWatcherHandle {
        let handle = self.shared.tmux_watchers.get(&self.channel).unwrap();
        TmuxWatcherHandle {
            tmux_session_name: handle.tmux_session_name.clone(),
            output_path: handle.output_path.clone(),
            paused: handle.paused.clone(),
            resume_offset: handle.resume_offset.clone(),
            cancel: handle.cancel.clone(),
            pause_epoch: handle.pause_epoch.clone(),
            turn_delivered: handle.turn_delivered.clone(),
            last_heartbeat_ts_ms: handle.last_heartbeat_ts_ms.clone(),
        }
    }
    pub async fn cancel_and_join(&self) {
        let handle = self.handle();
        let ticket = watcher_completion::observe(&handle.cancel).expect("observed task registered");
        handle.cancel.store(true, Ordering::SeqCst);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(8), ticket.wait())
                .await
                .unwrap(),
            Outcome::Returned
        );
    }
}

thread_local! {
    static BEFORE_REOPEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = Default::default();
}
pub(super) fn before_reopen() {
    if let Some(hook) = BEFORE_REOPEN.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}
fn at_reopen(action: impl FnOnce() + 'static) {
    BEFORE_REOPEN.with(|slot| *slot.borrow_mut() = Some(Box::new(action)));
}

struct HostPause {
    entered: Notify,
    resume: Notify,
}
static HOST_PAUSES: LazyLock<Mutex<HashMap<ChannelId, Arc<HostPause>>>> =
    LazyLock::new(Mutex::default);
pub(super) async fn after_host(channel: ChannelId) {
    let pause = HOST_PAUSES.lock().unwrap().remove(&channel);
    if let Some(pause) = pause {
        pause.entered.notify_one();
        pause.resume.notified().await;
    }
}
fn pause_host(channel: ChannelId) -> Arc<HostPause> {
    let pause = Arc::new(HostPause {
        entered: Notify::new(),
        resume: Notify::new(),
    });
    HOST_PAUSES.lock().unwrap().insert(channel, pause.clone());
    pause
}

fn test_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn o_install_cold_registry_needs_no_legacy_state() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    test_rt().block_on(async {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let fixture =
            crate::services::discord::tmux::InstallFixture::new(ProviderKind::Claude, 632_510_001)
                .await;
        let _retired = RetiredForTest::new("claude", fixture.channel.get());
        assert!(fixture.shared.core.lock().await.sessions.is_empty());
        assert_eq!(
            crate::services::discord::install_retired_o_watcher(fixture.request()).await,
            OOnlyInstallOutcome::Spawned
        );
        assert!(fixture.shared.core.lock().await.sessions.is_empty());
        let handle = fixture.handle();
        assert!(!handle.paused.load(Ordering::SeqCst));
        assert_eq!(*handle.resume_offset.lock().unwrap(), None);
        assert_eq!(handle.pause_epoch.load(Ordering::SeqCst), 0);
        assert!(!handle.turn_delivered.load(Ordering::SeqCst));
        fixture.cancel_and_join().await;
    });
}

#[test]
fn o_install_same_source_reuses_live_incumbent_and_overlap_spawns_once() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    test_rt().block_on(async {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let fixture = InstallFixture::new(ProviderKind::Claude, 632_510_002).await;
        let _retired = RetiredForTest::new("claude", fixture.channel.get());
        let pause = pause_host(fixture.channel);
        let first_operation = fixture.install();
        tokio::pin!(first_operation);
        tokio::select! { _ = pause.entered.notified() => {}, outcome = &mut first_operation => panic!("host pause missed: {outcome:?}") }
        assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
        pause.resume.notify_one();
        assert_eq!(first_operation.await, OOnlyInstallOutcome::AlreadyLive);
        let first = fixture.handle();
        assert_eq!(fixture.install().await, OOnlyInstallOutcome::AlreadyLive);
        assert!(Arc::ptr_eq(&first.cancel, &fixture.handle().cancel));
        fixture.cancel_and_join().await;
    });
}

#[test]
fn o_install_failure_then_retry_uses_current_source() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    test_rt().block_on(async {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let fixture = InstallFixture::new(ProviderKind::Claude, 632_510_003).await;
        let _retired = RetiredForTest::new("claude", fixture.channel.get());
        std::fs::rename(&fixture.output, format!("{}.unavailable", fixture.output)).unwrap();
        assert_eq!(
            fixture.install().await,
            OOnlyInstallOutcome::Deferred(OOnlyInstallReason::SourceUnavailable)
        );
        assert!(fixture.shared.tmux_watchers.get(&fixture.channel).is_none());
        std::fs::write(&fixture.output, []).unwrap();
        assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
        fixture.cancel_and_join().await;
    });
}

#[test]
fn o_install_rechecks_owner_source_binding_and_incarnation_after_host_await() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    test_rt().block_on(async {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        for (index, change) in [
            "owner",
            "source",
            "binding",
            "generation",
            "route",
            "competing-runtime",
        ]
        .into_iter()
        .enumerate()
        {
            let fixture = InstallFixture::new(ProviderKind::Claude, 632_510_020 + index as u64).await;
            let _retired = RetiredForTest::new("claude", fixture.channel.get());
            let pause = pause_host(fixture.channel);
            let operation = install_retired_o_watcher(fixture.request());
            tokio::pin!(operation);
            tokio::select! { _ = pause.entered.notified() => {}, outcome = &mut operation => panic!("host pause missed: {outcome:?}") }
            match change {
                "owner" => {
                    std::fs::write(tc::tmux_owner_path(&fixture.session), "/other-runtime").unwrap()
                }
                "source" => {
                    std::fs::rename(&fixture.output, format!("{}.old", fixture.output)).unwrap();
                    std::fs::write(&fixture.output, []).unwrap();
                }
                "binding" => dedupe::register_tmux_runtime_binding(
                    &fixture.session,
                    TuiRuntimeBinding {
                        runtime_kind: RuntimeHandoffKind::ClaudeTui,
                        output_path: fixture.output.clone(),
                        relay_output_path: None,
                        input_fifo_path: None,
                        session_id: Some("changed".into()),
                        last_offset: 0,
                        relay_last_offset: None,
                    },
                ),
                "generation" => std::fs::write(
                    tc::session_temp_path(&fixture.session, "generation"),
                    "changed",
                )
                .unwrap(),
                "route" => {
                    fixture.shared.settings.write().await.allowed_channel_ids =
                        vec![fixture.channel.get() + 1]
                }
                "competing-runtime" => {
                    let competitor = crate::services::discord::make_shared_data_for_tests();
                    {
                        let mut settings = competitor.settings.write().await;
                        settings.provider = ProviderKind::Claude;
                        settings.allowed_channel_ids = vec![fixture.channel.get()];
                    }
                    fixture.registry.register("claude".into(), competitor).await;
                }
                _ => unreachable!(),
            }
            pause.resume.notify_one();
            let outcome = operation.await;
            assert!(
                matches!(
                    outcome,
                    OOnlyInstallOutcome::Failed(_) | OOnlyInstallOutcome::Deferred(_)
                ),
                "{change}: {outcome:?}"
            );
            assert!(
                fixture.shared.tmux_watchers.get(&fixture.channel).is_none(),
                "{change}: claim must not reserve"
            );
        }
    });
}

#[test]
fn o_install_append_is_not_a_source_change_and_nonretired_is_dormant() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    test_rt().block_on(async {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let fixture = InstallFixture::new(ProviderKind::Claude, 632_510_004).await;
        assert_eq!(
            fixture.install().await,
            OOnlyInstallOutcome::Deferred(OOnlyInstallReason::NotRetired)
        );
        assert!(fixture.shared.tmux_watchers.get(&fixture.channel).is_none());
        let _retired = RetiredForTest::new("claude", fixture.channel.get());
        let pause = pause_host(fixture.channel);
        let operation = fixture.install();
        tokio::pin!(operation);
        tokio::select! { _ = pause.entered.notified() => {}, outcome = &mut operation => panic!("{outcome:?}") }
        std::fs::write(&fixture.output, b"{\"type\":\"unrelated-fixture-event\"}\n").unwrap();
        pause.resume.notify_one();
        assert_eq!(operation.await, OOnlyInstallOutcome::Spawned);
        fixture.cancel_and_join().await;
    });
}

#[test]
fn o_install_binding_busy_is_unknown_not_absent() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let rt = test_rt();
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let fixture = rt.block_on(InstallFixture::new(ProviderKind::Claude, 632_510_005));
    let _retired = RetiredForTest::new("claude", fixture.channel.get());
    let locked = dedupe::hold_binding_peek_lock_for_tests();
    assert_eq!(
        rt.block_on(fixture.install()),
        OOnlyInstallOutcome::Deferred(OOnlyInstallReason::BindingBusy)
    );
    assert!(fixture.shared.tmux_watchers.get(&fixture.channel).is_none());
    drop(locked);
}

#[test]
fn o_install_wrapper_directory_is_not_a_source() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    test_rt().block_on(async {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let fixture = InstallFixture::new(ProviderKind::Claude, 632_510_046).await;
        let _retired = RetiredForTest::new("claude", fixture.channel.get());
        std::fs::rename(&fixture.output, format!("{}.former", fixture.output)).unwrap();
        std::fs::create_dir(&fixture.output).unwrap();
        assert_eq!(
            fixture.install().await,
            OOnlyInstallOutcome::Failed(OOnlyInstallReason::SourceMismatch)
        );
        assert!(fixture.shared.tmux_watchers.get(&fixture.channel).is_none());
        std::fs::remove_dir(&fixture.output).unwrap();
        std::fs::write(&fixture.output, []).unwrap();
        assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
        fixture.cancel_and_join().await;
    });
}

#[test]
fn o_install_fifo_sources_are_refused_without_waiting() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    test_rt().block_on(async {
        const CHILD: &str = "ADK_N4D_FIFO_CHILD";
        const TEST: &str = "services::discord::tmux::watcher_lifecycle::o_only_install::tests::o_install_fifo_sources_are_refused_without_waiting";
        if std::env::var_os(CHILD).is_none() {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while child.try_wait().unwrap().is_none() {
                if std::time::Instant::now() >= deadline {
                    // Kill and join only this fixture child when a blocking-open mutant prevents exit.
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    assert!(
                        std::time::Instant::now() < deadline,
                        "FIFO installer waited for a writer: {output:?}"
                    );
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(), "{output:?}");
            assert!(stdout.contains("running 1 test"), "{stdout}");
            assert!(stdout.contains("1 passed"), "{stdout}");
            return;
        }
        use std::os::unix::ffi::OsStrExt;
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let claude = root.path().join("claude");
        let _claude = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "CLAUDE_CONFIG_DIR",
            &claude,
        );
        for (index, native) in [false, true].into_iter().enumerate() {
            let fixture = InstallFixture::new(ProviderKind::Claude, 632_510_047 + index as u64).await;
            let _retired = RetiredForTest::new("claude", fixture.channel.get());
            std::fs::rename(&fixture.output, format!("{}.former", fixture.output)).unwrap();
            let path = if native {
                let projects = claude.join("projects").join("test");
                std::fs::create_dir_all(&projects).unwrap();
                let id = uuid::Uuid::new_v4().hyphenated().to_string();
                let path = projects.join(format!("{id}.jsonl"));
                dedupe::register_tmux_runtime_binding(
                    &fixture.session,
                    TuiRuntimeBinding {
                        runtime_kind: RuntimeHandoffKind::ClaudeTui,
                        output_path: path.display().to_string(),
                        session_id: Some(id),
                        relay_output_path: None,
                        input_fifo_path: None,
                        last_offset: 0,
                        relay_last_offset: None,
                    },
                );
                path
            } else {
                std::path::PathBuf::from(&fixture.output)
            };
            let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            assert_eq!(
                fixture.install().await,
                OOnlyInstallOutcome::Failed(OOnlyInstallReason::SourceMismatch)
            );
            assert!(fixture.shared.tmux_watchers.get(&fixture.channel).is_none());
        }
    });
}

#[test]
fn o_install_direct_resume_commit_required_is_explicit_and_current_native_succeeds() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    test_rt().block_on(async {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let codex_home = root.path().join("codex");
        let sessions = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let _codex_home = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "CODEX_HOME",
            &codex_home,
        );
        let fixture = InstallFixture::new(ProviderKind::Codex, 632_510_006).await;
        let _retired = RetiredForTest::new("codex", fixture.channel.get());
        std::fs::rename(&fixture.output, format!("{}.absent", fixture.output)).unwrap();
        let id = uuid::Uuid::new_v4().hyphenated().to_string();
        let native = sessions.join(format!("rollout-2026-10-10-{id}.jsonl"));
        let initial_bytes = format!("{}\n", serde_json::json!({"type":"session_meta", "payload":{"id":id, "source":"cli", "cwd":"/test"}})).into_bytes();
        std::fs::write(&native, &initial_bytes).unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        for (name, body) in [
            ("tmux", "#!/bin/sh\nprintf '%s\n' 123\n".to_string()),
            (
                "ps",
                format!("#!/bin/sh\nprintf '%s\n' 'codex resume {id}'\n"),
            ),
        ] {
            let script = bin.join(name);
            std::fs::write(&script, body).unwrap();
            std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _path = crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(&bin);
        assert_eq!(
            fixture.install().await,
            OOnlyInstallOutcome::Failed(OOnlyInstallReason::RequiresDirectResumeCommit)
        );
        assert!(fixture.shared.tmux_watchers.get(&fixture.channel).is_none());
        assert!(dedupe::peek_tmux_runtime_binding(&fixture.session).is_none());
        assert!(
            crate::services::codex_tui::session::read_codex_tui_rollout_marker(&fixture.session)
                .is_none()
        );
        assert_eq!(std::fs::read(&native).unwrap(), initial_bytes);
        dedupe::register_tmux_runtime_binding(
            &fixture.session,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: native.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(id),
                last_offset: 3,
                relay_last_offset: None,
            },
        );
        assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
        assert_eq!(fixture.handle().output_path, native.display().to_string());
        fixture.cancel_and_join().await;
    });
}

#[test]
fn o_install_native_verifier_and_reopen_must_name_same_descriptor() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    test_rt().block_on(async {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let codex = root.path().join("codex");
        let claude = root.path().join("claude");
        let _codex =
            crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("CODEX_HOME", &codex);
        let _claude = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "CLAUDE_CONFIG_DIR",
            &claude,
        );
        for (index, provider) in [ProviderKind::Codex, ProviderKind::Claude]
            .into_iter()
            .enumerate()
        {
            let fixture = InstallFixture::new(provider.clone(), 632_510_030 + index as u64).await;
            let _retired = RetiredForTest::new(provider.as_str(), fixture.channel.get());
            std::fs::rename(&fixture.output, format!("{}.absent", fixture.output)).unwrap();
            let id = uuid::Uuid::new_v4().hyphenated().to_string();
            let native = match provider {
                ProviderKind::Codex => codex
                    .join("sessions")
                    .join(format!("rollout-2026-10-10-{id}.jsonl")),
                ProviderKind::Claude => claude
                    .join("projects")
                    .join("test")
                    .join(format!("{id}.jsonl")),
                _ => unreachable!(),
            };
            std::fs::create_dir_all(native.parent().unwrap()).unwrap();
            let record = if provider == ProviderKind::Codex {
                serde_json::json!({"type":"session_meta", "payload":{"id":id, "source":"cli", "cwd":"/test"}})
            } else {
                serde_json::json!({"type":"system", "sessionId":id})
            };
            let bytes = format!("{record}\n").into_bytes();
            std::fs::write(&native, &bytes).unwrap();
            dedupe::register_tmux_runtime_binding(
                &fixture.session,
                TuiRuntimeBinding {
                    runtime_kind: if provider == ProviderKind::Codex {
                        RuntimeHandoffKind::CodexTui
                    } else {
                        RuntimeHandoffKind::ClaudeTui
                    },
                    output_path: native.display().to_string(),
                    session_id: Some(id),
                    relay_output_path: None,
                    input_fifo_path: None,
                    last_offset: 0,
                    relay_last_offset: None,
                },
            );
            let swapped = native.clone();
            at_reopen(move || {
                std::fs::rename(&swapped, swapped.with_extension("old")).unwrap();
                std::fs::write(swapped, bytes).unwrap();
            });
            assert_eq!(
                fixture.install().await,
                OOnlyInstallOutcome::Failed(OOnlyInstallReason::SourceMismatch),
                "{provider:?}"
            );
            assert!(fixture.shared.tmux_watchers.get(&fixture.channel).is_none());
            assert_eq!(
                fixture.install().await,
                OOnlyInstallOutcome::Spawned,
                "fresh valid {provider:?} native must work"
            );
            fixture.cancel_and_join().await;
        }
    });
}

#[test]
fn o_install_real_watch_withheld_preserves_reason_and_can_retry() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    test_rt().block_on(async {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let fixture = InstallFixture::new(ProviderKind::Claude, 632_510_040).await;
        let _retired = RetiredForTest::new("claude", fixture.channel.get());
        let marker = tc::session_temp_path(&fixture.session, "host_kind");
        std::fs::write(&marker, "herdr").unwrap();
        assert_eq!(
            fixture.install().await,
            OOnlyInstallOutcome::Failed(OOnlyInstallReason::WatchWithheld)
        );
        assert!(fixture.shared.tmux_watchers.get(&fixture.channel).is_none());
        std::fs::write(marker, "tmux").unwrap();
        assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
        fixture.cancel_and_join().await;
    });
}

#[test]
fn o_install_cold_codex_marker_needs_no_binding_registration() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    test_rt().block_on(async {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let codex_home = root.path().join("codex");
        let _codex = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "CODEX_HOME",
            &codex_home,
        );
        let fixture = InstallFixture::new(ProviderKind::Codex, 632_510_041).await;
        let _retired = RetiredForTest::new("codex", fixture.channel.get());
        std::fs::rename(&fixture.output, format!("{}.absent", fixture.output)).unwrap();
        let id = uuid::Uuid::new_v4().hyphenated().to_string();
        let native = codex_home
            .join("sessions")
            .join(format!("rollout-2026-10-10-{id}.jsonl"));
        std::fs::create_dir_all(native.parent().unwrap()).unwrap();
        std::fs::write(&native, format!("{}\n", serde_json::json!({"type":"session_meta", "payload":{"id":id, "source":"cli", "cwd":"/test"}}))).unwrap();
        crate::services::codex_tui::session::write_codex_tui_rollout_marker(
            &fixture.session,
            &native,
            Some(&id),
        )
        .unwrap();
        assert!(dedupe::peek_tmux_runtime_binding(&fixture.session).is_none());
        assert!(fixture.shared.core.lock().await.sessions.is_empty());
        assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
        assert!(dedupe::peek_tmux_runtime_binding(&fixture.session).is_none());
        assert!(fixture.shared.core.lock().await.sessions.is_empty());
        fixture.cancel_and_join().await;
    });
}

#[test]
fn o_install_other_channel_incumbent_is_not_already_live() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    test_rt().block_on(async {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let fixture = InstallFixture::new(ProviderKind::Claude, 632_510_042).await;
        let _retired = RetiredForTest::new("claude", fixture.channel.get());
        let other = ChannelId::new(fixture.channel.get() + 1);
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        fixture.shared.tmux_watchers.insert(
            other,
            TmuxWatcherHandle {
                tmux_session_name: fixture.session.clone(),
                output_path: fixture.output.clone(),
                cancel: cancel.clone(),
                paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                resume_offset: Arc::new(Mutex::new(None)),
                pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                    crate::services::discord::tmux_watcher_now_ms(),
                )),
            },
        );
        assert_eq!(
            fixture.install().await,
            OOnlyInstallOutcome::Failed(OOnlyInstallReason::SourceMismatch)
        );
        assert!(!cancel.load(Ordering::SeqCst));
        assert!(fixture.shared.tmux_watchers.get(&fixture.channel).is_none());
        assert_eq!(
            fixture
                .shared
                .tmux_watchers
                .owner_channel_for_tmux_session(&fixture.session),
            Some(other)
        );
    });
}

#[test]
fn o_install_unknown_database_host_passes_real_unverified_classification() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    test_rt().block_on(async {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(50))
            .connect_lazy("postgres://test:test@127.0.0.1:1/n4d_unavailable")
            .unwrap();
        let fixture =
            InstallFixture::new_with_pool(ProviderKind::Claude, 632_510_043, Some(pool)).await;
        let _retired = RetiredForTest::new("claude", fixture.channel.get());
        assert_eq!(
            watch_host_of(
                &fixture.shared,
                &fixture.provider,
                fixture.channel.get(),
                &fixture.session
            )
            .await,
            WatchHost::Unverified
        );
        assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
        fixture.cancel_and_join().await;
    });
}

fn install_context(fixture: &InstallFixture, root: &Path, policy: Option<&str>) -> BindingContext {
    let context = BindingContext {
        schema: 1,
        provider: fixture.provider.as_str().into(),
        created_at: chrono::Utc::now(),
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        tmux_session: fixture.session.clone(),
        channel_id: Some(fixture.channel.get()),
        owner_runtime_root: tc::current_tmux_owner_marker(),
        host: None,
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: Some(root.canonicalize().unwrap()),
        first_prompt_digest: None,
        source_policy: policy.map(str::to_owned),
    };
    binding_context::PreparedIncarnation::create(context.clone()).unwrap();
    std::fs::write(
        tc::session_temp_path(&fixture.session, "spawn_nonce"),
        &context.execution_nonce,
    )
    .unwrap();
    context
}

struct BindingRoot(Option<std::path::PathBuf>);
impl BindingRoot {
    fn enter(root: &Path) -> Self {
        let saved = binding_events::test_root();
        binding_events::set_test_root(Some(root));
        Self(saved)
    }
}
impl Drop for BindingRoot {
    fn drop(&mut self) {
        binding_events::set_test_root(self.0.as_deref());
    }
}

#[test]
fn o_install_cold_claude_native_uses_current_durable_source_read_only() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    test_rt().block_on(async {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let _binding_root = BindingRoot::enter(root.path());
        let projects = root.path().join("projects");
        std::fs::create_dir_all(projects.join("test")).unwrap();
        let projects = projects.canonicalize().unwrap();
        let fixture = InstallFixture::new(ProviderKind::Claude, 632_510_044).await;
        let _retired = RetiredForTest::new("claude", fixture.channel.get());
        std::fs::rename(&fixture.output, format!("{}.absent", fixture.output)).unwrap();
        let id = uuid::Uuid::new_v4().hyphenated().to_string();
        let native = projects.join("test").join(format!("{id}.jsonl"));
        std::fs::write(
            &native,
            format!("{}\n", serde_json::json!({"type":"system", "sessionId":id})),
        )
        .unwrap();
        let context = install_context(&fixture, &projects, None);
        let file = std::fs::File::open(&native).unwrap();
        let SourceFileIdentity::Unix { dev, ino } = SourceFileIdentity::from_open_file(&file)
        else {
            panic!("Unix source identity required");
        };
        let source = SourceId {
            session_id: id.clone(),
            path: native.clone(),
            dev,
            ino,
        };
        binding_events::record_verified(
            &binding_events::Proposal {
                channel_id: fixture.channel.get(),
                provider: "claude",
                tmux_session: &fixture.session,
                session_id: Some(&id),
                path: native.to_str().unwrap(),
                replaced: None,
                cause: binding_events::CauseSource::Observed,
                hook: None,
            },
            &source,
        )
        .unwrap();
        let records = binding_events::records_strict(fixture.channel.get())
            .unwrap()
            .unwrap();
        assert_eq!(
            records[0].execution_nonce.as_deref(),
            Some(context.execution_nonce.as_str())
        );
        assert!(dedupe::peek_tmux_runtime_binding(&fixture.session).is_none());
        assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
        assert_eq!(
            binding_events::records_strict(fixture.channel.get())
                .unwrap()
                .unwrap(),
            records
        );
        assert!(dedupe::peek_tmux_runtime_binding(&fixture.session).is_none());
        fixture.cancel_and_join().await;
    });
}

#[test]
fn o_install_verified_codex_proof_without_delivery_permission_is_withheld() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    test_rt().block_on(async {
        use binding_context::{CapturedContext, HookBindingEnvelope};
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let _binding_root = BindingRoot::enter(root.path());
        let sessions = root.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        let sessions = sessions.canonicalize().unwrap();
        let fixture = InstallFixture::new(ProviderKind::Codex, 632_510_045).await;
        let _retired = RetiredForTest::new("codex", fixture.channel.get());
        std::fs::rename(&fixture.output, format!("{}.absent", fixture.output)).unwrap();
        let context = install_context(&fixture, &sessions, Some("verified"));
        let id = uuid::Uuid::new_v4().hyphenated().to_string();
        let native = sessions.join(format!("rollout-{id}.jsonl"));
        let created = (context.created_at + chrono::Duration::seconds(1)).to_rfc3339();
        let header = serde_json::json!({"type":"session_meta", "timestamp":created,
            "payload":{"id":id, "timestamp":created, "source":"cli", "cwd":"/test"}});
        std::fs::write(&native, format!("{header}\n")).unwrap();
        dedupe::register_tmux_channel(&fixture.session, fixture.channel.get());
        let envelope = HookBindingEnvelope {
            context: CapturedContext::Captured(context.clone()),
            observed: Default::default(),
        };
        let payload =
            serde_json::json!({"session_id":id, "transcript_path":native, "source":"startup"});
        let hook = binding_events::HookSignal::from_payload("session_start", &payload);
        let observation =
            dedupe::observe_verified_codex_hook(Some(&id), &payload, &hook, Some(&envelope));
        // Ownership commits while the default delivery permission withholds its publication.
        assert!(matches!(observation, Some(crate::services::claude_tui::hook_server::observation_ingress::IngressOutcome::Durable(crate::services::claude_tui::hook_server::adoption_retry::DurableKind::Pending))), "{observation:?}");
        let fold = binding_events::codex::read_ownership(&context).unwrap();
        assert!(!fold.conflicted && fold.pending.is_empty());
        let proof = fold.verified.unwrap();
        assert_eq!(proof.ownership.context, context);
        assert_eq!(proof.source.session_id, id);
        assert_eq!(proof.source.path, native);
        dedupe::reset_state_for_tests();
        assert!(dedupe::peek_tmux_runtime_binding(&fixture.session).is_none());
        assert_eq!(
            fixture.install().await,
            OOnlyInstallOutcome::Failed(OOnlyInstallReason::SourceMismatch)
        );
        assert!(fixture.shared.tmux_watchers.get(&fixture.channel).is_none());
    });
}
