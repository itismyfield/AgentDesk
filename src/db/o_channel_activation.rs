//! Read-only rows that keep a channel out of its first O activation: open intake routes and
//! sessions owned by another node.

use super::intake_outbox_open_status::INTAKE_OUTBOX_OPEN_STATUSES_SQL;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

/// Keeps intake writers behind the facts until the channel's init has been published.
pub struct IntakeFence {
    _transaction: Option<Transaction<'static, Postgres>>,
}

// Explicit release is exercised by tests until deferred activation starts using the fence.
#[cfg(test)]
impl IntakeFence {
    pub async fn release(mut self) -> Result<(), sqlx::Error> {
        if let Some(transaction) = self._transaction.take() {
            transaction.rollback().await?;
        }
        Ok(())
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ActivationRows {
    pub(crate) open_intake: i64,
    pub(crate) foreign_sessions: i64,
}

/// Counts the channel's open intake rows and its live sessions owned by any instance other than
/// `local_instance`, using the owner predicate the intake router resolves owners with.
pub(crate) async fn activation_rows(
    pool: &PgPool,
    channel_id: &str,
    local_instance: &str,
) -> Result<ActivationRows, sqlx::Error> {
    let open_intake: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM intake_outbox
          WHERE channel_id = $1 AND status IN ({INTAKE_OUTBOX_OPEN_STATUSES_SQL})"
    ))
    .bind(channel_id)
    .fetch_one(pool)
    .await?;
    let foreign_sessions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions
          WHERE channel_id = $1
            AND COALESCE(LOWER(BTRIM(status)), '') NOT IN ('disconnected', 'aborted')
            AND NULLIF(BTRIM(instance_id), '') IS NOT NULL
            AND BTRIM(instance_id) <> $2",
    )
    .bind(channel_id)
    .bind(local_instance)
    .fetch_one(pool)
    .await?;
    Ok(ActivationRows {
        open_intake,
        foreign_sessions,
    })
}

async fn read_activation_rows(
    connection: &mut PgConnection,
    channel_id: &str,
    local_instance: &str,
) -> Result<ActivationRows, sqlx::Error> {
    let open_intake: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM intake_outbox
          WHERE channel_id = $1 AND status IN ({INTAKE_OUTBOX_OPEN_STATUSES_SQL})"
    ))
    .bind(channel_id)
    .fetch_one(&mut *connection)
    .await?;
    let foreign_sessions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions
          WHERE channel_id = $1
            AND COALESCE(LOWER(BTRIM(status)), '') NOT IN ('disconnected', 'aborted')
            AND NULLIF(BTRIM(instance_id), '') IS NOT NULL
            AND BTRIM(instance_id) <> $2",
    )
    .bind(channel_id)
    .bind(local_instance)
    .fetch_one(&mut *connection)
    .await?;
    Ok(ActivationRows {
        open_intake,
        foreign_sessions,
    })
}

/// SHARE conflicts with every intake DML path, including writes made by another node.
pub(crate) async fn fenced_activation_rows(
    pool: &PgPool,
    channel_id: &str,
    local_instance: &str,
) -> Result<(IntakeFence, ActivationRows, i64), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '2s'")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("LOCK TABLE intake_outbox IN SHARE MODE")
        .execute(&mut *transaction)
        .await?;
    let rows = read_activation_rows(&mut transaction, channel_id, local_instance).await?;
    let queued_bodies = sqlx::query_scalar(
        "SELECT COUNT(*) FROM message_outbox
         WHERE target = $1 AND source = 'headless_turn' AND status IN ('pending', 'processing')",
    )
    .bind(format!("channel:{channel_id}"))
    .fetch_one(&mut *transaction)
    .await?;
    Ok((
        IntakeFence {
            _transaction: Some(transaction),
        },
        rows,
        queued_bodies,
    ))
}

#[cfg(test)]
#[path = "o_channel_activation_fence_tests.rs"]
mod fence_tests;

#[cfg(test)]
mod postgres_tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;

    async fn intake(pool: &PgPool, channel: &str, key: &str, status: &str) {
        sqlx::query(
            "INSERT INTO intake_outbox (
                target_instance_id, forwarded_by_instance_id, channel_id,
                user_msg_id, request_owner_id, user_text, turn_kind, agent_id,
                provider, status, claim_owner
             ) VALUES ('worker', 'leader', $1, $2, 'user', 'hi', 'standard', 'agent',
                'claude', $3, 'worker')",
        )
        .bind(channel)
        .bind(key)
        .bind(status)
        .execute(pool)
        .await
        .expect("seed intake row"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
    }

    async fn session(pool: &PgPool, key: &str, channel: &str, instance: &str, status: &str) {
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, channel_id, instance_id)
             VALUES ($1, 'claude', $2, $3, $4)",
        )
        .bind(key)
        .bind(status)
        .bind(channel)
        .bind(instance)
        .execute(pool)
        .await
        .expect("seed session row"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
    }

    async fn rows(pool: &PgPool, channel: &str) -> ActivationRows {
        activation_rows(pool, channel, "gateway")
            .await
            .expect("activation rows") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
    }

    #[tokio::test]
    async fn activation_rows_count_only_open_intake_and_foreign_live_sessions_of_the_channel_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        intake(&pool, "7", "done", "done").await;
        intake(&pool, "8", "other", "pending").await;
        session(&pool, "local", "7", "gateway", "idle").await;
        session(&pool, "gone", "7", "runner", "disconnected").await;
        session(&pool, "blank", "7", " ", "idle").await;
        session(&pool, "elsewhere", "8", "runner", "turn_active").await;
        assert_eq!(rows(&pool, "7").await, ActivationRows::default());

        intake(&pool, "7", "open", "spawned").await;
        session(&pool, "runner", "7", "runner", "idle").await;
        let expected = ActivationRows {
            open_intake: 1,
            foreign_sessions: 1,
        };
        assert_eq!(rows(&pool, "7").await, expected);
        pool.close().await;
        pg_db.drop().await;
    }
}
