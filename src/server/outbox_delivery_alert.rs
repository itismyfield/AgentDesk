//! #4260 silent-loss vector 3: operator alerting for terminal outbox delivery
//! failures, split out of the giant `server` root. A message that exhausts its
//! outbox retries flips to `status='failed'` (its own natural dead-letter,
//! migration 0001) but was previously only warned for a subset of sources.
//! Here EVERY terminal failure surfaces a structured warn, an
//! `outbox_delivery_failed` quality event, and one per-incident operator card
//! via the shared `kanban_human_alert_channel_id` convention. The destination
//! channel is never notified — it may itself be the failing target.

use sqlx::PgPool;

use super::PendingMessageOutboxRow;

/// Source tag for the ops card enqueued on a terminal outbox failure. The card
/// is itself an outbox row; if IT fails terminally we must NOT enqueue another
/// card for it, or a failing alert channel would loop forever.
pub(super) const OUTBOX_DELIVERY_ALERT_SOURCE: &str = "outbox_delivery_alert";

/// Truncate outbox content to a compact snippet for the ops card / quality
/// event payload — never echo a full (possibly large) failed message.
pub(super) fn outbox_alert_snippet(content: &str) -> String {
    const MAX: usize = 120;
    let trimmed = content.trim();
    let mut snippet: String = trimmed.chars().take(MAX).collect();
    if trimmed.chars().count() > MAX {
        snippet.push('…');
    }
    if snippet.is_empty() {
        snippet.push_str("(빈 내용)");
    }
    snippet
}

/// Resolve the operator alert target from the shared `kanban_human_alert_channel_id`
/// convention (same key the relay-signal alert pipeline uses).
/// `None` ⇒ unconfigured deploy ⇒ no ops card enqueued (guaranteed silent).
pub(super) async fn outbox_alert_target_pg(pg_pool: &PgPool) -> Option<String> {
    let value = sqlx::query_scalar::<_, String>(
        "SELECT value FROM kv_meta
         WHERE key = 'kanban_human_alert_channel_id'
           AND value IS NOT NULL AND btrim(value) <> ''
         LIMIT 1",
    )
    .fetch_optional(pg_pool)
    .await
    .ok()
    .flatten()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(if trimmed.starts_with("channel:") {
        trimmed.to_string()
    } else {
        format!("channel:{trimmed}")
    })
}

/// A message hit its terminal (non-retryable) outbox failure. Emits a structured
/// warn (relay-standard `channel_id` / `session_key` keys) and an
/// `outbox_delivery_failed` quality event (0012 enum) inline (both non-DB), then
/// spawns a detached task for the DB work — resolving the
/// `kanban_human_alert_channel_id` target and enqueueing one per-incident
/// operator card — so the outbox drain loop never awaits a pool acquire on the
/// alert path (#4260 dual r1, codex#1). Never propagates; enqueue failure only
/// logs. Delivery uses the #4449 announce-first/notify-fallback worker policy.
/// Returns the join handle of the spawned card task (`None` when the
/// recursion guard suppressed it) so tests can await deterministically;
/// production callers drop it.
pub(super) fn note_terminal_outbox_delivery_failure(
    pg_pool: &PgPool,
    row: &PendingMessageOutboxRow,
    error_text: &str,
) -> Option<tokio::task::JoinHandle<()>> {
    // Recursion guard: the ops card is itself an outbox row; if IT fails
    // terminally we still record the warn + quality event, but never enqueue a
    // card-for-a-card.
    let is_alert_source = row.source == OUTBOX_DELIVERY_ALERT_SOURCE;
    let channel_id = row.target.strip_prefix("channel:").map(str::to_string);
    let content_snippet = outbox_alert_snippet(&row.content);

    tracing::warn!(
        outbox_id = row.id,
        source = %row.source,
        target = %row.target,
        channel_id = channel_id.as_deref(),
        session_key = row.session_key.as_deref(),
        "[outbox] ❌ terminal delivery failure (silent-loss vector 3): {error_text}"
    );

    crate::services::observability::emit_agent_quality_event(
        crate::services::observability::AgentQualityEvent {
            source_event_id: Some(row.id.to_string()),
            correlation_id: row.session_key.clone(),
            agent_id: None,
            provider: None,
            channel_id: channel_id.clone(),
            card_id: None,
            dispatch_id: None,
            event_type: "outbox_delivery_failed".to_string(),
            payload: serde_json::json!({
                "outbox_id": row.id,
                "source": row.source,
                "target": row.target,
                "reason": error_text,
                "session_key": row.session_key,
                "content_snippet": content_snippet,
            }),
        },
    );

    if is_alert_source {
        return None;
    }

    let pool = pg_pool.clone();
    let outbox_id = row.id;
    let card = format!(
        "🚨 outbox 전송 최종 실패 (복구 필요)\n• row: {}\n• source: `{}`\n• target: `{}`\n• reason: {}\n• content: {}",
        row.id, row.source, row.target, error_text, content_snippet,
    );
    Some(tokio::spawn(async move {
        // Unconfigured deploy (no alert channel) ⇒ guaranteed silent.
        let Some(target) = outbox_alert_target_pg(&pool).await else {
            return;
        };
        // Per-incident dedupe: keyed on the unique row id, so each terminal
        // failure yields exactly one card (a row can only fail terminally once).
        let session_key = format!("outbox_delivery_failed:{outbox_id}");
        if let Err(error) = crate::services::message_outbox::enqueue_outbox_pg(
            &pool,
            crate::services::message_outbox::OutboxMessage {
                target: &target,
                content: &card,
                bot: crate::services::message_outbox::ACTIONABLE_OPS_ALERT_BOT,
                source: OUTBOX_DELIVERY_ALERT_SOURCE,
                reason_code: Some("outbox_delivery_failed"),
                session_key: Some(&session_key),
            },
        )
        .await
        {
            tracing::warn!(
                outbox_id,
                "[outbox] failed to enqueue ops alert for terminal delivery failure: {error}"
            );
        }
    }))
}
