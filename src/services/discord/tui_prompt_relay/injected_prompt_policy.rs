//! #3479 rank-5: pure injected-prompt classification + formatting policy.
//!
//! Behavior-preserving extraction of the self-contained classifier/parser
//! cluster from the `tui_prompt_relay` parent module. Every item here is pure
//! (no `shared.`/`http.`/async-IO coupling, no module-private static state);
//! the stateful dedupe/bridge helpers that drive these classifiers stay in the
//! parent. Items are `pub(super)` and re-imported by the parent via
//! `use self::injected_prompt_policy::{...}`, so call sites are unchanged.

use super::*;

// #3075: `strip_terminal_controls` and the ASCII `truncate_chars` are shared
// with the task-card renderer; the single definitions live in `tui_task_card`
// so the classifier, formatters, and card parser stay in sync. The parent's
// glob (`use super::*`) does not re-export these `use`-imported names, so the
// child module imports them directly to keep the moved bodies byte-identical.
use serde::Deserialize;

use super::super::tui_task_card::{
    strip_terminal_controls, truncate_chars_ascii as truncate_chars,
};

const SUBAGENT_NOTIFICATION_PREVIEW_CHARS: usize = 900;
const SUBAGENT_NOTIFICATION_PREVIEW_LINES: usize = 8;

/// Classification of TUI-injected prompt text. Each class drives different
/// lifecycle handling: human/task turns get active-turn ownership, continuation
/// banners stay passive, and slash-control echoes use command-kind rendering.
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
                | InjectedPromptClass::SubagentNotificationEvent
        )
    }

    /// Whether this injected class should keep the provider-output bridge tail.
    #[cfg(test)]
    pub(super) fn still_delivers_assistant_output(self) -> bool {
        !matches!(self, InjectedPromptClass::SubagentNotificationEvent)
    }

    pub(super) fn is_subagent_notification_event(self) -> bool {
        matches!(self, InjectedPromptClass::SubagentNotificationEvent)
    }
}

/// Pure classifier for injected TUI prompt text. Order is load-bearing:
/// continuation banners win before machine notifications and slash-control echoes.
pub(super) fn classify_injected_prompt(prompt: &str) -> InjectedPromptClass {
    if is_system_continuation_prompt(prompt) {
        InjectedPromptClass::SystemContinuation
    } else if is_start_anchored_subagent_notification(prompt) {
        InjectedPromptClass::SubagentNotificationEvent
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
        || normalized.starts_with("<local-command-stdout>Compacted")
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
    let normalized = normalized_start_anchored_injection(prompt);
    starts_with_xmlish_tag(&normalized, "subagent_notification")
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
            return &rest[idx + 1..];
        }
        return after_wrapper_line;
    }
    after_wrapper_line
}

pub(super) fn format_ssh_direct_prompt_notification(
    _provider: &str,
    tmux_session_name: &str,
    prompt: &str,
) -> String {
    let prompt = strip_terminal_controls(prompt);
    let preview =
        truncate_chars(prompt.trim(), SSH_DIRECT_PROMPT_PREVIEW_LIMIT).replace("```", "` ` `");
    format!(
        "터미널에 직접 주입된 입력 (tmux : `{}`):\n```text\n{}\n```",
        sanitize_inline_code(tmux_session_name),
        preview,
    )
}

#[derive(Debug, Deserialize)]
struct SubagentNotificationEnvelope {
    status: Option<SubagentNotificationStatus>,
}

#[derive(Debug, Deserialize)]
struct SubagentNotificationStatus {
    completed: Option<String>,
    failed: Option<String>,
}

pub(super) fn format_subagent_notification_card(tmux_session_name: &str, prompt: &str) -> String {
    match parse_subagent_notification(prompt) {
        Ok(SubagentNotificationRender::Completed(report)) => {
            format_subagent_notification_report(tmux_session_name, "✅", "completed", &report)
        }
        Ok(SubagentNotificationRender::Failed(report)) => {
            format_subagent_notification_report(tmux_session_name, "❌", "failed", &report)
        }
        Ok(SubagentNotificationRender::Unknown) => format!(
            "📋 Subagent notification (tmux : `{}`) — no completed/failed status",
            sanitize_inline_code(tmux_session_name),
        ),
        Err(_) => format!(
            "⚠️ Subagent notification (tmux : `{}`) — malformed payload omitted",
            sanitize_inline_code(tmux_session_name),
        ),
    }
}

enum SubagentNotificationRender {
    Completed(String),
    Failed(String),
    Unknown,
}

fn parse_subagent_notification(prompt: &str) -> Result<SubagentNotificationRender, ()> {
    let payload = extract_subagent_notification_payload(prompt)?;
    let envelope: SubagentNotificationEnvelope = serde_json::from_str(&payload).map_err(|_| ())?;
    let status = envelope.status.ok_or(())?;
    if let Some(report) = status.completed.filter(|report| !report.trim().is_empty()) {
        return Ok(SubagentNotificationRender::Completed(report));
    }
    if let Some(report) = status.failed.filter(|report| !report.trim().is_empty()) {
        return Ok(SubagentNotificationRender::Failed(report));
    }
    Ok(SubagentNotificationRender::Unknown)
}

fn extract_subagent_notification_payload(prompt: &str) -> Result<String, ()> {
    const OPEN: &str = "<subagent_notification";
    const CLOSE: &str = "</subagent_notification>";

    let normalized = normalized_start_anchored_injection(prompt);
    if !normalized.starts_with(OPEN) {
        return Err(());
    }
    let after_open_name = &normalized[OPEN.len()..];
    let Some(first) = after_open_name.chars().next() else {
        return Err(());
    };
    if first != '>' && !first.is_whitespace() {
        return Err(());
    }
    let open_end = normalized.find('>').ok_or(())? + 1;
    let after_open = &normalized[open_end..];
    let close_start = after_open.find(CLOSE).ok_or(())?;
    Ok(after_open[..close_start].trim().to_string())
}

fn format_subagent_notification_report(
    tmux_session_name: &str,
    icon: &str,
    status: &str,
    report: &str,
) -> String {
    let preview = subagent_report_preview(report);
    let mut lines = vec![format!(
        "{icon} Subagent {status} (tmux : `{}`)",
        sanitize_inline_code(tmux_session_name),
    )];
    if !preview.is_empty() {
        lines.push(String::new());
        lines.push(preview);
    }
    lines.join("\n")
}

fn subagent_report_preview(report: &str) -> String {
    let report = strip_terminal_controls(report);
    let mut collected: Vec<String> = Vec::new();
    for line in report.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        collected.push(sanitize_subagent_report_line(line));
        if collected.len() >= SUBAGENT_NOTIFICATION_PREVIEW_LINES {
            break;
        }
    }
    truncate_chars(&collected.join("\n"), SUBAGENT_NOTIFICATION_PREVIEW_CHARS)
}

fn sanitize_subagent_report_line(value: &str) -> String {
    value
        .replace('\r', " ")
        .replace('\n', " ")
        .replace("```", "` ` `")
}

/// Canonical command kind for slash-control dedupe and kind-only notes.
pub(super) fn slash_command_control_kind(prompt: &str) -> String {
    let (normalized, _peeled_caveat) = normalize_slash_command_control_prompt(prompt);
    if let Some(after) = normalized
        .find("<command-name>")
        .map(|idx| &normalized[idx + "<command-name>".len()..])
    {
        let name = after.split('<').next().unwrap_or("").trim();
        let name = name.split_whitespace().next().unwrap_or("");
        if !name.is_empty() {
            return name.to_string();
        }
    }
    if normalized.starts_with("/loop ") || normalized.starts_with("/loop\t") {
        return "/loop".to_string();
    }
    if let Some(rest) = normalized.strip_prefix("/compact") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return "/compact".to_string();
        }
    }
    if normalized.starts_with("<local-command-stdout>Compacted") {
        return "/compact".to_string();
    }
    if normalized.starts_with('/') {
        let name = normalized.split_whitespace().next().unwrap_or("");
        if name.len() > 1 {
            return name.to_string();
        }
    }
    "slash".to_string()
}

/// Kind-only slash-control note; `/loop` may include its directive body.
pub(super) fn format_slash_command_control_note(
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

pub(super) fn should_suppress_local_only_kind_note_after_continuation(
    kind: &str,
    last_continuation_at: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    if !matches!(kind, "/compact" | "slash") {
        return false;
    }
    last_continuation_at.is_some_and(|rendered_at| {
        now.checked_duration_since(rendered_at)
            .is_none_or(|age| age < COMPACT_REPLAY_KIND_NOTE_SUPPRESSION_WINDOW)
    })
}

pub(super) fn format_system_continuation_note(tmux_session_name: &str, prompt: &str) -> String {
    let prompt = strip_terminal_controls(prompt);
    let omitted_chars = format_count_with_commas(prompt.trim().chars().count());
    format!(
        "🧩 세션 컨텍스트 이어가기 (tmux : `{}`) — 시스템 주입 (활성 턴 아님) (요약 {}자 생략 — 채널 기록과 동일 내용)",
        sanitize_inline_code(tmux_session_name),
        omitted_chars,
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
