//! #4260 silent-loss vector 3: operator alerting for terminal outbox delivery
//! failures, split out of the giant `server` root. A message that exhausts its
//! outbox retries flips to `status='failed'` (its own natural dead-letter,
//! migration 0001) but was previously only warned for a subset of sources.
//! Here EVERY terminal failure surfaces a structured warn and an
//! `outbox_delivery_failed` quality event. #5993 retired the operator card
//! that also went to the shared human-alert channel; the destination channel is
//! never notified — it may itself be the failing target.

use super::PendingMessageOutboxRow;

/// Truncate outbox content to a compact snippet for the quality event payload —
/// never echo a full (possibly large) failed message.
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

/// A message hit its terminal (non-retryable) outbox failure. Emits a structured
/// warn (relay-standard `channel_id` / `session_key` keys) and an
/// `outbox_delivery_failed` quality event (0012 enum). Both are non-DB, so the
/// outbox drain loop never awaits anything on this path (#4260 dual r1,
/// codex#1). Never propagates.
pub(super) fn note_terminal_outbox_delivery_failure(
    row: &PendingMessageOutboxRow,
    error_text: &str,
) {
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
            channel_id,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #4260 vector 3 / #5993: a terminal outbox failure is recorded as an
    /// `outbox_delivery_failed` quality event on every source; the path takes no
    /// pool, so it cannot enqueue an operator card.
    #[test]
    fn terminal_outbox_failure_records_quality_event_without_ops_card() {
        let _runtime = crate::services::observability::test_runtime_lock();
        let row = |id: i64, source: &str| PendingMessageOutboxRow {
            id,
            target: "channel:123".to_string(),
            content: "undeliverable body".to_string(),
            bot: "notify".to_string(),
            source: source.to_string(),
            reason_code: None,
            session_key: Some("sess-1".to_string()),
            retry_count: 5,
            claim_owner: "owner".to_string(),
            claimed_at: chrono::Utc::now(),
        };
        let base_id = 5_993_001;
        note_terminal_outbox_delivery_failure(&row(base_id, "headless_turn"), "500: boom");
        note_terminal_outbox_delivery_failure(
            &row(base_id + 1, "outbox_delivery_alert"),
            "500: boom",
        );

        let recorded: Vec<_> = crate::services::observability::events::recent(usize::MAX)
            .into_iter()
            .filter(|event| {
                event.event_type == "agent_quality_event"
                    && event.payload["quality_event_type"] == "outbox_delivery_failed"
                    && [base_id, base_id + 1]
                        .contains(&event.payload["payload"]["outbox_id"].as_i64().unwrap_or(0))
            })
            .collect();
        assert_eq!(recorded.len(), 2, "{recorded:?}");
        for event in recorded {
            assert_eq!(event.payload["channel_id"], "123");
            assert_eq!(
                event.payload["payload"]["content_snippet"],
                "undeliverable body"
            );
        }
    }
}
