use super::gateway_handback_breaker::GatewayHandbackBreaker;
use super::gateway_lease::{
    GatewayLeaseAcquisition, try_acquire_discord_gateway_lease, try_acquire_observing_handback,
};
use super::*;
use crate::db::postgres;
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

#[path = "gateway_handback_mock.rs"]
mod mock;

const TOKEN: &str = "discord_7465737467617465";
const LABEL: &str = "gateway handback integration";

struct Fixture {
    _root: tempfile::TempDir,
    config: crate::config::Config,
    pool: sqlx::PgPool,
    admin: String,
    database: String,
    _paths: [crate::config::TestEnvVarGuard; 2],
    _lifecycle: postgres::PostgresTestLifecycleGuard,
    _environment: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let environment = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("agentdesk.yaml");
        let paths = [
            crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
                "AGENTDESK_ROOT_DIR",
                root.path(),
            ),
            crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
                "AGENTDESK_CONFIG",
                &config_path,
            ),
        ];
        let lifecycle = postgres::lock_test_lifecycle();
        let Some(base) = postgres::postgres_test_database_url_base() else {
            eprintln!("SKIP: isolated PostgreSQL fixture is not configured");
            return None;
        };
        let admin = format!(
            "{base}/{}",
            std::env::var("POSTGRES_TEST_ADMIN_DB").unwrap_or_else(|_| "postgres".into())
        );
        let database = format!("agentdesk_handback_{}", uuid::Uuid::new_v4().simple());
        postgres::create_test_database(&admin, &database, LABEL)
            .await
            .unwrap();
        let pool = postgres::connect_test_pool_and_migrate(&format!("{base}/{database}"), LABEL)
            .await
            .unwrap();
        let mut config = crate::config::Config::default();
        config.data.dir = root.path().join("data");
        config.cluster.enabled = true;
        config.cluster.instance_id = Some("backup".into());
        config.cluster.gateway_preferred_instance_id = Some("home".into());
        config.cluster.lease_ttl_secs = 600;
        crate::config::save_to_path(&config_path, &config).unwrap();
        sqlx::query("INSERT INTO worker_nodes(instance_id,status,last_heartbeat_at) VALUES ('home','online',NOW())")
            .execute(&pool).await.unwrap();
        Some(Self {
            _root: root,
            config,
            pool,
            admin,
            database,
            _paths: paths,
            _lifecycle: lifecycle,
            _environment: environment,
        })
    }

    fn breaker(&self) -> GatewayHandbackBreaker {
        GatewayHandbackBreaker::for_owner("claude", TOKEN)
    }

    async fn advertise(&self, waiting: bool) {
        let providers: Vec<&str> = if waiting { vec!["claude"] } else { vec![] };
        sqlx::query("UPDATE worker_nodes SET capabilities=$1,last_heartbeat_at=NOW() WHERE instance_id='home'")
            .bind(json!({"discord_gateway":{"waiting_providers":providers}})).execute(&self.pool).await.unwrap();
    }

    async fn publish_waiter(&self) -> bool {
        crate::services::cluster::node_registry::refresh_worker_node_runtime_capabilities(
            &self.pool, "home",
        )
        .await
        .unwrap();
        let capabilities: serde_json::Value =
            sqlx::query_scalar("SELECT capabilities FROM worker_nodes WHERE instance_id='home'")
                .fetch_one(&self.pool)
                .await
                .unwrap();
        crate::services::cluster::node_registry::node_awaits_gateway(
            &json!({"capabilities": capabilities}),
            "claude",
        )
    }

    async fn acquire(
        &self,
        shared: &Arc<SharedData>,
        breaker: &mut GatewayHandbackBreaker,
    ) -> GatewayLeaseOutcome {
        run_bot_acquire_gateway_lease(
            shared,
            TOKEN,
            &ProviderKind::Claude,
            &Arc::new(AtomicUsize::new(0)),
            &Arc::new(AtomicBool::new(true)),
            &Arc::new(health::HealthRegistry::new()),
            0,
            breaker,
        )
        .await
    }

    async fn backup(&self) -> Running {
        let shared = super::super::make_shared_data_for_tests_with_storage(Some(self.pool.clone()));
        let mut breaker = self.breaker();
        let GatewayLeaseOutcome::Proceed(Some(acquired)) =
            self.acquire(&shared, &mut breaker).await
        else {
            panic!("backup acquires lease")
        };
        Running::start(shared, acquired, Some(breaker)).await
    }

    async fn holder(&self) -> Option<i32> {
        sqlx::query_scalar("SELECT pid FROM pg_locks WHERE locktype='advisory' AND granted AND database=(SELECT oid FROM pg_database WHERE datname=current_database())")
            .fetch_optional(&self.pool).await.unwrap()
    }

    async fn unlock_observation(&self) -> (Instant, Instant) {
        tokio::time::timeout(Duration::from_secs(25), async {
            let mut last = Instant::now();
            loop {
                let start = Instant::now();
                if self.holder().await.is_none() {
                    return (last, Instant::now());
                }
                last = start;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("production keepalive releases the lease")
    }

    async fn ticks(&self, count: usize) {
        let pid = self.holder().await.unwrap();
        let mut previous = String::new();
        for _ in 0..count {
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    let stamp: Option<String> = sqlx::query_scalar("SELECT query_start::text FROM pg_stat_activity WHERE pid=$1 AND query='SELECT 1'")
                        .bind(pid).fetch_optional(&self.pool).await.unwrap();
                    if let Some(stamp) = stamp.filter(|stamp| *stamp != previous) { previous = stamp; break; }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }).await.expect("lease keepalive must continue while gateway serves");
        }
    }

    async fn close(self) {
        postgres::close_test_pool(self.pool, LABEL).await.unwrap();
        postgres::drop_test_database(&self.admin, &self.database, LABEL)
            .await
            .unwrap();
    }
}

struct Running {
    shared: Arc<SharedData>,
    manager: Arc<serenity::gateway::ShardManager>,
    backend: tokio::task::JoinHandle<()>,
    server: tokio::task::JoinHandle<()>,
    events: mock::Events,
    _held: Option<postgres::AdvisoryLockLease>,
}

impl Running {
    async fn start(
        shared: Arc<SharedData>,
        acquired: GatewayLeaseAcquisition,
        breaker: Option<GatewayHandbackBreaker>,
    ) -> Self {
        let (client, events, server) = mock::client().await;
        let manager = client.shard_manager.clone();
        let (lease_task, held) = match breaker {
            Some(breaker) => (
                Some(run_bot_spawn_gateway_lease_keepalive(
                    acquired.lease,
                    &shared,
                    &ProviderKind::Claude,
                    TOKEN.into(),
                    manager.clone(),
                    breaker,
                )),
                None,
            ),
            None => (None, Some(acquired.lease)),
        };
        let backend = tokio::spawn(async move {
            run_bot_run_gateway_backend(
                client,
                &ProviderKind::Claude,
                acquired.waiter,
                lease_task,
                None,
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicBool::new(true)),
                Arc::new(health::HealthRegistry::new()),
                0,
            )
            .await;
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            while events.ready.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("real client.start dispatches mock READY");
        shared.bot_connected.store(true, Ordering::SeqCst);
        Self {
            shared,
            manager,
            backend,
            server,
            events,
            _held: held,
        }
    }

    async fn ended(&mut self) {
        tokio::time::timeout(Duration::from_secs(10), &mut self.backend)
            .await
            .expect("backend exits after shard shutdown")
            .unwrap();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.backend.abort();
        self.server.abort();
    }
}

#[tokio::test]
async fn observed_acquisition_settles_each_pending_handback_pg() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let shared = super::super::make_shared_data_for_tests_with_storage(Some(fixture.pool.clone()));
    let mut breaker = fixture.breaker();
    for _ in 0..3 {
        let home = try_acquire_discord_gateway_lease(&fixture.pool, TOKEN, &ProviderKind::Claude)
            .await
            .unwrap()
            .unwrap();
        assert!(breaker.record_yield());
        assert!(matches!(
            fixture.acquire(&shared, &mut breaker).await,
            GatewayLeaseOutcome::Standby
        ));
        assert!(!breaker.suppressed());
        home.unlock().await.unwrap();
    }
    for _ in 0..2 {
        assert!(breaker.record_yield());
        let GatewayLeaseOutcome::Proceed(Some(acquired)) =
            fixture.acquire(&shared, &mut breaker).await
        else {
            panic!("empty handback must be reacquired")
        };
        acquired.lease.unlock().await.unwrap();
    }
    assert!(breaker.suppressed());
    fixture.close().await;
}

#[tokio::test]
async fn empty_handbacks_survive_restart_then_suppression_keeps_serving_and_self_fences_pg() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    assert_eq!(fixture.config.cluster.gateway_yield_grace_secs, 90);
    let mut running = fixture.backup().await;
    fixture.advertise(true).await;
    for repetition in 1..=2 {
        let (before_unlock, after_unlock) = fixture.unlock_observation().await;
        running.ended().await;
        drop(running);
        let reacquire_started = Instant::now();
        running = fixture.backup().await;
        assert!(reacquire_started.elapsed() >= Duration::from_secs(90));
        let ready_at = running.events.ready_at.lock().unwrap().unwrap();
        println!(
            "handback={repetition} observed_unlock_to_mock_READY_ms={}..{} restart=fresh_SharedData grace_config_secs=90",
            ready_at.duration_since(after_unlock).as_millis(),
            ready_at.duration_since(before_unlock).as_millis()
        );
    }
    assert!(fixture.breaker().suppressed());
    let standby = tokio::time::timeout(
        Duration::from_secs(5),
        fixture.acquire(&running.shared, &mut fixture.breaker()),
    )
    .await
    .expect("active suppression skips the initial ninety-second grace");
    assert!(matches!(standby, GatewayLeaseOutcome::Standby));
    let before = running.events.messages.load(Ordering::SeqCst);
    fixture.ticks(4).await;
    assert!(!running.backend.is_finished());
    assert!(running.events.messages.load(Ordering::SeqCst) > before);
    let pid = fixture.holder().await.unwrap();
    sqlx::query("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .execute(&fixture.pool)
        .await
        .unwrap();
    let foreign = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(lease) =
                try_acquire_discord_gateway_lease(&fixture.pool, TOKEN, &ProviderKind::Claude)
                    .await
                    .unwrap()
            {
                break lease;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("terminated isolated lease becomes available");
    tokio::time::timeout(Duration::from_secs(25), async {
        while !running.backend.is_finished() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("suppression must not bypass split-brain self-fence");
    running.ended().await;
    assert!(!running.shared.bot_connected.load(Ordering::SeqCst));
    foreign.unlock().await.unwrap();
    drop(running);
    fixture.close().await;
}

#[tokio::test]
async fn backend_exit_removes_home_waiter_until_live_home_takes_handback_pg() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let home = try_acquire_discord_gateway_lease(&fixture.pool, TOKEN, &ProviderKind::Claude)
        .await
        .unwrap()
        .unwrap();
    let mut preferred = fixture.config.clone();
    preferred.cluster.instance_id = Some("home".into());
    let config_path = fixture._root.path().join("agentdesk.yaml");
    crate::config::save_to_path(&config_path, &preferred).unwrap();
    let waiting_shared =
        super::super::make_shared_data_for_tests_with_storage(Some(fixture.pool.clone()));
    let mut breaker = fixture.breaker();
    let waiting = tokio::spawn(async move {
        run_bot_acquire_gateway_lease(
            &waiting_shared,
            TOKEN,
            &ProviderKind::Claude,
            &Arc::new(AtomicUsize::new(0)),
            &Arc::new(AtomicBool::new(true)),
            &Arc::new(health::HealthRegistry::new()),
            0,
            &mut breaker,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !fixture.publish_waiter().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("preferred acquisition publishes a live waiter");
    waiting.abort();
    assert!(matches!(waiting.await, Err(error) if error.is_cancelled()));
    assert!(
        !fixture.publish_waiter().await,
        "cancelled acquisition clears the heartbeat waiter"
    );
    crate::config::save_to_path(&config_path, &fixture.config).unwrap();
    let waiter = GatewayWaiterGuard::new("claude");
    let home_shared =
        super::super::make_shared_data_for_tests_with_storage(Some(fixture.pool.clone()));
    let mut running = Running::start(
        home_shared,
        GatewayLeaseAcquisition {
            lease: home,
            waiter: Some(waiter),
        },
        None,
    )
    .await;
    assert!(fixture.publish_waiter().await);
    running.manager.shutdown_all().await;
    running.ended().await;
    running._held.take().unwrap().unlock().await.unwrap();
    drop(running);
    assert!(
        !fixture.publish_waiter().await,
        "backend exit clears the heartbeat waiter"
    );
    let mut backup = fixture.backup().await;
    fixture.ticks(4).await;
    assert!(!backup.backend.is_finished());
    let waiter = GatewayWaiterGuard::new("claude");
    assert!(fixture.publish_waiter().await);
    fixture.unlock_observation().await;
    let home = try_acquire_discord_gateway_lease(&fixture.pool, TOKEN, &ProviderKind::Claude)
        .await
        .unwrap()
        .unwrap();
    backup.ended().await;
    let outcome = try_acquire_observing_handback(
        &fixture.pool,
        TOKEN,
        &ProviderKind::Claude,
        &mut fixture.breaker(),
    )
    .await
    .unwrap();
    assert!(
        outcome.is_none(),
        "backup is standby after the live home acquires"
    );
    assert!(
        !fixture.breaker().suppressed(),
        "consumed handback must not trip breaker"
    );
    home.unlock().await.unwrap();
    drop(waiter);
    drop(backup);
    fixture.close().await;
}
