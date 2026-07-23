use anyhow::{Result, anyhow};
use sqlx::PgPool;
use std::time::Duration;

/// A live turn refreshes its heartbeat roughly once per minute. Five minutes
/// leaves enough margin for transient database or scheduler delays while still
/// bounding how long a stale busy state can block mailbox injection.
pub(crate) const STALE_TURN_GRACE: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionReconcileOutcome {
    Reconciled,
    Unchanged,
    NotFound,
}

/// Reconcile every provably stale busy session.
///
/// The update is intentionally guarded by all three pieces of negative liveness
/// evidence in one SQL statement: a busy status, no dispatch id, and a stale
/// heartbeat. A live turn with either a dispatch id or a fresh heartbeat cannot
/// match, including if either changes concurrently before the row is locked.
pub(crate) async fn reconcile_stale_turns_pg(pool: &PgPool) -> Result<usize> {
    reconcile_stale_turns_matching_pg(pool, None).await
}

/// Reconcile one session for the operator API without weakening the same atomic
/// liveness guard used by startup and periodic sweeps.
pub(crate) async fn reconcile_stale_turn_by_key_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<SessionReconcileOutcome> {
    let reconciled = reconcile_stale_turns_matching_pg(pool, Some(session_key)).await?;
    if reconciled > 0 {
        return Ok(SessionReconcileOutcome::Reconciled);
    }

    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM sessions WHERE session_key = $1)",
    )
    .bind(session_key)
    .fetch_one(pool)
    .await
    .map_err(|error| anyhow!("check stale-turn reconcile session existence: {error}"))?;

    Ok(if exists {
        SessionReconcileOutcome::Unchanged
    } else {
        SessionReconcileOutcome::NotFound
    })
}

async fn reconcile_stale_turns_matching_pg(
    pool: &PgPool,
    session_key: Option<&str>,
) -> Result<usize> {
    let result = sqlx::query(
        "UPDATE sessions
            SET session_info = 'reconciled stale ' || status ||
                               ' (no dispatch, stale heartbeat)',
                status = 'idle'
          WHERE status IN ('turn_active', 'working')
            AND COALESCE(BTRIM(active_dispatch_id), '') = ''
            AND last_heartbeat < NOW() - ($1::BIGINT * INTERVAL '1 second')
            AND ($2::TEXT IS NULL OR session_key = $2)",
    )
    .bind(STALE_TURN_GRACE.as_secs() as i64)
    .bind(session_key)
    .execute(pool)
    .await
    .map_err(|error| anyhow!("reconcile stale busy sessions: {error}"))?;

    let reconciled = result.rows_affected() as usize;
    if reconciled > 0 {
        tracing::warn!(
            target: "reconcile",
            reconciled,
            session_key = session_key.unwrap_or("*"),
            grace_seconds = STALE_TURN_GRACE.as_secs(),
            "reconciled stale busy sessions with no dispatch and stale heartbeat"
        );
    }
    Ok(reconciled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;

    async fn allow_legacy_working_status(pool: &PgPool) {
        // The deployed release can contain the legacy `working` value even
        // though a fresh test schema's status check no longer accepts it.
        sqlx::query("ALTER TABLE sessions DROP CONSTRAINT sessions_status_known_check")
            .execute(pool)
            .await
            .unwrap();
    }

    async fn seed_session(
        pool: &PgPool,
        session_key: &str,
        status: &str,
        active_dispatch_id: Option<&str>,
        heartbeat_age_seconds: i64,
    ) {
        sqlx::query(
            "INSERT INTO sessions (
                session_key, status, active_dispatch_id, last_heartbeat, session_info
             ) VALUES (
                $1, $2, $3,
                NOW() - ($4::BIGINT * INTERVAL '1 second'), 'original'
             )",
        )
        .bind(session_key)
        .bind(status)
        .bind(active_dispatch_id)
        .bind(heartbeat_age_seconds)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn load_state(pool: &PgPool, session_key: &str) -> (String, Option<String>) {
        let row = sqlx::query("SELECT status, session_info FROM sessions WHERE session_key = $1")
            .bind(session_key)
            .fetch_one(pool)
            .await
            .unwrap();
        (
            row.try_get("status").unwrap(),
            row.try_get("session_info").unwrap(),
        )
    }

    #[tokio::test]
    async fn stale_busy_sessions_are_reconciled_but_live_turns_are_unchanged_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        allow_legacy_working_status(&pool).await;
        let stale_age = STALE_TURN_GRACE.as_secs() as i64 + 60;

        seed_session(&pool, "stale-turn", "turn_active", None, stale_age).await;
        seed_session(&pool, "stale-working", "working", Some("  "), stale_age).await;
        seed_session(
            &pool,
            "live-dispatch",
            "turn_active",
            Some("dispatch-live"),
            stale_age,
        )
        .await;
        seed_session(&pool, "live-heartbeat", "turn_active", None, 30).await;

        assert_eq!(reconcile_stale_turns_pg(&pool).await.unwrap(), 2);
        assert_eq!(
            load_state(&pool, "stale-turn").await,
            (
                "idle".to_string(),
                Some("reconciled stale turn_active (no dispatch, stale heartbeat)".to_string())
            )
        );
        assert_eq!(
            load_state(&pool, "stale-working").await,
            (
                "idle".to_string(),
                Some("reconciled stale working (no dispatch, stale heartbeat)".to_string())
            )
        );
        assert_eq!(
            load_state(&pool, "live-dispatch").await,
            ("turn_active".to_string(), Some("original".to_string()))
        );
        assert_eq!(
            load_state(&pool, "live-heartbeat").await,
            ("turn_active".to_string(), Some("original".to_string()))
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn keyed_reconcile_uses_the_same_guard_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        allow_legacy_working_status(&pool).await;
        let stale_age = STALE_TURN_GRACE.as_secs() as i64 + 60;

        seed_session(&pool, "keyed-stale", "turn_active", None, stale_age).await;
        seed_session(&pool, "keyed-live", "working", None, 30).await;

        assert_eq!(
            reconcile_stale_turn_by_key_pg(&pool, "keyed-stale")
                .await
                .unwrap(),
            SessionReconcileOutcome::Reconciled
        );
        assert_eq!(
            reconcile_stale_turn_by_key_pg(&pool, "keyed-live")
                .await
                .unwrap(),
            SessionReconcileOutcome::Unchanged
        );
        assert_eq!(
            reconcile_stale_turn_by_key_pg(&pool, "missing")
                .await
                .unwrap(),
            SessionReconcileOutcome::NotFound
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
