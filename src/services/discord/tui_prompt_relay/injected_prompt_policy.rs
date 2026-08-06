//! #3479 rank-5: pure injected-prompt classification + formatting policy.
//!
//! Behavior-preserving extraction of the self-contained classifier/parser
//! cluster from the `tui_prompt_relay` parent module. Every item here is pure
//! (no `shared.`/`http.`/async-IO coupling, no module-private static state);
//! the stateful dedupe/bridge helpers that drive these classifiers stay in the
//! parent. Items are `pub(super)` and re-imported by the parent via
//! `use self::injected_prompt_policy::{...}`, so call sites are unchanged.

use super::*;

// #3075: the terminal sanitizer is service-level so prompt observation can use
// it before Discord relay state exists; `tui_task_card` delegates to that same
// definition. The ASCII truncator remains task-card owned. The parent's glob
// (`use super::*`) does not re-export these `use`-imported names, so the child
// module imports them directly.
use super::super::response_sanitizer::subagent_notification_card;
use super::super::tui_task_card::{
    clamp_discord_message_content, strip_terminal_controls, truncate_chars_ascii as truncate_chars,
};

const LOCAL_COMMAND_STDOUT_OPEN: &str = "<local-command-stdout>";
const LOCAL_COMMAND_STDOUT_CLOSE: &str = "</local-command-stdout>";
const COMPACTED_LOCAL_COMMAND_STDOUT_PREFIX: &str = "<local-command-stdout>Compacted";

/// Classification of TUI-injected prompt text. Each class drives different
/// lifecycle handling: human turns get active-turn ownership, task/subagent
/// events and continuation banners stay passive, and slash-control echoes use
/// command-kind rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InjectedPromptClass {
    HumanTuiDirect,
    TaskNotificationEvent,
    SubagentNotificationEvent,
    SystemContinuation,
    SlashCommandControl,
}

impl InjectedPromptClass {
    /// Whether this class represents a human-driven active turn that should
    /// receive a `⏳` reaction and claim queue/inflight ownership.
    pub(super) fn is_human_active_turn(self) -> bool {
        matches!(self, InjectedPromptClass::HumanTuiDirect)
    }

    /// Neutral machine events suppress the active-turn lifecycle. Slash control
    /// echoes are not human turns, but keep the full synthetic lifecycle.
    pub(super) fn suppresses_user_turn_lifecycle(self) -> bool {
        matches!(
            self,
            InjectedPromptClass::SystemContinuation
                | InjectedPromptClass::TaskNotificationEvent
                | InjectedPromptClass::SubagentNotificationEvent
        )
    }

    /// Whether this injected class should keep the provider-output bridge tail.
    #[cfg(test)]
    pub(super) fn still_delivers_assistant_output(self) -> bool {
        !matches!(
            self,
            InjectedPromptClass::SystemContinuation
                | InjectedPromptClass::TaskNotificationEvent
                | InjectedPromptClass::SubagentNotificationEvent
        )
    }

    pub(super) fn is_subagent_notification_event(self) -> bool {
        matches!(self, InjectedPromptClass::SubagentNotificationEvent)
    }
}

/// #5188 (R3): the kind-aware active-turn lifecycle gate.
///
/// [`InjectedPromptClass::suppresses_user_turn_lifecycle`] is kind-blind, so
/// every `SlashCommandControl` took the full synthetic lifecycle (anchor + `⏳` +
/// inflight) even though its own Discord note says *"시스템 주입 (활성 턴 아님)"*.
/// For a SESSION-RESETTING control (`/clear`) that combination is not merely
/// cosmetic — it is the wedge:
///
/// * `/clear` makes Claude Code open a NEW transcript JSONL and freeze the
///   current one, and it emits no assistant output of its own;
/// * the inflight the lifecycle creates is pinned to the FROZEN transcript, so
///   nothing can ever deliver a terminal for it;
/// * every later turn on the channel then reads `FOREIGN prior inflight is still
///   live` → `ABORTING` → `KICKOFF: skipped queued turn`, permanently.
///
/// Two things disagreed — the note ("not an active turn") and the code (full
/// active turn). We resolve it in favour of the NOTE: a session-resetting
/// control creates no inflight at all, so there is nothing left to settle. The
/// user still gets the same "머신 슬래시 명령" note.
///
/// `/loop` and `/compact` are deliberately excluded: they DO produce assistant
/// output that must be relayed under a real turn (#3178), and `/compact` rewrites
/// the SAME transcript path rather than rotating away from it (#4549/#4841).
///
/// ## Load-bearing precondition: this gate only sees ENVELOPED slash commands
/// The gate fires on [`InjectedPromptClass::SlashCommandControl`], and
/// [`is_slash_command_control_prompt`] recognises `/clear` **only** inside a
/// `<command-name>/clear</command-name>` envelope (or the other machine-echo
/// forms it lists). A BARE `/clear` line classifies as
/// [`InjectedPromptClass::HumanTuiDirect`], so this gate never runs for it and
/// the pre-#5188 lifecycle — including the inflight pinned to the transcript
/// `/clear` is about to freeze — is created exactly as before.
///
/// Note where the dependency actually is: `slash_command_control_kind` returns
/// `/clear` for the bare line too, so the `slash_command_kind` argument is NOT
/// what discriminates. Only the class is.
///
/// That is safe today because the production evidence is the enveloped machine
/// form (the Discord note reads "머신 슬래시 명령", which only that path emits),
/// and it is pinned by `a_bare_clear_without_the_command_envelope_bypasses_the_gate`
/// below. It is nevertheless a coupling to the OBSERVED SHAPE of the prompt
/// rather than to its meaning: if a future input path ever delivers a bare
/// `/clear` — a raw pane write, a differently-wrapped injector — the wedge comes
/// back silently, and the fix belongs in the classifier, not here.
pub(super) fn injected_prompt_suppresses_user_turn_lifecycle(
    injected_class: InjectedPromptClass,
    slash_command_kind: Option<&str>,
) -> bool {
    if injected_class.suppresses_user_turn_lifecycle() {
        return true;
    }
    matches!(injected_class, InjectedPromptClass::SlashCommandControl)
        && slash_command_kind.is_some_and(
            crate::services::tui_prompt_control::is_session_resetting_slash_command_kind,
        )
}

/// Pure classifier for injected TUI prompt text. Order is load-bearing:
/// start-anchored subagent envelopes win before generic continuation banners
/// because Provider Session Reuse may wrap the machine-event tail.
pub(super) fn classify_injected_prompt(prompt: &str) -> InjectedPromptClass {
    if is_start_anchored_subagent_notification(prompt) {
        InjectedPromptClass::SubagentNotificationEvent
    } else if is_system_continuation_prompt(prompt) {
        InjectedPromptClass::SystemContinuation
    } else if is_task_notification_prompt(prompt) {
        InjectedPromptClass::TaskNotificationEvent
    } else if is_slash_command_control_prompt(prompt) {
        InjectedPromptClass::SlashCommandControl
    } else {
        InjectedPromptClass::HumanTuiDirect
    }
}

/// Detects machine slash-control echoes, start-anchored after terminal controls,
/// SSH-direct wrapper, and one complete leading local-command caveat.
pub(super) fn is_slash_command_control_prompt(prompt: &str) -> bool {
    let (normalized, peeled_caveat) = normalize_slash_command_control_prompt(prompt);
    if peeled_caveat && normalized.is_empty() {
        return true;
    }
    if normalized.starts_with("<command-message>")
        || normalized.starts_with("<command-name>")
        || starts_with_compacted_local_command_stdout(&normalized)
        || starts_with_complete_local_command_stdout(&normalized)
        || normalized.starts_with("/loop ")
    {
        return true;
    }
    // Raw `/compact` echo: match the whole slash-token only — the next char must
    // be whitespace or end-of-string, so neither an embedded quote nor
    // "/compactfoo" trips it. The bare no-arg "/compact" (EOS) is allowed.
    if let Some(rest) = normalized.strip_prefix("/compact") {
        return rest.is_empty() || rest.starts_with(char::is_whitespace);
    }
    false
}

fn starts_with_complete_local_command_stdout(normalized: &str) -> bool {
    // Open-only generic stdout falls back to HumanTuiDirect; the line scanner rolls back and retries.
    normalized
        .strip_prefix(LOCAL_COMMAND_STDOUT_OPEN)
        .is_some_and(|rest| rest.trim_end().ends_with(LOCAL_COMMAND_STDOUT_CLOSE))
}

fn starts_with_compacted_local_command_stdout(normalized: &str) -> bool {
    if !normalized.starts_with(COMPACTED_LOCAL_COMMAND_STDOUT_PREFIX) {
        return false;
    }
    let trimmed = normalized.trim_end();
    if trimmed.contains(LOCAL_COMMAND_STDOUT_CLOSE) {
        return trimmed.ends_with(LOCAL_COMMAND_STDOUT_CLOSE);
    }
    !trimmed.contains('\r') && !trimmed.contains('\n')
}

pub(super) fn slash_command_control_prompt_is_local_command_stdout(prompt: &str) -> bool {
    let (normalized, _peeled_caveat) = normalize_slash_command_control_prompt(prompt);
    starts_with_complete_local_command_stdout(&normalized)
}

pub(super) fn normalize_slash_command_control_prompt(prompt: &str) -> (String, bool) {
    let normalized = strip_terminal_controls(prompt);
    let normalized = normalized.trim_start();
    let normalized = strip_leading_injection_wrapper(normalized);
    let normalized = normalized.trim_start();
    let (normalized, peeled_caveat) = strip_leading_local_command_caveat(normalized);
    (normalized.trim_start().to_string(), peeled_caveat)
}

pub(super) fn strip_leading_local_command_caveat(text: &str) -> (&str, bool) {
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

/// Detects the `<task-notification>` auto-turn tag injected by Claude Code /
/// Codex when a background task reaches a terminal state. Start-anchored after
/// the same normalization used by the terminal bridge, so a human prompt that
/// quotes the tag mid-body remains a normal direct prompt (#3730).
pub(super) fn is_task_notification_prompt(prompt: &str) -> bool {
    is_start_anchored_task_notification(prompt)
}

/// #3393 finding 2: START-ANCHORED gate for the live-panel terminal BRIDGE only.
/// A REAL machine `<task-notification>` user-record begins with the tag after the
/// shared normalization pipeline (strip_terminal_controls → trim →
/// strip_leading_injection_wrapper → trim, mirroring #3100/#3388). A human direct
/// prompt that merely QUOTES a notification mid-message must NOT push terminal
/// StatusEvents — combined with finding 1's id requirement this closes the
/// false-close attack where a quoted live tool-use-id would otherwise finalize a
/// real running slot.
pub(super) fn is_start_anchored_task_notification(prompt: &str) -> bool {
    let normalized = normalized_start_anchored_injection(prompt);
    starts_with_xmlish_tag(&normalized, "task-notification")
}

/// Detects Codex `<subagent_notification>` envelopes as neutral machine events.
/// This is deliberately START-ANCHORED: a human prompt that quotes the XML-ish
/// tag mid-body remains a normal TUI-direct prompt.
pub(super) fn is_start_anchored_subagent_notification(prompt: &str) -> bool {
    subagent_notification_card::is_start_anchored_subagent_notification(prompt)
}

fn starts_with_xmlish_tag(text: &str, tag: &str) -> bool {
    let Some(rest) = text.strip_prefix('<') else {
        return false;
    };
    let Some(rest) = rest.strip_prefix(tag) else {
        return false;
    };
    rest.starts_with('>') || rest.chars().next().is_some_and(char::is_whitespace)
}

fn normalized_start_anchored_injection(prompt: &str) -> String {
    let normalized = strip_terminal_controls(prompt);
    let normalized = normalized.trim_start();
    let normalized = strip_leading_injection_wrapper(normalized);
    normalized.trim_start().to_string()
}

/// Detects start-anchored compact/session-continuation banners.
pub(super) fn is_system_continuation_prompt(prompt: &str) -> bool {
    let normalized = strip_terminal_controls(prompt);
    let normalized = normalized.trim_start();
    let normalized = strip_leading_injection_wrapper(normalized);
    let normalized = normalized.trim_start();
    const SYSTEM_CONTINUATION_OPENINGS: &[&str] = &[
        "This session is being continued from a previous conversation",
        "Please continue the conversation from where we left it off",
    ];
    SYSTEM_CONTINUATION_OPENINGS
        .iter()
        .any(|opening| normalized.starts_with(opening))
        || is_provider_session_reuse_marker(normalized)
}

fn is_provider_session_reuse_marker(normalized: &str) -> bool {
    const RESUMED_THREAD_PROLOGUE: &str = "The prior authoritative Discord, role, and tool \
         instructions already present in this Codex thread still apply. Treat only this turn's \
         user request, reply context, uploaded files, and memory recall below as new actionable \
         input.";
    const FRESH_FORK_PROLOGUE: &str = "The prior authoritative Discord, role, and tool \
         instructions already issued to this role in the current dcserver lifetime still apply. \
         Treat only this turn's user request, reply context, uploaded files, and memory recall \
         below as new actionable input.";

    let Some(rest) = normalized.strip_prefix("[Provider Session Reuse]") else {
        return false;
    };
    let rest = rest.trim_start();
    provider_reuse_prologue_has_prompt_tail(rest, RESUMED_THREAD_PROLOGUE)
        || provider_reuse_prologue_has_prompt_tail(rest, FRESH_FORK_PROLOGUE)
}

fn provider_reuse_prologue_has_prompt_tail(rest: &str, prologue: &str) -> bool {
    rest.strip_prefix(prologue)
        .is_some_and(|tail| tail.starts_with("\n\n"))
}

/// Removes a leading SSH-direct wrapper line/fence; mid-body quotes are untouched.
pub(super) fn strip_leading_injection_wrapper(text: &str) -> &str {
    crate::services::tui_prompt_control::strip_leading_injection_wrapper(text)
}

pub(super) fn format_ssh_direct_prompt_notification(
    _provider: &str,
    tmux_session_name: &str,
    prompt: &str,
) -> String {
    let prompt = strip_terminal_controls(prompt);
    let preview =
        truncate_chars(prompt.trim(), SSH_DIRECT_PROMPT_PREVIEW_LIMIT).replace("```", "` ` `");
    // #4032: clamp at the source — the card branch clamps internally; this
    // SSH-direct formatter and the slash-control note below are the remaining
    // producers feeding `say`, each clamped at its own source.
    clamp_discord_message_content(&format!(
        "터미널에 직접 주입된 입력 (tmux : `{}`):\n```text\n{}\n```",
        sanitize_inline_code(tmux_session_name),
        preview,
    ))
}

pub(super) fn format_subagent_notification_card(tmux_session_name: &str, prompt: &str) -> String {
    subagent_notification_card::format_subagent_notification_card(Some(tmux_session_name), prompt)
}

/// Canonical command kind for slash-control dedupe and kind-only notes.
pub(super) fn slash_command_control_kind(prompt: &str) -> String {
    let (normalized, _peeled_caveat) = normalize_slash_command_control_prompt(prompt);
    if starts_with_compacted_local_command_stdout(&normalized) {
        return "/compact".to_string();
    }
    if starts_with_complete_local_command_stdout(&normalized) {
        return "local-command-stdout".to_string();
    }
    if let Some(name) = first_xml_tag_token(&normalized, "command-name") {
        return name;
    }
    if let Some(name) = first_xml_tag_token(&normalized, "command-message") {
        return name;
    }
    if normalized.starts_with("/loop ") || normalized.starts_with("/loop\t") {
        return "/loop".to_string();
    }
    if let Some(rest) = normalized.strip_prefix("/compact") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return "/compact".to_string();
        }
    }
    if normalized.starts_with('/') {
        let name = normalized.split_whitespace().next().unwrap_or("");
        if name.len() > 1 {
            return name.to_string();
        }
    }
    "slash".to_string()
}

fn first_xml_tag_token(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let after = text.split_once(&open)?.1;
    let (body, _) = after.split_once(&close)?;
    let token = body.split_whitespace().next()?.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Slash-control note; `/loop` and generic local stdout may include a short body.
pub(super) fn format_slash_command_control_note(
    tmux_session_name: &str,
    kind: &str,
    raw_prompt: &str,
) -> String {
    // #4032: clamp at the source like the SSH-direct formatter — this note is
    // the third producer feeding the active-turn `say()` and the session name
    // segment is otherwise unbounded.
    clamp_discord_message_content(&format_slash_command_control_note_unclamped(
        tmux_session_name,
        kind,
        raw_prompt,
    ))
}

fn format_slash_command_control_note_unclamped(
    tmux_session_name: &str,
    kind: &str,
    raw_prompt: &str,
) -> String {
    let label = match kind {
        "/loop" => "🔁 자동 점검(/loop)",
        "/compact" => "🧹 컨텍스트 정리(/compact)",
        _ => "⚙️ 머신 슬래시 명령",
    };
    let header = format!(
        "{} (tmux : `{}`) — 시스템 주입 (활성 턴 아님)",
        label,
        sanitize_inline_code(tmux_session_name),
    );
    if kind == "/loop" {
        if let Some(body) = extract_loop_body(raw_prompt) {
            let preview = truncate_chars(body.trim(), SSH_DIRECT_PROMPT_PREVIEW_LIMIT)
                .replace("```", "` ` `");
            if !preview.is_empty() {
                return format!("{header}:\n```text\n{preview}\n```");
            }
        }
    }
    if kind == "local-command-stdout" {
        if let Some(body) = extract_local_command_stdout_body(raw_prompt) {
            let preview = truncate_chars(body.trim(), SSH_DIRECT_PROMPT_PREVIEW_LIMIT)
                .replace("```", "` ` `");
            if !preview.is_empty() {
                return format!("{header}:\n```text\n{preview}\n```");
            }
        }
    }
    header
}

pub(super) fn slash_command_control_prompt_is_caveat_only(prompt: &str) -> bool {
    let (normalized, peeled_caveat) = normalize_slash_command_control_prompt(prompt);
    peeled_caveat && normalized.is_empty()
}

/// Pull the human-facing `/loop` directive body from raw echo or command args.
pub(super) fn extract_loop_body(prompt: &str) -> Option<String> {
    let normalized = strip_terminal_controls(prompt);
    let normalized = normalized.trim_start();
    let normalized = strip_leading_injection_wrapper(normalized);
    let normalized = normalized.trim_start();
    if let Some(start) = normalized.find("<command-args>") {
        let after = &normalized[start + "<command-args>".len()..];
        if let Some((body, _rest)) = after.split_once("</command-args>") {
            let body = body.trim();
            if !body.is_empty() {
                return Some(body.to_string());
            }
        }
    }
    for prefix in ["/loop ", "/loop\t"] {
        if let Some(rest) = normalized.strip_prefix(prefix) {
            let body = rest.trim();
            if !body.is_empty() {
                return Some(body.to_string());
            }
        }
    }
    None
}

fn extract_local_command_stdout_body(prompt: &str) -> Option<String> {
    let (normalized, _peeled_caveat) = normalize_slash_command_control_prompt(prompt);
    let rest = normalized.strip_prefix(LOCAL_COMMAND_STDOUT_OPEN)?;
    let rest = rest.trim_end();
    if !rest.ends_with(LOCAL_COMMAND_STDOUT_CLOSE) {
        return None;
    }
    let body = &rest[..rest.len() - LOCAL_COMMAND_STDOUT_CLOSE.len()];
    let body = body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!body.is_empty()).then_some(body)
}

pub(super) fn format_system_continuation_note(tmux_session_name: &str, _prompt: &str) -> String {
    format!(
        "🧩 Session continued (compact/resume) · tmux: `{}`",
        sanitize_inline_code(tmux_session_name),
    )
}

pub(super) fn format_count_with_commas(count: usize) -> String {
    let digits = count.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (idx, ch) in digits.chars().enumerate() {
        if idx > 0 && (digits.len() - idx) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub(super) fn sanitize_inline_code(value: &str) -> String {
    value.replace('`', "'")
}

#[cfg(test)]
mod session_resetting_lifecycle_tests {
    use super::*;

    const CLEAR_ECHO: &str = "<command-name>/clear</command-name>";

    // #5188 (R3): the note and the code used to disagree — the note said "활성 턴
    // 아님" while the code ran the full active-turn lifecycle and left an inflight
    // pinned to the transcript `/clear` was about to freeze. Resolved in favour of
    // the note: no inflight is created, so there is nothing left to settle.
    #[test]
    fn clear_suppresses_the_active_turn_lifecycle_it_already_says_it_is_not() {
        let class = classify_injected_prompt(CLEAR_ECHO);
        assert_eq!(
            class,
            InjectedPromptClass::SlashCommandControl,
            "`/clear` is still a machine slash control"
        );
        assert_eq!(
            slash_command_control_kind(CLEAR_ECHO),
            "/clear",
            "the lifecycle gate keys off this kind"
        );
        assert!(
            !class.suppresses_user_turn_lifecycle(),
            "the kind-blind predicate still says 'full active turn' — which is the \
             bug; only the kind-aware gate may override it"
        );
        assert!(
            injected_prompt_suppresses_user_turn_lifecycle(class, Some("/clear")),
            "`/clear` rotates the session, so an inflight created for it is pinned \
             to a transcript that can never grow again and would wedge the channel"
        );
        // The user-facing note is unchanged: the fix makes the code agree with it,
        // it does not remove it.
        assert!(
            format_slash_command_control_note("AgentDesk-claude-adk-cc", "/clear", CLEAR_ECHO)
                .contains("활성 턴 아님"),
            "the `/clear` note must still render as a system injection"
        );
    }

    /// R7. Pins the precondition the whole gate rests on, so the coupling is a
    /// stated fact rather than an assumption nobody wrote down.
    ///
    /// The bypass is in the CLASSIFIER, not in the kind extractor.
    /// `slash_command_control_kind` happily reports `/clear` for a bare line, so
    /// inspecting the kind suggests the gate applies — but the gate is reached
    /// only through `SlashCommandControl`, and
    /// [`is_slash_command_control_prompt`] grants that class to `/clear` ONLY
    /// inside a `<command-name>` envelope. A bare `/clear` is `HumanTuiDirect`,
    /// so the gate does not run and the pre-#5188 lifecycle (inflight included)
    /// is created exactly as before — i.e. the wedge this issue fixed would be
    /// back.
    ///
    /// Production emits the enveloped form, so this is currently correct, not a
    /// live hole. It is asserted here so that a change to the input shape fails
    /// a test instead of silently re-opening the defect.
    #[test]
    fn a_bare_clear_without_the_command_envelope_bypasses_the_gate() {
        let bare = classify_injected_prompt("/clear");
        assert_eq!(
            bare,
            InjectedPromptClass::HumanTuiDirect,
            "an un-enveloped `/clear` is not recognised as a machine slash control"
        );
        assert_eq!(
            slash_command_control_kind("/clear"),
            "/clear",
            "the KIND extractor is envelope-independent — so the bypass is not \
             here, and reading the kind alone would suggest the gate applies"
        );
        assert!(
            !injected_prompt_suppresses_user_turn_lifecycle(bare, Some("/clear")),
            "even handed the kind explicitly, a HumanTuiDirect prompt keeps its \
             full lifecycle — so a bare `/clear` would still create the inflight \
             that #5188 removed. The gate protects the ENVELOPED form only"
        );

        // ...and the enveloped form, which is what production actually sends, is
        // the one that gets the protection.
        assert!(
            injected_prompt_suppresses_user_turn_lifecycle(
                classify_injected_prompt(CLEAR_ECHO),
                Some("/clear"),
            ),
            "the enveloped machine echo is the shape this gate is written for"
        );
    }

    // The blast radius must stop at session-resetting controls. `/loop` and
    // `/compact` DO produce assistant output that has to be relayed under a real
    // turn (#3178), and `/compact` rewrites the SAME transcript path rather than
    // rotating away from it (#4549/#4841).
    #[test]
    fn output_producing_slash_controls_keep_their_active_turn() {
        for (prompt, kind) in [
            ("/loop check the deploy", "/loop"),
            ("/compact", "/compact"),
            ("<command-name>/model</command-name>", "/model"),
        ] {
            let class = classify_injected_prompt(prompt);
            assert_eq!(
                class,
                InjectedPromptClass::SlashCommandControl,
                "{prompt} classification changed"
            );
            assert!(
                !injected_prompt_suppresses_user_turn_lifecycle(class, Some(kind)),
                "{kind} still owns a full active turn; suppressing it would drop its \
                 assistant output on the floor"
            );
        }
    }

    #[test]
    fn human_and_passive_classes_are_unaffected_by_the_kind_aware_gate() {
        let human = classify_injected_prompt("please fix the build");
        assert!(!injected_prompt_suppresses_user_turn_lifecycle(human, None));
        assert!(
            !injected_prompt_suppresses_user_turn_lifecycle(human, Some("/clear")),
            "a human prompt must never be suppressed just because a kind was passed"
        );

        let continuation = classify_injected_prompt(
            "This session is being continued from a previous conversation",
        );
        assert!(
            injected_prompt_suppresses_user_turn_lifecycle(continuation, None),
            "pre-existing passive suppression is preserved"
        );
    }
}
