//! Provider-neutral classification for local-completing TUI slash controls.
//!
//! Prompt observation runs below the Discord relay layer, so it must be able to
//! decide whether a transcript record can create an external-turn lifecycle
//! without depending on Discord command/rendering modules. Keep the compact
//! raw/envelope pairing identity here as well: it may collapse two
//! representations of one submission, but never two same-form human commands.

/// AgentDesk pass-through commands that complete locally in a Claude TUI.
pub(crate) const LOCAL_ONLY_SLASH_COMMANDS: [&str; 4] =
    ["/effort", "/compact", "/cost", "/context"];

/// Claude-native controls observed from a TUI that also complete locally.
pub(crate) const OBSERVATION_ONLY_LOCAL_SLASH_COMMANDS: [&str; 1] = ["/model"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocalOnlySlashControlForm {
    RawInvocation,
    CommandEnvelope,
    LocalCommandStdout,
    CaveatOnly,
}

impl LocalOnlySlashControlForm {
    pub(crate) fn is_raw_invocation(self) -> bool {
        matches!(self, Self::RawInvocation)
    }

    pub(crate) fn is_pairable_representation(self) -> bool {
        matches!(self, Self::RawInvocation | Self::CommandEnvelope)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalOnlySlashControl {
    pub(crate) kind: String,
    /// Whitespace-normalized command arguments. This is used only to pair a
    /// raw invocation with its own command envelope, never to dedupe two human
    /// submissions of the same form.
    pub(crate) normalized_args: String,
    pub(crate) form: LocalOnlySlashControlForm,
}

impl LocalOnlySlashControl {
    pub(crate) fn is_complementary_representation_of(&self, other: &Self) -> bool {
        self.form.is_pairable_representation()
            && other.form.is_pairable_representation()
            && self.form != other.form
            && self.kind == other.kind
            && self.normalized_args == other.normalized_args
    }
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
            normalized_args: String::new(),
            form: LocalOnlySlashControlForm::CaveatOnly,
        });
    }

    if starts_with_compacted_local_command_stdout(&normalized) {
        return Some(LocalOnlySlashControl {
            kind: "/compact".to_string(),
            normalized_args: String::new(),
            form: LocalOnlySlashControlForm::LocalCommandStdout,
        });
    }
    if starts_with_complete_local_command_stdout(&normalized) {
        return Some(LocalOnlySlashControl {
            kind: "local-command-stdout".to_string(),
            normalized_args: String::new(),
            form: LocalOnlySlashControlForm::LocalCommandStdout,
        });
    }

    if let Some((kind, args)) = command_envelope_invocation(&normalized)
        && is_local_only_slash_command_kind(&kind)
    {
        return Some(LocalOnlySlashControl {
            kind,
            normalized_args: normalize_args(&args),
            form: LocalOnlySlashControlForm::CommandEnvelope,
        });
    }
    if let Some((kind, args)) = raw_slash_invocation(&normalized)
        && is_local_only_slash_command_kind(&kind)
    {
        return Some(LocalOnlySlashControl {
            kind,
            normalized_args: normalize_args(&args),
            form: LocalOnlySlashControlForm::RawInvocation,
        });
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

fn normalize_args(args: &str) -> String {
    args.split_whitespace().collect::<Vec<_>>().join(" ")
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
    fn raw_and_envelope_are_complementary_only_when_kind_and_args_match() {
        let raw = classify_local_only_slash_control("/effort high").unwrap();
        let wrapper = classify_local_only_slash_control(
            "<command-message>effort</command-message><command-name>/effort high</command-name><command-args>high</command-args>",
        )
        .unwrap();
        let different = classify_local_only_slash_control("/effort low").unwrap();
        assert!(raw.is_complementary_representation_of(&wrapper));
        assert!(!raw.is_complementary_representation_of(&different));
        assert!(raw.form.is_raw_invocation());
    }
}
