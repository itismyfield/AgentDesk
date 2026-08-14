//! Read-only inventory of `intake_outbox` rows in `dispatched`.

use super::intake_outbox_force_fail::force_fail_provider_ready;
use super::intake_outbox_status::IntakeOutboxStatus;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

#[derive(Debug, sqlx::FromRow)]
struct DispatchedAuditProjection {
    id: i64,
    channel_id: String,
    user_msg_id: String,
    attempt_no: i32,
    parent_outbox_id: Option<i64>,
    dispatched_at: Option<DateTime<Utc>>,
    claim_owner: Option<String>,
    provider: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DispatchedAuditRow {
    pub(crate) id: i64,
    pub(crate) channel_id: String,
    pub(crate) user_msg_id: String,
    pub(crate) attempt_no: i32,
    pub(crate) parent_outbox_id: Option<i64>,
    pub(crate) dispatched_at: Option<DateTime<Utc>>,
    pub(crate) claim_owner: Option<String>,
    pub(crate) provider: String,
    pub(crate) provider_nonempty: bool,
}

/// Lists every dispatched row without an explicit multi-statement transaction,
/// row lock, or advisory lock. Each SELECT runs in an implicit transaction and
/// takes an ACCESS SHARE relation lock; it is compatible with DML but can delay
/// ACCESS EXCLUSIVE DDL for the duration of the query.
pub(crate) async fn list_dispatched_audit(
    pool: &PgPool,
) -> Result<Vec<DispatchedAuditRow>, sqlx::Error> {
    let rows: Vec<DispatchedAuditProjection> = sqlx::query_as(
        "SELECT id, channel_id, user_msg_id, attempt_no, parent_outbox_id,
                dispatched_at, claim_owner, provider
           FROM public.intake_outbox
          WHERE status = $1
          ORDER BY dispatched_at ASC NULLS FIRST, id ASC",
    )
    .bind(IntakeOutboxStatus::Dispatched)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| DispatchedAuditRow {
            id: row.id,
            channel_id: row.channel_id,
            user_msg_id: row.user_msg_id,
            attempt_no: row.attempt_no,
            parent_outbox_id: row.parent_outbox_id,
            dispatched_at: row.dispatched_at,
            claim_owner: row.claim_owner,
            provider_nonempty: force_fail_provider_ready(&row.provider),
            provider: row.provider,
        })
        .collect())
}

#[cfg(test)]
mod postgres_tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;

    async fn seed(
        pool: &PgPool,
        key: &str,
        status: IntakeOutboxStatus,
        dispatched_at: Option<DateTime<Utc>>,
        provider: &str,
    ) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO intake_outbox (
                target_instance_id, forwarded_by_instance_id, channel_id,
                user_msg_id, request_owner_id, user_text, turn_kind, agent_id,
                provider, status, claim_owner, dispatched_at
             ) VALUES (
                'worker', 'leader', $1, $1, 'user', 'hello', 'standard', 'agent',
                $2, $3, 'dispatch-worker', $4
             ) RETURNING id",
        )
        .bind(key)
        .bind(provider)
        .bind(status)
        .bind(dispatched_at)
        .fetch_one(pool)
        .await
        .expect("seed intake outbox audit row") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
    }

    async fn setup() -> (TestPostgresDb, PgPool) {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        (pg_db, pool)
    }

    async fn teardown(pg_db: TestPostgresDb, pool: PgPool) {
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn list_dispatched_audit_returns_only_dispatched_rows_pg() {
        let (pg_db, pool) = setup().await;
        let dispatched = seed(
            &pool,
            "dispatched",
            IntakeOutboxStatus::Dispatched,
            Some(Utc::now()),
            "claude",
        )
        .await;
        seed(&pool, "done", IntakeOutboxStatus::Done, None, "codex").await;

        let rows = list_dispatched_audit(&pool).await.expect("list audit rows"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, dispatched);
        teardown(pg_db, pool).await;
    }

    #[tokio::test]
    async fn list_dispatched_audit_includes_null_clock_rows_pg() {
        let (pg_db, pool) = setup().await;
        sqlx::query(
            "ALTER TABLE intake_outbox
             DROP CONSTRAINT intake_outbox_dispatched_requires_clock",
        )
        .execute(&pool)
        .await
        .expect("emulate a row created before the clock check"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        let id = seed(
            &pool,
            "null-clock",
            IntakeOutboxStatus::Dispatched,
            None,
            "claude",
        )
        .await;

        let rows = list_dispatched_audit(&pool).await.expect("list audit rows"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].dispatched_at, None);
        teardown(pg_db, pool).await;
    }

    #[tokio::test]
    async fn list_dispatched_audit_marks_empty_provider_false_pg() {
        let (pg_db, pool) = setup().await;
        seed(
            &pool,
            "empty-provider",
            IntakeOutboxStatus::Dispatched,
            Some(Utc::now()),
            " \t",
        )
        .await;

        let rows = list_dispatched_audit(&pool).await.expect("list audit rows"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        assert!(!rows[0].provider_nonempty);
        teardown(pg_db, pool).await;
    }

    #[tokio::test]
    async fn list_dispatched_audit_returns_empty_when_no_rows_pg() {
        let (pg_db, pool) = setup().await;
        assert!(
            list_dispatched_audit(&pool)
                .await
                .expect("list empty audit") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
                .is_empty()
        );
        teardown(pg_db, pool).await;
    }
}
