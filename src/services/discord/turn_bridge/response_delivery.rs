//! #3479: pure response-delivery + transcript-event helpers, moved verbatim out
//! of turn_bridge/mod.rs (behavior-preserving — only visibility prefixes set to
//! `pub(super)` and the two `super::` discord-level refs deepened to
//! `super::super::` from the child). All deps reached via `use super::*;`.

use super::*;

pub(super) fn push_transcript_event(
    events: &mut Vec<SessionTranscriptEvent>,
    event: SessionTranscriptEvent,
) {
    let has_payload = !event.content.trim().is_empty()
        || event
            .summary
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        || event
            .tool_name
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
    if has_payload
        || matches!(
            event.kind,
            SessionTranscriptEventKind::Thinking
                | SessionTranscriptEventKind::Result
                | SessionTranscriptEventKind::Error
                | SessionTranscriptEventKind::Task
                | SessionTranscriptEventKind::System
        )
    {
        events.push(event);
    }
}

pub(super) fn response_portion_after_offset(
    full_response: &str,
    response_sent_offset: usize,
) -> &str {
    full_response.get(response_sent_offset..).unwrap_or("")
}

pub(super) fn terminal_delivery_response_after_offset(
    full_response: &str,
    response_sent_offset: usize,
    empty_response_notice: Option<&str>,
) -> String {
    let raw_response =
        response_portion_after_offset(full_response, response_sent_offset).to_string();
    let stripped_response =
        super::super::response_sanitizer::strip_leading_tui_response_chrome(&raw_response);
    if !raw_response.trim().is_empty() && stripped_response.trim().is_empty() {
        return String::new();
    }
    let mut delivery_response = stripped_response;
    if delivery_response.trim().is_empty()
        && let Some(notice) = empty_response_notice
    {
        delivery_response = notice.to_string();
    }
    delivery_response
}

pub(super) fn done_result_requires_full_terminal_replay(
    full_response: &str,
    result: &str,
    response_sent_offset: usize,
    streamed_assistant_text_this_turn: bool,
) -> bool {
    response_sent_offset > 0
        && streamed_assistant_text_this_turn
        && result.len() > super::super::DISCORD_MSG_LIMIT
        && !result.trim().is_empty()
        && full_response.trim() == result.trim()
}

#[cfg(test)]
mod tests {
    use super::{
        done_result_requires_full_terminal_replay, terminal_delivery_response_after_offset,
    };
    use crate::services::discord::DISCORD_MSG_LIMIT;

    #[test]
    fn terminal_delivery_after_rollover_includes_authoritative_tail() {
        let frozen_prefix = "probe 품질 검토 결과, 현재 관측된 ";
        let terminal_body = format!("{frozen_prefix}기준으로는 실패입니다");

        let delivered_tail =
            terminal_delivery_response_after_offset(&terminal_body, frozen_prefix.len(), None);

        assert_eq!(delivered_tail, "기준으로는 실패입니다");
    }

    #[test]
    fn terminal_error_after_rollover_delivers_error_text_not_recovery_notice() {
        let error = "Error: transport failed".to_string();
        let mut response_sent_offset = 900usize;
        let mut state = crate::services::discord::InflightTurnState::new(
            crate::services::provider::ProviderKind::Claude,
            1,
            Some("adk-cc".to_string()),
            42,
            5001,
            5002,
            "prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-claude-adk-cc-1".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            10,
        );

        super::super::retry_state::sync_terminal_error_delivery_state(
            &error,
            &mut response_sent_offset,
            &mut state,
        );
        let delivered = terminal_delivery_response_after_offset(&error, response_sent_offset, None);

        assert_eq!(response_sent_offset, 0);
        assert_eq!(delivered, error);
        assert!(!delivered.contains("세션 복구 중"));
    }

    #[test]
    fn long_authoritative_done_after_rollover_replays_full_body() {
        let frozen_prefix = "probe 품질 ".repeat(220);
        let terminal_tail = "기준으로는 실패입니다";
        let terminal_body = format!("{frozen_prefix}{terminal_tail}");
        assert!(terminal_body.len() > DISCORD_MSG_LIMIT);

        assert!(done_result_requires_full_terminal_replay(
            &terminal_body,
            &terminal_body,
            frozen_prefix.len(),
            true,
        ));
    }
}
#[cfg(test)]
mod streaming_edit_text_tests {
    use super::*;

    #[test]
    fn empty_response_notice_is_delivery_only_not_history_payload() {
        let full_response = String::new();
        let rendered =
            terminal_delivery_response_after_offset(&full_response, 0, Some("(No response)"));

        assert_eq!(rendered, "(No response)");
        assert!(full_response.is_empty());
    }
}

#[cfg(test)]
mod headless_completion_footer_tests {
    fn compact_ws(input: &str) -> String {
        input.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// #4103: headless-dispatched turns (API / cron / routine, and E2E
    /// E-1/E-22/E-23) never emitted single-message completion chrome because the
    /// `enqueue_headless_delivery` SUCCESS arm set `terminal_delivery_committed`
    /// / `terminal_body_visible` but NOT `completion_footer_terminal_text`, so
    /// `note_turn_completed_footer` was never reached.
    ///
    /// The expected window is anchored on the `.await;` from the arm's
    /// `cleanup_headless_streaming_placeholder_after_delivery(...).await;` — that
    /// `.await; terminal_delivery_committed = true;` prefix occurs EXACTLY here.
    /// The sibling non-headless replace arm produces the byte-identical footer
    /// suffix but is prefixed by `if outcome {`, not `.await;`, so a bare-suffix
    /// assertion would still pass off the sibling even if this write were deleted
    /// or moved to the `Err` arm (the false-negative this anchor closes). The
    /// searched source is only the included terminal-delivery module, so this
    /// test's own expected literal cannot self-match.
    #[test]
    fn headless_enqueue_success_registers_completion_footer_text() {
        let source = compact_ws(include_str!("terminal_outcome_delivery.rs"));
        let expected = ".await; terminal_delivery_committed = true; \
             terminal_body_visible = true; \
             if single_message_panel_footer_mode { \
             completion_footer_terminal_text = Some(delivery_response.clone()); }";

        assert!(
            source.contains(expected),
            "headless enqueue_headless_delivery success arm must set \
             completion_footer_terminal_text right after its post-delivery \
             cleanup .await + terminal_body_visible commit (#4103 completion chrome)"
        );
    }
}
