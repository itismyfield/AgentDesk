use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::retry_prompt_codec::RetryPromptContext;

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
