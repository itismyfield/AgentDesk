use sqlx::PgPool;

/// Moves the session row's provider selector to `new` only while it still names `old`, so a
/// cleared row or a newer selector is not overwritten. Returns whether the row moved.
pub(crate) async fn move_provider_selector_pg(
    pool: &PgPool,
    session_row_id: i64,
    old: &str,
    new: &str,
) -> Result<bool, String> {
    sqlx::query(
        "UPDATE sessions
         SET claude_session_id = $2,
             raw_provider_session_id = $2,
             claude_session_id_recorded_at = NOW()
         WHERE id = $1
           AND claude_session_id = $3",
    )
    .bind(session_row_id)
    .bind(new)
    .bind(old)
    .execute(pool)
    .await
    .map(|result| result.rows_affected() > 0)
    .map_err(|error| format!("move session #{session_row_id} provider selector: {error}"))
}
