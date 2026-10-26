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
        let app = axum::Router::new()
            .route(
                "/api/dispatched-sessions/webhook",
                axum::routing::post(crate::services::dispatched_sessions::hook_session_with_query),
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
            if self.starts() > 0
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
        let context = admission::ReplayAdmissionContext::production(&host.pool, "node-p4", admission::incarnation()).for_channel(&channel.to_string());
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
            working_dir: host._workspace.path().to_str().unwrap(), system_prompt: None,
            allowed_tools: &[], cancel: token.clone(), codex_observation: None, remote_profile: None,
            tmux_session_name: Some("p4-owned-fixture"), teardown: None, host: &turn_host,
            channel_id: channel.get(), model: None, native_fast_mode: None, codex_goals: None,
            compact_percent: None, compact_lower_bound_tokens: 0, compact_token_limit: None,
            cache_ttl_minutes: None, dispatch_type: None, force_fresh: false, submission: None,
        };
        let (sender, _receiver) = std::sync::mpsc::channel();
        let error = provider_dispatch::execute(turn, start, sender).unwrap_err();
        assert!(error.contains("replay driver scope changed"), "{error}");
        assert!(token.replay_attempt().is_none(), "gate precedes permit spend and native input");
        assert_eq!(host.starts(), 0);
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
            let acknowledged_unknown: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM intake_outbox WHERE replay_retry_of_nonce IS NOT NULL AND replay_disposition='started_unclassified')")
                .fetch_one(&host.pool).await.unwrap();
            if acknowledged_unknown { break; }
            assert!(tokio::time::Instant::now()<deadline, "exact begin never reached the lost-ACK boundary");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        host.settled(channel).await;
        assert_eq!(host.starts(), 1, "a committed-but-unacknowledged begin cannot execute");
        assert!(host.shared.core.lock().await.sessions[&channel].memento_context_loaded, "local context reset runs only after ACK");
        host.close().await;
    });
}
