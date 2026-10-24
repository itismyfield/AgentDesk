//! #3479: pure response-delivery + transcript-event helpers, moved verbatim out
//! of turn_bridge/mod.rs (behavior-preserving — only visibility prefixes set to
//! `pub(super)` and the two `super::` discord-level refs deepened to
//! `super::super::` from the child). All deps reached via `use super::*;`.

use super::*;

const PROMPT_TOO_LONG_DETAIL_MARKER: &str = "__prompt too long__";

pub(super) fn prompt_too_long_guidance(detail: &str) -> String {
    crate::services::discord::commands::owner_error_response(
        "현재 대화가 provider의 컨텍스트 한도를 넘었어요.\n`/compact`로 대화를 압축하거나 요청을 짧게 줄여 다시 보내 주세요.",
        &format!("{PROMPT_TOO_LONG_DETAIL_MARKER}\n{}", detail.trim()),
    )
}

pub(super) fn prompt_too_long_detail_from_guidance(guidance: &str) -> Option<&str> {
    let (_, folded_detail) = guidance.split_once(PROMPT_TOO_LONG_DETAIL_MARKER)?;
    folded_detail
        .strip_prefix('\n')
        .unwrap_or(folded_detail)
        .strip_suffix("\n```||")
}

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

/// Whether a terminal delivery sends no assistant answer (nothing, or only a notice), normalized
/// as delivery does. Such a delivery only reads body ownership; one sending an answer claims it.
pub(super) fn no_answer(full_response: &str, response_sent_offset: usize, delivery: &str) -> bool {
    delivery.trim().is_empty()
        || terminal_delivery_response_after_offset(full_response, response_sent_offset, None)
            .trim()
            .is_empty()
}

pub(super) fn done_result_requires_full_terminal_replay(
    full_response: &str,
    result: &str,
    response_sent_offset: usize,
    streamed_assistant_text_this_turn: bool,
) -> bool {
    response_sent_offset > 0
        && streamed_assistant_text_this_turn
        && super::super::formatting::needs_multiple_messages(result)
        && !result.trim().is_empty()
        && full_response.trim() == result.trim()
}

#[cfg(test)]
mod tests {
    use super::{
        done_result_requires_full_terminal_replay, prompt_too_long_detail_from_guidance,
        prompt_too_long_guidance, terminal_delivery_response_after_offset,
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
    fn prompt_too_long_guidance_round_trips_folded_provider_detail() {
        let detail = "Error: request failed\nstderr: context_length_exceeded: private detail";
        let guidance = prompt_too_long_guidance(detail);

        assert_eq!(
            prompt_too_long_detail_from_guidance(&guidance),
            Some(detail)
        );
        assert!(guidance.contains("`/compact`"));
        assert!(guidance.contains("요청을 짧게"));
    }

    #[test]
    fn long_authoritative_done_after_rollover_replays_full_body() {
        // The fixture is Korean, so it must clear the limit in CHARACTERS, not
        // only in UTF-8 bytes: the old `repeat(220)` body was 2891 bytes but
        // only 1991 characters — it fit one Discord message and passed this
        // test purely on the byte comparison the predicate no longer makes.
        let frozen_prefix = "probe 품질 ".repeat(240);
        let terminal_tail = "기준으로는 실패입니다";
        let terminal_body = format!("{frozen_prefix}{terminal_tail}");
        assert!(terminal_body.len() > DISCORD_MSG_LIMIT);
        assert!(terminal_body.chars().count() > DISCORD_MSG_LIMIT);

        assert!(done_result_requires_full_terminal_replay(
            &terminal_body,
            &terminal_body,
            frozen_prefix.len(),
            true,
        ));
    }

    /// A Korean answer is ~3 UTF-8 bytes per character, so the old
    /// `result.len() > DISCORD_MSG_LIMIT` byte check declared a comfortably
    /// single-message body "long" at ~667 characters and reset the delivery
    /// offset to replay the whole answer down the terminal-chunk path. (It did
    /// not split the body: `split_message` already counts characters and
    /// returns one chunk here.) The gate must count characters, like
    /// `split_message` does.
    #[test]
    fn korean_answer_under_the_character_limit_stays_one_message() {
        let body = "한".repeat(900);
        assert!(
            body.len() > DISCORD_MSG_LIMIT,
            "2700 bytes, over the byte limit"
        );
        assert!(!done_result_requires_full_terminal_replay(
            &body, &body, 900, true
        ));

        let overflowing = "한".repeat(2100);
        assert!(done_result_requires_full_terminal_replay(
            &overflowing,
            &overflowing,
            900,
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
    use crate::services::discord::router::source_guard_support::{Lexed, forge, swap};
    use crate::test_support::CommentForgery;

    const DELIVERY: &str = include_str!("terminal_outcome_delivery.rs");
    const DISPOSITION: &str =
        "match super::headless_delivery::headless_delivery_disposition(&delivery_outcome)";
    const COMMIT_ARM: &str = "super::headless_delivery::HeadlessDeliveryDisposition::Commit";
    const CLEANUP: &str = "cleanup_headless_streaming_placeholder_after_delivery(";
    const COMMIT: &str = ".await; terminal_delivery_committed = true; terminal_body_visible = true; if single_message_panel_footer_mode { completion_footer_terminal_text = Some(delivery_response.clone()); }";

    /// Classification 2: in the real headless Commit arm, the cleanup await is directly followed by
    /// the terminal commit and footer text, comments and whitespace ignored. A lexical tripwire.
    fn headless_footer_gaps(delivery: &str) -> Vec<String> {
        let arm = Lexed::new(delivery)
            .compact()
            .block(&tight(DISPOSITION))
            .and_then(|outcome| outcome.arm(COMMIT_ARM));
        let Some(arm) = arm else {
            return vec!["missing the Commit arm of the headless delivery disposition".into()];
        };
        match arm.find(&tight(CLEANUP)) {
            Some(cleanup) if arm.find_after(cleanup, &tight(COMMIT)).is_some() => Vec::new(),
            found => vec![format!(
                "the Commit arm must commit and register the footer after its cleanup: {found:?}"
            )],
        }
    }

    fn tight(needle: &str) -> String {
        Lexed::new(needle).compact().text().to_string()
    }

    /// Headless-dispatched turns (API, cron, routine) must reach single-message completion
    /// chrome: the success arm sets the footer text right after its committed delivery.
    #[test]
    fn headless_enqueue_success_registers_completion_footer_text() {
        assert_eq!(headless_footer_gaps(DELIVERY), Vec::<String>::new());
    }

    #[test]
    fn headless_footer_guard_rejects_commented_and_moved_footer_writes() {
        for needle in [
            DISPOSITION,
            COMMIT_ARM,
            CLEANUP,
            "terminal_delivery_committed = true;",
            "terminal_body_visible = true;",
            "completion_footer_terminal_text = Some(delivery_response.clone());",
        ] {
            for forgery in CommentForgery::ALL {
                let forged = forge(DELIVERY, needle, forgery);
                assert!(
                    !headless_footer_gaps(&forged).is_empty(),
                    "{forgery:?} `{needle}`"
                );
            }
        }
        let cleanup = DELIVERY.find(CLEANUP).expect("headless cleanup call");
        let (head, tail) = DELIVERY.split_at(cleanup);
        let commit = "terminal_delivery_committed = true;";
        let reordered = [
            head,
            swap(tail, commit, "terminal_body_visible = true;").as_str(),
        ]
        .concat();
        assert!(!headless_footer_gaps(&reordered).is_empty());
        // The sibling replace arm writes the same footer; dropping it from the Commit arm still fails.
        let footer = "                        if single_message_panel_footer_mode {\n                            completion_footer_terminal_text = Some(delivery_response.clone());\n                        }\n";
        let at = cleanup + DELIVERY[cleanup..].find(footer).expect("Commit arm footer");
        let dropped = [&DELIVERY[..at], &DELIVERY[at + footer.len()..]].concat();
        assert!(!headless_footer_gaps(&dropped).is_empty());
    }
}
