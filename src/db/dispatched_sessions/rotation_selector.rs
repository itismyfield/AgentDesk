use sqlx::PgPool;

/// Moves the session row's provider selector to `new` only while it still names one of `left`,
/// so a cleared row or another chosen selector is not overwritten. Returns whether it moved.
pub(crate) async fn move_provider_selector_pg(
    pool: &PgPool,
    session_row_id: i64,
    left: &[String],
    new: &str,
) -> Result<bool, String> {
    sqlx::query(
        "UPDATE sessions
         SET claude_session_id = $2,
             raw_provider_session_id = $2,
             claude_session_id_recorded_at = NOW()
         WHERE id = $1
           AND claude_session_id = ANY($3::TEXT[])",
    )
    .bind(session_row_id)
    .bind(new)
    .bind(left)
    .execute(pool)
    .await
    .map(|result| result.rows_affected() > 0)
    .map_err(|error| format!("move session #{session_row_id} provider selector: {error}"))
}

/// Stores `session`, whose run began at `started`, as the provider selector of `session_key`'s row
/// unless the channel was cleared since then. Returns whether a row took it.
pub(crate) async fn store_provider_selector_pg(
    pool: &PgPool,
    session_key: &str,
    session: &str,
    channel_id: &str,
    started: chrono::DateTime<chrono::Utc>,
) -> Result<bool, String> {
    let resolve = crate::db::dispatched_session_canonical_identity::resolve_session_key_pg;
    let Some(key) = resolve(pool, session_key)
        .await
        .map_err(|error| format!("resolve provider selector locator: {error:?}"))?
    else {
        return Ok(false);
    };
    sqlx::query(
        "UPDATE sessions
         SET claude_session_id = $2,
             raw_provider_session_id = $2,
             claude_session_id_recorded_at = CASE
               WHEN claude_session_id IS DISTINCT FROM $2 THEN NOW()
               ELSE COALESCE(claude_session_id_recorded_at, NOW())
             END
         WHERE session_key = $1
           AND NOT EXISTS (SELECT 1 FROM channel_session_clear_boundaries
                           WHERE channel_id = $3 AND cleared_at >= $4)",
    )
    .bind(key)
    .bind(session)
    .bind(channel_id)
    .bind(started)
    .execute(pool)
    .await
    .map(|result| result.rows_affected() > 0)
    .map_err(|error| format!("store session {session_key} provider selector: {error}"))
}
