//! Registered runtime ownership through the real Codex restore producer and live readings.
use super::*;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::turn_presence::activity::{activity_now, reading_now};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

type PublicationCallback = Box<dyn FnOnce() + Send>;
struct PublicationHook {
    channel: u64,
    nth: usize,
    seen: usize,
    callback: PublicationCallback,
}
static PUBLICATION_HOOKS: std::sync::Mutex<Vec<PublicationHook>> =
    std::sync::Mutex::new(Vec::new());

pub(super) fn before_publication(channel: u64) {
    let callback = {
        let mut hooks = PUBLICATION_HOOKS.lock().unwrap();
        hooks
            .iter_mut()
            .find(|hook| hook.channel == channel)
            .and_then(|hook| {
                hook.seen += 1;
                (hook.seen == hook.nth).then_some(())
            })
            .map(|()| {
                let at = hooks
                    .iter()
                    .position(|hook| hook.channel == channel)
                    .unwrap();
                hooks.remove(at).callback
            })
    };
    if let Some(callback) = callback {
        callback();
    }
}

fn install_publication_hook(channel: u64, nth: usize, callback: PublicationCallback) {
    let mut hooks = PUBLICATION_HOOKS.lock().unwrap();
    assert!(!hooks.iter().any(|hook| hook.channel == channel));
    hooks.push(PublicationHook {
        channel,
        nth,
        seen: 0,
        callback,
    });
}

pub(super) fn isolated(test: &str) -> bool {
    const CHILD: &str = "ADK_A2_OWNER_TEST_CHILD";
    let test = test
        .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
        .unwrap_or(test);
    if std::env::var(CHILD).ok().as_deref() == Some(test) {
        return true;
    }
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, test)
        .env(
            crate::services::tui_o::cutover::test_override::CHILD_ENV,
            "1",
        )
        .env(
            crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
            "[]",
        )
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("AGENTDESK_CONFIG")
        .output()
        .unwrap();
    let logs = format!(
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    eprintln!("{logs}");
    assert!(result.status.success(), "{logs}");
    assert!(
        logs.contains("1 passed; 0 failed; 0 ignored;"),
        "child did not execute exactly one test: {logs}"
    );
    false
}

pub(super) async fn reached(notification: &tokio::sync::Notify) {
    tokio::time::timeout(Duration::from_secs(18), notification.notified())
        .await
        .unwrap();
}

pub(super) struct PrivateHost {
    socket: PathBuf,
    _namespace: crate::services::platform::tmux::session_inventory::tests::ChannelNamespace,
}
impl PrivateHost {
    fn new(rig: &Rig) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let socket = rig
            .root
            .path()
            .join(format!("tmux-{}", unsafe { libc::getuid() }))
            .join("default");
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        std::fs::set_permissions(
            socket.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let namespace =
            crate::services::platform::tmux::session_inventory::tests::ChannelNamespace::new(
                rig.channel.get(),
                socket.clone(),
            );
        Self {
            socket,
            _namespace: namespace,
        }
    }
    pub(super) fn command(&self, args: &[&str]) {
        let output = crate::services::platform::binary_resolver::runtime_command("tmux")
            .unwrap()
            .args(["-S"])
            .arg(&self.socket)
            .args(["-u", "-f", "/dev/null"])
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        assert!(output.status.success(), "private tmux {args:?}: {output:?}");
    }
    pub(super) fn start(&self, name: &str) {
        self.command(&["new-session", "-d", "-s", name, "sleep 600"]);
    }
    fn shutdown(&self) {
        let _ = crate::services::platform::binary_resolver::runtime_command("tmux")
            .unwrap()
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .output();
    }
}
impl Drop for PrivateHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(super) struct Fixture {
    // Restore process overrides before Rig releases the shared environment lock.
    _env: Vec<crate::config::TestEnvVarGuard>,
    pub(super) rig: Rig,
    pub(super) registry: Arc<HealthRegistry>,
    pub(super) foreign: Vec<Arc<SharedData>>,
    pub(super) foreign_name: String,
    pub(super) foreign_channel: ChannelId,
    pub(super) provider: ProviderKind,
    pub(super) host: PrivateHost,
    pub(super) ctx: poise::serenity_prelude::Context,
    pub(super) mock: discord_mock::DiscordMockState,
    pub(super) app: axum::Router,
    server: tokio::task::JoinHandle<()>,
    api_server: tokio::task::JoinHandle<()>,
    pump: tokio::task::JoinHandle<()>,
    _turn: TestConfirmation,
}
impl Fixture {
    pub(super) async fn connect_and_migrate_fixture(id: u64, provider: ProviderKind) -> Self {
        let _ = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .try_init();
        let live = crate::config_live_reload::test_live_guard(
            crate::config::test_env_lock::acquire_shared_test_env_lock(),
        );
        let mut rig = Rig::with_live_guard(id, false, false, live).await;
        let registry = Arc::new(HealthRegistry::new());
        let root = rig.root.path();
        let config_path = crate::runtime_layout::config_file_path(root);
        let codex_home = root.join("codex-home");
        let claude_home = root.join("claude-home");
        std::fs::create_dir_all(codex_home.join("sessions")).unwrap();
        std::fs::create_dir_all(claude_home.join("projects")).unwrap();
        let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
        let env = vec![
            set("AGENTDESK_CONFIG", &config_path),
            set("TMPDIR", root),
            set("CODEX_HOME", &codex_home),
            set("CLAUDE_CONFIG_DIR", &claude_home),
            set("AGENTDESK_CLAUDE_PATH", &root.join("provider-must-not-run")),
        ];
        let primary_name = if provider == ProviderKind::Codex {
            "adk-codex-tui-e2e".into()
        } else {
            format!("s1e-{id}")
        };
        let foreign_channel = ChannelId::new(id + 99);
        let foreign_basis = format!("a2-foreign-{id}");
        let foreign_name = ProviderKind::Codex.build_tmux_session_name(&foreign_basis);
        let mut config = crate::config_live_reload::current()
            .unwrap()
            .as_ref()
            .clone();
        config.agents[0].provider = provider.as_str().into();
        config.agents[0].channels = Default::default();
        config.agents[0].channels.insert(
            provider.as_str(),
            crate::config::AgentChannel::Detailed(crate::config::AgentChannelConfig {
                id: Some(id.to_string()),
                name: Some(primary_name.clone()),
                runtime: Some("tui".into()),
                ..Default::default()
            }),
        );
        let mut foreign_agent = config.agents[0].clone();
        foreign_agent.id = "a2-foreign".into();
        foreign_agent.provider = "codex".into();
        foreign_agent.channels = Default::default();
        foreign_agent.channels.insert(
            "codex",
            crate::config::AgentChannel::Detailed(crate::config::AgentChannelConfig {
                id: Some(foreign_channel.to_string()),
                name: Some(foreign_basis),
                runtime: Some("tui".into()),
                ..Default::default()
            }),
        );
        config.agents.push(foreign_agent);
        crate::config_live_reload::install(config.clone());
        std::fs::write(&config_path, serde_yaml::to_string(&config).unwrap()).unwrap();
        {
            let caller = Arc::get_mut(&mut rig.shared).unwrap();
            caller.provider = provider.clone();
            caller.token_hash = "discord_a6d1408a5a1d12ca".into();
            caller.health_registry = Arc::downgrade(&registry);
        }
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        {
            let mut core = rig.shared.core.lock().await;
            let session = core.sessions.get_mut(&rig.channel).unwrap();
            session.channel_name = Some(primary_name);
            session.current_path = Some(workspace.display().to_string());
        }
        {
            let mut settings = rig.shared.settings.write().await;
            settings.provider = provider.clone();
            settings.owner_user_id = Some(100);
            settings.allowed_user_ids = vec![200];
            settings.allowed_channel_ids = vec![id, foreign_channel.get()];
        }
        let mock = discord_mock::DiscordMockState::new();
        mock.park_first_placeholder.store(false, Ordering::SeqCst);
        mock.allow_channel(id);
        mock.allow_channel(foreign_channel.get());
        let (proxy, gateway, server) = discord_mock::start(mock.clone()).await;
        let ctx = discord_mock::serenity_context(proxy, gateway).await;
        rig.shared
            .http
            .cached_serenity_ctx
            .set(ctx.clone())
            .unwrap();
        rig.shared
            .http
            .cached_bot_token
            .set("test-token".into())
            .unwrap();
        let pool = rig.shared.pg_pool.clone().unwrap();
        let api_config = crate::config::Config::default();
        let engine = crate::engine::PolicyEngine::new(&api_config).unwrap();
        let broadcast = crate::eventbus::new_broadcast();
        let buffer = crate::eventbus::spawn_batch_flusher(broadcast.clone());
        let app = crate::server::routes::api_router_with_pg(
            engine,
            api_config,
            broadcast,
            buffer,
            Some(registry.clone()),
            Some(pool.clone()),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        Arc::get_mut(&mut rig.shared).unwrap().api_port = port;
        crate::services::discord::internal_api::init(port, Some(pool.clone()));
        let serving = axum::Router::new().nest("/api", app.clone());
        let api_server = tokio::spawn(async move {
            axum::serve(
                listener,
                serving.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });
        registry
            .register(provider.as_str().into(), rig.shared.clone())
            .await;
        registry
            .register_http(provider.as_str().into(), ctx.http.clone())
            .await;
        let mut foreign = Vec::new();
        for token in ["discord_e4bdfa034f57d568", "discord_8153e29f029b147c"] {
            let mut runtime = crate::services::discord::make_shared_data_for_tests_with_storage(
                Some(pool.clone()),
            );
            let mutable = Arc::get_mut(&mut runtime).unwrap();
            mutable.provider = ProviderKind::Codex;
            mutable.token_hash = token.into();
            mutable.health_registry = Arc::downgrade(&registry);
            runtime.settings.write().await.provider = ProviderKind::Codex;
            registry.register("codex".into(), runtime.clone()).await;
            foreign.push(runtime);
        }
        let host = PrivateHost::new(&rig);
        host.start(&foreign_name);
        crate::services::tmux_common::write_tmux_runtime_kind_marker(
            &foreign_name,
            crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
        )
        .unwrap();
        crate::services::tmux_common::write_tmux_channel_binding(
            &foreign_name,
            foreign_channel.get(),
        )
        .unwrap();
        let pump = tokio::spawn(async move {
            loop {
                sqlx::query(
                    "UPDATE message_outbox SET status='sent', sent_at=NOW() WHERE status='pending'",
                )
                .execute(&pool)
                .await
                .unwrap();
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        Self {
            rig,
            registry,
            foreign,
            foreign_name,
            foreign_channel,
            provider,
            host,
            ctx,
            mock,
            app,
            server,
            api_server,
            pump,
            _env: env,
            _turn: TestConfirmation::new(id),
        }
    }
    pub(super) fn produce_owners(&self) {
        for runtime in &self.foreign {
            assert_eq!(
                crate::services::discord::tui_prompt_relay::run_codex_rehydrate_pass_for_tests(
                    runtime,
                    &self.foreign_name
                ),
                Some(self.foreign_channel.get()),
                "the actual Codex producer did not restore its owner"
            );
        }
        assert!(
            self.rig
                .shared
                .tmux_watchers
                .owner_channel_for_tmux_session(&self.foreign_name)
                .is_none()
        );
    }
    pub(super) async fn observed(&self) -> Observed {
        let answer = activity_now(&self.rig.shared, &self.provider, self.rig.channel).await;
        if answer.activity == Activity::Unknown {
            let ports = Arc::new(LivePorts {
                root: Some(self.rig.root.path().to_owned()),
            });
            if let Ok(snapshot) =
                Snapshot::collect(&self.rig.shared, &self.provider, self.rig.channel, ports).await
            {
                eprintln!(
                    "A2_UNKNOWN scope={:?} exact_found={} canonical_found={} inflight={}",
                    snapshot.scope(&self.rig.shared).result,
                    matches!(snapshot.exact, HostedLookup::Found(_)),
                    matches!(snapshot.canonical, HostedLookup::Found(_)),
                    snapshot.files.row.as_ref().is_ok_and(Option::is_some)
                );
            }
        }
        answer
    }
    pub(super) fn disk_ids(&self) -> Vec<u64> {
        crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
            &self.provider,
            &self.rig.shared.token_hash,
            self.rig.channel,
        )
        .0
        .iter()
        .map(|item| item.message_id.get())
        .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.pump.abort();
        self.api_server.abort();
        self.server.abort();
        self.host.shutdown();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a2_codex_two_token_producer_agreement_and_second_only_are_idle_pg() {
    if !isolated(concat!(
        module_path!(),
        "::a2_codex_two_token_producer_agreement_and_second_only_are_idle_pg"
    )) {
        return;
    }
    let fixture = Fixture::connect_and_migrate_fixture(69_184_101, ProviderKind::Claude).await;
    assert_eq!(
        fixture.observed().await,
        observed(Activity::Unknown, "session_identity_unresolved")
    );
    fixture.produce_owners();
    assert_eq!(
        fixture.observed().await,
        observed(Activity::Idle, "no_session"),
        "two token registries agreeing on one channel must not conflict"
    );
    fixture.foreign[0]
        .tmux_watchers
        .clear_restored_owner_for_tmux_session(&fixture.foreign_name);
    assert_eq!(
        fixture.foreign[0]
            .tmux_watchers
            .owner_channel_for_tmux_session(&fixture.foreign_name),
        None
    );
    assert_eq!(
        fixture.foreign[1]
            .tmux_watchers
            .owner_channel_for_tmux_session(&fixture.foreign_name),
        Some(fixture.foreign_channel)
    );
    assert_eq!(
        fixture.observed().await,
        observed(Activity::Idle, "no_session"),
        "the second runtime for one provider must be observed"
    );
    assert!(fixture.mock.unhandled.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn a2_actual_reading_rejects_owner_aba_and_new_runtime_pg() {
    if !isolated(concat!(
        module_path!(),
        "::a2_actual_reading_rejects_owner_aba_and_new_runtime_pg"
    )) {
        return;
    }
    let fixture = Fixture::connect_and_migrate_fixture(69_184_102, ProviderKind::Claude).await;
    fixture.produce_owners();
    let reading = reading_now(&fixture.rig.shared, &fixture.provider, fixture.rig.channel).await;
    assert_eq!(reading.observed, observed(Activity::Idle, "no_session"));
    assert!(reading.publish_if_current(|| ()).await.is_some());
    fixture.foreign[1]
        .tmux_watchers
        .restore_owner_channel_for_tmux_session(&fixture.foreign_name, fixture.rig.channel);
    fixture.foreign[1]
        .tmux_watchers
        .restore_owner_channel_for_tmux_session(&fixture.foreign_name, fixture.foreign_channel);
    let calls = AtomicUsize::new(0);
    assert!(
        reading
            .publish_if_current(|| calls.fetch_add(1, Ordering::SeqCst))
            .await
            .is_none()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let mut replacement = crate::services::discord::make_shared_data_for_tests_with_storage(
        fixture.rig.shared.pg_pool.clone(),
    );
    let mutable = Arc::get_mut(&mut replacement).unwrap();
    mutable.provider = ProviderKind::Codex;
    mutable.token_hash = fixture.foreign[1].token_hash.clone();
    mutable.health_registry = Arc::downgrade(&fixture.registry);
    assert_eq!(
        crate::services::discord::tui_prompt_relay::run_codex_rehydrate_pass_for_tests(
            &replacement,
            &fixture.foreign_name
        ),
        Some(fixture.foreign_channel.get())
    );
    let reading = reading_now(&fixture.rig.shared, &fixture.provider, fixture.rig.channel).await;
    assert_eq!(reading.observed.activity, Activity::Idle);
    fixture.registry.register("codex".into(), replacement).await;
    assert!(
        reading
            .publish_if_current(|| calls.fetch_add(1, Ordering::SeqCst))
            .await
            .is_none()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn a2_final_publication_rejects_unrelated_owner_epoch_change_pg() {
    if !isolated(concat!(
        module_path!(),
        "::a2_final_publication_rejects_unrelated_owner_epoch_change_pg"
    )) {
        return;
    }
    let fixture = Fixture::connect_and_migrate_fixture(69_184_103, ProviderKind::Claude).await;
    fixture.produce_owners();
    let reading = reading_now(&fixture.rig.shared, &fixture.provider, fixture.rig.channel).await;
    assert_eq!(reading.observed, observed(Activity::Idle, "no_session"));
    let changed = Arc::new(AtomicUsize::new(0));
    let changed_hook = changed.clone();
    let foreign = fixture.foreign[1].clone();
    let owner = fixture.foreign_channel;
    install_publication_hook(
        fixture.rig.channel.get(),
        2,
        Box::new(move || {
            foreign
                .tmux_watchers
                .restore_owner_channel_for_tmux_session("a2-unrelated-not-in-inventory", owner);
            changed_hook.fetch_add(1, Ordering::SeqCst);
        }),
    );
    let calls = AtomicUsize::new(0);
    assert!(
        reading
            .publish_if_current(|| calls.fetch_add(1, Ordering::SeqCst))
            .await
            .is_none(),
        "a final-fence epoch change published the old Idle"
    );
    assert_eq!(
        changed.load(Ordering::SeqCst),
        1,
        "the last preflight hook was not reached"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        !PUBLICATION_HOOKS
            .lock()
            .unwrap()
            .iter()
            .any(|hook| hook.channel == fixture.rig.channel.get())
    );
}

#[path = "clear_owner_race_tests.rs"]
mod clear_owner_race_tests;

#[path = "live_owner_nonce_tests.rs"]
mod nonce_tests;
