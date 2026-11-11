use super::fail_or_retry;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::dispatched_sessions::RetryDispatchMeta;
use crate::services::dispatches::wait_queue::cached_wakes;
use sqlx::PgPool;
use std::time::Duration;

async fn setup() -> (TestPostgresDb, PgPool, RetryDispatchMeta) {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate_with_max_connections(8).await;
    let card_id = uuid::Uuid::new_v4().to_string();
    let origin_dispatch_id = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO kanban_cards (id, title, status) VALUES ($1, 'retry', 'in_progress')")
        .bind(&card_id)
        .execute(&pool)
        .await
        .expect("seed retry card");
    sqlx::query("INSERT INTO task_dispatches (id, kanban_card_id, status, dispatch_type, title, context, retry_count) VALUES ($1, $2, 'dispatched', 'implementation', 'retry', '{}', 2)")
        .bind(&origin_dispatch_id).bind(&card_id).execute(&pool).await.expect("seed retry origin");
    let meta = RetryDispatchMeta {
        origin_dispatch_id,
        card_id,
        to_agent_id: None,
        dispatch_type: Some("implementation".to_string()),
        title: Some("retry".to_string()),
        context: Some("{}".to_string()),
        retry_count: 2,
    };
    (database, pool, meta)
}

async fn wait_for_commit_barrier(pool: &PgPool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let blocked: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid WHERE a.datname = current_database() AND l.locktype = 'advisory' AND l.objid = 735309 AND NOT l.granted)",
            ).fetch_one(pool).await.expect("inspect commit barrier");
            if blocked { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("retry reaches deferred commit barrier");
}

async fn commit_boundary(reject: bool) {
    let (database, pool, meta) = setup().await;
    let origin_id = meta.origin_dispatch_id.clone();
    let card_id = meta.card_id.clone();
    sqlx::raw_sql(
        "CREATE FUNCTION retry_commit_barrier() RETURNS trigger LANGUAGE plpgsql AS $body$
         BEGIN
           PERFORM pg_advisory_xact_lock(735309);
           IF TG_ARGV[0] = 'reject' THEN RAISE EXCEPTION 'retry commit rejected'; END IF;
           RETURN NEW;
         END $body$;",
    )
    .execute(&pool)
    .await
    .expect("install deferred commit function");
    let trigger = if reject {
        "CREATE CONSTRAINT TRIGGER retry_commit_barrier AFTER UPDATE ON task_dispatches DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (NEW.status = 'failed') EXECUTE FUNCTION retry_commit_barrier('reject')"
    } else {
        "CREATE CONSTRAINT TRIGGER retry_commit_barrier AFTER UPDATE ON task_dispatches DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (NEW.status = 'failed') EXECUTE FUNCTION retry_commit_barrier('allow')"
    };
    sqlx::query(trigger)
        .execute(&pool)
        .await
        .expect("install deferred commit trigger");
    let mut barrier = pool.begin().await.expect("begin barrier holder");
    sqlx::query("SELECT pg_advisory_xact_lock(735309)")
        .execute(&mut *barrier)
        .await
        .expect("hold commit barrier");
    let retry = tokio::spawn({
        let pool = pool.clone();
        async move { fail_or_retry(&pool, "retry-wake-session", Some(meta)).await }
    });
    wait_for_commit_barrier(&pool).await;
    assert!(cached_wakes(&origin_id).is_empty(), "no wake before commit");
    let before: String = sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
        .bind(&origin_id)
        .fetch_one(&pool)
        .await
        .expect("read uncommitted origin");
    assert_eq!(before, "dispatched");
    barrier.commit().await.expect("release commit barrier");
    let result = retry.await.expect("join service retry");
    let after: String = sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
        .bind(&origin_id)
        .fetch_one(&pool)
        .await
        .expect("read committed origin");
    let replacements: Vec<String> =
        sqlx::query_scalar("SELECT id FROM task_dispatches WHERE kanban_card_id = $1 AND id <> $2")
            .bind(&card_id)
            .bind(&origin_id)
            .fetch_all(&pool)
            .await
            .expect("read replacement rows");
    if reject {
        assert!(
            result
                .expect_err("retry and fallback commit roll back")
                .contains("retry commit rejected")
        );
        assert_eq!(after, "dispatched");
        assert!(replacements.is_empty());
        assert!(cached_wakes(&origin_id).is_empty(), "rollback never wakes");
    } else {
        let (replacement, skipped) = result.expect("service retry commits");
        assert_eq!(after, "failed");
        assert_eq!(replacements.len(), 1);
        assert_eq!(replacement.as_deref(), Some(replacements[0].as_str()));
        assert_eq!(skipped, None);
        assert_eq!(
            cached_wakes(&origin_id),
            vec![(
                pool.connect_options().get_database().map(str::to_owned),
                "constraint_release",
                origin_id.clone(),
                "dispatch_terminal_status"
            )],
            "one wake with the original pool, origin, trigger and source"
        );
    }
    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn service_retry_wakes_once_only_after_commit_pg() {
    commit_boundary(false).await;
}

#[tokio::test]
async fn service_retry_and_fallback_rollback_never_wake_pg() {
    commit_boundary(true).await;
}

#[tokio::test]
async fn service_retry_missing_origin_error_never_wakes_pg() {
    let (database, pool, meta) = setup().await;
    let origin_id = meta.origin_dispatch_id.clone();
    sqlx::query("DELETE FROM task_dispatches WHERE id = $1")
        .bind(&origin_id)
        .execute(&pool)
        .await
        .expect("remove retry origin");
    let (replacement, skipped) = fail_or_retry(&pool, "retry-wake-session", Some(meta))
        .await
        .expect("missing origin fallback is an unchanged dispatch");
    assert_eq!((replacement, skipped), (None, None));
    assert!(cached_wakes(&origin_id).is_empty());
    pool.close().await;
    database.drop().await;
}
