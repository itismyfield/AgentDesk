//! Tests for `gateway_lease`.
//!
//! Split out of `gateway_lease.rs` (#4356): the file crossed the 700-LoC
//! namespace cap once these gained coverage of self-id resolution.

use super::super::gateway_lease_recovery::standby_retry_delay;
use super::create_gateway_self_fence_restart_request;
use super::*;
use crate::services::provider::ProviderKind;
use serde_json::json;

fn observability_capture() -> (tracing::subscriber::DefaultGuard, std::fs::File) {
    let file = tempfile::tempfile().unwrap();
    let reader = file.try_clone().unwrap();
    crate::logging::test_capture::pin_callsite_interest();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || file.try_clone().unwrap())
        .finish();
    (tracing::subscriber::set_default(subscriber), reader)
}

fn observability_logs(mut reader: std::fs::File) -> String {
    use std::io::{Read, Seek, SeekFrom};
    reader.seek(SeekFrom::Start(0)).unwrap();
    let mut logs = String::new();
    reader.read_to_string(&mut logs).unwrap();
    eprintln!("{logs}");
    logs
}

fn observability_breaker() -> GatewayHandbackBreaker {
    GatewayHandbackBreaker::with_sources(
        "claude",
        "discord_7465737467617465",
        None,
        || 0,
        Default::default,
    )
}

#[tokio::test]
async fn gateway_observability_snapshot_timeout_preserves_held_and_budget_pg() {
    use sqlx::Connection;

    let (_root, _live, _paths) = snapshot_root();
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let token = "discord_7465737467617465";
    let key = discord_gateway_lock_id(token);
    let held = try_acquire_discord_gateway_lease(&pool, token, &ProviderKind::Claude)
        .await
        .unwrap()
        .unwrap();
    // Only pooled diagnostics see this view; real lease tries use dedicated connections.
    sqlx::raw_sql(
        "CREATE SCHEMA diagnostic_timeout;
         CREATE VIEW diagnostic_timeout.pg_locks AS
         SELECT l.* FROM pg_catalog.pg_locks l
         CROSS JOIN (SELECT pg_advisory_xact_lock(914802699)) barrier",
    )
    .execute(&pool)
    .await
    .unwrap();
    let mut blocker = sqlx::PgConnection::connect_with(&pool.connect_options())
        .await
        .unwrap();
    let (_capture, reader) = observability_capture();
    for block_query in [true, false] {
        sqlx::query("SELECT pg_advisory_lock(914802699)")
            .execute(&mut blocker)
            .await
            .unwrap();
        let diagnostic_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path = diagnostic_timeout, pg_catalog")
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect_with((*pool.connect_options()).clone())
            .await
            .unwrap();
        let mut connection = diagnostic_pool.acquire().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        let occupied = if block_query {
            drop(connection);
            None
        } else {
            Some(connection)
        };
        let diagnostics = Diagnostics::default();
        diagnostics.context(&diagnostic_pool, "standby", "claude", key);
        let mut breaker = observability_breaker();
        let started = std::time::Instant::now();
        let (result, blocked) = tokio::join!(
            tokio::time::timeout(
                Duration::from_secs(3),
                try_acquire_observing_handback(
                    &diagnostic_pool,
                    token,
                    &ProviderKind::Claude,
                    &mut breaker,
                    &diagnostics,
                ),
            ),
            tokio::time::timeout(Duration::from_secs(3), async {
                while block_query {
                    let waiting: bool = sqlx::query_scalar(
                        "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                         WHERE pid = $1 AND wait_event = 'advisory'
                           AND query LIKE 'SELECT l.pid,%')",
                    )
                    .bind(pid)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                    if waiting {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }),
        );
        let elapsed = started.elapsed();
        let first = serde_json::to_value(diagnostics.projection()).unwrap();
        drop(occupied);
        sqlx::query("SELECT pg_advisory_unlock(914802699)")
            .execute(&mut blocker)
            .await
            .unwrap();
        let retry = if result.is_ok() {
            Some(
                tokio::time::timeout(
                    Duration::from_millis(300),
                    try_acquire_observing_handback(
                        &diagnostic_pool,
                        token,
                        &ProviderKind::Claude,
                        &mut breaker,
                        &diagnostics,
                    ),
                )
                .await,
            )
        } else {
            None
        };
        diagnostic_pool.close().await;
        blocked.expect("the real snapshot SELECT must reach the advisory lock wait");
        assert!(
            result
                .expect("diagnostic timeout must return the real held result within 3s")
                .unwrap()
                .is_none()
        );
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
        assert_eq!(first["last_result"], "held");
        assert_eq!(first["tries"], 1);
        assert_eq!(first["results"]["held"], 1);
        assert_eq!(first["snapshot_status"], "err");
        assert_eq!(first["snapshot_attempts"], 1);
        assert_eq!(first["snapshot_failures"], 1);
        assert!(first["holder"].is_null());
        assert!(retry.unwrap().unwrap().unwrap().is_none());
        let next = serde_json::to_value(diagnostics.projection()).unwrap();
        assert_eq!(next["tries"], 2);
        assert_eq!(next["last_result"], "held");
        assert_eq!(next["results"]["held"], 2);
        assert!(next["results"]["err"].is_null());
        assert!(next["results"]["refused_while_free"].is_null());
        assert_eq!(next["snapshot_attempts"], 1, "timeout consumes the budget");
        assert_eq!(next["snapshot_failures"], 1);
        eprintln!(
            "SNAPSHOT_TIMEOUT query_blocked={block_query} elapsed={elapsed:?} first={first} next={next}"
        );
    }
    assert_eq!(
        observability_logs(reader)
            .matches("⏳ GATEWAY-LEASE: waiting for adoption")
            .count(),
        2,
        "both timeout paths must reach the summary without duplicate minute lines"
    );
    blocker.close().await.unwrap();
    held.unlock().await.unwrap();
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn gateway_observability_a_nonpreferred_held_try_pg() {
    let (root, _live, _paths) = snapshot_root();
    let mut config = crate::config::Config::default();
    config.data.dir = root.path().join("data");
    config.cluster.enabled = true;
    config.cluster.instance_id = Some("backup".into());
    config.cluster.gateway_preferred_instance_id = Some("home".into());
    crate::config_live_reload::install(config);
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let token = "discord_7465737467617465";
    let held = try_acquire_discord_gateway_lease(&pool, token, &ProviderKind::Claude)
        .await
        .unwrap()
        .unwrap();
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let (_capture, reader) = observability_capture();
    let outcome = run_bot_acquire_gateway_lease(
        &shared,
        token,
        &ProviderKind::Claude,
        &Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        &Arc::new(std::sync::atomic::AtomicBool::new(true)),
        &Arc::new(health::HealthRegistry::new()),
        0,
        &mut observability_breaker(),
    )
    .await;
    assert!(matches!(outcome, GatewayLeaseOutcome::Standby));
    held.unlock().await.unwrap();
    pool.close().await;
    fixture.drop().await;
    let logs = observability_logs(reader);
    assert!(
        logs.contains("last_result=\"held\""),
        "actual false try must be recorded"
    );
    assert!(logs.contains("tries=1"), "count the real PostgreSQL try");
}

#[tokio::test]
async fn gateway_observability_b_waiter_refresh_timestamp_pg() {
    let (root, _live, _paths) = snapshot_root();
    let mut config = crate::config::Config::default();
    config.data.dir = root.path().join("data");
    config.cluster.enabled = true;
    let instance =
        crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
    config.cluster.instance_id = Some(instance.clone());
    config.cluster.gateway_preferred_instance_id = Some(instance.clone());
    crate::config_live_reload::install(config);
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    sqlx::query("INSERT INTO worker_nodes(instance_id,status,last_heartbeat_at,updated_at) VALUES ($1,'online',NOW(),'2000-01-01')")
        .bind(&instance).execute(&pool).await.unwrap();
    let token = "discord_7465737467617465";
    let held = try_acquire_discord_gateway_lease(&pool, token, &ProviderKind::Claude)
        .await
        .unwrap()
        .unwrap();
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let (_capture, reader) = observability_capture();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        acquire_as_preferred_gateway(
            &pool,
            token,
            &ProviderKind::Claude,
            &shared,
            &mut observability_breaker(),
        ),
    )
    .await;
    assert!(
        result.is_err(),
        "real competing holder keeps the preferred waiter blocked"
    );
    let updated: (bool, serde_json::Value) = sqlx::query_as(
        "SELECT updated_at > '2000-01-02',capabilities FROM worker_nodes WHERE instance_id=$1",
    )
    .bind(&instance)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(updated.0);
    assert!(
        crate::services::cluster::node_registry::node_awaits_gateway(
            &json!({"capabilities":updated.1}),
            "claude"
        )
    );
    held.unlock().await.unwrap();
    pool.close().await;
    fixture.drop().await;
    let logs = observability_logs(reader);
    assert!(
        logs.contains("waiter_refreshed_at="),
        "log successful production waiter publication time"
    );
    let waiting = logs
        .lines()
        .find(|line| line.contains("⏳ GATEWAY-LEASE"))
        .unwrap();
    for field in [
        "tries=",
        "last_result=",
        "preferred-poll",
        "holder:",
        "pid:",
        "backend_start:",
        "provider=",
        "advisory_key=",
    ] {
        assert!(waiting.contains(field), "missing {field}: {waiting}");
    }
    assert!(waiting.contains(&format!("advisory_key={}", discord_gateway_lock_id(token))));
}

#[test]
fn standby_retry_delay_stays_inside_thirty_to_sixty_seconds() {
    for _ in 0..128 {
        let delay = standby_retry_delay();
        assert!(delay >= Duration::from_secs(30));
        assert!(delay <= Duration::from_secs(60));
    }
}

#[test]
fn gateway_self_fence_publishes_process_restart_marker() {
    let root = tempfile::tempdir().expect("restart marker root");
    let nonce = "gateway-self-fence-test";
    assert!(
        create_gateway_self_fence_restart_request(root.path(), &ProviderKind::Codex, nonce)
            .expect("publish gateway self-fence request")
    );
    let marker = std::fs::read_to_string(root.path().join("restart_pending"))
        .expect("read canonical restart marker");
    assert!(marker.lines().any(|line| line == format!("nonce={nonce}")));
    assert!(
        marker
            .lines()
            .any(|line| line == "reason=gateway_lease_self_fence")
    );
    assert!(marker.lines().any(|line| line == "provider=codex"));
}

#[test]
fn failed_lease_does_not_authorize_worker_or_poller_startup() {
    assert!(!GatewayLeaseOutcome::Failed.starts_provider_runtime());
    assert!(GatewayLeaseOutcome::Standby.starts_provider_runtime());
    assert!(GatewayLeaseOutcome::Proceed(None).starts_provider_runtime());
}

fn cluster(enabled: bool, preferred: Option<&str>) -> crate::config::ClusterConfig {
    crate::config::ClusterConfig {
        enabled,
        gateway_preferred_instance_id: preferred.map(str::to_string),
        ..Default::default()
    }
}

#[test]
fn no_preference_configured_keeps_first_come_behavior() {
    assert!(resolve_gateway_preference(&cluster(true, None), "a".into()).is_none());
    // Blank / whitespace is treated as unset, not as an instance named "".
    assert!(resolve_gateway_preference(&cluster(true, Some("   ")), "a".into()).is_none());
}

#[test]
fn preference_is_ignored_when_clustering_is_off() {
    // A single-node deploy must never wait for a peer that cannot exist.
    assert!(
        resolve_gateway_preference(&cluster(false, Some("mac-book-release")), "a".into()).is_none()
    );
}

#[test]
fn preferred_node_recognizes_itself() {
    let pref = resolve_gateway_preference(
        &cluster(true, Some("  mac-book-release  ")),
        "mac-book-release".into(),
    )
    .expect("preference");
    assert_eq!(pref.preferred_instance_id, "mac-book-release");
    assert!(pref.self_is_preferred());
}

/// #4356. The tests above all *hand* the self-id to the pure function, so they
/// could not see that the caller derived it from
/// `resolve_self_instance_id_without_config()` — which returns `hostname-pid`
/// until `cluster::bootstrap` populates `SELF_INSTANCE_ID`, and the gateway
/// acquires its lease before that happens. The preferred node never recognized
/// itself in production.
///
/// `SELF_INSTANCE_ID` is deliberately left unset here: that is exactly the state
/// at gateway-acquire time, and the configured id must still win.
#[tokio::test]
async fn self_id_is_read_from_config_not_from_the_bootstrap_race() {
    let mut cluster = cluster(true, Some("mac-book-release"));
    cluster.instance_id = Some("mac-book-release".to_string());

    let self_id = resolve_self_instance_id_for_preference(&cluster).await;

    assert_eq!(self_id, "mac-book-release");
    let pref = resolve_gateway_preference(&cluster, self_id).expect("preference");
    assert!(
        pref.self_is_preferred(),
        "the configured preferred node must recognize itself before cluster bootstrap runs"
    );
}

#[tokio::test]
async fn configured_self_id_is_trimmed() {
    let mut cluster = cluster(true, Some("mac-book-release"));
    cluster.instance_id = Some("  mac-book-release  ".to_string());
    assert_eq!(
        resolve_self_instance_id_for_preference(&cluster).await,
        "mac-book-release"
    );
}

#[test]
fn non_preferred_node_knows_it_must_yield() {
    let pref = resolve_gateway_preference(
        &cluster(true, Some("mac-book-release")),
        "mac-mini-release".into(),
    )
    .expect("preference");
    assert!(!pref.self_is_preferred());
    assert_eq!(
        pref.yield_grace,
        Duration::from_secs(crate::config::ClusterConfig::default().gateway_yield_grace_secs)
    );
}

fn waiting_node(instance_id: &str, status: &str, waiting: &[&str]) -> serde_json::Value {
    json!({
        "instance_id": instance_id,
        "status": status,
        "capabilities": { "discord_gateway": { "waiting_providers": waiting } },
    })
}

#[test]
fn yields_only_to_a_preferred_node_that_wants_this_gateway() {
    let nodes = vec![
        waiting_node("mac-mini-release", "online", &[]),
        waiting_node("mac-book-release", "ONLINE", &["claude", "codex"]),
    ];
    assert!(should_yield_to_preferred(
        &nodes,
        "mac-book-release",
        "claude"
    ));
    assert!(should_yield_to_preferred(
        &nodes,
        "mac-book-release",
        "CODEX"
    ));
    // Same node, a provider it is not waiting for.
    assert!(!should_yield_to_preferred(
        &nodes,
        "mac-book-release",
        "gemini"
    ));
    assert!(!should_yield_to_preferred(
        &nodes,
        "ghost-release",
        "claude"
    ));
}

/// #4351 review (cdx, REJECT round 1). The preferred node's dcserver can be
/// up and heartbeating while its bot never starts — no token for this
/// provider, a startup failure, or an acquire that gave up. Yielding on
/// `status == "online"` alone handed the gateway to nobody: the holder
/// released, self-fenced, restarted, re-acquired, and yielded again — an
/// outage loop. Only a live `waiting_providers` advertisement may trigger a
/// yield.
#[test]
fn never_yields_to_a_preferred_node_that_is_merely_online() {
    let no_capability = vec![json!({"instance_id": "mac-book-release", "status": "online"})];
    assert!(!should_yield_to_preferred(
        &no_capability,
        "mac-book-release",
        "claude"
    ));

    let up_but_not_contending = vec![waiting_node("mac-book-release", "online", &[])];
    assert!(!should_yield_to_preferred(
        &up_but_not_contending,
        "mac-book-release",
        "claude"
    ));

    // Bot for the *other* provider is waiting; ours is not.
    let other_provider_only = vec![waiting_node("mac-book-release", "online", &["codex"])];
    assert!(!should_yield_to_preferred(
        &other_provider_only,
        "mac-book-release",
        "claude"
    ));
}

#[test]
fn never_yields_to_an_offline_or_malformed_preferred_node() {
    // A stale peer must not hold the gateway hostage.
    let offline = vec![waiting_node("mac-book-release", "offline", &["claude"])];
    assert!(!should_yield_to_preferred(
        &offline,
        "mac-book-release",
        "claude"
    ));

    let malformed = vec![
        json!({"instance_id": "mac-book-release"}),
        json!({"status": "online"}),
        json!("not-an-object"),
        json!({"instance_id": "mac-book-release", "status": "online",
               "capabilities": {"discord_gateway": {"waiting_providers": "claude"}}}),
    ];
    assert!(!should_yield_to_preferred(
        &malformed,
        "mac-book-release",
        "claude"
    ));
}

fn snapshot_root() -> (
    tempfile::TempDir,
    crate::config_live_reload::TestLiveGuard,
    [crate::config::TestEnvVarGuard; 2],
) {
    let root = tempfile::tempdir().unwrap();
    let live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let paths = [
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        ),
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CONFIG",
            &root.path().join("config.yaml"),
        ),
    ];
    (root, live, paths)
}

#[tokio::test]
async fn gateway_snapshot_preference_and_application_name_ignore_disk_edits() {
    let (root, _live, _paths) = snapshot_root();
    let mut live = crate::config::Config::default();
    live.data.dir = root.path().join("data");
    live.cluster.enabled = true;
    live.cluster.instance_id = Some("snapshot-node".into());
    live.cluster.gateway_preferred_instance_id = Some("snapshot-node".into());
    live.cluster.gateway_yield_grace_secs = 7;
    let mut disk = live.clone();
    disk.cluster.instance_id = Some("disk-node".into());
    disk.cluster.gateway_preferred_instance_id = Some("disk-preferred".into());
    disk.cluster.gateway_yield_grace_secs = 99;
    crate::config::save_to_path(&root.path().join("config.yaml"), &disk).unwrap();
    crate::config_live_reload::install(live);
    let preference = gateway_preference().await.unwrap();
    assert!(preference.self_is_preferred());
    assert_eq!(preference.preferred_instance_id, "snapshot-node");
    assert_eq!(preference.yield_grace, Duration::from_secs(7));
    let recovery =
        super::super::gateway_lease_recovery::gateway_lease_application_name(&ProviderKind::Claude);
    let expected = super::super::gateway_lease_recovery::gateway_lease_application_name_for(
        "snapshot-node",
        std::process::id(),
        "claude",
    );
    assert_eq!(
        recovery, expected,
        "gateway identity must use the installed snapshot"
    );
}

#[tokio::test(start_paused = true)]
async fn gateway_snapshot_wait_warning_uses_captured_live_timing() {
    use std::io::{Read, Seek, SeekFrom};
    let (root, _live, _paths) = snapshot_root();
    let mut live = crate::config::Config::default();
    live.data.dir = root.path().join("data");
    live.cluster.gateway_lease_idle_expiry_secs = 0;
    live.cluster.gateway_yield_grace_secs = 0;
    let mut disk = live.clone();
    disk.cluster.gateway_lease_idle_expiry_secs = 200;
    disk.cluster.gateway_yield_grace_secs = 100;
    crate::config::save_to_path(&root.path().join("config.yaml"), &disk).unwrap();
    crate::config_live_reload::install(live);
    let file = tempfile::tempfile().unwrap();
    let mut reader = file.try_clone().unwrap();
    crate::logging::test_capture::pin_callsite_interest();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || file.try_clone().unwrap())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let diagnostics = Diagnostics::default();
    let mut warning = expiry::PreferredWaitWarning::start();
    crate::config_live_reload::install(disk);
    tokio::time::advance(Duration::from_secs(59)).await;
    warning.observe(&diagnostics, &ProviderKind::Claude);
    let mut logs = String::new();
    reader.seek(SeekFrom::Start(0)).unwrap();
    reader.read_to_string(&mut logs).unwrap();
    assert!(!logs.contains("preferred node has waited longer"));
    tokio::time::advance(Duration::from_secs(1)).await;
    warning.observe(&diagnostics, &ProviderKind::Claude);
    logs.clear();
    reader.seek(SeekFrom::Start(0)).unwrap();
    reader.read_to_string(&mut logs).unwrap();
    assert_eq!(logs.matches("preferred node has waited longer").count(), 1);
    tokio::time::advance(Duration::from_secs(599)).await;
    warning.observe(&diagnostics, &ProviderKind::Claude);
    logs.clear();
    reader.seek(SeekFrom::Start(0)).unwrap();
    reader.read_to_string(&mut logs).unwrap();
    assert_eq!(logs.matches("preferred node has waited longer").count(), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    warning.observe(&diagnostics, &ProviderKind::Claude);
    logs.clear();
    reader.seek(SeekFrom::Start(0)).unwrap();
    reader.read_to_string(&mut logs).unwrap();
    assert_eq!(logs.matches("preferred node has waited longer").count(), 2);
}

#[tokio::test]
async fn gateway_snapshot_preferred_waiter_uses_live_lease_ttl_pg() {
    let (root, _live, _paths) = snapshot_root();
    let mut live = crate::config::Config::default();
    live.data.dir = root.path().join("data");
    live.cluster.lease_ttl_secs = 600;
    let mut disk = live.clone();
    disk.cluster.lease_ttl_secs = 1;
    crate::config::save_to_path(&root.path().join("config.yaml"), &disk).unwrap();
    crate::config_live_reload::install(live);
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    sqlx::query("INSERT INTO worker_nodes(instance_id,status,last_heartbeat_at,capabilities) VALUES ('snapshot-preferred','online',NOW()-INTERVAL '10 seconds',$1)")
        .bind(json!({"discord_gateway":{"waiting_providers":["claude"]}}))
        .execute(&pool).await.unwrap();
    let waiting =
        preferred_gateway_is_waiting(&pool, "snapshot-preferred", &ProviderKind::Claude).await;
    pool.close().await;
    fixture.drop().await;
    assert!(waiting, "snapshot TTL keeps the known waiter online");
}

async fn execution_owner_rows(pool: &sqlx::PgPool, channel: u64) -> Vec<serde_json::Value> {
    sqlx::query_scalar(
        "SELECT to_jsonb(o) FROM intake_session_owners o
          WHERE provider = 'claude' AND raw_channel_id = $1 ORDER BY generation",
    )
    .bind(channel.to_string())
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Losing the gateway transport stops Discord intake, not native execution: the PG execution
/// owner, the permit it grants and the channel's provider session all survive the self-fence.
#[tokio::test]
async fn gateway_self_fence_keeps_the_execution_owner_and_native_session_pg() {
    use crate::services::tui_o::ownership::execution;
    let root = crate::config::TestRuntimeRootGuard::new();
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let channel = 5_100_800_001;
    execution::tests::admission(&pool, "claude", channel, "fenced-node").await;
    let owners = execution_owner_rows(&pool, channel).await;
    let read = || execution::read(&pool, "claude", channel, "fenced-node", || Ok(false));
    let permit = read()
        .await
        .unwrap()
        .expect("the admitted owner grants execution");

    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let channel_id = serenity::ChannelId::new(channel);
    let session = crate::services::discord::DiscordSession {
        session_id: Some("native-session".into()),
        memento_context_loaded: false,
        memento_reflected: false,
        current_path: None,
        history: Vec::new(),
        pending_uploads: Default::default(),
        cleared: false,
        channel_id: Some(channel),
        channel_name: Some("fenced".into()),
        category_name: None,
        last_active: tokio::time::Instant::now(),
        worktree: None,
        born_generation: shared.restart.current_generation,
    };
    shared
        .core
        .lock()
        .await
        .sessions
        .insert(channel_id, session);
    shared
        .bot_connected
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let http = Arc::new(serenity::Http::new("unused-token"));
    let (shard_manager, _) =
        serenity::gateway::ShardManager::new(serenity::gateway::ShardManagerOptions {
            data: Arc::new(serenity::prelude::RwLock::new(
                serenity::prelude::TypeMap::new(),
            )),
            event_handlers: vec![],
            raw_event_handlers: vec![],
            framework: Arc::new(std::sync::OnceLock::new()),
            shard_index: 0,
            shard_init: 0,
            shard_total: 1,
            ws_url: Arc::new(tokio::sync::Mutex::new("ws://127.0.0.1:1".into())),
            cache: Arc::new(serenity::Cache::new()),
            http,
            intents: serenity::GatewayIntents::empty(),
            presence: None,
        });

    self_fence_gateway(&shared, &ProviderKind::Claude, &shard_manager).await;

    assert!(
        !shared
            .bot_connected
            .load(std::sync::atomic::Ordering::SeqCst)
            && root.path().join("restart_pending").exists(),
        "premise: the self-fence ran"
    );
    assert_eq!(
        execution_owner_rows(&pool, channel).await,
        owners,
        "transport loss must not end or transfer the execution owner"
    );
    let after = read()
        .await
        .unwrap()
        .expect("the owner still grants execution");
    assert!(after.same_execution(&permit), "{after:?} != {permit:?}");
    assert_eq!(
        shared.core.lock().await.sessions[&channel_id]
            .session_id
            .as_deref(),
        Some("native-session"),
        "transport loss must not clear the native provider session"
    );
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn gateway_observability_actual_try_error_and_setup_error_pg() {
    let (_root, _live, _paths) = snapshot_root();
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let role = format!("diag_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(
        format!("CREATE ROLE {role} LOGIN").as_str(),
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("REVOKE EXECUTE ON FUNCTION pg_try_advisory_lock(bigint) FROM PUBLIC")
        .execute(&pool)
        .await
        .unwrap();
    let restricted = sqlx::postgres::PgPoolOptions::new()
        .connect_with((*pool.connect_options()).clone().username(&role))
        .await
        .unwrap();
    let diagnostics = Diagnostics::default();
    diagnostics.context(
        &restricted,
        "standby",
        "claude",
        discord_gateway_lock_id("1234"),
    );
    assert!(
        try_acquire_observing_handback(
            &restricted,
            "1234",
            &ProviderKind::Claude,
            &mut observability_breaker(),
            &diagnostics
        )
        .await
        .is_err()
    );
    let view = diagnostics.projection();
    assert_eq!(view.tries, 1, "actual failing SQL is still a try");
    assert_eq!(view.last_result, Some(LeaseResult::Err));
    restricted.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP ROLE {role}").as_str()))
        .execute(&pool)
        .await
        .unwrap();
    let unreachable = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy_with((*pool.connect_options()).clone().username(&role));
    assert!(
        try_acquire_observing_handback(
            &unreachable,
            "1234",
            &ProviderKind::Claude,
            &mut observability_breaker(),
            &diagnostics
        )
        .await
        .is_err()
    );
    assert_eq!(
        diagnostics.projection().tries,
        1,
        "connection failure adds no SQL try"
    );
    assert_eq!(diagnostics.projection().results[&LeaseResult::Err], 2);
    unreachable.close().await;
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn gateway_observability_health_memory_only_pg() {
    let (_root, _live, _paths) = snapshot_root();
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    shared.restart.gateway_lease_diagnostics.context(
        &pool,
        "standby",
        "claude",
        discord_gateway_lock_id("1234"),
    );
    let lease = try_acquire_observing_handback(
        &pool,
        "1234",
        &ProviderKind::Claude,
        &mut observability_breaker(),
        &shared.restart.gateway_lease_diagnostics,
    )
    .await
    .unwrap()
    .unwrap();
    lease.unlock().await.unwrap();
    let registry = health::HealthRegistry::new();
    registry.register_standby("claude".into(), shared).await;
    for _ in 0..5 {
        let snapshot =
            serde_json::to_value(health::build_health_snapshot(&registry).await).unwrap();
        let text = snapshot.to_string();
        assert!(text.contains("\"tries\":1"), "{text}");
        assert!(text.contains("\"acquired\":1"), "{text}");
    }
    if std::env::var_os("I7924_HEALTH_STATEMENT_AUDIT").is_some() {
        audit_health_statements(&registry, &pool).await;
    } else {
        eprintln!("HEALTH_MEMORY_ONLY statement_audit=NOT_REQUESTED");
    }
    pool.close().await;
    let snapshot = serde_json::to_value(health::build_health_snapshot(&registry).await).unwrap();
    assert!(snapshot.to_string().contains("\"tries\":1"));
    fixture.drop().await;
}

async fn audit_health_statements(registry: &health::HealthRegistry, pool: &sqlx::PgPool) {
    use crate::test_support::wait::{WAIT_CAP, await_within};
    use sqlx::Connection;
    let path = std::env::var("I7924_PG_LOG")
        .expect("I7924_PG_LOG required for the requested health statement audit");
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let begin = format!("I7924_HEALTH_BEGIN_{nonce}");
    let end = format!("I7924_HEALTH_END_{nonce}");
    // Keep sentinel traffic off the pool so its return cleanup stays outside the audit.
    let mut sentinel = sqlx::PgConnection::connect_with(&pool.connect_options())
        .await
        .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SELECT '{begin}'").as_str()))
        .execute(&mut sentinel)
        .await
        .unwrap();
    for _ in 0..5 {
        health::build_health_snapshot(registry).await;
    }
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SELECT '{end}'").as_str()))
        .execute(&mut sentinel)
        .await
        .unwrap();
    let log = await_within(
        async {
            loop {
                let log = std::fs::read_to_string(&path).expect("read health statement log");
                if log.contains(&end) {
                    break log;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        },
        WAIT_CAP,
    )
    .await;
    let after_begin = log.find(&begin).expect("begin sentinel must reach the log");
    let after_begin = after_begin + log[after_begin..].find('\n').unwrap() + 1;
    let end_marker = log.find(&end).unwrap();
    let before_end = log[..end_marker].rfind('\n').map_or(0, |i| i + 1);
    assert_eq!(
        log[after_begin..before_end].trim(),
        "",
        "health requests issued PostgreSQL statements between confirmed sentinels"
    );
    eprintln!("HEALTH_STATEMENTS requests=5 new_statements=0 confirmed_sentinels=2 log={path}");
}

#[tokio::test]
async fn gateway_observability_provider_key_identity_pg() {
    let (_root, _live, _paths) = snapshot_root();
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let tokens = ["discord_1234000000000001", "discord_1234000000000002"];
    let held = try_acquire_discord_gateway_lease(&pool, tokens[0], &ProviderKind::Claude)
        .await
        .unwrap()
        .unwrap();
    let registry = health::HealthRegistry::new();
    let (_capture, reader) = observability_capture();
    for token in tokens {
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let diagnostics = &shared.restart.gateway_lease_diagnostics;
        diagnostics.context(&pool, "standby", "claude", discord_gateway_lock_id(token));
        let lease = try_acquire_observing_handback(
            &pool,
            token,
            &ProviderKind::Claude,
            &mut observability_breaker(),
            diagnostics,
        )
        .await
        .unwrap();
        if let Some(lease) = lease {
            diagnostics.waiter_refreshed();
            lease.unlock().await.unwrap();
        }
        registry.register_standby("claude".into(), shared).await;
    }
    let snapshot = serde_json::to_value(health::build_health_snapshot(&registry).await).unwrap();
    let providers = snapshot["providers"].as_array().unwrap();
    for (token, result) in tokens.into_iter().zip(["held", "acquired"]) {
        let key = discord_gateway_lock_id(token);
        let view = providers
            .iter()
            .find(|p| p["gateway_lease"]["advisory_key"] == key)
            .expect("same-provider health must identify each advisory key");
        assert_eq!(view["gateway_lease"]["provider"], "claude");
        assert_eq!(view["gateway_lease"]["last_result"], result);
    }
    let logs = observability_logs(reader);
    let acquired_key = discord_gateway_lock_id(tokens[1]);
    for event in ["PostgreSQL granted advisory", "waiter refresh completed"] {
        let line = logs.lines().find(|line| line.contains(event)).unwrap();
        assert!(line.contains("provider=\"claude\""), "{line}");
        assert!(
            line.contains(&format!("advisory_key={acquired_key}")),
            "{line}"
        );
    }
    held.unlock().await.unwrap();
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn gateway_observability_c_different_pg_new_advisory_pg() {
    let (_root, _live, _paths) = snapshot_root();
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let supplied_other = std::env::var("AGENTDESK_OBSERVABILITY_OTHER_PG").ok();
    let other_base = supplied_other
        .clone()
        .unwrap_or_else(|| crate::db::postgres::postgres_test_database_url_base().unwrap());
    let other = sqlx::PgPool::connect(&format!("{other_base}/postgres"))
        .await
        .unwrap();
    let token = "discord_7465737467617465";
    let held = try_acquire_discord_gateway_lease(&pool, token, &ProviderKind::Claude)
        .await
        .unwrap()
        .unwrap();
    let diagnostics = Diagnostics::default();
    diagnostics.context(&pool, "standby", "claude", discord_gateway_lock_id(token));
    assert!(
        try_acquire_observing_handback(
            &pool,
            token,
            &ProviderKind::Claude,
            &mut observability_breaker(),
            &diagnostics
        )
        .await
        .unwrap()
        .is_none()
    );
    let old = diagnostics.projection();
    assert!(old.holder.is_some());
    diagnostics.context(&other, "startup", "claude", discord_gateway_lock_id(token));
    let (_capture, reader) = observability_capture();
    let second = try_acquire_observing_handback(
        &other,
        token,
        &ProviderKind::Claude,
        &mut observability_breaker(),
        &diagnostics,
    )
    .await
    .unwrap()
    .unwrap();
    let targets: Vec<(i32, String)> = vec![
        sqlx::query_as("SELECT inet_server_port(), current_database()")
            .fetch_one(&pool)
            .await
            .unwrap(),
        sqlx::query_as("SELECT inet_server_port(), current_database()")
            .fetch_one(&other)
            .await
            .unwrap(),
    ];
    assert_ne!(targets[0], targets[1]);
    if supplied_other.is_some() {
        assert_ne!(
            targets[0].0, targets[1].0,
            "lane reproduction must use two actual PG servers"
        );
    }
    eprintln!("actual PostgreSQL lock domains: {targets:?}");
    let view = diagnostics.projection();
    assert_ne!(view.pg_target, old.pg_target);
    assert_eq!(view.last_result, Some(LeaseResult::Acquired));
    assert_eq!(view.tries, 2);
    assert!(
        view.holder.is_none(),
        "a previous PG holder is not evidence about this new grant"
    );
    assert!(view.snapshot_at.is_none());
    assert_eq!(
        serde_json::to_value(view).unwrap()["snapshot_status"],
        "unknown"
    );
    second.unlock().await.unwrap();
    held.unlock().await.unwrap();
    other.close().await;
    pool.close().await;
    fixture.drop().await;
    let logs = observability_logs(reader);
    assert!(logs.contains("last_result=\"acquired\""));
    assert!(logs.contains("pg_target="));
}

#[tokio::test]
async fn gateway_observability_yield_wait_free_is_a_real_short_circuit_pg() {
    let (_root, _live, _paths) = snapshot_root();
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    sqlx::query("INSERT INTO worker_nodes(instance_id,status,last_heartbeat_at,updated_at,capabilities) VALUES ('home','online',NOW(),NOW(),$1)")
        .bind(json!({"discord_gateway":{"waiting_providers":["claude"]}})).execute(&pool).await.unwrap();
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    assert!(
        preferred_gateway_is_waiting(&pool, "home", &ProviderKind::Claude).await,
        "the fixture must publish a real preferred waiter"
    );
    let pref = GatewayPreference {
        preferred_instance_id: "home".into(),
        self_instance_id: "backup".into(),
        yield_grace: Duration::from_secs(10),
    };
    assert!(
        tokio::time::timeout(
            Duration::from_secs(1),
            yield_to_preferred_gateway(
                &pool,
                "7465737467617465",
                &pref,
                &ProviderKind::Claude,
                &shared
            )
        )
        .await
        .is_err()
    );
    let view = shared.restart.gateway_lease_diagnostics.projection();
    assert_eq!(view.tries, 0);
    assert_eq!(view.last_result, Some(LeaseResult::RefusedWhileFree));
    assert_eq!(view.snapshot_attempts, 1);
    pool.close().await;
    fixture.drop().await;
}
