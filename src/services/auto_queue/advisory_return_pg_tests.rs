use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::auto_queue::route::fsm::attempt_restore_dispatch;
use crate::services::auto_queue::route::planning::record_entry_dispatch_failure;
use crate::services::auto_queue::route::slot_routes::rebind_slot_with_pg;
use crate::services::auto_queue::route::view::{RestoreDispatchCandidate, RestoreEntryRecord};
use crate::services::auto_queue::route::{
    ActivateBody, AutoQueueActivateDeps, RebindSlotBody, activate_with_deps_pg,
};
use crate::services::settings::clamp_retry_limit;
use crate::test_support::wait::{WAIT_CAP, await_within, poll_within};
use sqlx::{Connection, PgConnection, PgPool};
use std::sync::Arc;

#[derive(Clone, Copy)]
enum Operation {
    Allocate,
    Rebind,
    Activate,
}

impl Operation {
    fn lock_key(self) -> (&'static str, &'static str) {
        match self {
            Self::Allocate => ("aq_slot_alloc", "lock-agent"),
            Self::Rebind => ("aq_slot_rebind:lock-agent", "0"),
            Self::Activate => ("aq_activate", "lock-run"),
        }
    }

    async fn run(self, pool: PgPool) -> Result<(), String> {
        match self {
            Self::Allocate => crate::db::auto_queue::allocate_slot_for_group_agent_pg(
                &pool,
                "lock-run",
                1,
                "lock-agent",
            )
            .await
            .map(|_| ()),
            Self::Rebind => rebind_slot_with_pg(
                "lock-agent",
                0,
                &RebindSlotBody {
                    run_id: "lock-run".into(),
                    thread_group: 1,
                },
                &pool,
            )
            .await
            .map(|_| ())
            .map_err(|error| error.to_string()),
            Self::Activate => {
                let config = crate::config::Config::default();
                let engine = crate::engine::PolicyEngine::new_with_pg(
                    &config,
                    Some(pool.clone()),
                    crate::services::discord::health::slot_row_host_refusal,
                )
                .expect("create activate engine");
                activate_with_deps_pg(
                    &AutoQueueActivateDeps {
                        pg_pool: Some(pool),
                        slot_row_guard: engine.slot_row_guard(),
                        engine,
                        config: Arc::new(config),
                        health_registry: None,
                        guild_id: None,
                    },
                    ActivateBody {
                        run_id: Some("lock-run".into()),
                        repo: None,
                        agent_id: None,
                        thread_group: None,
                        unified_thread: None,
                        active_only: None,
                    },
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
            }
        }
    }
}

async fn cancelled_holder_returns_without_lock(operation: Operation, during_acquire: bool) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(2).await;
    sqlx::raw_sql(
        "INSERT INTO agents (id, name, provider) VALUES ('lock-agent', 'Lock agent', 'codex');
         INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('lock-run', 'repo', 'lock-agent', 'active');
         INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
         VALUES ('lock-agent', 0, '{}'::jsonb);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let mut observer = PgConnection::connect(&db.database_url).await.unwrap();
    let mut blocker = PgConnection::connect(&db.database_url).await.unwrap();
    if during_acquire {
        sqlx::raw_sql(
            "CREATE FUNCTION public.pg_advisory_lock(integer, integer) RETURNS void
             LANGUAGE plpgsql AS $$ BEGIN
               PERFORM pg_catalog.pg_advisory_lock($1, $2);
               PERFORM pg_catalog.pg_advisory_xact_lock(7851000::bigint);
             END $$;",
        )
        .execute(&mut observer)
        .await
        .unwrap();
        let mut first = pool.acquire().await.unwrap();
        let mut second = pool.acquire().await.unwrap();
        for conn in [&mut first, &mut second] {
            sqlx::query("SET search_path = public, pg_catalog")
                .execute(&mut **conn)
                .await
                .unwrap();
        }
        drop((first, second));
    }
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut blocker)
        .await
        .unwrap();
    let mut barrier = blocker.begin().await.unwrap();
    sqlx::query(if during_acquire {
        "SELECT pg_catalog.pg_advisory_xact_lock(7851000::bigint)"
    } else {
        "LOCK TABLE auto_queue_entries IN ACCESS EXCLUSIVE MODE"
    })
    .execute(&mut *barrier)
    .await
    .unwrap();
    let worker_pool = pool.clone();
    let worker = tokio::spawn(async move { operation.run(worker_pool).await });
    let (namespace, key) = operation.lock_key();
    let holder_pid = await_within(
        async {
            loop {
                let holder: Option<i32> = sqlx::query_scalar(
                    "SELECT pid FROM pg_locks
                     WHERE locktype = 'advisory' AND granted AND objsubid = 2
                       AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
                       AND classid = hashtext($1)::oid AND objid = hashtext($2)::oid
                       AND EXISTS (SELECT 1 FROM pg_stat_activity
                                   WHERE datname = current_database()
                                     AND $3 = ANY(pg_blocking_pids(pid)))",
                )
                .bind(namespace)
                .bind(key)
                .bind(blocker_pid)
                .fetch_optional(&mut observer)
                .await
                .unwrap();
                if let Some(pid) = holder {
                    break pid;
                }
                tokio::task::yield_now().await;
            }
        },
        WAIT_CAP,
    )
    .await;
    worker.abort();
    assert!(
        await_within(worker, WAIT_CAP)
            .await
            .unwrap_err()
            .is_cancelled()
    );
    barrier.rollback().await.unwrap();
    let mut first = await_within(pool.acquire(), WAIT_CAP).await.unwrap();
    let mut second = await_within(pool.acquire(), WAIT_CAP).await.unwrap();
    let first_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *first)
        .await
        .unwrap();
    let second_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *second)
        .await
        .unwrap();
    if ![first_pid, second_pid].contains(&holder_pid) {
        await_within(
            async {
                loop {
                    let ended: bool = sqlx::query_scalar(
                        "SELECT NOT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid = $1)",
                    )
                    .bind(holder_pid)
                    .fetch_one(&mut observer)
                    .await
                    .unwrap();
                    if ended {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            },
            WAIT_CAP,
        )
        .await;
    }
    eprintln!("{namespace}: holder={holder_pid}, next pool sessions=[{first_pid}, {second_pid}]");
    let available: bool =
        sqlx::query_scalar("SELECT pg_catalog.pg_try_advisory_lock(hashtext($1), hashtext($2))")
            .bind(namespace)
            .bind(key)
            .fetch_one(&mut observer)
            .await
            .unwrap();
    assert!(
        available,
        "cancelled {namespace} left its session lock in the pool: holder={holder_pid}, reused=[{first_pid}, {second_pid}]"
    );
    sqlx::query("SELECT pg_advisory_unlock(hashtext($1), hashtext($2))")
        .bind(namespace)
        .bind(key)
        .execute(&mut observer)
        .await
        .unwrap();
    drop((first, second));
    observer.close().await.unwrap();
    blocker.close().await.unwrap();
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn cancelled_slot_allocation_returns_without_advisory_lock_pg() {
    cancelled_holder_returns_without_lock(Operation::Allocate, false).await;
}

#[tokio::test]
async fn cancelled_slot_rebind_returns_without_advisory_lock_pg() {
    cancelled_holder_returns_without_lock(Operation::Rebind, false).await;
}

#[tokio::test]
async fn cancelled_activate_returns_without_advisory_lock_pg() {
    cancelled_holder_returns_without_lock(Operation::Activate, false).await;
}

#[tokio::test]
async fn cancelled_slot_allocation_acquire_returns_without_advisory_lock_pg() {
    cancelled_holder_returns_without_lock(Operation::Allocate, true).await;
}

#[tokio::test]
async fn cancelled_slot_rebind_acquire_returns_without_advisory_lock_pg() {
    cancelled_holder_returns_without_lock(Operation::Rebind, true).await;
}

#[tokio::test]
async fn cancelled_activate_acquire_returns_without_advisory_lock_pg() {
    cancelled_holder_returns_without_lock(Operation::Activate, true).await;
}

#[test]
fn clamp_retry_limit_bounds() {
    // Minimum boundary: 0 should be clamped to 1
    assert_eq!(clamp_retry_limit(0), 1);

    // Normal values should remain unchanged
    assert_eq!(clamp_retry_limit(1), 1);
    assert_eq!(clamp_retry_limit(3), 3);
    assert_eq!(clamp_retry_limit(10), 10);

    // Maximum boundary: > i64::MAX should be clamped to i64::MAX
    assert_eq!(clamp_retry_limit(i64::MAX as u64), i64::MAX);
    assert_eq!(clamp_retry_limit(i64::MAX as u64 + 10), i64::MAX);
    assert_eq!(clamp_retry_limit(u64::MAX), i64::MAX);
}

async fn setup_restore_candidate(pool: &PgPool, with_existing_dispatch: bool) {
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('fsm-gate-agent', 'FSM Gate Agent', 'claude', 'fsm-gate-channel')",
    )
    .execute(pool)
    .await
    .expect("seed FSM gate agent");
    sqlx::query(
        "INSERT INTO auto_queue_runs
                (id, repo, agent_id, status, max_concurrent_threads)
             VALUES ('fsm-gate-run', 'fsm-gate-repo', 'fsm-gate-agent', 'active', 1)",
    )
    .execute(pool)
    .await
    .expect("seed FSM gate run");
    sqlx::query(
        "INSERT INTO kanban_cards
                (id, title, status, assigned_agent_id, metadata)
             VALUES (
                'fsm-gate-card', 'FSM Gate Card', 'in_progress', 'fsm-gate-agent',
                '{\"sandbox_preflight\":true,\"production_mutation_allowed\":false}'::jsonb
             )",
    )
    .execute(pool)
    .await
    .expect("seed FSM gate card");
    sqlx::query(
        "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES (
                'fsm-gate-entry', 'fsm-gate-run', 'fsm-gate-card',
                'fsm-gate-agent', 'pending', 0
             )",
    )
    .execute(pool)
    .await
    .expect("seed FSM gate entry");
    if with_existing_dispatch {
        seed_recovered_dispatch(pool, "fsm-existing-dispatch").await;
    }
}

async fn seed_recovered_dispatch(pool: &PgPool, dispatch_id: &str) {
    sqlx::query(
        "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
             VALUES (
                $1, 'fsm-gate-card', 'fsm-gate-agent',
                'implementation', 'dispatched', 'Recovered dispatch'
             )",
    )
    .bind(dispatch_id)
    .execute(pool)
    .await
    .expect("seed FSM recovered dispatch");
    sqlx::query(
        "UPDATE kanban_cards
             SET latest_dispatch_id = $1
             WHERE id = 'fsm-gate-card'",
    )
    .bind(dispatch_id)
    .execute(pool)
    .await
    .expect("link FSM recovered dispatch");
}

fn restore_deps(pool: &PgPool) -> AutoQueueActivateDeps {
    let config = crate::config::Config::default();
    let engine = crate::engine::PolicyEngine::new_with_pg(
        &config,
        Some(pool.clone()),
        crate::services::discord::health::slot_row_host_refusal,
    )
    .expect("create FSM gate policy engine");
    AutoQueueActivateDeps {
        pg_pool: Some(pool.clone()),
        slot_row_guard: engine.slot_row_guard(),
        engine,
        config: Arc::new(config),
        health_registry: None,
        guild_id: None,
    }
}

fn restore_candidate() -> RestoreDispatchCandidate {
    RestoreDispatchCandidate {
        entry: RestoreEntryRecord {
            entry_id: "fsm-gate-entry".to_string(),
            card_id: "fsm-gate-card".to_string(),
            agent_id: "fsm-gate-agent".to_string(),
            thread_group: 0,
        },
        title: "FSM Gate Card".to_string(),
    }
}

async fn begin_run_token_holder(database_url: &str) -> PgConnection {
    let mut conn = PgConnection::connect(database_url)
        .await
        .expect("connect FSM run-token holder");
    sqlx::query("BEGIN")
        .execute(&mut conn)
        .await
        .expect("begin FSM run-token holder");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('aq_run:' || 'fsm-gate-run'))")
        .execute(&mut conn)
        .await
        .expect("hold FSM run token");
    conn
}

async fn run_token_waiter_sample(conn: &mut PgConnection) -> Result<(), String> {
    sqlx::query("SELECT pg_stat_clear_snapshot()")
        .execute(&mut *conn)
        .await
        .expect("clear FSM backend-status snapshot");
    let waiting = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
                 SELECT 1
                 FROM pg_stat_activity
                 WHERE datname = current_database()
                   AND wait_event_type = 'Lock'
                   AND query LIKE '%pg_advisory_xact_lock%aq_run:%'
             )",
    )
    .fetch_one(&mut *conn)
    .await
    .expect("inspect FSM run-token waiter");
    if waiting {
        Ok(())
    } else {
        Err("FSM restore path did not wait for the run token".into())
    }
}

async fn cancel_run_and_release_token(conn: &mut PgConnection) {
    sqlx::query(
        "UPDATE auto_queue_runs
             SET status = 'cancelled', completed_at = NOW()
             WHERE id = 'fsm-gate-run'",
    )
    .execute(&mut *conn)
    .await
    .expect("cancel FSM gate run");
    sqlx::query("COMMIT")
        .execute(&mut *conn)
        .await
        .expect("commit FSM gate cancellation");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_attach_existing_path_rechecks_cancelled_run_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_restore_candidate(&pool, true).await;
    let mut token_holder = begin_run_token_holder(&pg_db.database_url).await;
    let deps = restore_deps(&pool);
    let attempt = tokio::task::spawn_blocking(move || {
        attempt_restore_dispatch(&deps, "fsm-gate-run", &restore_candidate())
    });

    poll_within(
        async || run_token_waiter_sample(&mut token_holder).await,
        std::time::Duration::from_secs(5),
        "PostgreSQL lock waiter",
    )
    .await
    .unwrap();
    cancel_run_and_release_token(&mut token_holder).await;
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), attempt)
        .await
        .expect("FSM attach-existing attempt completes")
        .expect("join FSM attach-existing attempt")
        .expect_err("FSM attach-existing path must reject a cancelled run");
    assert!(error.contains("fsm-gate-run is cancelled"), "{error}");
    let entry_status: String =
        sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = 'fsm-gate-entry'")
            .fetch_one(&pool)
            .await
            .expect("load rejected FSM attach-existing entry");
    assert_eq!(entry_status, crate::db::auto_queue::ENTRY_STATUS_PENDING);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_reattach_path_rechecks_cancelled_run_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_restore_candidate(&pool, false).await;
    let mut token_holder = begin_run_token_holder(&pg_db.database_url).await;
    let deps = restore_deps(&pool);
    let attempt = tokio::task::spawn_blocking(move || {
        attempt_restore_dispatch(&deps, "fsm-gate-run", &restore_candidate())
    });

    poll_within(
        async || run_token_waiter_sample(&mut token_holder).await,
        std::time::Duration::from_secs(5),
        "PostgreSQL lock waiter",
    )
    .await
    .unwrap();
    seed_recovered_dispatch(&pool, "fsm-recovered-dispatch").await;
    cancel_run_and_release_token(&mut token_holder).await;
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), attempt)
        .await
        .expect("FSM recovery reattach attempt completes")
        .expect("join FSM recovery reattach attempt")
        .expect("FSM recovery path reports a rejected reattach as not dispatched");
    assert!(!result.dispatched);
    let entry_status: String =
        sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = 'fsm-gate-entry'")
            .fetch_one(&pool)
            .await
            .expect("load rejected FSM recovery entry");
    assert_eq!(entry_status, crate::db::auto_queue::ENTRY_STATUS_PENDING);

    pool.close().await;
    pg_db.drop().await;
}

fn must_ok<T, E: std::fmt::Debug>(result: Result<T, E>, context: &str) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{context}: {error:?}"), // agentdesk-audit: allow-unwrap — test-only helper in #[cfg(test)] module
    }
}

#[tokio::test]
async fn production_failure_wrapper_preserves_restoring_slot_binding_pg() {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    must_ok(
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, status, max_concurrent_threads)
                 VALUES ('run-restore-wrapper', 'restoring', 1)",
        )
        .execute(&pool)
        .await,
        "seed restoring run",
    );
    must_ok(
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                     id, run_id, agent_id, status, retry_count, slot_index, thread_group
                 ) VALUES (
                     'entry-restore-wrapper', 'run-restore-wrapper', 'agent-1',
                     'dispatched', 0, 0, 0
                 )",
        )
        .execute(&pool)
        .await,
        "seed restoring dispatched entry",
    );
    must_ok(
        sqlx::query(
            "INSERT INTO auto_queue_slots (
                     agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
                 ) VALUES ('agent-1', 0, 'run-restore-wrapper', 0, '{}'::jsonb)",
        )
        .execute(&pool)
        .await,
        "seed restoring slot",
    );
    let config = crate::config::Config::default();
    let engine = must_ok(
        crate::engine::PolicyEngine::new_with_pg(
            &config,
            Some(pool.clone()),
            crate::services::discord::health::slot_row_host_refusal,
        ),
        "create test policy engine",
    );
    let deps = AutoQueueActivateDeps {
        pg_pool: Some(pool.clone()),
        slot_row_guard: engine.slot_row_guard(),
        engine,
        config: Arc::new(config),
        health_registry: None,
        guild_id: None,
    };

    let result = must_ok(
        record_entry_dispatch_failure(
            &deps,
            "entry-restore-wrapper",
            "restore_run_create_dispatch_failed",
        ),
        "production failure wrapper",
    );
    assert_eq!(
        result.to_status,
        crate::db::auto_queue::ENTRY_STATUS_PENDING
    );
    let state = must_ok(
        sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT e.status, r.status, s.assigned_run_id
                 FROM auto_queue_entries e
                 JOIN auto_queue_runs r ON r.id = e.run_id
                 JOIN auto_queue_slots s ON s.agent_id = e.agent_id AND s.slot_index = 0
                 WHERE e.id = 'entry-restore-wrapper'",
        )
        .fetch_one(&pool)
        .await,
        "load production-wrapper state",
    );
    assert_eq!(
        state,
        (
            "pending".to_string(),
            "restoring".to_string(),
            Some("run-restore-wrapper".to_string()),
        )
    );

    pool.close().await;
    pg_db.drop().await;
}
