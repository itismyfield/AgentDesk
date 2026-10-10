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
        config.cluster.gateway_yield_grace_secs = 5;
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
    let grace_secs = fixture.config.cluster.gateway_yield_grace_secs;
    let mut running = fixture.backup().await;
    fixture.advertise(true).await;
    for repetition in 1..=2 {
        let (before_unlock, after_unlock) = fixture.unlock_observation().await;
        running.ended().await;
        drop(running);
        let reacquire_started = Instant::now();
        running = fixture.backup().await;
        assert!(reacquire_started.elapsed() >= Duration::from_secs(grace_secs));
        let ready_at = running.events.ready_at.lock().unwrap().unwrap();
        println!(
            "handback={repetition} observed_unlock_to_mock_READY_ms={}..{} restart=fresh_SharedData grace_config_secs={grace_secs}",
            ready_at.duration_since(after_unlock).as_millis(),
            ready_at.duration_since(before_unlock).as_millis()
        );
    }
    assert!(fixture.breaker().suppressed());
    let standby = tokio::time::timeout(
        Duration::from_secs(2),
        fixture.acquire(&running.shared, &mut fixture.breaker()),
    )
    .await
    .expect("active suppression skips the configured initial grace");
    assert!(matches!(standby, GatewayLeaseOutcome::Standby));
    let before = running.events.messages.load(Ordering::SeqCst);
    fixture.ticks(2).await;
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
    home.unlock().await.unwrap();
    let home_shared =
        super::super::make_shared_data_for_tests_with_storage(Some(fixture.pool.clone()));
    let GatewayLeaseOutcome::Proceed(Some(acquired)) =
        fixture.acquire(&home_shared, &mut fixture.breaker()).await
    else {
        panic!("preferred home acquires the released lease")
    };
    crate::config::save_to_path(&config_path, &fixture.config).unwrap();
    let mut running = Running::start(home_shared, acquired, None).await;
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
    fixture.ticks(2).await;
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

impl Fixture {
    fn save_config(&self, edit: impl FnOnce(&mut crate::config::Config)) {
        let mut config = self.config.clone();
        edit(&mut config);
        crate::config::save_to_path(&self._root.path().join("agentdesk.yaml"), &config).unwrap();
    }

    async fn peer_takes_lease_within(&self, within: Duration) -> postgres::AdvisoryLockLease {
        tokio::time::timeout(within, async {
            loop {
                if let Some(lease) =
                    try_acquire_discord_gateway_lease(&self.pool, TOKEN, &ProviderKind::Claude)
                        .await
                        .unwrap()
                {
                    break lease;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("the idle holder session expires and frees the lease")
    }
}

#[tokio::test]
async fn preferred_node_takes_the_lease_once_a_silent_holder_session_expires_pg() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.save_config(|config| {
        config.cluster.instance_id = Some("home".into());
        config.cluster.gateway_lease_idle_expiry_secs = 2;
    });
    // An earlier process's lease whose client went silent: held, never kept alive.
    let silent = try_acquire_discord_gateway_lease(&fixture.pool, TOKEN, &ProviderKind::Claude)
        .await
        .unwrap()
        .unwrap();
    let silent_pid = fixture.holder().await.unwrap();
    let shared = super::super::make_shared_data_for_tests_with_storage(Some(fixture.pool.clone()));
    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        fixture.acquire(&shared, &mut fixture.breaker()),
    )
    .await
    .expect("the preferred waiter acquires once the silent session expires");
    let GatewayLeaseOutcome::Proceed(Some(acquired)) = outcome else {
        panic!("preferred node must take the freed lease")
    };
    assert_ne!(fixture.holder().await, Some(silent_pid));
    acquired.lease.unlock().await.unwrap();
    drop(silent);
    fixture.close().await;
}

#[tokio::test]
async fn configured_idle_expiry_reaches_the_lease_session_and_zero_disables_it_pg() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    for (secs, expected) in [(2, "2s"), (0, "0")] {
        fixture.save_config(|config| config.cluster.gateway_lease_idle_expiry_secs = secs);
        let mut lease =
            try_acquire_discord_gateway_lease(&fixture.pool, TOKEN, &ProviderKind::Claude)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(lease.session_idle_timeout().await.unwrap(), expected);
        if secs == 0 {
            tokio::time::sleep(Duration::from_secs(4)).await;
            let peer =
                try_acquire_discord_gateway_lease(&fixture.pool, TOKEN, &ProviderKind::Claude)
                    .await
                    .unwrap();
            assert!(peer.is_none(), "a disabled expiry keeps an idle lease");
        }
        lease.unlock().await.unwrap();
    }
    fixture.close().await;
}

#[tokio::test]
async fn idle_expired_holder_loses_the_lease_to_a_peer_and_self_fences_pg() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.save_config(|config| config.cluster.gateway_lease_idle_expiry_secs = 2);
    let mut running = fixture.backup().await;
    // The production keepalive ticks every 15s, so a 2s expiry ends the session between ticks.
    let mut peer = fixture
        .peer_takes_lease_within(Duration::from_secs(10))
        .await;
    tokio::time::timeout(Duration::from_secs(25), async {
        while !running.backend.is_finished() {
            peer.keepalive().await.unwrap();
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("the holder self-fences on its next keepalive tick");
    running.ended().await;
    assert!(!running.shared.bot_connected.load(Ordering::SeqCst));
    peer.unlock().await.unwrap();
    drop(running);
    fixture.close().await;
}

/// A keepalive loss and a later yield each withdraw the approvals resting on the provider's
/// Gateway witness; an approval resting elsewhere stays.
#[tokio::test]
async fn b2b1_lease_loss_and_yield_withdraw_only_gateway_approvals_pg() {
    use crate::services::discord::turn_presence::entrypoints::{Fence, tests};
    use tests::Probe;
    // Whether the node still reported itself serving at each Gateway withdrawal.
    struct Serving(Arc<SharedData>, std::sync::Mutex<Vec<bool>>);
    impl Fence for Serving {
        fn withdraw(&self, _: u64, _: bool, _: &'static str) {}
        fn withdraw_gateway(
            &self,
            _: &Arc<crate::services::tui_o::ownership::OwnershipGate>,
            _: &'static str,
        ) {
            let serving = self.0.bot_connected.load(Ordering::SeqCst);
            self.1.lock().unwrap().push(serving);
        }
        fn suspend(&self) {}
        fn resume(&self) {}
    }
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let mut running = fixture.backup().await;
    let probe = Probe::install(&running.shared);
    let serving = Arc::new(Serving(running.shared.clone(), Default::default()));
    let fence = Arc::downgrade(&serving) as std::sync::Weak<dyn Fence>;
    let _seen = tests::install(&running.shared, fence);
    let gate = crate::services::tui_o::ownership::gate("claude");
    let uncertain = probe.arm_gateway(6_325_621, gate.clone());
    let elsewhere = probe.arm(6_325_622);
    let pid = fixture.holder().await.unwrap();
    sqlx::query("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .execute(&fixture.pool)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(25), async {
        while fixture.holder().await.is_none_or(|holder| holder == pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the free lease is re-acquired");
    assert!(
        !Probe::current(&uncertain),
        "the keepalive loss withdrew it"
    );
    assert!(!running.backend.is_finished(), "the gateway keeps serving");
    let yielded = probe.arm_gateway(6_325_623, gate.clone());
    serving.1.lock().unwrap().clear();
    fixture.advertise(true).await;
    fixture.unlock_observation().await;
    running.ended().await;
    let first = serving.1.lock().unwrap().first().copied();
    assert_eq!(
        first,
        Some(true),
        "the yield withdrew before it stopped serving"
    );
    assert!(
        !Probe::current(&yielded),
        "the yield's self-fence withdrew it"
    );
    assert!(Probe::current(&elsewhere));
    drop(running);
    fixture.close().await;
}
