//! Provider-neutral classification for local-completing TUI slash controls.
//!
//! Prompt observation runs below the Discord relay layer, so it must be able to
//! decide whether a transcript record can create an external-turn lifecycle
//! without depending on Discord command/rendering modules. Local command
//! records are deliberately never text/time-paired: duplicate notes are safer
//! than swallowing a later human command.

/// AgentDesk pass-through commands that complete locally in a Claude TUI.
pub(crate) const LOCAL_ONLY_SLASH_COMMANDS: [&str; 4] =
    ["/effort", "/compact", "/cost", "/context"];

/// Claude-native controls observed from a TUI that also complete locally.
pub(crate) const OBSERVATION_ONLY_LOCAL_SLASH_COMMANDS: [&str; 1] = ["/model"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalOnlySlashControl {
    pub(crate) kind: String,
}

/// Returns the local-only control carried by `prompt`, if it is a complete,
/// start-anchored local command representation. Unknown slash commands and
/// `/loop` deliberately return `None`: they retain their normal external-turn
/// lifecycle and raw/envelope dedupe behavior.
pub(crate) fn classify_local_only_slash_control(prompt: &str) -> Option<LocalOnlySlashControl> {
    let (normalized, peeled_caveat) = normalize_local_control_prompt(prompt);
    if peeled_caveat && normalized.is_empty() {
        return Some(LocalOnlySlashControl {
            kind: "slash".to_string(),
        });
    }

    if starts_with_compacted_local_command_stdout(&normalized) {
        return Some(LocalOnlySlashControl {
            kind: "/compact".to_string(),
        });
    }
    if starts_with_complete_local_command_stdout(&normalized) {
        return Some(LocalOnlySlashControl {
            kind: "local-command-stdout".to_string(),
        });
    }

    if let Some((kind, _args)) = command_envelope_invocation(&normalized)
        && is_local_only_slash_command_kind(&kind)
    {
        return Some(LocalOnlySlashControl { kind });
    }
    if let Some((kind, _args)) = raw_slash_invocation(&normalized)
        && is_local_only_slash_command_kind(&kind)
    {
        return Some(LocalOnlySlashControl { kind });
    }
    None
}

pub(crate) fn is_local_only_slash_command_kind(kind: &str) -> bool {
    LOCAL_ONLY_SLASH_COMMANDS.contains(&kind)
        || OBSERVATION_ONLY_LOCAL_SLASH_COMMANDS.contains(&kind)
}

/// Strip ANSI/terminal control sequences while preserving meaningful layout.
/// This intentionally mirrors the TUI task-card sanitizer; it lives at the
/// service layer because pre-publish prompt observation cannot depend on
/// Discord rendering code.
pub(crate) fn strip_terminal_controls(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek().copied() == Some('[') {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        if ch.is_control() && ch != '\n' && ch != '\r' && ch != '\t' {
            continue;
        }
        output.push(ch);
    }
    output
}

fn normalize_local_control_prompt(prompt: &str) -> (String, bool) {
    let normalized = strip_terminal_controls(prompt);
    let normalized = normalized.trim_start();
    let normalized = strip_leading_injection_wrapper(normalized);
    let normalized = normalized.trim_start();
    let (normalized, peeled_caveat) = strip_leading_local_command_caveat(normalized);
    (normalized.trim_start().to_string(), peeled_caveat)
}

/// Removes one start-anchored SSH-direct injection wrapper. Human text that
/// merely quotes the marker mid-body is intentionally left untouched.
pub(crate) fn strip_leading_injection_wrapper(text: &str) -> &str {
    const WRAPPER_MARKER: &str = "터미널에 직접 주입된 입력";
    if !text.starts_with(WRAPPER_MARKER) {
        return text;
    }
    let Some(after_wrapper_line) = text.find('\n').map(|idx| &text[idx + 1..]) else {
        return text;
    };
    let trimmed = after_wrapper_line.trim_start_matches(['\r', '\n']);
    if let Some(rest) = trimmed.strip_prefix("```") {
        if let Some(idx) = rest.find('\n') {
            return strip_trailing_injection_code_fence(&rest[idx + 1..]);
        }
        return after_wrapper_line;
    }
    after_wrapper_line
}

/// Returns true only for a structured task lifecycle record at the beginning of
/// an observed prompt. This seam is deliberately provider-neutral so the
/// pre-publish dedupe layer can classify status records before it records any
/// generic external-input lease. Human text quoting the tag mid-prompt is not a
/// lifecycle record.
pub(crate) fn is_start_anchored_task_notification_prompt(prompt: &str) -> bool {
    let normalized = strip_terminal_controls(prompt);
    let normalized = strip_leading_injection_wrapper(normalized.trim_start()).trim_start();
    let Some(rest) = normalized.strip_prefix("<task-notification") else {
        return false;
    };
    rest.starts_with('>') || rest.chars().next().is_some_and(char::is_whitespace)
}

fn strip_trailing_injection_code_fence(text: &str) -> &str {
    let trimmed = text.trim_end();
    let Some(before_fence) = trimmed.strip_suffix("```") else {
        return text;
    };
    if before_fence.is_empty() || before_fence.ends_with('\r') || before_fence.ends_with('\n') {
        before_fence
    } else {
        text
    }
}

fn strip_leading_local_command_caveat(text: &str) -> (&str, bool) {
    const OPEN: &str = "<local-command-caveat>";
    const CLOSE: &str = "</local-command-caveat>";
    if !text.starts_with(OPEN) {
        return (text, false);
    }
    let Some(end) = text.find(CLOSE) else {
        return (text, false);
    };
    (&text[end + CLOSE.len()..], true)
}

fn starts_with_complete_local_command_stdout(normalized: &str) -> bool {
    const OPEN: &str = "<local-command-stdout>";
    const CLOSE: &str = "</local-command-stdout>";
    normalized
        .strip_prefix(OPEN)
        .is_some_and(|rest| rest.trim_end().ends_with(CLOSE))
}

fn starts_with_compacted_local_command_stdout(normalized: &str) -> bool {
    const PREFIX: &str = "<local-command-stdout>Compacted";
    const CLOSE: &str = "</local-command-stdout>";
    if !normalized.starts_with(PREFIX) {
        return false;
    }
    let trimmed = normalized.trim_end();
    if trimmed.contains(CLOSE) {
        return trimmed.ends_with(CLOSE);
    }
    !trimmed.contains('\r') && !trimmed.contains('\n')
}

fn command_envelope_invocation(normalized: &str) -> Option<(String, String)> {
    if !(normalized.starts_with("<command-message>") || normalized.starts_with("<command-name>")) {
        return None;
    }
    let command_name = first_xml_tag_token(normalized, "command-name")
        .or_else(|| first_xml_tag_token(normalized, "command-message"))?;
    let (kind, name_args) = raw_slash_invocation(&command_name)?;
    let args = first_xml_tag_token(normalized, "command-args").unwrap_or(name_args);
    Some((kind, args))
}

fn first_xml_tag_token(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let after = text.split_once(&open)?.1;
    let (body, _) = after.split_once(&close)?;
    let token = body.trim();
    (!token.is_empty()).then(|| token.to_string())
}

fn raw_slash_invocation(value: &str) -> Option<(String, String)> {
    let value = value.trim();
    let (name, args) = match value.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args),
        None => (value, ""),
    };
    if !name.starts_with('/') || name.len() <= 1 {
        return None;
    }
    Some((name.to_ascii_lowercase(), args.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_known_local_controls_without_matching_prefixes_or_mid_body_text() {
        for prompt in [
            "/compact",
            "/compact now",
            "/effort high",
            "/cost",
            "/context",
            "/model",
        ] {
            assert!(
                classify_local_only_slash_control(prompt).is_some(),
                "{prompt}"
            );
        }
        for prompt in ["/compactfoo", "tell me about /compact", "/loop 5m"] {
            assert!(
                classify_local_only_slash_control(prompt).is_none(),
                "{prompt}"
            );
        }
    }

    #[test]
    fn recognizes_raw_and_envelope_without_using_them_as_a_dedup_key() {
        let raw = classify_local_only_slash_control("/effort high").unwrap();
        let wrapper = classify_local_only_slash_control(
            "<command-message>effort</command-message><command-name>/effort high</command-name><command-args>high</command-args>",
        )
        .unwrap();
        assert_eq!(raw.kind, "/effort");
        assert_eq!(wrapper.kind, "/effort");
    }
}
