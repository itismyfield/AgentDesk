//! Provider-neutral classification for local-completing TUI slash controls.
//!
//! Prompt observation runs below the Discord relay layer, so it must be able to
//! decide whether a transcript record can create an external-turn lifecycle
//! without depending on Discord command/rendering modules. The Discord relay
//! collapses a raw echo plus `<command-*>` envelope, and any sub-two-second
//! same-kind machine repeat, into one cosmetic marker. This suppresses only the
//! Discord note after local execution has completed; it cannot swallow command
//! injection or execution, and the first marker is always retained.

/// AgentDesk pass-through commands that complete locally in a Claude TUI.
pub(crate) const LOCAL_ONLY_SLASH_COMMANDS: [&str; 4] =
    ["/effort", "/compact", "/cost", "/context"];

/// Claude-native controls observed from a TUI that also complete locally.
pub(crate) const OBSERVATION_ONLY_LOCAL_SLASH_COMMANDS: [&str; 1] = ["/model"];

/// #5188: slash controls that RESET the provider session — Claude Code opens a
/// brand-new transcript JSONL and stops writing to the current one.
///
/// These are load-bearing for turn lifecycle, not just rendering. Such a command
/// produces NO assistant output, and any inflight created for it is bound to a
/// transcript that will never grow again — so it can never receive a terminal
/// signal and wedges the channel (`FOREIGN prior inflight is still live` on every
/// later turn). They are deliberately kept OUT of
/// [`is_local_only_slash_command_kind`] so the existing note-dedupe behaviour for
/// local-only controls is untouched; this list only governs the active-turn
/// lifecycle gate.
pub(crate) const SESSION_RESETTING_SLASH_COMMANDS: [&str; 1] = ["/clear"];

/// #5660: Codex TUI controls that complete locally inside the wrapper.
///
/// Deliberately a registry separate from [`LOCAL_ONLY_SLASH_COMMANDS`], which
/// `local_only_whitelist_matches_passthrough_command_set` pins to the Claude
/// passthrough variant set. The classification primitives below are shared
/// across providers; the command registries are not.
pub(crate) const CODEX_LOCAL_CONTROLS: [&str; 2] = ["/model", "/help"];

/// #5660 rule R3: single-segment filesystem roots that must reach the provider
/// as prompt text instead of being read as a command. Stored lowercase and
/// matched case-insensitively, so `/TMP` and `/users` are protected too. An
/// enumerated list, never a `Path::exists` probe: routing must not vary with
/// host filesystem state.
pub(crate) const FS_ROOT_SEGMENTS: [&str; 20] = [
    "applications",
    "bin",
    "dev",
    "etc",
    "home",
    "library",
    "mnt",
    "opt",
    "private",
    "proc",
    "root",
    "run",
    "sbin",
    "srv",
    "system",
    "tmp",
    "usr",
    "users",
    "var",
    "volumes",
];

/// #5660: how one raw Codex wrapper input line must be routed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CodexInputClass {
    /// Forward to the provider as a turn, exactly as before this issue.
    ProviderPrompt,
    /// Completes locally in the wrapper; no provider turn is created.
    LocalControl { name: String, args: String },
    /// Command-shaped but outside the registry. Rejected explicitly with the
    /// raw spelling echoed back, never forwarded as a prompt.
    UnsupportedControl { raw_name: String },
}

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

/// #5188: does this slash-control kind rotate the provider session (new
/// transcript JSONL, old one frozen)? See [`SESSION_RESETTING_SLASH_COMMANDS`].
pub(crate) fn is_session_resetting_slash_command_kind(kind: &str) -> bool {
    SESSION_RESETTING_SLASH_COMMANDS.contains(&kind)
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

/// #5660: classifies one Codex wrapper input line.
///
/// The default is inverted relative to an allowlist: anything command-shaped is
/// handled (run locally or refused by name), and only tokens that resolve as
/// filesystem paths escape to the provider. An allowlist cannot satisfy
/// "unsupported `/...` must not fall through" — falling through is exactly what
/// an allowlist does with everything it does not list. Terminal input (S2) and,
/// once the upstream admission gate lands, Discord intake (S3) call this same
/// function, so the two origins cannot drift on what counts as a command.
pub(crate) fn classify_codex_input(text: &str) -> CodexInputClass {
    let normalized = strip_terminal_controls(text);
    let normalized = normalized.trim();
    // External prompts are base64-framed and routinely multi-line. Only a
    // single line can be a command, and reading just its first line would
    // silently drop the rest of the body.
    if normalized.contains('\n') || normalized.contains('\r') {
        return CodexInputClass::ProviderPrompt;
    }
    // Start-anchored by construction: this returns None unless the very first
    // token begins with `/`, so a slash quoted mid-sentence stays a prompt.
    let Some((raw_name, args)) = raw_slash_invocation_parts(normalized) else {
        return CodexInputClass::ProviderPrompt;
    };
    if is_fs_path_token(&raw_name) {
        return CodexInputClass::ProviderPrompt;
    }
    let norm_name = raw_name.to_ascii_lowercase();
    if CODEX_LOCAL_CONTROLS.contains(&norm_name.as_str()) {
        CodexInputClass::LocalControl {
            name: norm_name,
            args,
        }
    } else {
        CodexInputClass::UnsupportedControl { raw_name }
    }
}

/// #5660 rule R: must this command token be preserved as a filesystem path?
/// Judged on the raw spelling, before any case normalization. R3 compares only
/// the leading ASCII alphanumeric run, so `/tmp에`, `/ETC를` and `/opt)` stay
/// paths; `/model에` runs to `model`, not a root, so it stays command-shaped.
fn is_fs_path_token(raw_name: &str) -> bool {
    let rest = &raw_name[1..];
    let root_run = rest
        .find(|ch: char| !ch.is_ascii_alphanumeric())
        .map_or(rest, |end| &rest[..end]);
    rest.contains('/')
        || raw_name.contains('.')
        || FS_ROOT_SEGMENTS
            .iter()
            .any(|root| root_run.eq_ignore_ascii_case(root))
}

/// The split and start-anchoring rule of record. Returns the command token
/// exactly as written; only [`raw_slash_invocation`] lowercases, so case
/// normalization has a single home.
pub(crate) fn raw_slash_invocation_parts(value: &str) -> Option<(String, String)> {
    let value = value.trim();
    let (name, args) = match value.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args),
        None => (value, ""),
    };
    if !name.starts_with('/') || name.len() <= 1 {
        return None;
    }
    Some((name.to_string(), args.trim().to_string()))
}

fn raw_slash_invocation(value: &str) -> Option<(String, String)> {
    raw_slash_invocation_parts(value).map(|(name, args)| (name.to_ascii_lowercase(), args))
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

    #[test]
    fn codex_local_controls_carry_the_normalized_name_and_the_raw_args() {
        assert_eq!(
            classify_codex_input("/model"),
            CodexInputClass::LocalControl {
                name: "/model".to_string(),
                args: String::new(),
            }
        );
        assert_eq!(
            classify_codex_input("/model gpt-5.6-codex"),
            CodexInputClass::LocalControl {
                name: "/model".to_string(),
                args: "gpt-5.6-codex".to_string(),
            }
        );
        assert_eq!(
            classify_codex_input("/help"),
            CodexInputClass::LocalControl {
                name: "/help".to_string(),
                args: String::new(),
            }
        );
    }

    #[test]
    fn absolute_path_prompts_are_never_taken_as_codex_commands() {
        for prompt in [
            "/Users/itismyfield/x.rs 읽어줘",
            "/tmp/a.log 봐줘",
            "/etc/hosts.bak 을 비교해줘",
            "/tmp 용량을 설명해줘",
            "/Users 목록을 설명해줘",
            "/users 목록",
            "/TMP 용량",
            "/Volumes 설명해줘",
            "//example.com 열어줘",
            "/foo/i 정규식을 설명해줘",
            "/tmp에 뭐가 있어?",
            "/Users에서 찾아줘",
            "/ETC를 봐줘",
            "/opt) 를 봐줘",
        ] {
            assert_eq!(
                classify_codex_input(prompt),
                CodexInputClass::ProviderPrompt,
                "{prompt}"
            );
        }
    }

    #[test]
    fn command_shaped_input_outside_the_registry_is_refused_not_forwarded() {
        let long_name = format!("/{}", "a".repeat(33));
        for raw in [
            "/frobnicate",
            "/모델",
            "/1status",
            "/data",
            "/optimize",
            "/model에",
            &long_name,
        ] {
            assert_eq!(
                classify_codex_input(raw),
                CodexInputClass::UnsupportedControl {
                    raw_name: raw.to_string(),
                },
                "{raw}"
            );
        }
    }

    #[test]
    fn quoted_mid_sentence_and_multiline_slashes_stay_provider_prompts() {
        for prompt in [
            "이 파일에서 /model 을 찾아줘",
            "\"/model\" 의미를 설명해줘",
            "./x 읽어줘",
            "../x 읽어줘",
            "~/x 읽어줘",
            "https://example.com 열어줘",
            "/model\n실제 요청",
            "/",
        ] {
            assert_eq!(
                classify_codex_input(prompt),
                CodexInputClass::ProviderPrompt,
                "{prompt}"
            );
        }
    }

    #[test]
    fn splitting_the_primitive_leaves_the_claude_lowercasing_contract_intact() {
        assert_eq!(
            raw_slash_invocation("/Model X"),
            Some(("/model".to_string(), "X".to_string()))
        );
        assert_eq!(
            raw_slash_invocation_parts("/Model X"),
            Some(("/Model".to_string(), "X".to_string()))
        );
        assert_eq!(raw_slash_invocation_parts("/"), None);
    }
}
