//! Redacted thinking marker and transcript-event helpers for stream content arms.

use super::*;

pub(super) fn thinking_status_line() -> String {
    "💭 Thinking...".to_string()
}

pub(super) fn redacted_thinking_transcript_event(
    _summary: Option<String>,
) -> SessionTranscriptEvent {
    SessionTranscriptEvent {
        kind: SessionTranscriptEventKind::Thinking,
        tool_name: None,
        summary: None,
        content: String::new(),
        status: Some("info".to_string()),
        is_error: false,
    }
}

#[cfg(test)]
mod thinking_redaction_tests {
    use super::*;

    // U-6 Policy clause 1 + clause 4: the transcript event we record for a
    // Thinking stream message must carry no raw model reasoning. Both
    // `summary` and `content` must be empty regardless of the input the
    // model sent, and the kind must be `Thinking` (so consumers can apply
    // the neutral marker policy in clause 2).
    #[test]
    fn redacted_thinking_event_drops_summary_and_keeps_content_blank() {
        let event = redacted_thinking_transcript_event(Some(
            "internal scratchpad reasoning that must not leak".to_string(),
        ));

        assert_eq!(event.kind, SessionTranscriptEventKind::Thinking);
        assert!(event.tool_name.is_none());
        assert!(
            event.summary.is_none(),
            "summary leaked: {:?}",
            event.summary
        );
        assert!(
            event.content.is_empty(),
            "content leaked: {:?}",
            event.content
        );
        assert_eq!(event.status.as_deref(), Some("info"));
        assert!(!event.is_error);
    }

    // Calling the redaction function with `None` summary keeps the same
    // invariants — defense in depth against future callers that might
    // attempt to pass through model text accidentally.
    #[test]
    fn redacted_thinking_event_with_none_summary_still_blank() {
        let event = redacted_thinking_transcript_event(None);

        assert!(event.summary.is_none());
        assert!(event.content.is_empty());
    }

    // U-6 Policy clause 2: the user-visible thinking marker is a single
    // neutral string with no model text, no timers, no token counts.
    // It must be a stable identifier that the relay can deduplicate on.
    #[test]
    fn thinking_status_line_is_neutral_single_marker() {
        let line = thinking_status_line();

        assert_eq!(line, "💭 Thinking...");
    }

    // U-6 Policy clause 2 (stability): repeated calls must return the
    // exact same marker string. The Thinking dispatch path uses this for
    // both `current_tool_line` replacement and dedupe — if it ever drifted
    // into a non-deterministic form (timestamp, counter, locale variant),
    // the relay could emit multiple markers per turn or fail to match the
    // previous one.
    #[test]
    fn thinking_status_line_is_stable_across_repeated_calls() {
        let baseline = thinking_status_line();
        for _ in 0..10 {
            assert_eq!(thinking_status_line(), baseline);
        }
    }
}
