use crate::services::provider::ProviderKind;

use super::RetryPromptContext;

pub(in crate::services::discord) fn key(
    kind: &str,
    provider: &ProviderKind,
    channel: u64,
) -> String {
    if kind == "session_retry_context" {
        format!("{kind}:{channel}")
    } else {
        format!("{kind}:{}:{channel}", provider.as_str())
    }
}

pub(in crate::services::discord) fn peek(pool: Option<&sqlx::PgPool>, key: &str) -> Option<String> {
    let Some(pool) = pool else {
        return crate::services::discord::internal_api::get_kv_value(key)
            .ok()
            .flatten();
    };
    let key = key.to_owned();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |pool| async move {
            sqlx::query_scalar::<_, String>(
                "SELECT value FROM kv_meta WHERE key = $1
                 AND (expires_at IS NULL OR expires_at > NOW())",
            )
            .bind(key)
            .fetch_optional(&pool)
            .await
            .map_err(|error| error.to_string())
        },
        |error| error,
    )
    .ok()
    .flatten()
}

pub(in crate::services::discord) async fn consume_confirmed(
    pool: Option<&sqlx::PgPool>,
    provider: &ProviderKind,
    channel: u64,
    turn_id: &str,
    context: &RetryPromptContext,
) -> Result<(), String> {
    for (kind, value) in [
        ("session_retry_context", &context.selected.recovery),
        ("voluntary_feedback_reminder", &context.selected.feedback),
        ("turn_end_wip_warning", &context.selected.wip),
    ] {
        let Some(value) = value else { continue };
        let pool = pool.ok_or("prompt prelude settlement requires PostgreSQL")?;
        // Delete only the borrowed value: a newer channel stash belongs to the next input.
        let removed = sqlx::query("DELETE FROM kv_meta WHERE key = $1 AND value = $2")
            .bind(key(kind, provider, channel))
            .bind(value)
            .execute(pool)
            .await
            .map_err(|error| error.to_string())?
            .rows_affected();
        if removed > 0 && kind == "session_retry_context" {
            crate::services::observability::recovery_audit::mark_recovery_audit_consumed(
                pool,
                &channel.to_string(),
                turn_id,
            )
            .await
            .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}
