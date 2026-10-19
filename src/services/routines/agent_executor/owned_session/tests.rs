//! Recorder boundaries and the exact ownership token carried across boot recovery.
use super::*;

const THREAD: &str = "7157";
const TMUX: &str = "AgentDesk-claude-receipt-boot-target";

struct Fixture {
    db: crate::db::auto_queue::test_support::TestPostgresDb,
    pool: Arc<PgPool>,
    store: RoutineStore,
    executor: RoutineAgentExecutor,
    claimed: ClaimedRoutineRun,
}

impl Fixture {
    async fn new() -> Self {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = Arc::new(db.connect_and_migrate().await);
        sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_cc) VALUES ('receipt-agent', 'receipt', 'claude', '7156')")
            .execute(pool.as_ref()).await.unwrap();
        sqlx::query("INSERT INTO routines (id, agent_id, script_ref, name, execution_strategy, discord_thread_id, in_flight_run_id) VALUES ('receipt-routine', 'receipt-agent', 'receipt.js', 'receipt', 'fresh', $1, 'receipt-run')")
            .bind(THREAD).execute(pool.as_ref()).await.unwrap();
        sqlx::query("INSERT INTO routine_runs (id, routine_id, status, action, turn_id, lease_expires_at) VALUES ('receipt-run', 'receipt-routine', 'running', 'agent', 'discord:7157:1', NOW() - INTERVAL '1 minute')")
            .execute(pool.as_ref()).await.unwrap();
        Self {
            db,
            store: RoutineStore::new_with_timezone_and_checkpoint_limit(pool.clone(), "UTC", 4096),
            executor: RoutineAgentExecutor::new(pool.clone(), None, 30),
            pool,
            claimed: ClaimedRoutineRun {
                run_id: "receipt-run".into(),
                routine_id: "receipt-routine".into(),
                agent_id: Some("receipt-agent".into()),
                fallback_agent_id: None,
                max_retries: 0,
                script_ref: "receipt.js".into(),
                name: "receipt".into(),
                execution_strategy: "fresh".into(),
                checkpoint: None,
                discord_thread_id: Some(THREAD.into()),
                timeout_secs: None,
                lease_expires_at: Utc::now(),
            },
        }
    }

    async fn record(&self, result: Value, receipt: Option<&str>) {
        self.executor
            .record_owned_fresh_session(&self.store, &self.claimed, &result, receipt)
            .await;
    }

    async fn owned(&self) -> Option<String> {
        sqlx::query_scalar("SELECT owned_tmux_session FROM routine_runs WHERE id = 'receipt-run'")
            .fetch_one(self.pool.as_ref())
            .await
            .unwrap()
    }

    async fn session(&self, key: &str, thread: Option<&str>) {
        sqlx::query("INSERT INTO sessions (agent_id, provider, session_key, status, thread_channel_id, channel_id, active_dispatch_id, claude_session_id, raw_provider_session_id, last_heartbeat) VALUES ('receipt-agent', 'claude', $1, 'turn_active', $2, $3, 'keep-dispatch', 'keep-provider', 'keep-raw', NOW())")
            .bind(key).bind(thread).bind(THREAD).execute(self.pool.as_ref()).await.unwrap();
    }

    async fn row(&self, key: &str) -> Value {
        sqlx::query_scalar("SELECT to_jsonb(s) FROM sessions AS s WHERE session_key = $1")
            .bind(key)
            .fetch_one(self.pool.as_ref())
            .await
            .unwrap()
    }

    async fn recover(&self) -> RecoveredRoutineRun {
        let mut runs = self.store.recover_stale_running_runs().await.unwrap();
        assert_eq!(runs.len(), 1, "the expired run must actually be recovered");
        runs.pop().unwrap()
    }

    async fn close(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

fn result() -> Value {
    json!({"channel_id": THREAD, "execution_mode": "tmux"})
}

fn key(token: &str) -> String {
    format!(
        "claude/{token}/{}:{TMUX}",
        crate::services::platform::shell::hostname_short()
    )
}

#[tokio::test]
async fn owned_session_recorder_preserves_legacy_fallback_and_receipt_precedence_pg() {
    let fixture = Fixture::new().await;
    fixture.record(result(), None).await;
    assert_eq!(
        fixture.owned().await,
        None,
        "no guessed ownership without a row"
    );
    let legacy = "old-host:AgentDesk-claude-concrete-thread-session";
    fixture.session(legacy, Some(THREAD)).await;
    fixture.record(result(), None).await;
    assert_eq!(fixture.owned().await.as_deref(), Some(legacy));
    let receipt = key("seeded-receipt");
    fixture.record(result(), Some(&receipt)).await;
    assert_eq!(
        fixture.owned().await.as_deref(),
        Some(receipt.as_str()),
        "the concrete starter receipt must beat the latest-thread resolver"
    );
    fixture.close().await;
}

#[tokio::test]
async fn owned_session_recorder_skips_non_owned_routes_and_closed_runs_pg() {
    let mut fixture = Fixture::new().await;
    let receipt = key("route-boundary");
    for excluded in [
        json!({"channel_id": THREAD, "execution_mode": "process"}),
        json!({"channel_id": THREAD, "is_dm": true}),
        json!({"channel_id": THREAD, "dm_user_id": "123"}),
    ] {
        fixture.record(excluded.clone(), Some(&receipt)).await;
        assert_eq!(fixture.owned().await, None, "{excluded}");
    }
    fixture.claimed.execution_strategy = "persistent".into();
    fixture.record(result(), Some(&receipt)).await;
    assert_eq!(fixture.owned().await, None);
    fixture.claimed.execution_strategy = "fresh".into();
    sqlx::query("UPDATE routine_runs SET status = 'succeeded' WHERE id = 'receipt-run'")
        .execute(fixture.pool.as_ref())
        .await
        .unwrap();
    fixture.record(result(), Some(&receipt)).await;
    assert_eq!(
        fixture.owned().await,
        None,
        "old NULL runs are not backfilled"
    );
    fixture.close().await;
}

#[tokio::test]
async fn boot_receipt_recovery_disconnects_only_exact_token_row_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new().await;
    let receipt = key("owned-token");
    let foreign = key("foreign-token");
    fixture.session(&receipt, None).await;
    fixture.session(&foreign, Some(THREAD)).await;
    let foreign_before = fixture.row(&foreign).await;
    fixture.record(result(), Some(&receipt)).await;
    let recovered = fixture.recover().await;
    assert_eq!(
        recovered.owned_tmux_session.as_deref(),
        Some(receipt.as_str())
    );
    let kills = crate::services::platform::tmux::kill_requests::count(TMUX);
    fixture
        .executor
        .teardown_recovered_fresh_session(&fixture.store, &recovered)
        .await;
    let owned = fixture.row(&receipt).await;
    assert_eq!(owned["status"], "disconnected");
    for field in [
        "active_dispatch_id",
        "claude_session_id",
        "raw_provider_session_id",
    ] {
        assert_eq!(owned[field], Value::Null, "{field}");
    }
    assert_eq!(
        fixture.row(&foreign).await,
        foreign_before,
        "same tmux suffix must not widen the disconnect"
    );
    assert_eq!(
        crate::services::platform::tmux::kill_requests::count(TMUX),
        kills,
        "the existing namespaced-key remote guard still skips local tmux kill"
    );
    fixture.close().await;
}

#[tokio::test]
async fn boot_receipt_recovery_keeps_reused_session_for_replacement_and_null_legacy_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new().await;
    let receipt = key("reused-token");
    fixture.session(&receipt, None).await;
    fixture.record(result(), Some(&receipt)).await;
    let recovered = fixture.recover().await;
    sqlx::query("INSERT INTO routine_runs (id, routine_id, status, owned_tmux_session, lease_expires_at) VALUES ('replacement-run', 'receipt-routine', 'running', $1, NOW() + INTERVAL '1 hour')")
        .bind(&receipt).execute(fixture.pool.as_ref()).await.unwrap();
    let before = fixture.row(&receipt).await;
    fixture
        .executor
        .teardown_recovered_fresh_session(&fixture.store, &recovered)
        .await;
    assert_eq!(
        fixture.row(&receipt).await,
        before,
        "a replacement run owns the reused deterministic name"
    );
    sqlx::query("UPDATE routine_runs SET status = 'succeeded' WHERE id = 'replacement-run'")
        .execute(fixture.pool.as_ref())
        .await
        .unwrap();
    let null_owned = RecoveredRoutineRun {
        owned_tmux_session: None,
        ..recovered
    };
    fixture
        .executor
        .teardown_recovered_fresh_session(&fixture.store, &null_owned)
        .await;
    assert_eq!(
        fixture.row(&receipt).await,
        before,
        "legacy NULL ownership grants no teardown authority"
    );
    assert_eq!(
        crate::services::platform::tmux::kill_requests::count(TMUX),
        0
    );
    fixture.close().await;
}
