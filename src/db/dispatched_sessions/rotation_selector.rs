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
