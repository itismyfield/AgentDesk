use sqlx::PgPool;

/// Persist an operator-supplied provider session selector without disturbing
/// the rest of the live session row. The `recorded_at` CASE is intentionally
/// identical to `upsert_hook_session_pg`: repeated observations of the same
/// selector do not extend the missing-transcript grace window.
pub(crate) async fn upsert_rebind_session_override_pg(
    pool: &PgPool,
    session_key: &str,
    provider: &str,
    session_id: &str,
) -> Result<(), String> {
    let claude_session_id = (provider == "claude").then_some(session_id);
    sqlx::query(
        "INSERT INTO sessions (
            session_key, provider, status, claude_session_id,
            raw_provider_session_id, claude_session_id_recorded_at, last_heartbeat
         ) VALUES (
            $1, $2, 'idle', $3, $4,
            CASE WHEN $3 IS NOT NULL THEN NOW() ELSE NULL END, NOW()
         )
         ON CONFLICT(session_key) DO UPDATE SET
            claude_session_id = COALESCE(EXCLUDED.claude_session_id, sessions.claude_session_id),
            claude_session_id_recorded_at = CASE
              WHEN EXCLUDED.claude_session_id IS NULL THEN sessions.claude_session_id_recorded_at
              WHEN sessions.claude_session_id IS DISTINCT FROM EXCLUDED.claude_session_id THEN NOW()
              ELSE COALESCE(sessions.claude_session_id_recorded_at, NOW())
            END,
            raw_provider_session_id = COALESCE(EXCLUDED.raw_provider_session_id, sessions.raw_provider_session_id),
            last_heartbeat = NOW()",
    )
    .bind(session_key)
    .bind(provider)
    .bind(claude_session_id)
    .bind(session_id)
    .execute(pool)
    .await
    .map(|_| ())
    .map_err(|error| format!("upsert rebind session override {session_key}: {error}"))
}

#[cfg(test)]
mod tests {
    use sqlx::Row;

    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;

    #[tokio::test]
    async fn health_rebind_override_upserts_selectors_with_recorded_at_guard_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "claude/test/host:AgentDesk-claude-rebind-override";
        let first_id = "4c474e5d-37e7-4b6a-bcf7-d68854a31c49";
        let second_id = "2d941d6e-a582-4a2d-8fc4-f61b876f2bf2";

        upsert_rebind_session_override_pg(&pool, session_key, "claude", first_id)
            .await
            .expect("insert override selector");
        sqlx::query(
            "UPDATE sessions
                SET claude_session_id_recorded_at = NOW() - INTERVAL '61 seconds'
              WHERE session_key = $1",
        )
        .bind(session_key)
        .execute(&pool)
        .await
        .expect("age recorded-at guard");
        upsert_rebind_session_override_pg(&pool, session_key, "claude", first_id)
            .await
            .expect("repeat same selector");
        let same_age: i64 = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM (NOW() - claude_session_id_recorded_at))::BIGINT
               FROM sessions WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(&pool)
        .await
        .expect("same selector age");
        assert!(same_age >= 60, "same selector must preserve recorded_at");

        upsert_rebind_session_override_pg(&pool, session_key, "claude", second_id)
            .await
            .expect("replace selector");
        let row = sqlx::query(
            "SELECT claude_session_id, raw_provider_session_id,
                    EXTRACT(EPOCH FROM (NOW() - claude_session_id_recorded_at))::BIGINT AS age
               FROM sessions WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(&pool)
        .await
        .expect("load replaced selector");
        assert_eq!(row.get::<String, _>("claude_session_id"), second_id);
        assert_eq!(row.get::<String, _>("raw_provider_session_id"), second_id);
        assert!(row.get::<i64, _>("age") < 60);

        pool.close().await;
        pg_db.drop().await;
    }
}
