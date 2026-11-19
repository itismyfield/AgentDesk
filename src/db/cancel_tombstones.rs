//! PG mirror of the in-memory cancel tombstones (`RECENT_TURN_STOPS` in
//! `services/discord/tmux_kill_policy.rs`), so a dcserver restart between a cancel and
//! the watcher's death observation still suppresses the misleading 🔴 lifecycle notice.
//!
//! Rows are deleted by `client_id` after an in-memory hit, consumed one-shot after a
//! restart, and swept by the `storage.cancel_tombstone_prune` maintenance job.

use std::sync::OnceLock;

use sqlx::{PgPool, Row};
use uuid::Uuid;

/// Runtime pool, set once at boot by `crate::server::run`, for callers with no
/// `PgPool` in scope (e.g. `turn_lifecycle::stop_turn_with_policy`).
static GLOBAL_PG_POOL: OnceLock<PgPool> = OnceLock::new();

pub fn set_global_pool(pool: PgPool) {
    let _ = GLOBAL_PG_POOL.set(pool);
}

pub fn global_pool() -> Option<&'static PgPool> {
    GLOBAL_PG_POOL.get()
}

/// Row lifetime; mirrors the in-memory `RECENT_TURN_STOP_TTL`.
pub const CANCEL_TOMBSTONE_TTL_SECS: i64 = 10 * 60;

/// A watcher death only matches tombstones recorded within this window (enforced in
/// SQL); mirrors the in-memory `RECENT_TURN_STOP_METADATA_FALLBACK_TTL`.
pub const CANCEL_TOMBSTONE_FALLBACK_TTL_SECS: i64 = 60;

/// Mirrors the in-memory `CANCEL_TEARDOWN_GRACE_BYTES`: output past the cancel offset
/// plus this teardown slack belongs to a follow-up turn, not to the cancel.
pub const CANCEL_TEARDOWN_GRACE_BYTES: i64 = 4 * 1024;

/// Insert a tombstone. `client_id` is the in-memory entry's UUID, so an in-process
/// hit can later delete exactly this row.
pub async fn insert_cancel_tombstone(
    pool: &PgPool,
    client_id: Uuid,
    channel_id: i64,
    tmux_session_name: Option<&str>,
    stop_output_offset: Option<i64>,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO cancel_tombstones (
            client_id, channel_id, tmux_session_name, stop_output_offset, reason,
            recorded_at, expires_at
         ) VALUES ($1, $2, $3, $4, $5, NOW(), NOW() + ($6::bigint || ' seconds')::interval)
         ON CONFLICT (client_id) DO NOTHING",
    )
    .bind(client_id)
    .bind(channel_id)
    .bind(tmux_session_name)
    .bind(stop_output_offset)
    .bind(reason)
    .bind(CANCEL_TOMBSTONE_TTL_SECS)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Delete the rows mirroring in-memory hits. Callers wait for the matching insert
/// first, so a zero-row delete means another consumer already removed the row.
pub async fn delete_cancel_tombstones_by_client_ids(
    pool: &PgPool,
    client_ids: &[Uuid],
) -> Result<u64, sqlx::Error> {
    if client_ids.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query("DELETE FROM cancel_tombstones WHERE client_id = ANY($1)")
        .bind(client_ids)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Find and delete matching tombstones in one transaction, so suppression stays
/// one-shot per cancel. `true` means the watcher death was cancel-induced.
///
/// Matches like the in-memory `cancel_induced_watcher_death`, minus its generation check:
/// same channel, session equal or NULL, recorded within the fallback window, and (when
/// both offsets are known) `current <= stop + CANCEL_TEARDOWN_GRACE_BYTES`.
pub async fn consume_cancel_tombstone(
    pool: &PgPool,
    channel_id: i64,
    tmux_session_name: &str,
    current_output_offset: Option<i64>,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Row locks keep a concurrent consume (any worker or dcserver) from double-suppressing.
    let rows = sqlx::query(
        "SELECT id, tmux_session_name, stop_output_offset
         FROM cancel_tombstones
         WHERE channel_id = $1
           AND recorded_at >= NOW() - ($2::bigint || ' seconds')::interval
           AND (tmux_session_name IS NULL OR tmux_session_name = $3)
         ORDER BY recorded_at DESC
         FOR UPDATE",
    )
    .bind(channel_id)
    .bind(CANCEL_TOMBSTONE_FALLBACK_TTL_SECS)
    .bind(tmux_session_name)
    .fetch_all(&mut *tx)
    .await?;

    let mut suppress_ids: Vec<i64> = Vec::new();
    for row in rows {
        let id: i64 = row.try_get("id")?;
        let stop_offset: Option<i64> = row.try_get("stop_output_offset")?;

        if let (Some(stop), Some(current)) = (stop_offset, current_output_offset) {
            if current > stop.saturating_add(CANCEL_TEARDOWN_GRACE_BYTES) {
                // Follow-up turn output already past the cancel boundary.
                continue;
            }
        }
        suppress_ids.push(id);
    }

    if suppress_ids.is_empty() {
        tx.rollback().await?;
        return Ok(false);
    }

    sqlx::query("DELETE FROM cancel_tombstones WHERE id = ANY($1)")
        .bind(&suppress_ids)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(true)
}

/// Sweep expired rows left behind when no watcher ever observes the death.
pub async fn prune_expired_cancel_tombstones(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM cancel_tombstones WHERE expires_at < NOW()")
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
pub(super) mod tests {
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use sqlx::{AssertSqlSafe, Connection, PgConnection};

    use super::*;

    const MIGRATION: &str = include_str!("../../migrations/postgres/0024_cancel_tombstones.sql");

    pub(crate) struct StorePgFixture {
        pub(crate) pool: PgPool,
        admin: PgConnection,
        schema: String,
    }

    impl StorePgFixture {
        pub(crate) async fn connect_test_pool(migration: &'static str) -> Self {
            let base = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE")
                .expect("store PG tests require an explicit fixture URL");
            let url = url::Url::parse(&base).expect("parse store test PostgreSQL URL");
            assert!(
                url.host_str().is_some() && url.port().is_some(),
                "store test PostgreSQL URL must specify a host and port"
            );
            let database =
                std::env::var("POSTGRES_TEST_ADMIN_DB").unwrap_or_else(|_| "postgres".into());
            let options = base
                .parse::<PgConnectOptions>()
                .expect("parse store test PostgreSQL options")
                .database(&database);
            let mut admin = PgConnection::connect_with(&options)
                .await
                .expect("connect to store test PostgreSQL");
            // The schema identifier contains only the fixed prefix and UUID hex.
            let schema = format!("adk_store_test_{}", Uuid::new_v4().simple());
            sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
                .execute(&mut admin)
                .await
                .expect("create isolated store test schema");
            let pool = PgPoolOptions::new()
                .max_connections(4)
                .connect_with(
                    options.options([("search_path", schema.as_str()), ("lock_timeout", "2s")]),
                )
                .await
                .expect("connect pool to isolated store test schema");
            sqlx::raw_sql(migration)
                .execute(&pool)
                .await
                .expect("apply store repository migration");
            Self {
                pool,
                admin,
                schema,
            }
        }

        pub(crate) async fn close(mut self) {
            self.pool.close().await;
            sqlx::query(AssertSqlSafe(format!(
                "DROP SCHEMA {} CASCADE",
                self.schema
            )))
            .execute(&mut self.admin)
            .await
            .expect("drop isolated store test schema");
        }
    }

    #[tokio::test]
    async fn concurrent_consumers_suppress_a_cancel_only_once_pg() {
        let fixture = StorePgFixture::connect_test_pool(MIGRATION).await;
        let pool = &fixture.pool;
        insert_cancel_tombstone(pool, Uuid::new_v4(), 42, Some("pane"), Some(100), "cancel")
            .await
            .unwrap();

        let (first, second) = tokio::join!(
            consume_cancel_tombstone(pool, 42, "pane", Some(100)),
            consume_cancel_tombstone(pool, 42, "pane", Some(100))
        );
        assert_ne!(first.unwrap(), second.unwrap());
        assert!(
            !consume_cancel_tombstone(pool, 42, "pane", None)
                .await
                .unwrap()
        );
        fixture.close().await;
    }

    #[tokio::test]
    async fn consume_respects_session_and_inclusive_output_boundary_pg() {
        let fixture = StorePgFixture::connect_test_pool(MIGRATION).await;
        let pool = &fixture.pool;
        insert_cancel_tombstone(pool, Uuid::new_v4(), 42, Some("pane"), Some(100), "cancel")
            .await
            .unwrap();

        let boundary = 100 + CANCEL_TEARDOWN_GRACE_BYTES;
        assert!(
            !consume_cancel_tombstone(pool, 42, "other", None)
                .await
                .unwrap()
        );
        assert!(
            !consume_cancel_tombstone(pool, 42, "pane", Some(boundary + 1))
                .await
                .unwrap()
        );
        assert!(
            consume_cancel_tombstone(pool, 42, "pane", Some(boundary))
                .await
                .unwrap()
        );
        insert_cancel_tombstone(pool, Uuid::new_v4(), 42, None, None, "cancel")
            .await
            .unwrap();
        assert!(
            consume_cancel_tombstone(pool, 42, "new-pane", Some(boundary + 1))
                .await
                .unwrap()
        );
        fixture.close().await;
    }

    #[tokio::test]
    async fn old_rows_are_not_consumed_and_expiry_prunes_only_expired_rows_pg() {
        let fixture = StorePgFixture::connect_test_pool(MIGRATION).await;
        let pool = &fixture.pool;
        let old = Uuid::new_v4();
        let fresh = Uuid::new_v4();
        for (id, channel) in [(old, 41), (fresh, 42)] {
            insert_cancel_tombstone(pool, id, channel, None, None, "cancel")
                .await
                .unwrap();
        }
        sqlx::query(
            "UPDATE cancel_tombstones SET recorded_at = NOW() - interval '2 minutes',
                 expires_at = NOW() - interval '1 minute' WHERE client_id = $1",
        )
        .bind(old)
        .execute(pool)
        .await
        .unwrap();

        assert!(
            !consume_cancel_tombstone(pool, 41, "pane", None)
                .await
                .unwrap()
        );
        assert_eq!(prune_expired_cancel_tombstones(pool).await.unwrap(), 1);
        assert_eq!(
            delete_cancel_tombstones_by_client_ids(pool, &[fresh])
                .await
                .unwrap(),
            1
        );
        assert!(
            !consume_cancel_tombstone(pool, 42, "pane", None)
                .await
                .unwrap()
        );
        fixture.close().await;
    }
}
