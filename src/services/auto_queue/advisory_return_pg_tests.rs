use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::test_support::wait::{WAIT_CAP, await_within};
use sqlx::{Connection, PgConnection, PgPool};

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
            Self::Rebind => slot_routes::rebind_slot_with_pg(
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
                let engine = crate::engine::PolicyEngine::new_with_pg(&config, Some(pool.clone()))
                    .expect("create activate engine");
                activate_with_deps_pg(
                    &AutoQueueActivateDeps {
                        pg_pool: Some(pool),
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
