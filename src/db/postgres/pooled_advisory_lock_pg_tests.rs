use super::PooledAdvisoryLock;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::postgres;
use crate::test_support::wait::{WAIT_CAP, await_within, join_within};
use sqlx::{Connection, PgConnection, PgPool};

const NAMESPACE: &str = "pooled_lock_return";
const KEY: &str = "key";

struct Fixture {
    db: TestPostgresDb,
    pool: PgPool,
    observer: PgConnection,
}

impl Fixture {
    async fn new() -> Self {
        let db = TestPostgresDb::create().await;
        let pool = postgres::connect_test_pool(&db.database_url, NAMESPACE)
            .await
            .unwrap();
        let observer = PgConnection::connect(&db.database_url).await.unwrap();
        Self { db, pool, observer }
    }

    async fn prepare(&mut self, sql: &'static str) -> i32 {
        sqlx::raw_sql(sql)
            .execute(&mut self.observer)
            .await
            .unwrap();
        let mut conn = self.pool.acquire().await.unwrap();
        sqlx::query("SET search_path = public, pg_catalog")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *conn)
            .await
            .unwrap()
    }

    async fn holder_pid(&mut self) -> i32 {
        sqlx::query_scalar(
            "SELECT pid FROM pg_locks WHERE locktype = 'advisory' AND granted AND objsubid = 2
             AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
             AND classid = hashtext($1)::oid AND objid = hashtext($2)::oid",
        )
        .bind(NAMESPACE)
        .bind(KEY)
        .fetch_one(&mut self.observer)
        .await
        .unwrap()
    }

    async fn assert_return(&mut self, holder_pid: i32, reused: bool) {
        let mut conn = await_within(self.pool.acquire(), WAIT_CAP).await.unwrap();
        let next_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(next_pid == holder_pid, reused);
        if !reused {
            await_within(
                async {
                    loop {
                        let ended: bool = sqlx::query_scalar(
                            "SELECT NOT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid = $1)",
                        )
                        .bind(holder_pid)
                        .fetch_one(&mut self.observer)
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
        let available: bool = sqlx::query_scalar(
            "SELECT pg_catalog.pg_try_advisory_lock(hashtext($1), hashtext($2))",
        )
        .bind(NAMESPACE)
        .bind(KEY)
        .fetch_one(&mut self.observer)
        .await
        .unwrap();
        assert!(
            available,
            "holder {holder_pid} left a lock; next session {next_pid}"
        );
        sqlx::query("SELECT pg_catalog.pg_advisory_unlock(hashtext($1), hashtext($2))")
            .bind(NAMESPACE)
            .bind(KEY)
            .execute(&mut self.observer)
            .await
            .unwrap();
    }

    async fn close(self) {
        self.observer.close().await.unwrap();
        self.pool.close().await;
        self.db.drop().await;
    }
}

#[tokio::test]
async fn pooled_advisory_unlock_reuses_the_same_session_pg() {
    let mut fixture = Fixture::new().await;
    let lock = PooledAdvisoryLock::acquire(&fixture.pool, NAMESPACE, KEY)
        .await
        .unwrap();
    let pid = fixture.holder_pid().await;
    lock.unlock().await.unwrap();
    fixture.assert_return(pid, true).await;
    fixture.close().await;
}

#[tokio::test]
async fn pooled_advisory_unlock_error_discards_the_session_pg() {
    let mut fixture = Fixture::new().await;
    fixture
        .prepare(
            "CREATE FUNCTION public.pg_advisory_unlock(integer, integer) RETURNS boolean
         LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected unlock failure'; END $$;",
        )
        .await;
    let lock = PooledAdvisoryLock::acquire(&fixture.pool, NAMESPACE, KEY)
        .await
        .unwrap();
    let pid = fixture.holder_pid().await;
    let error = lock.unlock().await.unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("P0001")
    );
    fixture.assert_return(pid, false).await;
    fixture.close().await;
}

#[tokio::test]
async fn pooled_advisory_acquire_error_discards_the_session_pg() {
    let mut fixture = Fixture::new().await;
    let pid = fixture
        .prepare(
            "CREATE FUNCTION public.pg_advisory_lock(integer, integer) RETURNS void
         LANGUAGE plpgsql AS $$ BEGIN
           PERFORM pg_catalog.pg_advisory_lock($1, $2);
           RAISE EXCEPTION 'injected acquire failure';
         END $$;",
        )
        .await;
    let Err(error) = PooledAdvisoryLock::acquire(&fixture.pool, NAMESPACE, KEY).await else {
        panic!("the acquisition must report the server error");
    };
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("P0001")
    );
    fixture.assert_return(pid, false).await;
    fixture.close().await;
}

#[tokio::test]
async fn pooled_advisory_drop_without_a_runtime_discards_the_session_pg() {
    let mut fixture = Fixture::new().await;
    let lock = PooledAdvisoryLock::acquire(&fixture.pool, NAMESPACE, KEY)
        .await
        .unwrap();
    let pid = fixture.holder_pid().await;
    join_within(std::thread::spawn(move || drop(lock)), WAIT_CAP);
    fixture.assert_return(pid, false).await;
    fixture.close().await;
}

#[tokio::test]
async fn pooled_advisory_cancelled_unlock_discards_the_session_pg() {
    let mut fixture = Fixture::new().await;
    fixture
        .prepare(
            "CREATE FUNCTION public.pg_advisory_unlock(integer, integer) RETURNS boolean
         LANGUAGE plpgsql AS $$ BEGIN
           PERFORM pg_catalog.pg_advisory_xact_lock(7851001::bigint);
           RETURN pg_catalog.pg_advisory_unlock($1, $2);
         END $$;",
        )
        .await;
    let lock = PooledAdvisoryLock::acquire(&fixture.pool, NAMESPACE, KEY)
        .await
        .unwrap();
    let holder_pid = fixture.holder_pid().await;
    let mut blocker = PgConnection::connect(&fixture.db.database_url)
        .await
        .unwrap();
    let mut barrier = blocker.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(7851001::bigint)")
        .execute(&mut *barrier)
        .await
        .unwrap();
    let unlock = tokio::spawn(async move { lock.unlock().await });
    await_within(
        async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid = $1
                 AND wait_event_type = 'Lock' AND wait_event = 'advisory')",
                )
                .bind(holder_pid)
                .fetch_one(&mut fixture.observer)
                .await
                .unwrap();
                if waiting {
                    break;
                }
                tokio::task::yield_now().await;
            }
        },
        WAIT_CAP,
    )
    .await;
    unlock.abort();
    assert!(
        await_within(unlock, WAIT_CAP)
            .await
            .unwrap_err()
            .is_cancelled()
    );
    barrier.rollback().await.unwrap();
    blocker.close().await.unwrap();
    fixture.assert_return(holder_pid, false).await;
    fixture.close().await;
}
