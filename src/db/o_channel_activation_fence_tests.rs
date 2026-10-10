use super::{ActivationRows, activation_rows, fenced_activation_rows};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox::{FailedPreAcceptSweepOutcome, sweep_failed_pre_accept_once};
use crate::services::message_outbox::{OutboxMessage, enqueue_outbox_pg_returning_id_with_ttl};
use sqlx::PgPool;
use std::time::Duration;

async fn intake(pool: &PgPool, channel: &str, status: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO intake_outbox (
            target_instance_id, forwarded_by_instance_id, channel_id, user_msg_id,
            request_owner_id, user_text, turn_kind, agent_id, provider, status,
            admission_kind, dispatched_at, completed_at
         ) VALUES ('worker', 'leader', $1, $2, 'user', 'hi', 'standard', 'agent',
            'claude', $3, 'local', CASE WHEN $3 = 'dispatched' THEN NOW() END,
            CASE WHEN $3 = 'failed_pre_accept' THEN NOW() END) RETURNING id",
    )
    .bind(channel)
    .bind(format!("message-{channel}-{status}"))
    .bind(status)
    .fetch_one(pool)
    .await
}

async fn session(
    pool: &PgPool,
    key: &str,
    channel: &str,
    instance: Option<&str>,
    status: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, channel_id, instance_id)
         VALUES ($1, 'claude', $2, $3, $4)",
    )
    .bind(key)
    .bind(status)
    .bind(channel)
    .bind(instance)
    .execute(pool)
    .await?;
    Ok(())
}

async fn message(
    pool: &PgPool,
    target: &str,
    source: &str,
    status: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let id = enqueue_outbox_pg_returning_id_with_ttl(
        pool,
        OutboxMessage {
            target,
            content: "body",
            bot: "captain",
            source,
            reason_code: None,
            session_key: None,
        },
        0,
    )
    .await?
    .ok_or_else(|| sqlx::Error::Protocol("fixture enqueue returned no row".into()))?;
    sqlx::query("UPDATE message_outbox SET status = $1 WHERE id = $2")
        .bind(status)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn retryable_parent(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query(
        "INSERT INTO worker_nodes (instance_id, status, labels, capabilities, last_heartbeat_at)
         VALUES ('worker', 'online', '[]',
            '{\"intake_worker\":{\"enabled\":true,\"providers\":[\"claude\"]}}', NOW())",
    )
    .execute(pool)
    .await?;
    intake(pool, "7", "failed_pre_accept").await
}

async fn independent_pool(pg_db: &TestPostgresDb) -> PgPool {
    crate::db::postgres::connect_test_pool_with_max_connections(
        &pg_db.database_url,
        "o activation fence tests",
        1,
    )
    .await
    .expect("independent fence test connection") // agentdesk-audit: allow-unwrap — isolated PostgreSQL fixture
}

async fn child_count(pool: &PgPool, parent: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox WHERE parent_outbox_id = $1")
        .bind(parent)
        .fetch_one(pool)
        .await
}

async fn wait_for_sweep_lock(pool: &PgPool, backend: i32) -> Result<bool, sqlx::Error> {
    for _ in 0..200 {
        let blocked: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM pg_locks
                 WHERE pid = $1 AND relation = 'intake_outbox'::regclass
                   AND mode = 'RowExclusiveLock' AND NOT granted)",
        )
        .bind(backend)
        .fetch_one(pool)
        .await?;
        if blocked {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(false)
}

#[tokio::test]
async fn fenced_rows_preserve_all_open_status_and_legacy_session_predicates_pg()
-> Result<(), sqlx::Error> {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    for status in ["pending", "claimed", "accepted", "spawned", "dispatched"] {
        intake(&pool, status, status).await?;
        let expected = activation_rows(&pool, status, "gateway").await?;
        let (hold, actual, queued) = fenced_activation_rows(&pool, status, "gateway").await?;
        assert_eq!(actual, expected);
        assert_eq!(actual.open_intake, 1, "{status} is open intake");
        assert_eq!(queued, 0);
        hold.release().await?;
    }
    for status in ["done", "unknown", "failed_pre_accept", "failed_post_accept"] {
        intake(&pool, status, status).await?;
        let (hold, actual, _) = fenced_activation_rows(&pool, status, "gateway").await?;
        assert_eq!(actual.open_intake, 0, "{status} is terminal");
        hold.release().await?;
    }
    session(&pool, "local", "7", Some(" gateway "), "idle").await?;
    session(&pool, "gone", "7", Some("runner"), "disconnected").await?;
    session(&pool, "aborted", "7", Some("runner"), "aborted").await?;
    session(&pool, "blank", "7", Some(" "), "idle").await?;
    session(&pool, "old", "7", None, "idle").await?;
    session(&pool, "other", "8", Some("runner"), "turn_active").await?;
    session(&pool, "foreign", "7", Some(" runner "), "idle").await?;
    let expected = activation_rows(&pool, "7", "gateway").await?;
    let (hold, actual, _) = fenced_activation_rows(&pool, "7", "gateway").await?;
    assert_eq!(actual, expected);
    assert_eq!(actual.foreign_sessions, 1);
    hold.release().await?;
    pool.close().await;
    pg_db.drop().await;
    Ok(())
}

#[tokio::test]
async fn fenced_rows_count_pending_and_processing_headless_bodies_only_pg()
-> Result<(), Box<dyn std::error::Error>> {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    for status in ["pending", "processing", "sent", "failed"] {
        message(&pool, "channel:7", "headless_turn", status).await?;
    }
    message(&pool, "channel:8", "headless_turn", "pending").await?;
    message(&pool, "channel:7", "system", "pending").await?;
    message(&pool, "7", "headless_turn", "processing").await?;
    let (hold, rows, queued) = fenced_activation_rows(&pool, "7", "gateway").await?;
    assert_eq!(rows, ActivationRows::default());
    assert_eq!(queued, 2);
    hold.release().await?;
    sqlx::query(
        "UPDATE message_outbox SET status = 'sent'
         WHERE target = 'channel:7' AND source = 'headless_turn'
           AND status IN ('pending', 'processing')",
    )
    .execute(&pool)
    .await?;
    let (hold, _, queued) = fenced_activation_rows(&pool, "7", "gateway").await?;
    assert_eq!(queued, 0);
    hold.release().await?;
    pool.close().await;
    pg_db.drop().await;
    Ok(())
}

#[tokio::test]
async fn sweep_committed_before_fence_is_visible_in_fresh_facts_pg() -> Result<(), sqlx::Error> {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let parent = retryable_parent(&pool).await?;
    assert_eq!(activation_rows(&pool, "7", "gateway").await?.open_intake, 0);
    let sweep_pool = independent_pool(&pg_db).await;
    let outcome = sweep_failed_pre_accept_once(&sweep_pool, "leader", 5, 60, None).await?;
    assert!(
        matches!(outcome, FailedPreAcceptSweepOutcome::Retried { source_id, .. } if source_id == parent)
    );
    let (hold, rows, _) = fenced_activation_rows(&pool, "7", "gateway").await?;
    assert_eq!(
        rows.open_intake, 1,
        "the committed retry must block activation"
    );
    assert_eq!(child_count(&pool, parent).await?, 1);
    hold.release().await?;
    sweep_pool.close().await;
    pool.close().await;
    pg_db.drop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fence_blocks_actual_sweep_backend_until_release_or_drop_pg() -> Result<(), sqlx::Error> {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let parent = retryable_parent(&pool).await?;
    for explicit_release in [true, false] {
        let sweep_pool = independent_pool(&pg_db).await;
        let backend: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&sweep_pool)
            .await?;
        let (hold, rows, _) = fenced_activation_rows(&pool, "7", "gateway").await?;
        assert_eq!(rows.open_intake, 0);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let task_pool = sweep_pool.clone();
        let sweep = tokio::spawn(async move {
            let current_backend: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&task_pool)
                .await?;
            assert_eq!(
                current_backend, backend,
                "the sweep uses its independent backend"
            );
            let _ = started_tx.send(());
            sweep_failed_pre_accept_once(&task_pool, "leader", 5, 60, None).await
        });
        started_rx.await.expect("sweep start barrier"); // agentdesk-audit: allow-unwrap — PostgreSQL test barrier
        let blocked = wait_for_sweep_lock(&pool, backend).await?;
        let no_child = child_count(&pool, parent).await? == 0;
        let unfinished = !sweep.is_finished();
        if explicit_release {
            hold.release().await?;
        } else {
            drop(hold);
        }
        let outcome = tokio::time::timeout(Duration::from_secs(5), sweep)
            .await
            .expect("sweep finishes after fence release") // agentdesk-audit: allow-unwrap — PostgreSQL lock deadline
            .expect("join actual sweep")?; // agentdesk-audit: allow-unwrap — PostgreSQL test task
        assert!(
            blocked,
            "sweep backend must wait for RowExclusiveLock, not pool capacity"
        );
        assert!(
            unfinished && no_child,
            "no retry child may commit before fence release"
        );
        assert!(
            matches!(outcome, FailedPreAcceptSweepOutcome::Retried { source_id, .. } if source_id == parent)
        );
        assert_eq!(child_count(&pool, parent).await?, 1);
        sqlx::query("DELETE FROM intake_outbox WHERE parent_outbox_id = $1")
            .bind(parent)
            .execute(&pool)
            .await?;
        sweep_pool.close().await;
    }
    pool.close().await;
    pg_db.drop().await;
    Ok(())
}

#[tokio::test]
async fn intake_fence_uses_app_role_update_privilege_and_rejects_select_only_pg()
-> Result<(), sqlx::Error> {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let role = format!("adk_fence_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE ROLE {role} NOLOGIN NOSUPERUSER NOINHERIT"))
        .execute(&pool)
        .await?;
    sqlx::query(&format!("GRANT USAGE ON SCHEMA public TO {role}"))
        .execute(&pool)
        .await?;
    sqlx::query(&format!(
        "GRANT SELECT, INSERT, UPDATE ON intake_outbox TO {role}"
    ))
    .execute(&pool)
    .await?;
    sqlx::query(&format!(
        "GRANT SELECT ON sessions, message_outbox TO {role}"
    ))
    .execute(&pool)
    .await?;
    let app_pool = independent_pool(&pg_db).await;
    sqlx::query(&format!("SET ROLE {role}"))
        .execute(&app_pool)
        .await?;
    let identity: (String, bool) = sqlx::query_as(
        "SELECT current_user::text, rolsuper FROM pg_roles WHERE rolname = current_user",
    )
    .fetch_one(&app_pool)
    .await?;
    assert_eq!(identity, (role.clone(), false));
    let (hold, rows, queued) = fenced_activation_rows(&app_pool, "7", "gateway").await?;
    assert_eq!(rows, ActivationRows::default());
    assert_eq!(queued, 0);
    hold.release().await?;
    sqlx::query(&format!(
        "REVOKE INSERT, UPDATE ON intake_outbox FROM {role}"
    ))
    .execute(&pool)
    .await?;
    let result = fenced_activation_rows(&app_pool, "7", "gateway").await;
    match result {
        Err(sqlx::Error::Database(error)) => assert_eq!(error.code().as_deref(), Some("42501")),
        Err(error) => panic!("expected LOCK permission denial, got {error}"),
        Ok((hold, _, _)) => {
            hold.release().await?;
            panic!("SELECT-only role must not acquire SHARE fence");
        }
    }
    app_pool.close().await;
    sqlx::query(&format!("DROP OWNED BY {role}"))
        .execute(&pool)
        .await?;
    sqlx::query(&format!("DROP ROLE {role}"))
        .execute(&pool)
        .await?;
    pool.close().await;
    pg_db.drop().await;
    Ok(())
}
