use sqlx::PgPool;

/// Rebind only while the row is still idle, still owned by the executing node,
/// and, when supplied, the target provider session id is not bound to any other
/// session row. This closes owner/binding races between fresh inventory discovery
/// and the durable update without changing explicit raw-id compatibility.
pub(crate) async fn rebind_session_provider_with_guard_pg(
    pool: &PgPool,
    session_key: &str,
    target_cwd: &str,
    target_session_id: &str,
    require_target_unbound: Option<&str>,
    expected_owner_instance_id: &str,
) -> Result<u64, String> {
    sqlx::query(
        "UPDATE sessions AS target
         SET cwd = $2,
             claude_session_id = $3,
             raw_provider_session_id = $3,
             claude_session_id_recorded_at = CASE
               WHEN target.claude_session_id IS DISTINCT FROM $3 THEN NOW()
               ELSE COALESCE(target.claude_session_id_recorded_at, NOW())
             END
         WHERE target.session_key = $1
           AND target.active_dispatch_id IS NULL
           AND target.instance_id = $5
           AND (
             $4::TEXT IS NULL
             OR NOT EXISTS (
               SELECT 1
               FROM sessions AS bound
               WHERE bound.session_key <> target.session_key
                 AND (
                   bound.claude_session_id = $4
                   OR bound.raw_provider_session_id = $4
                 )
             )
           )",
    )
    .bind(session_key)
    .bind(target_cwd)
    .bind(target_session_id)
    .bind(require_target_unbound)
    .bind(expected_owner_instance_id)
    .execute(pool)
    .await
    .map(|result| result.rows_affected())
    .map_err(|error| format!("rebind session {session_key} provider binding: {error}"))
}
