use super::*;

const VOICE_SILENCE_MARKER: &str = "ADK_VOICE_SILENCE";
const VOICE_HANDOFF_BACKGROUND_MARKER: &str = "ADK_VOICE_HANDOFF_BACKGROUND:";

pub(super) fn foreground_ack_text(transcript: &str, language: &str) -> String {
    let english = language.trim().to_ascii_lowercase().starts_with("en");
    let looks_like_work = looks_like_background_work_request(transcript);
    match (english, looks_like_work) {
        (true, true) => {
            "Got it. I will start that in the channel and come back briefly.".to_string()
        }
        (true, false) => "Got it. I am checking that now.".to_string(),
        (false, true) => "알겠어요. 채널에서 바로 진행하고 짧게 다시 알려드릴게요.".to_string(),
        (false, false) => "알겠어요. 바로 확인할게요.".to_string(),
    }
}

/// #3908: decide whether a foreground-ack cancellation must SUPPRESS the spoken
/// fallback. A genuine interruption (user barge-in / explicit stop / session
/// teardown) suppresses and keeps the channel silent; a self-inflicted
/// foreground-ack timeout does NOT — the timeout flips the shared cancel token
/// only so #2250 can terminate the detached model child, yet the user is still
/// owed an audible fallback acknowledgement.
///
/// A user barge-in must always DOMINATE the timeout regardless of ordering. On
/// the shared token the cancel KIND is first-wins while the free-form LABEL is
/// last-wins (`provider::CancelToken::set_cancel_source`), so a barge-in racing
/// the self-timeout leaves its trace in exactly one of the two fields: the kind
/// if it cancelled first, the label if it cancelled last. Keep the fallback
/// only when BOTH signals are still the `voice_foreground_ack_timeout` self-
/// cancel (no barge-in ever touched this token); otherwise suppress.
pub(super) fn ack_cancel_suppresses_fallback(
    cancel_token: &crate::services::provider::CancelToken,
) -> bool {
    use crate::services::provider::CancelSource;
    if !cancel_token.cancelled.load(Ordering::Relaxed) {
        return false;
    }
    let kind_is_timeout = cancel_token.cancel_source_kind() == Some(CancelSource::WatchdogTimeout);
    let label_is_timeout = cancel_token
        .cancel_source()
        .is_some_and(|label| CancelSource::classify(&label) == CancelSource::WatchdogTimeout);
    !(kind_is_timeout && label_is_timeout)
}

pub(super) fn parse_voice_foreground_decision(
    text: &str,
    transcript: &str,
    language: &str,
    max_chars: usize,
) -> VoiceForegroundDecision {
    let trimmed = text.trim();
    if let Some(marker_line) = first_voice_foreground_marker_candidate(trimmed) {
        if marker_line.eq_ignore_ascii_case(VOICE_SILENCE_MARKER) {
            return VoiceForegroundDecision::Silence;
        }
        if let Some(summary) =
            parse_voice_background_handoff_summary(&marker_line, transcript, language, max_chars)
        {
            return VoiceForegroundDecision::HandoffBackground(summary);
        }
    }
    let spoken = foreground_spoken_only_with_limit(trimmed, language, max_chars);
    if spoken.trim().is_empty() {
        VoiceForegroundDecision::Silence
    } else {
        VoiceForegroundDecision::Speak(spoken)
    }
}

fn first_voice_foreground_marker_candidate(text: &str) -> Option<String> {
    let mut skipped_leading_fence = false;
    for raw_line in text.lines() {
        let line = strip_voice_marker_leading_wrappers(raw_line);
        if line.is_empty() {
            continue;
        }

        if let Some(after_fence) = strip_code_fence_prefix(line) {
            let after_fence = strip_voice_marker_trailing_wrappers(after_fence);
            if starts_with_voice_foreground_marker(after_fence) {
                return Some(after_fence.to_string());
            }
            if !skipped_leading_fence {
                skipped_leading_fence = true;
                continue;
            }
        }

        return Some(strip_voice_marker_trailing_wrappers(line).to_string());
    }
    None
}

fn strip_voice_marker_leading_wrappers(mut line: &str) -> &str {
    for _ in 0..8 {
        let trimmed = line.trim();
        let Some(first) = trimmed.chars().next() else {
            return "";
        };
        if first == '>' {
            line = &trimmed[first.len_utf8()..];
            continue;
        }
        if let Some(rest) = ["- ", "* ", "+ "]
            .iter()
            .find_map(|prefix| trimmed.strip_prefix(prefix))
        {
            line = rest;
            continue;
        }
        if matches!(first, '"' | '\'') {
            let rest = trimmed[first.len_utf8()..].trim_start();
            if starts_with_wrapped_voice_foreground_marker(rest)
                || strip_code_fence_prefix(rest).is_some()
            {
                line = rest;
                continue;
            }
        }
        return trimmed;
    }
    line.trim()
}

fn strip_voice_marker_trailing_wrappers(mut line: &str) -> &str {
    for _ in 0..4 {
        let trimmed = line.trim();
        if let Some(rest) = trimmed
            .strip_suffix("```")
            .or_else(|| trimmed.strip_suffix("~~~"))
        {
            line = rest;
            continue;
        }
        if let Some(last) = trimmed.chars().last()
            && matches!(last, '"' | '\'')
        {
            line = &trimmed[..trimmed.len() - last.len_utf8()];
            continue;
        }
        return trimmed;
    }
    line.trim()
}

fn strip_code_fence_prefix(line: &str) -> Option<&str> {
    line.strip_prefix("```")
        .or_else(|| line.strip_prefix("~~~"))
}

fn starts_with_voice_foreground_marker(line: &str) -> bool {
    line.eq_ignore_ascii_case(VOICE_SILENCE_MARKER)
        || strip_ascii_case_prefix(line, VOICE_HANDOFF_BACKGROUND_MARKER).is_some()
}

fn starts_with_wrapped_voice_foreground_marker(line: &str) -> bool {
    starts_with_voice_foreground_marker(strip_voice_marker_trailing_wrappers(line))
}

fn strip_ascii_case_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let candidate = text.get(..prefix.len())?;
    candidate
        .eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

fn parse_voice_background_handoff_summary(
    marker_line: &str,
    transcript: &str,
    language: &str,
    max_chars: usize,
) -> Option<String> {
    let summary = strip_ascii_case_prefix(marker_line, VOICE_HANDOFF_BACKGROUND_MARKER)?.trim();
    if summary.is_empty() {
        Some(fallback_voice_background_handoff_summary(
            transcript, language, max_chars,
        ))
    } else {
        Some(summary.to_string())
    }
}

fn fallback_voice_background_handoff_summary(
    transcript: &str,
    language: &str,
    max_chars: usize,
) -> String {
    let summary = foreground_spoken_only_with_limit(transcript, language, max_chars);
    let summary = summary.trim();
    if !summary.is_empty() && !contains_voice_foreground_marker(summary) {
        return summary.to_string();
    }
    if language.trim().to_ascii_lowercase().starts_with("en") {
        "User requested background work.".to_string()
    } else {
        "사용자가 백그라운드 작업을 요청함".to_string()
    }
}

fn contains_voice_foreground_marker(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains(&VOICE_SILENCE_MARKER.to_ascii_lowercase())
        || lower.contains(&VOICE_HANDOFF_BACKGROUND_MARKER.to_ascii_lowercase())
}

fn looks_like_background_work_request(transcript: &str) -> bool {
    let text = transcript.to_ascii_lowercase();
    [
        "구현",
        "수정",
        "확인",
        "검토",
        "테스트",
        "배포",
        "이슈",
        "로그",
        "파일",
        "검색",
        "만들",
        "고쳐",
        "implement",
        "fix",
        "test",
        "deploy",
        "issue",
        "log",
        "file",
        "search",
        "review",
    ]
    .iter()
    .any(|needle| {
        if needle.is_ascii() {
            text.split(|ch: char| !ch.is_ascii_alphanumeric())
                .any(|word| word == *needle)
        } else {
            text.contains(needle)
        }
    })
}
