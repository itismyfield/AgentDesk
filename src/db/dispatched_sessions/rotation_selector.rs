use sqlx::PgPool;

/// Moves a session row's provider selector to `new` only while it still names one of `left`,
/// so a cleared row or a newer selector is not overwritten. Returns whether a row moved.
pub(crate) async fn move_provider_selector_pg(
    pool: &PgPool,
    session_key: &str,
    left: &[String],
    new: &str,
) -> Result<bool, String> {
    let Some(session_key) =
        super::resolve_session_locator_pg(pool, session_key, "provider selector move").await?
    else {
        return Ok(false);
    };
    sqlx::query(
        "UPDATE sessions
         SET claude_session_id = $2,
             raw_provider_session_id = $2,
             claude_session_id_recorded_at = NOW()
         WHERE session_key = $1
           AND claude_session_id = ANY($3::TEXT[])",
    )
    .bind(&session_key)
    .bind(new)
    .bind(left)
    .execute(pool)
    .await
    .map(|result| result.rows_affected() > 0)
    .map_err(|error| format!("move session {session_key} provider selector: {error}"))
}
