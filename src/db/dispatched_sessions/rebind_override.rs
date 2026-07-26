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
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin rebind session override: {error}"))?;
    sqlx::query("SELECT agentdesk_lock_session_locator($1)")
        .bind(session_key)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("lock rebind session override locator: {error}"))?;

    let targets = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT session_key, provider
         FROM (
             SELECT s.session_key, s.provider, 1 AS source_rank
             FROM sessions s
             WHERE s.session_key = $1
             UNION ALL
             SELECT s.session_key, s.provider, 2 AS source_rank
             FROM session_key_aliases a
             JOIN sessions s ON s.id = a.session_id
             WHERE a.session_key = $1
         ) evidence
         ORDER BY source_rank",
    )
    .bind(session_key)
    .fetch_all(&mut *tx)
    .await
    .map_err(|error| format!("resolve rebind session override: {error}"))?;
    if targets.len() > 1 {
        return Err("rebind session override locator has divergent evidence".to_string());
    }

    if let Some((resolved_session_key, owner_provider)) = targets.into_iter().next() {
        if owner_provider
            .as_deref()
            .is_some_and(|owner| owner != provider)
        {
            return Err("rebind session override provider ownership mismatch".to_string());
        }
        sqlx::query(
            "UPDATE sessions
             SET claude_session_id = COALESCE($2, claude_session_id),
                 claude_session_id_recorded_at = CASE
                   WHEN $2 IS NULL THEN claude_session_id_recorded_at
                   WHEN claude_session_id IS DISTINCT FROM $2 THEN NOW()
                   ELSE COALESCE(claude_session_id_recorded_at, NOW())
                 END,
                 raw_provider_session_id = COALESCE($3, raw_provider_session_id),
                 last_heartbeat = NOW()
             WHERE session_key = $1",
        )
        .bind(resolved_session_key)
        .bind(claude_session_id)
        .bind(session_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("update rebind session override: {error}"))?;
    } else {
        sqlx::query(
            "INSERT INTO sessions (
                session_key, provider, status, claude_session_id,
                raw_provider_session_id, claude_session_id_recorded_at, last_heartbeat
             ) VALUES (
                $1, $2, 'idle', $3, $4,
                CASE WHEN $3 IS NOT NULL THEN NOW() ELSE NULL END, NOW()
             )",
        )
        .bind(session_key)
        .bind(provider)
        .bind(claude_session_id)
        .bind(session_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("insert rebind session override: {error}"))?;
    }

    tx.commit()
        .await
        .map_err(|error| format!("commit rebind session override: {error}"))
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
