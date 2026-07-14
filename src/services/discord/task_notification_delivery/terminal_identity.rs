//! Provider-entry identity for terminal task cards (#4295).

use super::*;

impl TaskCardEvent {
    pub(in crate::services::discord) fn from_task_prompt(
        channel_id: u64,
        provider: &str,
        session_key: &str,
        raw_prompt: &str,
    ) -> Self {
        Self::from_task_prompt_with_source_event_id(
            channel_id,
            provider,
            session_key,
            raw_prompt,
            None,
        )
    }

    pub(in crate::services::discord) fn from_task_prompt_with_source_event_id(
        channel_id: u64,
        provider: &str,
        session_key: &str,
        raw_prompt: &str,
        source_event_id: Option<&str>,
    ) -> Self {
        let note = super::super::tui_task_card::parse_task_notification(raw_prompt);
        let task_id = note.task_id.clone().and_then(clean_owned);
        let tool_use_id = note.tool_use_id.clone().and_then(clean_owned);
        let kind = note.kind().to_string();
        let normalized_payload = normalized_task_payload_fingerprint(
            task_id.as_deref(),
            tool_use_id.as_deref(),
            note.status.as_deref().unwrap_or(""),
            note.summary.as_deref().unwrap_or(""),
        );
        let terminal_delivery_fingerprint = source_event_id
            .map(str::trim)
            .filter(|source_event_id| !source_event_id.is_empty())
            .map(|source_event_id| terminal_source_fingerprint(provider, source_event_id));
        // The provider entry id is a supplementary replay identity, never the
        // card's claim key. XML observation and stream-JSON promotion must keep
        // converging on the same semantic row.
        let event_key = semantic_event_key(
            task_id.as_deref(),
            tool_use_id.as_deref(),
            &normalized_payload,
        );
        Self {
            scope: source_bound_scope(
                channel_id,
                provider,
                session_key,
                event_key,
                terminal_delivery_fingerprint,
            ),
            task_id,
            tool_use_id,
            kind,
            payload: TaskCardPayload::Task(note),
        }
    }

    pub(in crate::services::discord) fn from_subagent_prompt(
        channel_id: u64,
        provider: &str,
        session_key: &str,
        raw_prompt: &str,
    ) -> Self {
        Self::from_subagent_prompt_with_source_event_id(
            channel_id,
            provider,
            session_key,
            raw_prompt,
            None,
        )
    }

    pub(in crate::services::discord) fn from_subagent_prompt_with_source_event_id(
        channel_id: u64,
        provider: &str,
        session_key: &str,
        raw_prompt: &str,
        source_event_id: Option<&str>,
    ) -> Self {
        let semantic = super::super::response_sanitizer::subagent_notification_card::semantic_event(
            raw_prompt,
        )
        .unwrap_or_else(|| {
            super::super::response_sanitizer::subagent_notification_card::SubagentNotificationSemantic {
                task_id: None,
                tool_use_id: None,
                payload_fingerprint: fingerprint(&[
                    "malformed-subagent",
                    &super::super::tui_task_card::strip_terminal_controls(raw_prompt),
                ]),
            }
        });
        let terminal_delivery_fingerprint = source_event_id
            .map(str::trim)
            .filter(|source_event_id| !source_event_id.is_empty())
            .map(|source_event_id| terminal_source_fingerprint(provider, source_event_id));
        let event_key = semantic_event_key(
            semantic.task_id.as_deref(),
            semantic.tool_use_id.as_deref(),
            &semantic.payload_fingerprint,
        );
        Self {
            scope: source_bound_scope(
                channel_id,
                provider,
                session_key,
                event_key,
                terminal_delivery_fingerprint,
            ),
            task_id: semantic.task_id,
            tool_use_id: semantic.tool_use_id,
            kind: "subagent".to_string(),
            payload: TaskCardPayload::Subagent(
                super::super::response_sanitizer::subagent_notification_card::format_subagent_notification_card(
                    Some(session_key),
                    raw_prompt,
                ),
            ),
        }
    }
}

fn source_bound_scope(
    channel_id: u64,
    provider: &str,
    session_key: &str,
    event_key: String,
    terminal_delivery_fingerprint: Option<String>,
) -> TaskCardScope {
    match terminal_delivery_fingerprint {
        Some(fingerprint) => TaskCardScope::new(channel_id, provider, session_key, event_key)
            .with_terminal_delivery_fingerprint(fingerprint),
        None => TaskCardScope::new(channel_id, provider, session_key, event_key),
    }
}

fn terminal_source_fingerprint(provider: &str, source_event_id: &str) -> String {
    full_fingerprint(&[
        "task-terminal-source-v1",
        &provider.trim().to_ascii_lowercase(),
        source_event_id,
    ])
}
