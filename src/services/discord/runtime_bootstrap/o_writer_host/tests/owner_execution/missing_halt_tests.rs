use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLocation, HostedOwner, HostedState, tests::record,
};
use crate::services::discord::turn_presence::activity::{self, Activity};
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

type EventHook = Box<dyn FnMut(&str) + Send>;

#[derive(Clone, Default)]
struct Log(
    Arc<std::sync::Mutex<Vec<u8>>>,
    Arc<std::sync::Mutex<Option<(PathBuf, PathBuf)>>>,
    Arc<std::sync::Mutex<Option<EventHook>>>,
);
impl Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        if let Some(hook) = self.2.lock().unwrap().as_mut() {
            hook(&String::from_utf8_lossy(bytes));
        }
        if String::from_utf8_lossy(bytes).contains("writer stopped and stays stopped")
            && let Some((from, to)) = self.1.lock().unwrap().take()
        {
            std::fs::rename(from, to).unwrap();
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Log {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
impl Log {
    fn count(&self, text: &str) -> usize {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .matches(text)
            .count()
    }
}

struct PgOutage {
    command: std::sync::mpsc::Sender<Option<bool>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl PgOutage {
    fn install(pool: &sqlx::PgPool, stage: Arc<AtomicUsize>) -> (Self, EventHook) {
        let options = pool.connect_options();
        let (command, commands) = std::sync::mpsc::channel();
        let (acknowledge, acknowledgements) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            use sqlx::Connection;
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let mut connection = sqlx::PgConnection::connect_with(&options).await.unwrap();
                    let mut hidden = false;
                    while let Ok(Some(hide)) = commands.recv() {
                        let query = if hide {
                            "ALTER TABLE sessions RENAME TO sessions_retry_outage"
                        } else {
                            "ALTER TABLE sessions_retry_outage RENAME TO sessions"
                        };
                        sqlx::query(query).execute(&mut connection).await.unwrap();
                        hidden = hide;
                        acknowledge.send(()).unwrap();
                    }
                    if hidden {
                        sqlx::query("ALTER TABLE sessions_retry_outage RENAME TO sessions")
                            .execute(&mut connection)
                            .await
                            .unwrap();
                    }
                });
        });
        let sender = command.clone();
        let (mut hidden, mut restored) = (false, false);
        let hook: EventHook = Box::new(move |text| {
            if !hidden && text.contains("missing-source evidence changed; recovering") {
                sender.send(Some(true)).unwrap();
                acknowledgements
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
                hidden = true;
            } else if hidden
                && !restored
                && (text.contains("missing-source retry awaits readable metadata")
                    || text.contains("stage=\"hosted\" status=\"start\""))
            {
                stage.store(
                    if text.contains("awaits readable metadata") {
                        1
                    } else {
                        2
                    },
                    AtomicOrdering::SeqCst,
                );
                sender.send(Some(false)).unwrap();
                acknowledgements
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
                restored = true;
            }
        });
        (
            Self {
                command,
                thread: Some(thread),
            },
            hook,
        )
    }
    fn finish(mut self) {
        self.command.send(None).unwrap();
        self.thread.take().unwrap().join().unwrap();
    }
}
impl Drop for PgOutage {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.command.send(None);
            let _ = thread.join();
        }
    }
}

fn isolated(name: &str) -> bool {
    const CHILD: &str = "ADK_TEST_O_BINDING_CASE";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return true;
    }
    let qualified = name
        .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
        .unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", qualified, "--nocapture"])
        .env(CHILD, name)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!(
        "MISSING_HALT_CHILD {qualified} status={}\n{stdout}\n{stderr}",
        output.status
    );
    assert!(
        output.status.success(),
        "isolated production-path case failed"
    );
    assert!(
        stdout.contains("1 passed; 0 failed; 0 ignored;"),
        "child must execute exactly one test"
    );
    false
}

async fn until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("production writer must reach the requested state");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wake {
    Source,
    Presence,
    Binding,
    ReplayedBinding,
    InterruptedPresence,
    EarlySource,
    RuntimeSource,
    AbsentRuntime,
    Stop,
    Retire,
    Replaced,
}

async fn scenario(role: &str, wake: Wake) {
    let source_returns = !matches!(wake, Wake::Presence | Wake::InterruptedPresence);
    let root = crate::config::TestRuntimeRootGuard::new();
    let _binding = activity::tests::BindingRoot::enter(root.path());
    let _live = crate::config_live_reload::test_live_guard_after_shared_test_env_lock();
    let _namespace = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "TMUX_TMPDIR",
        &root.path().canonicalize().unwrap(),
    );
    let _pane = crate::config::TestEnvVarGuard::remove_after_shared_test_env_lock("TMUX");
    let channel = 78_780_000 + u64::from(role == "worker") * 10 + u64::from(source_returns);
    let name = format!("missing-halt-{channel}");
    let pane = ProviderKind::Claude.build_tmux_session_name(&name);
    crate::config::session_hosts::install_for_test(Some("mac-a"), &[(channel, "mac-a")]);
    let config: crate::config::Config = serde_json::from_value(json!({"server":{},
        "cluster":{"enabled":true,"instance_id":"mac-a","gateway_preferred_instance_id":"mac-a"},
        "agents":[{"id":"missing-agent","name":"fixture","provider":"claude",
            "channels":{"claude":{"id":channel.to_string(),"name":name,"runtime":"tui"}}}],
        "tui_o":{"writer":{"channels":[channel],"dormant_channels": if role == "worker" { vec![channel] } else { vec![] }}}})).unwrap();
    crate::config_live_reload::install(config.clone());
    let _selected = cutover::test_override::force_boot(
        BootChannels::validate(&config)
            .unwrap()
            .adopted(Adoption::Committed),
    );
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(channel);
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .unwrap();
    eprintln!("MISSING_HALT_PG_REAL database={database} role={role} wake={wake:?}");
    if role == "worker" {
        execution::tests::admission(&pool, "claude", channel, "mac-a").await;
        let permit = execution::read(&pool, "claude", channel, "mac-a", || Ok(false))
            .await
            .unwrap()
            .unwrap();
        cutover::boot_snapshot()
            .unwrap()
            .install_execution(channel, Some(Ok(permit)));
    }
    let (_registry, shared) = runtime(&pool, ProviderKind::Claude, channel, role).await;
    crate::services::discord::host_defer_gate::tests::map_channel(
        &shared,
        serenity::ChannelId::new(channel),
        &name,
    )
    .await;
    let discord = Discord::new().await;
    let server = gateway_context(&shared, channel).await;
    let _rest = crate::services::discord::shared_state::test_rest::install(discord.http.clone());
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: shared.token_hash.clone(),
        channel_id: channel.to_string(),
        logical_key: pane.clone(),
        owner_node: "mac-a".into(),
        runtime_root: root.path().display().to_string(),
    };
    let mut row = record(&owner, "orphan-execution", HostedState::Pending);
    if wake == Wake::AbsentRuntime {
        row.state = HostedState::Retired;
        let endpoint = crate::config::session_hosts::herdr_endpoint(channel).unwrap();
        row.location = Some(HostedLocation {
            host: "herdr".into(),
            execution_node: endpoint.execution_node,
            endpoint_config_key: endpoint.key,
            socket_addr: endpoint.socket_path.display().to_string(),
            named_session: endpoint.herdr_session,
            pane_id: "orphan-pane".into(),
        });
    }
    let key = crate::services::discord::adk_session::build_namespaced_session_key(
        &shared.token_hash,
        &ProviderKind::Claude,
        &pane,
    );
    crate::services::discord::inflight::seed_session_row_hashed(
        &pool,
        &key,
        channel,
        &shared.token_hash,
        Some(serde_json::to_value(&row).unwrap()),
    )
    .await;
    let reading = activity::reading_now(
        &shared,
        &ProviderKind::Claude,
        serenity::ChannelId::new(channel),
    )
    .await;
    assert_eq!(
        reading.observed.activity,
        if wake == Wake::AbsentRuntime {
            Activity::Idle
        } else {
            Activity::Unknown
        }
    );
    assert!(reading.publish_if_current(|| ()).await.is_some());
    let saved = root.path().join("source-kept");
    let (store, era, source) = missing_store_with(root.path(), channel, |path| {
        std::fs::hard_link(path, &saved).unwrap();
    });
    crate::services::tui_o::channel_policy::install(&config).unwrap();
    let mut original = retained_bytes(root.path(), channel, &source);
    let log = Log::default();
    let outage_stage = Arc::new(AtomicUsize::new(0));
    let mut outage = None;
    if wake == Wake::InterruptedPresence {
        let (guard, hook) = PgOutage::install(&pool, outage_stage.clone());
        *log.2.lock().unwrap() = Some(hook);
        outage = Some(guard);
    }
    if wake == Wake::ReplayedBinding {
        let path = root
            .path()
            .join(crate::services::tui_prompt_dedupe::binding_events::BINDING_EVENTS_DIR)
            .join(format!("{channel}.log"));
        let (mut saved, mut restored) = (None, false);
        *log.2.lock().unwrap() = Some(Box::new(move |text| {
            if saved.is_none() && text.contains("missing-source evidence changed; recovering") {
                saved = Some(std::fs::read(&path).unwrap());
                std::fs::write(&path, b"").unwrap();
            } else if !restored
                && text.contains("stage=\"hosted\" status=\"start\"")
                && let Some(bytes) = &saved
            {
                std::fs::write(&path, bytes).unwrap();
                restored = true;
            }
        }));
    }
    if wake == Wake::EarlySource {
        *log.1.lock().unwrap() = Some((saved.clone(), source.path.clone()));
    }
    if matches!(wake, Wake::RuntimeSource | Wake::AbsentRuntime) {
        std::fs::rename(&saved, &source.path).unwrap();
    }
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(
                if matches!(wake, Wake::ReplayedBinding | Wake::InterruptedPresence) {
                    tracing::Level::DEBUG
                } else {
                    tracing::Level::INFO
                },
            )
            .with_writer(log.clone())
            .finish(),
    );
    let gate = crate::services::tui_o::ownership::gate("claude");
    gate.acquired();
    let mut handles = host::start_managed(
        ShadowProvider::Claude,
        true,
        cutover::boot_ownership(),
        || {
            parts_with_rest(
                &shared,
                &ProviderKind::Claude,
                None,
                Some(discord.http.clone()),
            )
        },
    );
    assert_eq!(handles.len(), 1);
    let generation = host::process_readiness().hosted_by(channel).unwrap();
    if matches!(wake, Wake::RuntimeSource | Wake::AbsentRuntime) {
        accepts(channel).await;
        std::fs::rename(&source.path, &saved).unwrap();
    }
    until(|| log.count("writer stopped and stays stopped") == 1).await;
    if matches!(wake, Wake::RuntimeSource | Wake::AbsentRuntime) {
        let held = retained_bytes(root.path(), channel, &source);
        for (path, hash) in &original {
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                assert_eq!(
                    held.get(path),
                    Some(hash),
                    "live capture must preserve its cursor"
                );
            }
        }
        original = held;
    }
    let tui_o = TuiOConfig::default();
    if wake == Wake::AbsentRuntime {
        let mut successor = host::start_managed(
            ShadowProvider::Claude,
            true,
            cutover::boot_ownership(),
            || {
                parts_with_rest(
                    &shared,
                    &ProviderKind::Claude,
                    None,
                    Some(discord.http.clone()),
                )
            },
        );
        assert_eq!(
            successor.len(),
            1,
            "the sessionless host must release its first claim"
        );
        accepts(channel).await;
        assert!(!handles[0].live());
        assert_ne!(
            host::process_readiness().hosted_by(channel),
            Some(generation)
        );
        assert_eq!(log.count("stage=\"hosted\" status=\"start\""), 2);
        assert_eq!(retained_bytes(root.path(), channel, &source), original);
        assert!(
            !crate::services::tui_o::alarm::health_reasons()
                .contains(&format!("tui_o:halted:{channel}"))
        );
        handles.remove(0).stop_and_join().await.unwrap();
        successor.remove(0).stop_and_join().await.unwrap();
        assert!(!host::process_readiness().is_hosted(channel));
        gate.lost();
        server.abort();
        pool.close().await;
        eprintln!(
            "MISSING_HALT_CASE role={role} wake={wake:?} boots=2 existing sessionless recovery preserved PG=real skip=0"
        );
        return;
    }
    for _ in 0..if wake == Wake::EarlySource { 0 } else { 4 } {
        super::super::super::super::super::owner_cleanup::refresh_writers(
            &shared,
            &ProviderKind::Claude,
            Some(&tui_o),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    let stable = activity::reading_now(
        &shared,
        &ProviderKind::Claude,
        serenity::ChannelId::new(channel),
    )
    .await;
    assert_eq!(stable.observed.activity, Activity::Unknown);
    eprintln!(
        "MISSING_HALT_STABLE role={role} wake={wake:?} boots={} halts={} generation={:?} presence={:?}",
        log.count("stage=\"hosted\" status=\"start\""),
        log.count("writer stopped and stays stopped"),
        host::process_readiness().hosted_by(channel),
        stable.observed
    );
    assert_eq!(
        log.count("writer stopped and stays stopped"),
        1,
        "unchanged Missing/Unknown must halt once"
    );
    assert_eq!(
        host::process_readiness().hosted_by(channel),
        Some(generation),
        "unchanged Missing/Unknown must retain its claim"
    );
    assert!(handles[0].live(), "halted host must wait with its claim");
    if wake != Wake::EarlySource {
        assert!(!host::channel_accepts(channel));
    }
    if wake != Wake::EarlySource {
        assert_eq!(
            log.count("stage=\"hosted\" status=\"start\""),
            1,
            "unchanged observation must not boot an actor"
        );
    }
    assert_eq!(retained_bytes(root.path(), channel, &source), original);
    if matches!(wake, Wake::Stop | Wake::Retire | Wake::Replaced) {
        if wake == Wake::Retire {
            cutover::boot_snapshot()
                .unwrap()
                .execution(channel)
                .unwrap()
                .permit
                .unwrap()
                .gate()
                .close();
            until(|| !host::process_readiness().is_hosted(channel)).await;
        } else if wake == Wake::Replaced {
            std::fs::write(&source.path, b"{}\n{}\n").unwrap();
            until(|| !handles[0].live()).await;
            assert_eq!(log.count("stage=\"hosted\" status=\"start\""), 2);
        }
        handles.remove(0).stop_and_join().await.unwrap();
        assert!(!host::process_readiness().is_hosted(channel));
        assert!(!host::channel_accepts(channel));
        assert!(
            crate::services::tui_o::alarm::health_reasons()
                .contains(&format!("tui_o:halted:{channel}"))
        );
        assert_eq!(retained_bytes(root.path(), channel, &source), original);
        assert!(discord.posts.lock().unwrap().is_empty());
        gate.lost();
        eprintln!("MISSING_HALT_CASE role={role} wake={wake:?} PG=real skip=0");
        server.abort();
        pool.close().await;
        return;
    }
    if matches!(wake, Wake::Binding | Wake::ReplayedBinding) {
        use crate::services::tui_prompt_dedupe::binding_events as p5;
        let dir = root.path().join(p5::BINDING_EVENTS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let now = chrono::Utc::now();
        let mut event = p5::BindingEvent {
            seq: 1,
            channel_id: channel,
            provider: "claude".into(),
            tmux_session: pane.clone(),
            execution_nonce: Some("durable-new-incarnation".into()),
            old: None,
            new: p5::BindingTarget::Source(source.clone()),
            cause: p5::BindingCause::Startup,
            parent_hint: None,
            evidence: p5::BindingEvidence {
                hook_event: Some("SessionStart".into()),
                received_at: now,
            },
            committed_at: now,
        };
        let path = dir.join(format!("{channel}.log"));
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
        until(|| log.count("writer stopped and stays stopped") >= 2).await;
        event.seq = 2;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&event).unwrap()).unwrap();
        event.seq = 3;
        event.new = p5::BindingTarget::Rejected {
            payload_session_id: "rejected".into(),
            payload_transcript_path: None,
            reason: "audit only".into(),
        };
        writeln!(file, "{}", serde_json::to_string(&event).unwrap()).unwrap();
        file.sync_all().unwrap();
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert_eq!(
            log.count("stage=\"hosted\" status=\"start\""),
            2,
            "duplicate binding and reject audit must not wake"
        );
    }
    if wake == Wake::Source && role == "gateway" {
        sqlx::query("ALTER TABLE sessions RENAME TO sessions_temporarily_unreadable")
            .execute(&pool)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let failed = activity::reading_now(
            &shared,
            &ProviderKind::Claude,
            serenity::ChannelId::new(channel),
        )
        .await;
        assert!(
            matches!(
                failed.observed.reason,
                "session_probe_failed" | "probe_failed"
            ),
            "a real PG read must fail: {:?}",
            failed.observed
        );
        sqlx::query("ALTER TABLE sessions_temporarily_unreadable RENAME TO sessions")
            .execute(&pool)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(
            log.count("stage=\"hosted\" status=\"start\""),
            1,
            "PG read failure and recovery must not create evidence"
        );
    }
    if wake == Wake::InterruptedPresence {
        let mut retired = record(&owner, "orphan-execution", HostedState::Retired);
        let endpoint = crate::config::session_hosts::herdr_endpoint(channel).unwrap();
        retired.location = Some(HostedLocation {
            host: "herdr".into(),
            execution_node: endpoint.execution_node,
            endpoint_config_key: endpoint.key,
            socket_addr: endpoint.socket_path.display().to_string(),
            named_session: endpoint.herdr_session,
            pane_id: "orphan-pane".into(),
        });
        sqlx::query("UPDATE sessions SET hosted_execution=$2 WHERE session_key=$1")
            .bind(&key)
            .bind(serde_json::to_value(retired).unwrap())
            .execute(&pool)
            .await
            .unwrap();
        accepts(channel).await;
        assert_eq!(
            outage_stage.load(AtomicOrdering::SeqCst),
            1,
            "the retry must wait for actual PG metadata recovery before booting"
        );
        assert_eq!(log.count("writer stopped and stays stopped"), 1);
        assert_eq!(retained_bytes(root.path(), channel, &source), original);
    } else if source_returns {
        if wake != Wake::EarlySource {
            std::fs::rename(&saved, &source.path).unwrap();
        }
        accepts(channel).await;
    } else {
        let mut successor = record(&owner, "new-execution", HostedState::Pending);
        sqlx::query("UPDATE sessions SET hosted_execution=$2 WHERE session_key=$1")
            .bind(&key)
            .bind(serde_json::to_value(&successor).unwrap())
            .execute(&pool)
            .await
            .unwrap();
        until(|| log.count("writer stopped and stays stopped") >= 2).await;
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert_eq!(
            log.count("stage=\"hosted\" status=\"start\""),
            2,
            "a new nonce permits exactly one failed retry"
        );
        successor.state = HostedState::Retired;
        let endpoint = crate::config::session_hosts::herdr_endpoint(channel).unwrap();
        successor.location = Some(HostedLocation {
            host: "herdr".into(),
            execution_node: endpoint.execution_node,
            endpoint_config_key: endpoint.key,
            socket_addr: endpoint.socket_path.display().to_string(),
            named_session: endpoint.herdr_session,
            pane_id: "orphan-pane".into(),
        });
        sqlx::query("UPDATE sessions SET hosted_execution=$2 WHERE session_key=$1")
            .bind(&key)
            .bind(serde_json::to_value(&successor).unwrap())
            .execute(&pool)
            .await
            .unwrap();
        accepts(channel).await;
        assert_eq!(retained_bytes(root.path(), channel, &source), original);
    }
    assert_eq!(
        host::process_readiness().hosted_by(channel),
        Some(generation)
    );
    assert_eq!(
        log.count("stage=\"hosted\" status=\"start\""),
        if matches!(wake, Wake::Presence | Wake::Binding | Wake::ReplayedBinding) {
            3
        } else {
            2
        }
    );
    assert!(
        !crate::services::tui_o::alarm::health_reasons()
            .contains(&format!("tui_o:halted:{channel}"))
    );
    assert!(
        !store
            .open_channel(&era, channel)
            .unwrap()
            .unwrap()
            .cursor(&source)
            .unwrap()
            .retired
    );
    assert!(discord.posts.lock().unwrap().is_empty());
    handles.remove(0).stop_and_join().await.unwrap();
    assert!(!host::process_readiness().is_hosted(channel));
    gate.lost();
    eprintln!(
        "MISSING_HALT_CASE role={role} source_returns={source_returns} generation={generation} boots={} skip=0 PG=real",
        log.count("stage=\"hosted\" status=\"start\"")
    );
    server.abort();
    if let Some(guard) = outage.take() {
        guard.finish();
    }
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn missing_halt_gateway_source_reappears_pg() {
    if isolated(concat!(
        module_path!(),
        "::missing_halt_gateway_source_reappears_pg"
    )) {
        scenario("gateway", Wake::Source).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_halt_worker_source_reappears_pg() {
    if isolated(concat!(
        module_path!(),
        "::missing_halt_worker_source_reappears_pg"
    )) {
        scenario("worker", Wake::Source).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_halt_gateway_presence_changes_pg() {
    if isolated(concat!(
        module_path!(),
        "::missing_halt_gateway_presence_changes_pg"
    )) {
        scenario("gateway", Wake::Presence).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_halt_worker_presence_changes_pg() {
    if isolated(concat!(
        module_path!(),
        "::missing_halt_worker_presence_changes_pg"
    )) {
        scenario("worker", Wake::Presence).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_halt_durable_binding_retries_once_pg() {
    if isolated(concat!(
        module_path!(),
        "::missing_halt_durable_binding_retries_once_pg"
    )) {
        scenario("gateway", Wake::Binding).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_halt_source_returns_before_wait_pg() {
    if isolated(concat!(
        module_path!(),
        "::missing_halt_source_returns_before_wait_pg"
    )) {
        scenario("gateway", Wake::EarlySource).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_halt_runtime_loss_holds_claim_pg() {
    if isolated(concat!(
        module_path!(),
        "::missing_halt_runtime_loss_holds_claim_pg"
    )) {
        scenario("gateway", Wake::RuntimeSource).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_halt_stop_releases_claim_pg() {
    if isolated(concat!(
        module_path!(),
        "::missing_halt_stop_releases_claim_pg"
    )) {
        scenario("gateway", Wake::Stop).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_halt_execution_retirement_releases_claim_pg() {
    if isolated(concat!(
        module_path!(),
        "::missing_halt_execution_retirement_releases_claim_pg"
    )) {
        scenario("worker", Wake::Retire).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_halt_replaced_path_keeps_existing_stop_pg() {
    if isolated(concat!(
        module_path!(),
        "::missing_halt_replaced_path_keeps_existing_stop_pg"
    )) {
        scenario("gateway", Wake::Replaced).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_halt_replayed_binding_is_not_new_evidence_pg() {
    if isolated(concat!(
        module_path!(),
        "::missing_halt_replayed_binding_is_not_new_evidence_pg"
    )) {
        scenario("gateway", Wake::ReplayedBinding).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_halt_retry_waits_for_current_pg_publication_pg() {
    if isolated(concat!(
        module_path!(),
        "::missing_halt_retry_waits_for_current_pg_publication_pg"
    )) {
        scenario("gateway", Wake::InterruptedPresence).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_halt_sessionless_runtime_loss_keeps_main_recovery_pg() {
    if isolated(concat!(
        module_path!(),
        "::missing_halt_sessionless_runtime_loss_keeps_main_recovery_pg"
    )) {
        scenario("gateway", Wake::AbsentRuntime).await;
    }
}
