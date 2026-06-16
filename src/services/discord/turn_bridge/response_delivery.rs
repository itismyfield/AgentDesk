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
