//! Real health reservation, webhook and CLI witnesses for non-TUI replay starts.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::admin_host_guard::tests::{api_child, intake_replay_rt};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::{health, mailbox_snapshot};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

fn child(name: &str) -> bool {
    api_child(name.strip_prefix("agentdesk::").unwrap_or(name))
}

struct Host {
    shared: Arc<SharedData>,
    registry: Arc<health::HealthRegistry>,
    pool: sqlx::PgPool,
    servers: Vec<tokio::task::JoinHandle<()>>,
    calls: std::path::PathBuf,
    channels: Vec<ChannelId>,
    _workspace: tempfile::TempDir,
    vars: Vec<crate::config::TestEnvVarGuard>,
    db: TestPostgresDb,
    _legacy: crate::services::tui_o::cutover::test_override::ChannelsGuard,
    root: crate::config::TestRuntimeRootGuard,
}

impl Host {
    async fn new(channels: &[ChannelId]) -> Self {
        Self::with_storage(channels, true).await
    }

    async fn with_storage(channels: &[ChannelId], storage: bool) -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_test_writer()
            .try_init();
        let legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let workspace = tempfile::tempdir().unwrap();
        let config_path = workspace.path().join("fixture.yaml");
        std::fs::write(&config_path, "server: {}\ndata: {dir: data}\nagents: []\n").unwrap();
        let cli = workspace.path().join("claude");
        let tmux = workspace.path().join("tmux");
        let calls = workspace.path().join("calls");
        let wrapper = workspace.path().join("process-wrapper");
        std::fs::write(&wrapper, r#"#!/usr/bin/env python3
import sys, pathlib, subprocess, json
args = sys.argv[1:]
assert args[0] == 'tmux-wrapper'
value = lambda key: args[args.index(key)+1]
command = args[args.index('--')+1:]
result = subprocess.run(command, input=pathlib.Path(value('--prompt-file')).read_bytes(), capture_output=True)
output = result.stdout
if result.returncode:
    output += (json.dumps({'type':'result','is_error':True,'result':result.stderr.decode()})+'\n').encode()
pathlib.Path(value('--output-file')).write_bytes(output)
sys.exit(result.returncode)
"#).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(&tmux, "#!/nonexistent/p4-tmux-fixture-interpreter\n").unwrap();
        std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(&cli, format!(
            "#!/bin/sh\nprintf 'start\\n' >> '{}'\ncat > '{}.prompt'\nprintf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"fixture answer\",\"session_id\":\"73300000-0000-4000-8000-000000000004\"}}'\n",
            calls.display(), calls.display()
        )).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
        let vars = vec![
            crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(
                workspace.path(),
            ),
            set("AGENTDESK_CONFIG", &config_path),
            set("AGENTDESK_CLAUDE_PATH", &cli),
            set("AGENTDESK_TMUX_PATH", &tmux),
            set("CLAUDE_CONFIG_DIR", workspace.path()),
            set("ADK_TEST_PROCESS_WRAPPER_EXE", &wrapper),
        ];
        crate::services::platform::tmux::invalidate_availability_cache();
        let config = crate::config::Config::default();
        crate::services::tui_o::channel_policy::install(&config).unwrap();
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let version: String = sqlx::query_scalar("SELECT version()")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(version.contains("PostgreSQL"));
        let broadcast_tx = crate::eventbus::new_broadcast();
        let app_state = crate::app_state::AppState {
            pg_pool: Some(pool.clone()),
            engine: crate::engine::PolicyEngine::new(&config).unwrap(),
            config: Arc::new(config),
            batch_buffer: crate::eventbus::spawn_batch_flusher(broadcast_tx.clone()),
            broadcast_tx,
            health_registry: None,
            cluster_instance_id: None,
        };
        let app =
            axum::Router::new()
                .route(
                    "/api/dispatched-sessions/webhook",
                    axum::routing::post(
                        |state: axum::extract::State<crate::app_state::AppState>,
                         query: axum::extract::Query<
                            crate::services::dispatched_sessions::HookSessionQuery,
                        >,
                         body: axum::Json<
                            crate::services::dispatched_sessions::HookSessionBody,
                        >| async move {
                            let terminal = body.status.as_deref() == Some("idle");
                            let result =
                                crate::services::dispatched_sessions::hook_session_with_query(
                                    state, query, body,
                                )
                                .await;
                            if terminal
                                && std::env::var("ADK_TEST_REPLAY_PEER_ROLE").ok().as_deref()
                                    == Some("owner")
                            {
                                println!("P5_TERMINAL_SNAPSHOT_BARRIER");
                                tokio::time::sleep(Duration::from_millis(500)).await;
                            }
                            result
                        },
                    ),
                )
                .fallback(|| async { axum::Json(serde_json::json!({"ok": true})) })
                .with_state(app_state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        crate::services::discord::internal_api::init(port, None);
        let mock = discord_mock::DiscordMockState::new();
        for channel in channels {
            mock.allow_channel(channel.get());
        }
        let (proxy, gateway, discord) = discord_mock::start(mock).await;
        let ctx = discord_mock::serenity_context(proxy, gateway).await;
        let registry = Arc::new(health::HealthRegistry::new());
        registry
            .register_http("claude".into(), ctx.http.clone())
            .await;
        registry
            .set_utility_bot_http_for_tests(
                crate::services::discord::bot_role::UtilityBotRole::Notify,
                ctx.http.clone(),
            )
            .await;
        let mut shared = crate::services::discord::make_shared_data_for_tests_with_storage(
            storage.then(|| pool.clone()),
        );
        {
            let inner = Arc::get_mut(&mut shared).unwrap();
            inner.api_port = port;
            inner.token_hash = crate::services::discord::settings::discord_token_hash("test-token");
            inner.health_registry = Arc::downgrade(&registry);
        }
        shared
            .http
            .cached_serenity_ctx
            .set(ctx)
            .unwrap_or_else(|_| panic!("context already set"));
        shared
            .http
            .cached_bot_token
            .set("test-token".into())
            .unwrap();
        for &channel in channels {
            crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, "")
                .await;
            let mut core = shared.core.lock().await;
            let session = core.sessions.get_mut(&channel).unwrap();
            session.channel_name = Some(format!("headless-fixture-{channel}"));
            session.current_path = Some(workspace.path().display().to_string());
        }
        registry.register("claude".into(), shared.clone()).await;
        Self {
            shared,
            registry,
            pool,
            servers: vec![server, discord],
            calls,
            channels: channels.to_vec(),
            _workspace: workspace,
            vars,
            db,
            _legacy: legacy,
            root,
        }
    }

    async fn start(
        &self,
        channel: ChannelId,
        source: &str,
        metadata: Option<serde_json::Value>,
        reservation: health::HeadlessAgentTurnReservation,
    ) -> Result<HeadlessTurnStartOutcome, HeadlessTurnStartError> {
        health::start_reserved_headless_agent_turn_with_owner_channel(
            &self.registry,
            channel,
            channel,
            ProviderKind::Claude,
            "caller rendered original".into(),
            Some(source.into()),
            metadata,
            Some(format!("headless-fixture-{channel}")),
            None,
            reservation,
        )
        .await
    }

    fn starts(&self) -> usize {
        std::fs::read_to_string(&self.calls)
            .unwrap_or_default()
            .lines()
            .count()
    }

    async fn settled(&self, channel: ChannelId) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            crate::server::outbox_worker::drain_message_outbox_once(
                &Arc::new(self.pool.clone()),
                &self.registry,
                "p4-owned-fixture",
            )
            .await;
            let (protected_pending, delivery_pending):(bool,bool)=sqlx::query_as(
                "SELECT EXISTS(SELECT 1 FROM intake_outbox WHERE channel_id=$1 AND replay_disposition IN ('started_unclassified','startup_failed_no_effect')),
                        EXISTS(SELECT 1 FROM message_outbox WHERE target=$2 AND source='headless_turn' AND status IN ('pending','sending'))")
                .bind(channel.to_string()).bind(format!("channel:{channel}")).fetch_one(&self.pool).await.unwrap();
            if self.starts() > 0
                && !protected_pending
                && !delivery_pending
                && mailbox_snapshot(&self.shared, channel)
                    .await
                    .cancel_token
                    .is_none()
                && !crate::services::discord::live_bridge::is_live(
                    &ProviderKind::Claude,
                    channel.get(),
                )
            {
                crate::server::outbox_worker::drain_message_outbox_once(
                    &Arc::new(self.pool.clone()),
                    &self.registry,
                    "p4-final-fixture-drain",
                )
                .await;
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "headless turn did not settle; native starts={}; token={}; live={}; diagnostic={}",
                self.starts(),
                mailbox_snapshot(&self.shared, channel)
                    .await
                    .cancel_token
                    .is_some(),
                crate::services::discord::live_bridge::is_live(
                    &ProviderKind::Claude,
                    channel.get()
                ),
                std::fs::read_to_string(
                    crate::services::discord::runtime_store::agentdesk_root()
                        .unwrap()
                        .join("debug/claude.log")
                )
                .unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn native_started(&self, expected: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while self.starts() < expected {
            assert!(
                tokio::time::Instant::now() < deadline,
                "native positive control {expected}: {}",
                self.starts()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(self.starts(), expected);
    }

    fn direct_metadata(reservation: &health::HeadlessAgentTurnReservation) -> serde_json::Value {
        serde_json::json!({"routine_id":"p4-direct-request", "turn_id":reservation.turn_id(),
            "execution_strategy":"fresh", "execution_mode":"process"})
    }

    async fn close(mut self) {
        for &channel in &self.channels {
            let name = ProviderKind::Claude
                .build_tmux_session_name(&format!("headless-fixture-{channel}"));
            if let Some(handle) = crate::services::session_backend::remove_process_session(&name) {
                crate::services::session_backend::terminate_process_handle(handle);
            }
            self.shared.mailboxes.remove_fixture_for_test(channel);
        }
        for server in self.servers.drain(..) {
            server.abort();
        }
        self.pool.close().await;
        let Self { db, vars, root, .. } = self;
        drop(db);
        drop(vars);
        drop(root);
    }
}

#[test]
fn p4_u3_health_reservation_webhook_and_native_start_pg() {
    let name = concat!(
        module_path!(),
        "::p4_u3_health_reservation_webhook_and_native_start_pg"
    );
    if !child(name) {
        return;
    }
    intake_replay_rt().block_on(async {
        let channels = [ChannelId::new(7_330_401), ChannelId::new(7_330_402), ChannelId::new(7_330_403)];
        let host = Host::new(&channels).await;
        for (index, (channel, source)) in channels.into_iter().zip(["routine", "api", "agent-recovery"]).enumerate() {
            let reservation = health::reserve_headless_agent_turn(channel);
            let turn_id = reservation.turn_id().to_owned();
            let metadata = (source == "routine").then(|| serde_json::json!({
                "routine_id": "fixture-routine", "routine_run_id": "durable-run-1", "turn_id": turn_id,
                "execution_strategy": "fresh", "execution_mode": "process"
            }));
            let outcome = host.start(channel, source, metadata, reservation.clone()).await.unwrap();
            assert_eq!(outcome.turn_id, turn_id, "reservation must not be reissued");
            host.native_started(index + 1).await;
            if source == "routine" { host.settled(channel).await; }
            assert_eq!(host.starts(), index + 1, "{source}: native positive control");
            let rows: Vec<(Option<String>,)> = sqlx::query_as("SELECT session_key FROM sessions WHERE channel_id = $1")
                .bind(channel.to_string()).fetch_all(&host.pool).await.unwrap();
            assert!(!rows.is_empty(), "session binding must come from the real webhook");
            let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM intake_outbox WHERE replay_disposition IS NOT NULL")
                .fetch_one(&host.pool).await.unwrap();
            assert_eq!(receipts, 0, "off must perform no canonical write");
            println!("U3 source={source} turn_id={turn_id} session_rows={rows:?} cumulative_native_starts={} canonical_receipts={receipts}", host.starts());
        }
        assert_eq!(host.starts(), 3);
        host.close().await;
    });
}

#[test]
fn p4_active_headless_start_registers_canonical_original_pg() {
    let name = concat!(
        module_path!(),
        "::p4_active_headless_start_registers_canonical_original_pg"
    );
    if !child(name) {
        return;
    }
    intake_replay_rt().block_on(async {
        let channel = ChannelId::new(7_330_404);
        let host = Host::new(&[channel]).await;
        let _active = crate::services::discord::router::intake_dispatch::replay_admission::test_activation::activate(channel.get());
        let reservation = health::reserve_headless_agent_turn(channel);
        let outcome = host.start(channel, "api", Some(Host::direct_metadata(&reservation)), reservation.clone()).await;
        assert!(outcome.is_ok(), "protected headless start was refused: {outcome:?}");
        host.settled(channel).await;
        let originals: Vec<(String,)> = sqlx::query_as("SELECT user_text FROM intake_outbox WHERE replay_disposition IS NOT NULL")
            .fetch_all(&host.pool).await.unwrap();
        assert_eq!(originals, vec![("caller rendered original".into(),)]);
        assert_eq!(host.starts(), 1);
        host.close().await;
    });
}

impl Host {
    fn stale_cli(&self, activity: Option<&str>) {
        let before = match activity {
            Some("text") => {
                "printf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"partial headless answer\"}]}}'\n"
            }
            Some("tool") => {
                "printf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Bash\",\"input\":{}}]}}'\n"
            }
            None => "",
            _ => panic!("unknown activity"),
        };
        let script = format!(
            "#!/bin/sh\nprintf 'start\\n' >> '{}'\ncat > '{}.prompt'\n{}if [ $(wc -l < '{}') -eq 1 ]; then\nprintf '%s' 'Error: No conversation found' >&2\nexit 1\nfi\nprintf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"fixture answer\",\"session_id\":\"73300000-0000-4000-8000-000000000004\"}}'\n",
            self.calls.display(),
            self.calls.display(),
            before,
            self.calls.display()
        );
        std::fs::write(self._workspace.path().join("claude"), script).unwrap();
    }

    async fn disposition(&self, expected: &str) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(35);
        loop {
            crate::server::outbox_worker::drain_message_outbox_once(
                &Arc::new(self.pool.clone()),
                &self.registry,
                "p5-disposition-fixture-drain",
            )
            .await;
            let row: Option<(String, serde_json::Value)> = sqlx::query_as(
                "SELECT replay_disposition, replay_preserved FROM intake_outbox WHERE replay_disposition IS NOT NULL ORDER BY id LIMIT 1")
                .fetch_optional(&self.pool).await.unwrap();
            if let Some((disposition, preserved)) = row
                && disposition == expected
            {
                return preserved;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "never reached {expected}; native starts={}",
                self.starts()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[test]
fn p4_exact_headless_quiet_stale_keeps_reservation_and_metadata_pg() {
    let name = concat!(
        module_path!(),
        "::p4_exact_headless_quiet_stale_keeps_reservation_and_metadata_pg"
    );
    if !child(name) {
        return;
    }
    intake_replay_rt().block_on(async {
        let channel = ChannelId::new(7_330_405);
        let host = Host::new(&[channel]).await;
        host.stale_cli(None);
        let _active = crate::services::discord::router::intake_dispatch::replay_admission::test_activation::activate(channel.get());
        let reservation = health::reserve_headless_agent_turn(channel);
        let turn_id = reservation.turn_id().to_owned();
        let metadata = serde_json::json!({
            "routine_id": "same-routine", "routine_run_id": "same-durable-run", "turn_id": turn_id,
            "execution_strategy": "fresh", "execution_mode": "process", "delivery_bot": "claude", "silent": false
        });
        let outcome = host.start(channel, "routine", Some(metadata.clone()), reservation).await;
        assert!(outcome.is_ok(), "protected headless start was refused: {outcome:?}");
        assert_eq!(outcome.unwrap().turn_id, turn_id);
        let preserved = host.disposition("classified_normal").await;
        host.settled(channel).await;
        assert_eq!(host.starts(), 2, "one exact retry, same headless path");
        let responses: i64 = sqlx::query_scalar("SELECT count(*) FROM message_outbox WHERE source='headless_turn' AND content LIKE '%fixture answer%' AND status='sent'")
            .fetch_one(&host.pool).await.unwrap();
        assert_eq!(responses, 1, "quiet stale yields one actually delivered response");
        let receipts: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT replay_request_key, user_text, replay_retry_of_nonce FROM intake_outbox WHERE replay_disposition IS NOT NULL")
            .fetch_all(&host.pool).await.unwrap();
        assert_eq!(receipts.len(), 1, "retry cannot issue a new synthetic identity");
        assert!(receipts[0].0.contains(&turn_id));
        assert_eq!(receipts[0].1, "caller rendered original");
        assert!(receipts[0].2.is_some(), "second native start needs the exact permit");
        assert_eq!(preserved["projection"]["binding"]["metadata"], metadata);
        assert_eq!(preserved["projection"]["binding"]["source"], "routine");
        let prompt = std::fs::read_to_string(host.calls.with_extension("prompt")).unwrap();
        assert!(prompt.contains("caller rendered original"));
        assert_eq!(crate::services::discord::replay_policy::activation::reads(&format!("headless_retry_wait:{channel}")),1,"the actual gateway must use the owner wait");
        {
        let deadline=tokio::time::Instant::now()+Duration::from_secs(5);
        while crate::services::discord::inflight::load_inflight_state(&ProviderKind::Claude,channel.get()).is_some() {
            crate::server::outbox_worker::drain_message_outbox_once(&Arc::new(host.pool.clone()),&host.registry,"p5-wait-fixture-drain").await;
            assert!(tokio::time::Instant::now()<deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let (receipt,message):(i64,String)=sqlx::query_as("SELECT id,user_msg_id FROM intake_outbox WHERE replay_disposition='classified_normal'").fetch_one(&host.pool).await.unwrap();
        let message=message.parse::<u64>().unwrap();
        let mut retiring=InflightTurnState::new(ProviderKind::Claude,channel.get(),None,7,message,7330599,"original".into(),None,None,None,None,0);
        retiring.replay_receipt_id=Some(receipt);
        crate::services::discord::inflight::save_inflight_state_create_new(&retiring).unwrap();
        let waiting=super::replay_start::wait_for_retry_owner(&host.shared,&ProviderKind::Claude,channel,receipt,message);
        tokio::pin!(waiting);
        assert!(tokio::time::timeout(Duration::from_millis(100),&mut waiting).await.is_err(),"an idle mailbox does not retire its own snapshot");
        let path=crate::services::discord::runtime_store::agentdesk_root().unwrap().join(format!("runtime/discord_inflight/claude/{channel}.json"));
        std::fs::rename(&path,host._workspace.path().join("retired-own-snapshot")).unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(2),waiting).await.unwrap());
        retiring.user_msg_id+=1;
        crate::services::discord::inflight::save_inflight_state_create_new(&retiring).unwrap();
        assert!(!tokio::time::timeout(Duration::from_secs(2),super::replay_start::wait_for_retry_owner(&host.shared,&ProviderKind::Claude,channel,receipt,message)).await.unwrap(),"a foreign snapshot is refused, never retired by retry");
        assert!(path.exists());
        std::fs::rename(&path,host._workspace.path().join("retained-foreign-snapshot")).unwrap();
        assert_eq!(host.starts(),2,"the wait does not grant another start");
        }
        host.close().await;
    });
}

#[test]
fn p4_headless_activity_stale_withheld_does_not_restart_pg() {
    let name = concat!(
        module_path!(),
        "::p4_headless_activity_stale_withheld_does_not_restart_pg"
    );
    if !child(name) {
        return;
    }
    intake_replay_rt().block_on(async {
        for (id, activity) in [(7_330_406, "text"), (7_330_407, "tool")] {
            let channel = ChannelId::new(id);
            let host = Host::new(&[channel]).await;
            host.stale_cli(Some(activity));
            let _active = crate::services::discord::router::intake_dispatch::replay_admission::test_activation::activate(channel.get());
            let reservation = health::reserve_headless_agent_turn(channel);
            let outcome = host.start(channel, "api", Some(Host::direct_metadata(&reservation)), reservation.clone()).await;
            assert!(outcome.is_ok(), "protected headless start was refused: {outcome:?}");
            host.disposition("withheld").await;
            host.settled(channel).await;
            assert_eq!(host.starts(), 1, "{activity}: effectful stale cannot rerun");
            let repeated = host.start(channel, "api", Some(Host::direct_metadata(&reservation)), reservation).await;
            assert!(!repeated.is_ok_and(|result| result.status == HeadlessTurnStartStatus::Started), "held request cannot start on re-entry");
            assert_eq!(host.starts(), 1);
            let notices: i64 = sqlx::query_scalar("SELECT count(*) FROM message_outbox WHERE source = 'lifecycle_notifier' AND reason_code = 'stale_replay_withheld'")
                .fetch_one(&host.pool).await.unwrap();
            assert_eq!(notices, 1, "durable event, not the synthetic headless gateway");
            let row: (serde_json::Value, String) = sqlx::query_as("SELECT replay_preserved, replay_hold_reason FROM intake_outbox WHERE replay_disposition='withheld'")
                .fetch_one(&host.pool).await.unwrap();
            assert!(row.0["stale_replay_notice"]["hold_nonce"].is_string());
            assert_eq!(row.0["recovery_snapshot"]["user_text"], "caller rendered original");
            assert!(row.1.contains("activity"), "{activity}: actual reader activity, not a broken fixture");
            if activity == "text" {
                assert!(row.0["recovery_snapshot"]["full_response"].as_str().unwrap().contains("partial headless answer"));
            }
            host.settled(channel).await;
            let sent: i64 = sqlx::query_scalar("SELECT count(*) FROM message_outbox WHERE reason_code='stale_replay_withheld' AND status='sent'")
                .fetch_one(&host.pool).await.unwrap();
            assert_eq!(sent, 1, "production outbox drain delivered the durable event");
            host.close().await;
        }
    });
}

#[test]
fn p4_headless_distinct_reservations_same_body_do_not_collapse_pg() {
    let name = concat!(
        module_path!(),
        "::p4_headless_distinct_reservations_same_body_do_not_collapse_pg"
    );
    if !child(name) {
        return;
    }
    intake_replay_rt().block_on(async {
        let channel = ChannelId::new(7_330_408);
        let host = Host::new(&[channel]).await;
        let _active = crate::services::discord::router::intake_dispatch::replay_admission::test_activation::activate(channel.get());
        let mut ids = Vec::new();
        for index in 0..2 {
            let reservation = health::reserve_headless_agent_turn(channel);
            ids.push(reservation.turn_id().to_owned());
            let outcome = host.start(channel, "api", Some(Host::direct_metadata(&reservation)), reservation).await;
            assert!(outcome.is_ok(), "protected headless start was refused: {outcome:?}");
            assert_eq!(outcome.unwrap().status, HeadlessTurnStartStatus::Started);
            host.native_started(index + 1).await;
            host.settled(channel).await;
        }
        assert_ne!(ids[0], ids[1]);
        let count: i64 = sqlx::query_scalar("SELECT count(DISTINCT replay_request_key) FROM intake_outbox WHERE replay_disposition IS NOT NULL")
            .fetch_one(&host.pool).await.unwrap();
        assert_eq!(count, 2, "same body and source do not identify the same request");
        assert_eq!(host.starts(), 2);
        host.close().await;
    });
}

#[test]
fn p4_off_headless_without_store_still_starts_pg() {
    let name = concat!(
        module_path!(),
        "::p4_off_headless_without_store_still_starts_pg"
    );
    if !child(name) {
        return;
    }
    intake_replay_rt().block_on(async {
        let channel = ChannelId::new(7_330_409);
        let host = Host::with_storage(&[channel], false).await;
        assert!(host.shared.pg_pool.is_none());
        let reservation = health::reserve_headless_agent_turn(channel);
        let outcome = host.start(channel, "api", None, reservation).await.unwrap();
        assert_eq!(outcome.status, HeadlessTurnStartStatus::Started);
        host.native_started(1).await;
        let receipts: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM intake_outbox WHERE replay_disposition IS NOT NULL",
        )
        .fetch_one(&host.pool)
        .await
        .unwrap();
        assert_eq!(receipts, 0);
        host.close().await;
    });
}

#[test]
fn p4_reopened_store_new_state_consumes_held_reservation_pg() {
    let name = concat!(
        module_path!(),
        "::p4_reopened_store_new_state_consumes_held_reservation_pg"
    );
    if !child(name) {
        return;
    }
    intake_replay_rt().block_on(async {
        let channel = ChannelId::new(7_330_410);
        let mut host = Host::new(&[channel]).await;
        host.stale_cli(Some("text"));
        let _active = crate::services::discord::router::intake_dispatch::replay_admission::test_activation::activate(channel.get());
        let reservation = health::reserve_headless_agent_turn(channel);
        let metadata = Host::direct_metadata(&reservation);
        host.start(channel, "api", Some(metadata.clone()), reservation.clone()).await.unwrap();
        host.disposition("withheld").await;
        host.settled(channel).await;
        let before: (i64, serde_json::Value) = sqlx::query_as("SELECT id, replay_preserved FROM intake_outbox WHERE replay_disposition='withheld'")
            .fetch_one(&host.pool).await.unwrap();
        let reopened = host.db.connect_and_migrate().await;
        let mut new_shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(reopened));
        assert!(new_shared.core.lock().await.sessions.is_empty());
        let registry = Arc::new(health::HealthRegistry::new());
        {
            let inner = Arc::get_mut(&mut new_shared).unwrap();
            inner.api_port = host.shared.api_port;
            inner.token_hash = host.shared.token_hash.clone();
            inner.health_registry = Arc::downgrade(&registry);
        }
        new_shared.http.cached_serenity_ctx.set(host.shared.http.cached_serenity_ctx.get().unwrap().clone())
            .unwrap_or_else(|_| panic!("context already set"));
        new_shared.http.cached_bot_token.set("test-token".into()).unwrap();
        registry.register("claude".into(), new_shared.clone()).await;
        host.registry = registry;
        host.shared = new_shared;
        let result = host.start(channel, "api", Some(metadata), reservation).await.unwrap();
        assert_eq!(result.status, HeadlessTurnStartStatus::Consumed);
        assert_eq!(host.starts(), 1);
        let after: Vec<(i64, serde_json::Value)> = sqlx::query_as("SELECT id, replay_preserved FROM intake_outbox WHERE replay_disposition IS NOT NULL")
            .fetch_all(&host.pool).await.unwrap();
        assert_eq!(after, vec![before], "new process memory cannot mint an execution or notice");
        host.close().await;
    });
}

#[test]
fn p4_final_dispatch_refuses_a_changed_driver_scope_pg() {
    let name = concat!(
        module_path!(),
        "::p4_final_dispatch_refuses_a_changed_driver_scope_pg"
    );
    if !child(name) {
        return;
    }
    intake_replay_rt().block_on(async {
        use crate::services::discord::router::intake_dispatch::replay_admission as admission;
        use crate::services::discord::replay_policy::permit::EffectTarget;
        use super::super::provider_dispatch::{self, StreamingTurn};
        let channel = ChannelId::new(7_330_411);
        let host = Host::new(&[channel]).await;
        let _active = admission::test_activation::activate(channel.get());
        let input = admission::discord_input("claude", channel.get(), 7330411, [], "original");
        let context = admission::ReplayAdmissionContext::production(&host.pool, "node-p4", admission::incarnation()).for_scope(&channel.to_string(),"claude","legacy-prompt","owned_direct",false);
        let projection = admission::PreparedEffectProjection {
            target: EffectTarget::ProviderStart { provider: "claude".into(), channel: channel.to_string() },
            input_hash: admission::input_hash("prepared"),
            binding: serde_json::json!({"driver_scope":"legacy-prompt", "runtime_scope":"owned_direct"}),
            session_key: None,
        };
        let start = admission::admit_start(&context, &input, projection, None).await.unwrap();
        let token = Arc::new(CancelToken::new());
        let turn_host = crate::services::turn_host::TurnHost::Tmux;
        let provider = ProviderKind::Claude;
        let turn = StreamingTurn {
            pool: Some(&host.pool), provider: &provider, prompt: "prepared", session_id: None,
            shared: None,
            working_dir: host._workspace.path().to_str().unwrap(), system_prompt: None,
            allowed_tools: &[], cancel: token.clone(), codex_observation: None,
            tmux_session_name: Some("p4-owned-fixture"), teardown: None, host: &turn_host,
            channel_id: channel.get(), model: None, native_fast_mode: None, codex_goals: None,
 effort: None,
            compact_percent: None, compact_lower_bound_tokens: 0, compact_token_limit: None,
            cache_ttl_minutes: None, dispatch_type: None, force_fresh: false, submission: None,
        };
        let (sender, _receiver) = std::sync::mpsc::channel();
        let error = provider_dispatch::execute(turn, start, sender, None).unwrap_err();
        assert!(error.contains("replay driver scope changed"), "{error}");
        assert!(token.replay_attempt().is_none(), "gate precedes permit spend and native input");
        assert_eq!(host.starts(), 0);
        host.disposition("withheld").await;
        let notices:i64=sqlx::query_scalar("SELECT count(*) FROM message_outbox WHERE reason_code='stale_replay_withheld'").fetch_one(&host.pool).await.unwrap();
        assert_eq!(notices,1,"a begun scope refusal must notify");
        host.close().await;
    });
}

#[test]
fn p4_exact_begin_ack_loss_never_clears_or_starts_again_pg() {
    let name = concat!(
        module_path!(),
        "::p4_exact_begin_ack_loss_never_clears_or_starts_again_pg"
    );
    if !child(name) {
        return;
    }
    intake_replay_rt().block_on(async {
        use crate::services::discord::router::intake_dispatch::replay_admission as admission;
        let channel = ChannelId::new(7_330_412);
        let host = Host::new(&[channel]).await;
        host.stale_cli(None);
        let release = host._workspace.path().join("stale-go");
        let cli = host._workspace.path().join("claude");
        let script = std::fs::read_to_string(&cli).unwrap().replace("if [ $(wc -l", &format!("while [ ! -f '{}' ]; do sleep 0.01; done\nif [ $(wc -l", release.display()));
        std::fs::write(cli, script).unwrap();
        let _active = admission::test_activation::activate(channel.get());
        let reservation = health::reserve_headless_agent_turn(channel);
        host.start(channel, "api", Some(Host::direct_metadata(&reservation)), reservation).await.unwrap();
        host.native_started(1).await;
        host.shared.core.lock().await.sessions.get_mut(&channel).unwrap()
            .memento_context_loaded = true;
        admission::test_activation::lose_next_ack(channel.get());
        std::fs::write(release, "1").unwrap();
        let deadline = tokio::time::Instant::now()+Duration::from_secs(15);
        loop {
            let acknowledged_unknown: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM intake_outbox WHERE replay_retry_of_nonce IS NOT NULL AND replay_disposition IN ('started_unclassified','withheld'))")
                .fetch_one(&host.pool).await.unwrap();
            if acknowledged_unknown { break; }
            assert!(tokio::time::Instant::now()<deadline, "exact begin never reached the lost-ACK boundary");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        host.disposition("withheld").await;
        host.settled(channel).await;
        assert_eq!(host.starts(), 1, "a committed-but-unacknowledged begin cannot execute");
        assert!(host.shared.core.lock().await.sessions[&channel].memento_context_loaded, "local context reset runs only after ACK");
        host.close().await;
    });
}

#[test]
fn p5_env_on_exact_retry_replies_to_original_source_pg() {
    let name = concat!(
        module_path!(),
        "::p5_env_on_exact_retry_replies_to_original_source_pg"
    );
    if !child(name) {
        return;
    }
    intake_replay_rt().block_on(async {
        let channel = ChannelId::new(7_330_501);
        let mut host = Host::new(&[channel]).await;
        host.stale_cli(None);
        host.vars.push(crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_STALE_REPLAY_V1", std::path::Path::new("1")));
        host.vars.push(crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_STALE_REPLAY_CHANNELS", std::path::Path::new(&channel.to_string())));
        assert!(crate::services::discord::replay_policy::activation::production_requested(&channel.to_string()));
        let reservation = health::reserve_headless_agent_turn(channel);
        let turn_id = reservation.turn_id().to_owned();
        let metadata = serde_json::json!({
            "routine_id": "same-routine", "routine_run_id": "same-durable-run", "turn_id": turn_id,
            "execution_strategy": "fresh", "execution_mode": "process", "delivery_bot": "claude", "silent": false
        });
        let outcome = host.start(channel, "routine", Some(metadata.clone()), reservation).await;
        assert!(outcome.is_ok(), "protected headless start was refused: {outcome:?}");
        assert_eq!(outcome.unwrap().turn_id, turn_id);
        host.native_started(1).await;
        let registered: i64 = sqlx::query_scalar("SELECT count(*) FROM intake_outbox WHERE replay_disposition IS NOT NULL").fetch_one(&host.pool).await.unwrap();
        assert_eq!(registered, 1, "real env supplier must register before the first effect");
        let preserved = host.disposition("classified_normal").await;
        host.settled(channel).await;
        assert_eq!(host.starts(), 2, "one exact retry, same headless path");
        let responses: i64 = sqlx::query_scalar("SELECT count(*) FROM message_outbox WHERE source='headless_turn' AND content LIKE '%fixture answer%' AND status='sent' AND target=$1")
            .bind(format!("channel:{channel}")).fetch_one(&host.pool).await.unwrap();
        assert_eq!(responses, 1, "quiet stale yields one actually delivered response");
        let receipts: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT replay_request_key, user_text, replay_retry_of_nonce FROM intake_outbox WHERE replay_disposition IS NOT NULL")
            .fetch_all(&host.pool).await.unwrap();
        assert_eq!(receipts.len(), 1, "retry cannot issue a new synthetic identity");
        assert!(receipts[0].0.contains(&turn_id));
        assert_eq!(receipts[0].1, "caller rendered original");
        assert!(receipts[0].2.is_some(), "second native start needs the exact permit");
        assert_eq!(preserved["projection"]["binding"]["metadata"], metadata);
        assert_eq!(preserved["projection"]["binding"]["source"], "routine");
        let prompt = std::fs::read_to_string(host.calls.with_extension("prompt")).unwrap();
        assert!(prompt.contains("caller rendered original"));
        host.close().await;
    });
}

#[test]
fn p5_off_no_rows_keeps_effects_with_two_replay_reads_pg() {
    if !child(concat!(
        module_path!(),
        "::p5_off_no_rows_keeps_effects_with_two_replay_reads_pg"
    )) {
        return;
    }
    intake_replay_rt().block_on(async {
        let channel=ChannelId::new(7_330_502);
        let host=Host::new(&[channel]).await;
        let reservation=health::reserve_headless_agent_turn(channel);
        host.start(channel,"routine",Some(serde_json::json!({"routine_id":"p5-off","routine_run_id":"p5-off-run","turn_id":reservation.turn_id(),"execution_strategy":"fresh","execution_mode":"process","delivery_bot":"claude","silent":false})),reservation).await.unwrap();
        host.settled(channel).await;
        assert_eq!(host.starts(),1);
        assert_eq!(crate::services::discord::replay_policy::activation::reads(&channel.to_string()),2);
        let counts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM intake_outbox WHERE replay_disposition IS NOT NULL),(SELECT count(*) FROM message_outbox WHERE reason_code='stale_replay_withheld'),(SELECT count(*) FROM message_outbox WHERE source='headless_turn' AND content LIKE '%fixture answer%' AND status='sent')").fetch_one(&host.pool).await.unwrap();
        assert_eq!(counts,(0,0,1));
        host.close().await;
    });
}

#[test]
fn p5_unsupported_process_backend_keeps_existing_effects_pg() {
    if !child(concat!(
        module_path!(),
        "::p5_unsupported_process_backend_keeps_existing_effects_pg"
    )) {
        return;
    }
    intake_replay_rt().block_on(async {
        let channel=ChannelId::new(7_330_503);
        let host=Host::new(&[channel]).await;
        let _active=crate::services::discord::router::replay_test_activation::activate(channel.get());
        let reservation=health::reserve_headless_agent_turn(channel);
        host.start(channel,"api",None,reservation).await.unwrap();
        host.native_started(1).await;
        assert_eq!(host.starts(),1);
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM intake_outbox WHERE replay_disposition IS NOT NULL").fetch_one(&host.pool).await.unwrap();
        assert_eq!(count,0,"unsupported process backend must remain legacy");
        println!("P5 PROCESS_BACKEND native=1 receipt=0 completion=NOT_ASSERTED(U3 FAIL) no_effect=NOT_PROVEN");
        host.close().await;
    });
}

#[test]
fn p5_off_sql_failure_is_retryable_with_the_original_reservation_pg() {
    if !child(concat!(
        module_path!(),
        "::p5_off_sql_failure_is_retryable_with_the_original_reservation_pg"
    )) {
        return;
    }
    intake_replay_rt().block_on(async {
        let channel=ChannelId::new(7_330_504);
        let host=Host::new(&[channel]).await;
        let reservation=health::reserve_headless_agent_turn(channel);
        sqlx::query("ALTER TABLE intake_outbox RENAME TO p5_temporarily_unavailable").execute(&host.pool).await.unwrap();
        let failed=host.start(channel,"routine",Some(Host::direct_metadata(&reservation)),reservation.clone()).await;
        assert!(matches!(&failed,Err(HeadlessTurnStartError::Conflict(reason)) if reason.contains("retry")),"{failed:?}");
        assert_eq!(host.starts(),0,"lookup failure cannot be read as no protected rows");
        sqlx::query("ALTER TABLE p5_temporarily_unavailable RENAME TO intake_outbox").execute(&host.pool).await.unwrap();
        host.start(channel,"routine",Some(Host::direct_metadata(&reservation)),reservation).await.unwrap();
        host.settled(channel).await;
        assert_eq!(host.starts(),1,"retry uses the original reservation");
        host.close().await;
    });
}

#[test]
fn p5_exact_unsupported_scope_is_held_without_legacy_reregistration_pg() {
    if !child(concat!(
        module_path!(),
        "::p5_exact_unsupported_scope_is_held_without_legacy_reregistration_pg"
    )) {
        return;
    }
    intake_replay_rt().block_on(async {
        use crate::services::discord::router::intake_dispatch::replay_admission as admission;
        use crate::services::discord::replay_policy::{self,ReplayDecision,ExactHandoff};
        use crate::services::discord::replay_policy::live::Seen;
        use crate::services::discord::replay_policy::permit::EffectTarget;
        let channel=ChannelId::new(7_330_518);let host=Host::new(&[channel]).await;
        let _active=crate::services::discord::router::replay_test_activation::activate(channel.get());
        let input=admission::discord_input("claude",channel.get(),7330518,[],"original");
        let context=admission::ReplayAdmissionContext::production(&host.pool,"p5",admission::incarnation()).for_scope(&channel.to_string(),"claude","legacy-prompt","owned_direct",false);
        let target=EffectTarget::ProviderStart{provider:"claude".into(),channel:channel.to_string()};
        let projection=|| admission::PreparedEffectProjection {target:target.clone(),input_hash:admission::input_hash("prepared"),binding:serde_json::json!({"driver_scope":"legacy-prompt","runtime_scope":"owned_direct"}),session_key:None};
        let start=admission::admit_start(&context,&input,projection(),None).await.unwrap();
        let token=CancelToken::new();start.spend(&target,"prepared",&token).unwrap();
        let live=token.replay_attempt().unwrap();
        for fact in [Seen::StdoutEof,Seen::Exited{waited:true},Seen::StderrDrained] {live.record(fact);}
        let (attempt,seen)=live.take_for_decision(Duration::from_millis(50)).await.unwrap();
        let ReplayDecision::AllowStartupRetry(permit)=replay_policy::decide_stale(&host.pool,Some(attempt),&seen,None).await else {panic!("exact permit");};
        let unstarted=permit.abandonment();
        let mut state=InflightTurnState::new(ProviderKind::Claude,channel.get(),None,7,7330518,7331518,"original".into(),None,None,None,None,0);
        let (result,handoff)=replay_policy::hand_over(permit,async {
            super::super::intake_turn::replay_start::common::begin(&host.shared,&ProviderKind::Claude,&mut state,input,projection(),replay_policy::take_continuation(),super::super::provider_dispatch::replay_scope::Scope{driver:crate::services::provider_hosting::ProviderSessionDriver::LegacyPrompt,runtime:"remote"}).await
        }).await;
        assert!(result.is_err());assert_eq!(handoff,ExactHandoff::Refused);
        unstarted.abandon("exact retry handoff refused").await;
        host.disposition("withheld").await;
        assert_eq!(host.starts(),0,"a refused scope never invokes a provider");
        let counts:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM intake_outbox WHERE replay_disposition IS NOT NULL),(SELECT count(*) FROM message_outbox WHERE reason_code='stale_replay_withheld')").fetch_one(&host.pool).await.unwrap();
        assert_eq!(counts,(1,1));host.close().await;
    });
}

#[test]
fn p5_process_start_env_variants_use_the_production_selector_pg() {
    const MARKER: &str = "ADK_TEST_STALE_REPLAY_VARIANT";
    let name = concat!(
        module_path!(),
        "::p5_process_start_env_variants_use_the_production_selector_pg"
    )
    .strip_prefix("agentdesk::")
    .unwrap();
    if let Ok(label) = std::env::var(MARKER) {
        intake_replay_rt().block_on(async {
            let channel=ChannelId::new(7_330_519);let host=Host::new(&[channel]).await;
            let selected=crate::services::discord::replay_policy::activation::production_requested(&channel.to_string());
            assert_eq!(selected,label=="on");
            if selected {host.stale_cli(None);}
            let reservation=health::reserve_headless_agent_turn(channel);
            let metadata=serde_json::json!({"routine_id":"p5-env","routine_run_id":"p5-env-run","turn_id":reservation.turn_id(),"execution_strategy":"fresh","execution_mode":"process","delivery_bot":"claude","silent":false});
            host.start(channel,"routine",Some(metadata),reservation).await.unwrap();host.settled(channel).await;
            let counts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM intake_outbox WHERE replay_disposition IS NOT NULL),(SELECT count(*) FROM message_outbox WHERE reason_code='stale_replay_withheld'),(SELECT count(*) FROM message_outbox WHERE source='headless_turn' AND content LIKE '%fixture answer%' AND status='sent' AND target=$1)").bind(format!("channel:{channel}")).fetch_one(&host.pool).await.unwrap();
            assert_eq!(counts,(if selected {1_i64} else {0},0,1));assert_eq!(host.starts(),if selected {2}else{1});
            println!("P5_ENV_VARIANT label={label} selected={selected} native={} receipt={} notice={} original_source_reply={}",host.starts(),counts.0,counts.1,counts.2);
            host.close().await;
        });
        return;
    }
    for (label, flag, channels) in [
        ("unset", None, Some("7330519")),
        ("zero", Some("0"), Some("7330519")),
        ("false", Some("false"), Some("7330519")),
        ("invalid", Some("invalid"), Some("7330519")),
        ("missing-list", Some("1"), None),
        ("bad-list", Some("1"), Some("7330519,bad")),
        ("unlisted", Some("1"), Some("7330520")),
        ("on", Some("1"), Some("7330519")),
    ] {
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", name, "--nocapture"])
            .env(MARKER, label)
            .env_remove("AGENTDESK_STALE_REPLAY_V1")
            .env_remove("AGENTDESK_STALE_REPLAY_CHANNELS");
        if let Some(flag) = flag {
            cmd.env("AGENTDESK_STALE_REPLAY_V1", flag);
        }
        if let Some(channels) = channels {
            cmd.env("AGENTDESK_STALE_REPLAY_CHANNELS", channels);
        }
        let output = cmd.output().unwrap();
        assert!(
            output.status.success(),
            "{label}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<_> = stdout
            .lines()
            .filter(|l| l.starts_with("P5_ENV_VARIANT"))
            .collect();
        assert_eq!(
            lines.len(),
            1,
            "{label}: child must execute exactly one witness"
        );
        println!("{}", lines[0]);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn p5_live_foreign_boot_does_not_preempt_the_owner_pg() {
    const ROLE: &str = "ADK_TEST_REPLAY_PEER_ROLE";
    const DATABASE: &str = "ADK_TEST_REPLAY_PEER_DATABASE";
    const SCENARIO: &str = "ADK_TEST_REPLAY_PEER_SCENARIO";
    let name = concat!(
        module_path!(),
        "::p5_live_foreign_boot_does_not_preempt_the_owner_pg"
    )
    .strip_prefix("agentdesk::")
    .unwrap();
    let role = std::env::var(ROLE).unwrap_or_default();
    if role == "peer" && std::env::var(SCENARIO).ok().as_deref() == Some("same-root-overlap") {
        let root = std::env::var("ADK_TEST_REPLAY_PEER_ROOT").unwrap();
        assert!(
            crate::cli::dcserver::replay_recovery::acquire_for_tests(std::path::Path::new(&root))
                .is_err()
        );
        println!("P5_SAME_ROOT_OVERLAP_REFUSED");
        return;
    }
    if role == "peer" {
        intake_replay_rt().block_on(async {
            let _root = crate::config::TestRuntimeRootGuard::new();
            let pool = sqlx::PgPool::connect(&std::env::var(DATABASE).unwrap())
                .await
                .unwrap();
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
                pool.clone(),
            ));
            let http = Arc::new(serenity::Http::new("fixture-peer"));
            assert!(
                !crate::services::discord::replay_policy::activation::production_requested(
                    "7330522"
                )
            );
            assert!(
                crate::services::discord::inflight::load_inflight_states(&ProviderKind::Claude)
                    .is_empty()
            );
            if std::env::var(SCENARIO).ok().as_deref() == Some("db-direct") {
                let id: i64 = sqlx::query_scalar(
                    "SELECT id FROM intake_outbox WHERE replay_disposition='started_unclassified'",
                )
                .fetch_one(&pool)
                .await
                .unwrap();
                let result = crate::db::replay_disposition::write::recover_hold_notice(
                    &pool,
                    id,
                    "claude",
                    "7330522",
                    None,
                    None,
                    crate::db::replay_disposition::hold_notice::NoticeRouting {
                        bot: "notify",
                        session_key: None,
                    },
                )
                .await;
                assert!(matches!(
                    result,
                    Err(crate::db::replay_disposition::write::WriteError::CasMiss)
                ));
            } else {
                crate::services::discord::recovery_engine::restore_inflight_turns(
                    &http,
                    &shared,
                    &ProviderKind::Claude,
                )
                .await;
            }
            pool.close().await;
            println!("P5_PEER_SCAN_DONE off=true local_inflight=0");
        });
        return;
    }
    if role != "owner" {
        for scenario in [
            "quiet-control",
            "quiet-peer",
            "db-direct",
            "same-root-overlap",
        ] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name, "--nocapture"])
                .env(ROLE, "owner")
                .env(SCENARIO, scenario)
                .env("AGENTDESK_INSTANCE_ID", "p5-live-owner")
                .env("AGENTDESK_STALE_REPLAY_V1", "1")
                .env("AGENTDESK_STALE_REPLAY_CHANNELS", "7330522")
                .output()
                .unwrap();
            print!("{}", String::from_utf8_lossy(&output.stdout));
            assert!(
                output.status.success(),
                "{scenario} owner process: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    }
    intake_replay_rt().block_on(async {
        let scenario=std::env::var(SCENARIO).unwrap();
        let channel=ChannelId::new(7_330_522);let host=Host::new(&[channel]).await;
        let locked_root=crate::services::discord::runtime_store::agentdesk_root().unwrap();
        std::fs::create_dir_all(locked_root.join("runtime")).unwrap();
        let authority=crate::cli::dcserver::replay_recovery::acquire_for_tests(&locked_root).unwrap();
        host.shared.restart.replay_restart.set(authority).unwrap();
        assert!(crate::services::discord::replay_policy::activation::production_requested(&channel.to_string()));
        let metadata=|reservation:&health::HeadlessAgentTurnReservation,run:&str| serde_json::json!({
            "routine_id":"p5-live-owner","routine_run_id":run,"turn_id":reservation.turn_id(),
            "execution_strategy":"fresh","execution_mode":"process","delivery_bot":"claude","silent":false});
        let control=health::reserve_headless_agent_turn(channel);
        host.start(channel,"routine",Some(metadata(&control,"control")),control).await.unwrap();
        host.settled(channel).await;
        let control_deadline=tokio::time::Instant::now()+Duration::from_secs(5);
        while crate::services::discord::inflight::load_inflight_state(&ProviderKind::Claude,channel.get()).is_some() {
            assert!(tokio::time::Instant::now()<control_deadline,"control snapshot must finish cleanup");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let control_replies:i64=sqlx::query_scalar("SELECT count(*) FROM message_outbox WHERE source='headless_turn' AND status='sent' AND content LIKE '%fixture answer%'")
            .fetch_one(&host.pool).await.unwrap();
        assert_eq!((host.starts(),control_replies),(1,1),"healthy ordinary-completion control");

        let release=host._workspace.path().join("live-release");let pid_file=host._workspace.path().join("live-pid");
        let script=format!(r#"#!/bin/sh
printf 'start\n' >> '{}'
printf '%s' "$$" > '{}'
cat > '{}.prompt'
if [ $(wc -l < '{}') -eq 2 ]; then
while [ -d '{}' ] && [ ! -f '{}' ]; do sleep 0.01; done
printf '%s' 'Error: No conversation found' >&2
exit 1
fi
printf '%s\n' '{{"type":"result","subtype":"success","result":"fixture answer","session_id":"73300000-0000-4000-8000-000000000005"}}'
"#,host.calls.display(),pid_file.display(),host.calls.display(),host.calls.display(),host._workspace.path().display(),release.display());
        std::fs::write(host._workspace.path().join("claude"),script).unwrap();
        let subject=health::reserve_headless_agent_turn(channel);
        host.start(channel,"routine",Some(metadata(&subject,"subject")),subject).await.unwrap();
        host.native_started(2).await;
        let pid=std::fs::read_to_string(&pid_file).unwrap();
        let alive=std::path::Path::new(&format!("/proc/{pid}")).exists()
            && mailbox_snapshot(&host.shared,channel).await.cancel_token.is_some() && !release.exists();
        let before:(i64,String,String)=sqlx::query_as("SELECT id,replay_disposition,target_instance_id FROM intake_outbox WHERE replay_disposition IS NOT NULL ORDER BY id DESC LIMIT 1")
            .fetch_one(&host.pool).await.unwrap();
        sqlx::query("INSERT INTO worker_nodes (instance_id,process_id,status,last_heartbeat_at) VALUES ($1,$2,'online',NOW()) ON CONFLICT(instance_id) DO UPDATE SET status='online',last_heartbeat_at=NOW()")
            .bind(&before.2).bind(std::process::id() as i32).execute(&host.pool).await.unwrap();
        let peer=if scenario!="quiet-control" {
            Some(std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact",name,"--nocapture"]).env(ROLE,"peer").env(SCENARIO,&scenario).env("ADK_TEST_REPLAY_PEER_ROOT",&locked_root).env(DATABASE,&host.db.database_url)
                .env("AGENTDESK_INSTANCE_ID","p5-live-peer").env("AGENTDESK_STALE_REPLAY_V1","0")
                .env("AGENTDESK_STALE_REPLAY_CHANNELS","").output())
        } else {None};
        let after:String=sqlx::query_scalar("SELECT replay_disposition FROM intake_outbox WHERE id=$1").bind(before.0).fetch_one(&host.pool).await.unwrap();
        std::fs::write(&release,"finish").unwrap();
        let settled=tokio::time::timeout(Duration::from_secs(8),async {
            loop {
                crate::server::outbox_worker::drain_message_outbox_once(
                    &Arc::new(host.pool.clone()),&host.registry,"p5-retry-fixture-drain",
                ).await;
                let disposition:String=sqlx::query_scalar("SELECT replay_disposition FROM intake_outbox WHERE id=$1")
                    .bind(before.0).fetch_one(&host.pool).await.unwrap();
                if matches!(disposition.as_str(),"classified_normal"|"withheld") {break;}
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            host.settled(channel).await;
        }).await.is_ok();
        let final_disposition:String=sqlx::query_scalar("SELECT replay_disposition FROM intake_outbox WHERE id=$1").bind(before.0).fetch_one(&host.pool).await.unwrap();
        let replies:i64=sqlx::query_scalar("SELECT count(*) FROM message_outbox WHERE source='headless_turn' AND status='sent' AND content LIKE '%fixture answer%'")
            .fetch_one(&host.pool).await.unwrap();
        let notices:i64=sqlx::query_scalar("SELECT count(*) FROM message_outbox WHERE reason_code='stale_replay_withheld'").fetch_one(&host.pool).await.unwrap();
        let starts=host.starts();
        println!("P5_LIVE_FOREIGN scenario={scenario} before={} after={} final={final_disposition} owner={} native_alive={alive} native={starts} control_reply={control_replies} subject_reply={} notice={notices} settled={settled}",before.1,after,before.2,replies-control_replies);
        host.close().await;
        if let Some(peer)=peer {let peer=peer.unwrap();assert!(peer.status.success(),"peer failed: {}\n{}",String::from_utf8_lossy(&peer.stdout),String::from_utf8_lossy(&peer.stderr));}
        assert!(alive);assert_eq!(before.1,"started_unclassified");
        assert_eq!(after,"started_unclassified","an off peer cannot dispose a live foreign attempt");
        assert!(settled);assert_eq!(starts,3,"quiet stale still owes exactly one startup retry");
        assert_eq!(replies,2,"quiet stale still owes its original successful response");
        assert_eq!(notices,0,"no active owner has been lost");
    });
}

/// The real headless start of a scheduled snapshot turn marks its durable row with the
/// launch turn id as isolated, the mark every completer settles that delivery by.
#[test]
fn a_scheduled_snapshot_headless_start_marks_its_row_isolated_pg() {
    use crate::services::scheduled_messages::turn_completion::tests::launch_agent_delivery;
    let name = concat!(
        module_path!(),
        "::a_scheduled_snapshot_headless_start_marks_its_row_isolated_pg"
    );
    if !child(name) {
        return;
    }
    intake_replay_rt().block_on(async {
        let channel = ChannelId::new(7_420_501);
        let host = Host::new(&[channel]).await;
        let reservation = health::reserve_headless_agent_turn(channel);
        let turn_id = reservation.turn_id().to_owned();
        let launched =
            launch_agent_delivery(&host.pool, "i7420-headless", &turn_id, true, "fail").await;
        // Same metadata and snapshot label the scheduler passes on a snapshot launch.
        let label = format!("scheduled:{}", launched.message_id);
        let metadata = serde_json::json!({
            "agent_id": "i7420-headless", "scheduled_message_id": launched.message_id,
            "turn_id": turn_id, "parent_channel_id": channel.get().to_string(),
            "scheduled_snapshot_session_label": label,
        });
        let outcome = health::start_reserved_headless_agent_turn_with_owner_channel(
            &host.registry,
            channel,
            channel,
            ProviderKind::Claude,
            "scheduled prompt".into(),
            Some("scheduled_message".into()),
            Some(metadata),
            Some(format!("headless-fixture-{channel}")),
            Some(label),
            reservation,
        )
        .await
        .expect("scheduled headless start");
        assert_eq!(outcome.turn_id, turn_id);
        let row = crate::services::discord::inflight::load_inflight_state(
            &ProviderKind::Claude,
            channel.get(),
        )
        .expect("the started row");
        assert_eq!(
            row.scheduled,
            crate::services::discord::scheduled_turn::ScheduledTurnMark::Scheduled {
                turn_id,
                isolated: true,
                outcome: None,
            }
        );
        host.close().await;
    });
}

#[path = "replay_start_origin_tests.rs"]
mod origin_tests;
