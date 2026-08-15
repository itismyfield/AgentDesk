use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox::mark_done;
use crate::db::intake_outbox_status::IntakeOutboxStatus;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::provider::ProviderKind;
use chrono::{DateTime, Utc};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use tokio::sync::Barrier;
use tracing_subscriber::fmt::writer::MakeWriter;

const READY_CAPABILITIES: SettlementCapabilities = SettlementCapabilities {
    stamp_dispatched: true,
    settle_and_sweep: true,
};
const BELOW_SETTLE_CAPABILITIES: SettlementCapabilities = SettlementCapabilities {
    stamp_dispatched: false,
    settle_and_sweep: false,
};

async fn seed(
    pool: &sqlx::PgPool,
    key: &str,
    status: IntakeOutboxStatus,
    spawned_at: Option<DateTime<Utc>>,
    dispatched_at: Option<DateTime<Utc>>,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO public.intake_outbox (
            target_instance_id, forwarded_by_instance_id, channel_id,
            user_msg_id, request_owner_id, user_text, turn_kind, agent_id,
            status, claim_owner, spawned_at, dispatched_at
         ) VALUES (
            'worker', 'leader', $1, $1, 'user', 'hello', 'standard', 'agent',
            $2, 'dispatch-worker', $3, $4
         ) RETURNING id",
    )
    .bind(key)
    .bind(status)
    .bind(spawned_at)
    .bind(dispatched_at)
    .fetch_one(pool)
    .await
    .expect("seed intake outbox row") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
}

async fn status(pool: &sqlx::PgPool, id: i64) -> IntakeOutboxStatus {
    sqlx::query_scalar("SELECT status FROM public.intake_outbox WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("read intake outbox status") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
}

async fn audit(
    pool: &sqlx::PgPool,
    id: i64,
) -> (
    IntakeOutboxStatus,
    Option<DateTime<Utc>>,
    Option<String>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
) {
    sqlx::query_as(
        "SELECT status, completed_at, claim_owner, spawned_at, dispatched_at
         FROM public.intake_outbox WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("read intake outbox audit fields") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
}

fn state_for(id: i64) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        42,
        Some("settlement-test".to_owned()),
        7,
        8,
        9,
        "hello".to_owned(),
        None,
        Some("AgentDesk-claude-adk-settlement-test".to_owned()),
        None,
        None,
        0,
    );
    state.adopt_intake_outbox(Some(id));
    state
}

async fn shared_with_pool(pool: sqlx::PgPool) -> Arc<SharedData> {
    crate::services::discord::make_shared_data_for_tests_with_storage_and_intake_capabilities(
        Some(pool),
        crate::services::discord::runtime_bootstrap::intake_delivery_capability::SettlementCapabilityCache::for_test(
            READY_CAPABILITIES,
        ),
    )
}

#[derive(Clone)]
struct CapturingWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CapturingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("capture settlement log").extend(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturingWriter {
    type Writer = CapturingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn classify_preserve_precedes_every_terminal_receipt() {
    assert_eq!(
        classify(true, true, true, false, true),
        BridgeTurnDisposition::PreservedForRetry
    );
    assert_eq!(
        classify(false, false, false, true, true),
        BridgeTurnDisposition::PreservedForRetry
    );
}

#[tokio::test]
async fn settle_from_spawned_and_from_dispatched_both_reach_done_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let now = Utc::now();
    let spawned = seed(
        &pool,
        "settle-spawned",
        IntakeOutboxStatus::Spawned,
        Some(now),
        None,
    )
    .await;
    let dispatched = seed(
        &pool,
        "settle-dispatched",
        IntakeOutboxStatus::Dispatched,
        Some(now),
        Some(now),
    )
    .await;
    let shared = shared_with_pool(pool.clone()).await;
    settle_intake_row_at_bridge_exit(
        &shared,
        &state_for(spawned),
        BridgeTurnDisposition::Committed,
        READY_CAPABILITIES,
    )
    .await;
    settle_intake_row_at_bridge_exit(
        &shared,
        &state_for(dispatched),
        BridgeTurnDisposition::Committed,
        READY_CAPABILITIES,
    )
    .await;
    assert_eq!(status(&pool, spawned).await, IntakeOutboxStatus::Done);
    assert_eq!(status(&pool, dispatched).await, IntakeOutboxStatus::Done);
    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn settle_is_idempotent_and_preserves_dispatch_audit_fields_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    // PostgreSQL stores timestamptz at microsecond precision, so the seeded
    // values must be pre-truncated or the round-trip comparison fails on
    // hosts whose clock reports nanoseconds (Linux; macOS reports micros).
    let spawned_at = chrono::DateTime::from_timestamp_micros(Utc::now().timestamp_micros())
        .expect("valid microsecond timestamp");
    let dispatched_at = spawned_at + chrono::Duration::seconds(1);
    let id = seed(
        &pool,
        "settle-idempotent",
        IntakeOutboxStatus::Dispatched,
        Some(spawned_at),
        Some(dispatched_at),
    )
    .await;
    let shared = shared_with_pool(pool.clone()).await;
    settle_intake_row_at_bridge_exit(
        &shared,
        &state_for(id),
        BridgeTurnDisposition::Committed,
        READY_CAPABILITIES,
    )
    .await;
    let first = audit(&pool, id).await;
    let second_won = settle_with_lock_timeout(&pool, id, IntakeSettlementSource::Committed)
        .await
        .expect("repeat settlement query");
    let second = audit(&pool, id).await;
    assert!(!second_won, "the idempotent second CAS must be a no-op");
    assert_eq!(first.0, IntakeOutboxStatus::Done);
    assert_eq!(first.1, second.1, "completed_at must be preserved");
    assert_eq!(first.2, Some("dispatch-worker".to_owned()));
    assert_eq!(first.3, Some(spawned_at));
    assert_eq!(first.4, Some(dispatched_at));
    assert_eq!(
        (first.0, first.2, first.3, first.4),
        (second.0, second.2, second.3, second.4)
    );
    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn settle_does_not_touch_terminal_or_pre_spawn_states_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let now = Utc::now();
    let statuses = [
        IntakeOutboxStatus::Pending,
        IntakeOutboxStatus::Claimed,
        IntakeOutboxStatus::Accepted,
        IntakeOutboxStatus::Done,
        IntakeOutboxStatus::Unknown,
        IntakeOutboxStatus::FailedPreAccept,
        IntakeOutboxStatus::FailedPostAccept,
    ];
    let shared = shared_with_pool(pool.clone()).await;
    for (index, state) in statuses.into_iter().enumerate() {
        let id = seed(
            &pool,
            &format!("settle-noop-{index}"),
            state,
            Some(now),
            None,
        )
        .await;
        settle_intake_row_at_bridge_exit(
            &shared,
            &state_for(id),
            BridgeTurnDisposition::Committed,
            READY_CAPABILITIES,
        )
        .await;
        assert_eq!(status(&pool, id).await, state);
    }
    pool.close().await;
    database.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_mark_done_and_settlement_converge_under_either_order_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let now = Utc::now();
    let settlement_first = seed(
        &pool,
        "settle-converge-settlement-first",
        IntakeOutboxStatus::Spawned,
        Some(now),
        None,
    )
    .await;
    let settlement_won =
        settle_with_lock_timeout(&pool, settlement_first, IntakeSettlementSource::Committed)
            .await
            .expect("settlement-first CAS");
    let worker_won = mark_done(&pool, settlement_first, "dispatch-worker")
        .await
        .expect("worker CAS after settlement commit");
    assert_eq!((settlement_won, worker_won), (true, false));

    let worker_first = seed(
        &pool,
        "settle-converge-worker-first",
        IntakeOutboxStatus::Spawned,
        Some(now),
        None,
    )
    .await;
    let worker_won = mark_done(&pool, worker_first, "dispatch-worker")
        .await
        .expect("worker-first CAS");
    let settlement_won =
        settle_with_lock_timeout(&pool, worker_first, IntakeSettlementSource::Committed)
            .await
            .expect("settlement CAS after worker commit");
    assert_eq!((worker_won, settlement_won), (true, false));

    let concurrent = seed(
        &pool,
        "settle-converge-concurrent",
        IntakeOutboxStatus::Spawned,
        Some(now),
        None,
    )
    .await;
    let barrier = Arc::new(Barrier::new(2));
    let settlement_barrier = Arc::clone(&barrier);
    let settlement_pool = pool.clone();
    let settlement = async move {
        settlement_barrier.wait().await;
        settle_with_lock_timeout(
            &settlement_pool,
            concurrent,
            IntakeSettlementSource::Committed,
        )
        .await
    };
    let worker_barrier = Arc::clone(&barrier);
    let worker_pool = pool.clone();
    let worker = async move {
        worker_barrier.wait().await;
        mark_done(&worker_pool, concurrent, "dispatch-worker").await
    };
    let (settlement_won, worker_won) = tokio::join!(settlement, worker);
    let settlement_won = settlement_won.expect("concurrent settlement CAS");
    let worker_won = worker_won.expect("concurrent worker CAS");
    assert_eq!(
        usize::from(settlement_won) + usize::from(worker_won),
        1,
        "exactly one concurrent CAS must win"
    );

    for id in [settlement_first, worker_first, concurrent] {
        assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Done);
    }
    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn settlement_rolls_back_with_caller_transaction_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let now = Utc::now();
    let id = seed(
        &pool,
        "settle-rollback",
        IntakeOutboxStatus::Spawned,
        Some(now),
        None,
    )
    .await;
    let mut transaction = pool.begin().await.expect("begin caller transaction"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
    assert!(
        settle_intake_done_from_receipt(&mut transaction, id, IntakeSettlementSource::Committed,)
            .await
            .expect("settle in caller transaction")
    ); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
    assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Spawned);
    transaction
        .rollback()
        .await
        .expect("rollback caller transaction"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
    assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Spawned);
    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn preserved_for_retry_leaves_row_open_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let now = Utc::now();
    let id = seed(
        &pool,
        "settle-preserve",
        IntakeOutboxStatus::Dispatched,
        Some(now),
        Some(now),
    )
    .await;
    let shared = shared_with_pool(pool.clone()).await;
    settle_intake_row_at_bridge_exit(
        &shared,
        &state_for(id),
        BridgeTurnDisposition::PreservedForRetry,
        READY_CAPABILITIES,
    )
    .await;
    assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Dispatched);
    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn relay_owner_handoff_closes_row_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let now = Utc::now();
    let id = seed(
        &pool,
        "settle-relay-owner",
        IntakeOutboxStatus::Spawned,
        Some(now),
        None,
    )
    .await;
    let shared = shared_with_pool(pool.clone()).await;
    let disposition = classify(false, false, false, false, true);
    assert_eq!(disposition, BridgeTurnDisposition::RelayOwnerHandoff);
    settle_intake_row_at_bridge_exit(&shared, &state_for(id), disposition, READY_CAPABILITIES)
        .await;
    assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Done);
    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn cancel_prompt_replace_commit_closes_row_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let now = Utc::now();
    let id = seed(
        &pool,
        "settle-cancel-replace",
        IntakeOutboxStatus::Spawned,
        Some(now),
        None,
    )
    .await;
    let shared = shared_with_pool(pool.clone()).await;
    let disposition = classify(false, true, false, false, false);
    assert_eq!(disposition, BridgeTurnDisposition::Committed);
    settle_intake_row_at_bridge_exit(&shared, &state_for(id), disposition, READY_CAPABILITIES)
        .await;
    assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Done);
    pool.close().await;
    database.drop().await;
}

/// Strips comments and string literals so occurrence counts below can only
/// match executable source. Rust block comments nest; strings use a
/// double-quote scan with escape handling (raw strings in the scanned file
/// would need hash-aware handling, which this contract does not require).
fn executable_source_only(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let bytes = source.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            let mut depth = 1usize;
            i += 2;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

#[test]
fn terminal_outcome_delivery_awaits_one_settlement_call_with_branch_flags() {
    let bridge_source = executable_source_only(include_str!("../mod.rs"));
    assert_eq!(
        bridge_source
            .matches("intake_settlement::bind_bridge_turn_snapshot")
            .count(),
        1,
        "the bridge must bind exactly one turn-start snapshot"
    );
    let spawn = bridge_source
        .find("pub(super) fn spawn_turn_bridge")
        .expect("bridge spawn remains present");
    let bind = bridge_source
        .find("intake_settlement::bind_bridge_turn_snapshot")
        .expect("bridge spawn binds the turn snapshot");
    let task = bridge_source
        .find("task_supervisor::spawn_observed")
        .expect("bridge task spawn remains present");
    assert!(
        spawn < bind && bind < task,
        "snapshot binds before task spawn"
    );

    let source = executable_source_only(include_str!("../terminal_outcome_delivery.rs"));
    assert_eq!(
        source
            .matches("intake_settlement::settle_intake_row_at_bridge_exit")
            .count(),
        1,
        "terminal delivery must have exactly one settlement call outside comments and strings"
    );
    let compact = source.split_whitespace().collect::<Vec<_>>().join(" ");
    let expected = "intake_settlement::settle_intake_row_at_bridge_exit( &shared_owned, &inflight_state, intake_settlement::classify( terminal_delivery_committed, status_panel_terminal_committed, preserve_inflight_for_cleanup_retry, bridge_skip_holder_owns_inflight, bridge_output_owner.is_some(), ), inflight_state.intake_delivery_capabilities(), ) .await;";
    assert!(
        compact.contains(expected),
        "terminal delivery must await settlement with every disposition flag"
    );
}

#[tokio::test]
async fn settlement_sql_error_is_swallowed_and_counted() {
    let source = IntakeSettlementSource::Committed;
    let before = counters().write_failed[source_index(source)].load(Ordering::Relaxed);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgresql://localhost/settlement-error")
        .expect("construct lazy settlement error pool");
    let shared = shared_with_pool(pool.clone()).await;
    pool.close().await;
    let logs = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::ERROR)
        .with_ansi(false)
        .without_time()
        .with_writer(CapturingWriter(Arc::clone(&logs)))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    settle_intake_row_at_bridge_exit(
        &shared,
        &state_for(99),
        BridgeTurnDisposition::Committed,
        READY_CAPABILITIES,
    )
    .await;
    let after = counters().write_failed[source_index(source)].load(Ordering::Relaxed);
    assert_eq!(after, before + 1);
    let logs = String::from_utf8(logs.lock().expect("read settlement logs").clone())
        .expect("settlement logs are UTF-8");
    assert!(logs.contains("intake_settlement_write_failed"));
    assert!(logs.contains("intake settlement SQL failed"));
}

#[tokio::test]
async fn settlement_row_lock_timeout_is_swallowed_and_counted_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let id = seed(
        &pool,
        "settle-lock-timeout",
        IntakeOutboxStatus::Spawned,
        Some(Utc::now()),
        None,
    )
    .await;
    let mut blocker = pool.begin().await.expect("begin competing transaction");
    sqlx::query("SELECT id FROM public.intake_outbox WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut *blocker)
        .await
        .expect("lock intake row");
    let shared = shared_with_pool(pool.clone()).await;
    let source = IntakeSettlementSource::Committed;
    let before = counters().write_failed[source_index(source)].load(Ordering::Relaxed);
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        settle_intake_row_at_bridge_exit(
            &shared,
            &state_for(id),
            BridgeTurnDisposition::Committed,
            READY_CAPABILITIES,
        ),
    )
    .await
    .expect("settlement must return within its lock-wait ceiling");
    let after = counters().write_failed[source_index(source)].load(Ordering::Relaxed);
    assert_eq!(after, before + 1);
    assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Spawned);
    blocker
        .rollback()
        .await
        .expect("release competing row lock");
    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn fresh_off_and_observe_leave_own_rows_for_worker_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let now = Utc::now();
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage_and_intake_capabilities(
        Some(pool.clone()),
        crate::services::discord::runtime_bootstrap::intake_delivery_capability::SettlementCapabilityCache::for_test(
            BELOW_SETTLE_CAPABILITIES,
        ),
    );
    for stage in ["off", "observe"] {
        let id = seed(
            &pool,
            &format!("settle-stage-{stage}"),
            IntakeOutboxStatus::Spawned,
            Some(now),
            None,
        )
        .await;
        settle_intake_row_at_bridge_exit(
            &shared,
            &state_for(id),
            BridgeTurnDisposition::Committed,
            BELOW_SETTLE_CAPABILITIES,
        )
        .await;
        assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Spawned);
        assert!(mark_done(&pool, id, "dispatch-worker").await.unwrap());
        assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Done);
    }
    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn stale_capability_probe_after_downgrade_cannot_block_own_row_settlement_pg() {
    use crate::config::IntakeDeliverySettlementStage;
    use crate::services::discord::runtime_bootstrap::intake_delivery_capability::bootstrap_from_receiver_for_test;

    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let id = seed(
        &pool,
        "settle-stale-capability",
        IntakeOutboxStatus::Spawned,
        Some(Utc::now()),
        None,
    )
    .await;
    let probe_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&database.database_url)
        .await
        .expect("connect single-slot probe pool");
    let mut config = crate::config::Config::default();
    config.runtime.intake_delivery_settlement = IntakeDeliverySettlementStage::Enforce;
    let (updates, receiver) = tokio::sync::watch::channel(Some(Arc::new(config.clone())));
    let cache = bootstrap_from_receiver_for_test(Some(probe_pool.clone()), receiver).await;
    assert_eq!(cache.generation_for_test(), 1);
    assert_eq!(cache.current(), READY_CAPABILITIES);
    let turn_snapshot = cache.current();
    assert!(
        crate::db::intake_outbox_dispatch_stamp::mark_dispatched(&pool, id)
            .await
            .unwrap()
    );
    assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Dispatched);
    let held_probe_slot = probe_pool.acquire().await.expect("hold probe pool slot");

    config.runtime.intake_delivery_settlement = IntakeDeliverySettlementStage::Enforce;
    updates
        .send(Some(Arc::new(config.clone())))
        .expect("send same-stage Enforce update");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while cache.generation_for_test() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("same-stage Enforce probe must start while the pool slot is held");

    config.runtime.intake_delivery_settlement = IntakeDeliverySettlementStage::Off;
    updates
        .send(Some(Arc::new(config)))
        .expect("send Off downgrade");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while cache.generation_for_test() < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Off downgrade must supersede the blocked probe");
    drop(held_probe_slot);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while cache.stale_results_for_test() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the old Settle probe must complete and be discarded after Off");
    assert_eq!(cache.current(), BELOW_SETTLE_CAPABILITIES);

    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage_and_intake_capabilities(
            Some(pool.clone()),
            Arc::clone(&cache),
        );
    settle_intake_row_at_bridge_exit(
        &shared,
        &state_for(id),
        BridgeTurnDisposition::Committed,
        turn_snapshot,
    )
    .await;
    assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Done);
    drop(updates);
    probe_pool.close().await;
    pool.close().await;
    database.drop().await;
}
