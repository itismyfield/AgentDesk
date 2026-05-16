//! Codex TUI input handling: prompt delivery + readiness detection.
//!
//! Issue: #2171 — Implement Codex TUI input readiness detector.
//!
//! ## Why a Codex-specific module?
//!
//! The legacy hosting paths reuse `claude_tui::input` markers
//! (`Ready for input (type message + Enter)` banner and the lone `❯`
//! glyph) to decide when a tmux-hosted TUI is ready to accept a new
//! prompt. Codex TUI does not draw either of those — its bottom
//! composer is a rounded input box with the cursor block (`▌`) inside
//! the box, framed by Unicode box-drawing edges and surrounded by
//! footer hint lines (`Esc to interrupt`, `Ctrl+J newline`, etc.).
//! Re-using the Claude marker produces false negatives (we never see
//! `❯`, so we never inject) and false positives (model output may
//! contain a `❯` glyph and trip the detector mid-turn).
//!
//! ## Signal source (priority order)
//!
//! The detector combines two complementary signals:
//!
//! 1. **Pane capture markers (primary).** The Codex TUI input
//!    composer is recognisable by its bottom-of-pane box-drawing
//!    border AND a footer hint line containing `Esc to interrupt`,
//!    `Ctrl+J newline`, or similar. We scan the most recent
//!    [`PROMPT_READY_SCAN_LINES`] non-empty lines. Both halves must
//!    be present so prompt-like glyphs *inside* assistant output
//!    cannot be mistaken for readiness.
//!
//! 2. **Live pane (gate).** A dead pane cannot be ready; we fail
//!    fast with a structured error instead of waiting out the full
//!    timeout, so the caller can decide to recreate the session.
//!
//! A rollout-event-driven signal (turn-complete from
//! `codex_tui::rollout_tail`) was considered as a third source and
//! deliberately **not** added here. The rollout terminal event tells
//! the bridge that the *turn* finished, but the TUI may still be
//! repainting its composer frame for ~one tick after. Today the
//! caller already gates on the rollout `Done` (via `RuntimeReady`
//! handoff in `execute_streaming_local_tui_tmux`) and only then asks
//! this module whether the pane is *visually* ready. Folding the
//! rollout event into this module would couple TUI input to rollout
//! plumbing and duplicate work. If a future PR proves the pane
//! marker is too flaky (e.g. across Codex CLI versions that change
//! the footer copy), add a rollout-event channel as signal #1 and
//! demote the pane scan to corroboration — see the follow-up note
//! in `codex_tui::rollout_tail::tail_rollout_file_until_assistant_response`.
//!
//! ## Timeout / fail-safe
//!
//! Fresh launches get a longer budget than follow-ups, matching the
//! Claude TUI split. The timeout returns a structured error prefixed
//! with [`PROMPT_READY_TIMEOUT_ERROR_PREFIX`] so callers can decide
//! whether to recreate the session or surface a user-visible error
//! — same contract as `claude_tui::input::is_prompt_ready_timeout_error`.
//! Combined with the Codex TUI cancel boundary (PR #2284), a hung TUI
//! has two independent escape hatches: this readiness timeout (caller
//! recreates) and the rollout deadline (caller emits `Done`).

use std::process::Output;
use std::time::{Duration, Instant};

const DEFAULT_LITERAL_CHUNK_CHARS: usize = 1800;
const PROMPT_READY_CAPTURE_SCROLLBACK: i32 = -80;
const PROMPT_READY_DEBUG_TAIL_LINES: usize = 24;
const PROMPT_READY_DEBUG_TAIL_BYTES: usize = 4096;
/// Number of trailing non-empty lines we scan for the composer box +
/// footer hint pair. Kept tight so prompt-like glyphs deep in assistant
/// output cannot be re-interpreted as a fresh composer.
const PROMPT_READY_SCAN_LINES: usize = 14;

pub const FRESH_PROMPT_READY_TIMEOUT: Duration = Duration::from_secs(120);
pub const FOLLOWUP_PROMPT_READY_TIMEOUT: Duration = Duration::from_secs(45);
const PROMPT_READY_TIMEOUT_ERROR_PREFIX: &str = "timeout waiting for codex tui";
const PROMPT_READY_SESSION_DEAD_ERROR: &str =
    "codex tui session died before prompt input was ready";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptReadinessKind {
    FreshTurn,
    Followup,
}

impl PromptReadinessKind {
    fn timeout(self) -> Duration {
        match self {
            Self::FreshTurn => FRESH_PROMPT_READY_TIMEOUT,
            Self::Followup => FOLLOWUP_PROMPT_READY_TIMEOUT,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::FreshTurn => "fresh",
            Self::Followup => "follow-up",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptReadinessSnapshot {
    pub composer_marker_detected: bool,
    pub tmux_pane_alive: bool,
    pub capture_available: bool,
    pub pane_tail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TuiInputAction {
    Literal(String),
    PasteBuffer(String),
    Enter,
    Escape,
}

/// Plan the sequence of tmux input actions required to deliver `prompt`
/// to a Codex TUI composer. Multiline prompts use a paste buffer so
/// embedded newlines do not get interpreted as `Enter` submissions.
pub fn plan_prompt_submit(prompt: &str) -> Result<Vec<TuiInputAction>, String> {
    let normalized_prompt;
    let prompt = if prompt.contains('\r') {
        normalized_prompt = prompt.replace("\r\n", "\n").replace('\r', "\n");
        normalized_prompt.as_str()
    } else {
        prompt
    };
    validate_prompt_text(prompt)?;
    validate_prompt_not_empty(prompt)?;
    let mut actions = if prompt.contains('\n') {
        vec![TuiInputAction::PasteBuffer(prompt.to_string())]
    } else {
        split_literal_chunks(prompt, DEFAULT_LITERAL_CHUNK_CHARS)
            .into_iter()
            .map(TuiInputAction::Literal)
            .collect::<Vec<_>>()
    };
    actions.push(TuiInputAction::Enter);
    Ok(actions)
}

pub fn plan_cancel() -> Vec<TuiInputAction> {
    vec![TuiInputAction::Escape]
}

/// Inject a fresh-turn prompt: waits up to `FRESH_PROMPT_READY_TIMEOUT`
/// for the composer to appear before sending.
pub fn send_fresh_prompt(session_name: &str, prompt: &str) -> Result<(), String> {
    send_prompt_with_readiness(session_name, prompt, PromptReadinessKind::FreshTurn)
}

/// Inject a follow-up prompt: waits up to `FOLLOWUP_PROMPT_READY_TIMEOUT`
/// for the composer to redraw after the previous turn before sending.
pub fn send_followup_prompt(session_name: &str, prompt: &str) -> Result<(), String> {
    send_prompt_with_readiness(session_name, prompt, PromptReadinessKind::Followup)
}

pub fn is_prompt_ready_timeout_error(error: &str) -> bool {
    error.starts_with(PROMPT_READY_TIMEOUT_ERROR_PREFIX)
}

pub fn is_session_dead_error(error: &str) -> bool {
    error == PROMPT_READY_SESSION_DEAD_ERROR
}

/// Capture the current pane and classify whether the Codex composer
/// is visible. Returned regardless of timing so callers can log the
/// state at decision points.
pub fn prompt_readiness_snapshot(session_name: &str) -> PromptReadinessSnapshot {
    let pane = crate::services::platform::tmux::capture_pane(
        session_name,
        PROMPT_READY_CAPTURE_SCROLLBACK,
    );
    let composer_marker_detected = pane
        .as_deref()
        .is_some_and(pane_looks_ready_for_codex_prompt);
    let pane_tail = pane
        .as_deref()
        .map(prompt_ready_debug_tail)
        .unwrap_or_else(|| "<capture unavailable>".to_string());
    PromptReadinessSnapshot {
        composer_marker_detected,
        tmux_pane_alive: crate::services::tmux_diagnostics::tmux_session_has_live_pane(
            session_name,
        ),
        capture_available: pane.is_some(),
        pane_tail,
    }
}

/// Block until the Codex TUI composer is visible or `timeout` elapses.
/// Returns `Ok(())` on success, a session-dead error if the tmux pane
/// disappears, or a timeout error prefixed with
/// [`PROMPT_READY_TIMEOUT_ERROR_PREFIX`].
pub fn wait_until_codex_tui_input_ready(
    session_name: &str,
    readiness: PromptReadinessKind,
) -> Result<(), String> {
    let timeout = readiness.timeout();
    let start = Instant::now();
    let mut wait_interval = Duration::from_millis(100);
    loop {
        let snapshot = prompt_readiness_snapshot(session_name);
        if snapshot.composer_marker_detected {
            return Ok(());
        }
        if !snapshot.tmux_pane_alive {
            return Err(PROMPT_READY_SESSION_DEAD_ERROR.to_string());
        }
        if start.elapsed() >= timeout {
            log_prompt_ready_timeout(session_name, readiness, timeout, &snapshot);
            return Err(format!(
                "{PROMPT_READY_TIMEOUT_ERROR_PREFIX} {} prompt input readiness after {}s; reason=composer_not_detected; previous_tui_turn_still_running=true; capture_available={}",
                readiness.label(),
                timeout.as_secs(),
                snapshot.capture_available
            ));
        }
        std::thread::sleep(wait_interval);
        wait_interval = std::cmp::min(wait_interval * 2, Duration::from_millis(1000));
    }
}

fn send_prompt_with_readiness(
    session_name: &str,
    prompt: &str,
    readiness: PromptReadinessKind,
) -> Result<(), String> {
    let actions = plan_prompt_submit(prompt)?;
    wait_until_codex_tui_input_ready(session_name, readiness)?;
    run_actions(session_name, &actions)
}

pub fn send_cancel(session_name: &str) -> Result<(), String> {
    run_actions(session_name, &plan_cancel())
}

fn run_actions(session_name: &str, actions: &[TuiInputAction]) -> Result<(), String> {
    for action in actions {
        let output = match action {
            TuiInputAction::Literal(text) => {
                crate::services::platform::tmux::send_literal(session_name, text)?
            }
            TuiInputAction::PasteBuffer(text) => {
                let buffer_name = format!("agentdesk-codex-tui-input-{}", uuid::Uuid::new_v4());
                let load_output = crate::services::platform::tmux::load_buffer(&buffer_name, text)?;
                ensure_tmux_success(load_output, action)?;
                crate::services::platform::tmux::paste_buffer(session_name, &buffer_name, true)?
            }
            TuiInputAction::Enter => {
                crate::services::platform::tmux::send_keys(session_name, &["Enter"])?
            }
            TuiInputAction::Escape => {
                crate::services::platform::tmux::send_keys(session_name, &["Escape"])?
            }
        };
        ensure_tmux_success(output, action)?;
    }
    Ok(())
}

fn ensure_tmux_success(output: Output, action: &TuiInputAction) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let action_name = match action {
        TuiInputAction::Literal(_) => "literal",
        TuiInputAction::PasteBuffer(_) => "paste-buffer",
        TuiInputAction::Enter => "enter",
        TuiInputAction::Escape => "escape",
    };
    if stderr.is_empty() {
        Err(format!(
            "tmux send {action_name} failed: {}",
            output.status
        ))
    } else {
        Err(format!("tmux send {action_name} failed: {stderr}"))
    }
}

/// Pane-capture classifier: returns true when the recent tail looks
/// like the Codex composer waiting for input.
///
/// The classifier requires BOTH:
/// - a Codex composer box edge (a line that is "mostly" box-drawing
///   chars), and
/// - a footer hint line (matches one of [`CODEX_TUI_FOOTER_HINTS`]).
///
/// Both halves must appear within the most recent
/// `PROMPT_READY_SCAN_LINES` non-empty lines so old composer frames
/// scrolled deep into pane history cannot trip the detector after
/// the model produced new output.
pub(crate) fn pane_looks_ready_for_codex_prompt(pane: &str) -> bool {
    let recent: Vec<&str> = pane
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(PROMPT_READY_SCAN_LINES)
        .collect();
    let saw_footer = recent.iter().any(|line| line_is_codex_footer_hint(line));
    let saw_box_edge = recent.iter().any(|line| line_is_codex_composer_edge(line));
    saw_footer && saw_box_edge
}

/// Codex TUI footer hints printed below the composer box. Matching any
/// substring is sufficient; we keep the set narrow on purpose so model
/// output containing these phrases verbatim is unlikely.
const CODEX_TUI_FOOTER_HINTS: &[&str] = &[
    "Esc to interrupt",
    "esc to interrupt",
    "Ctrl+J newline",
    "Ctrl+J for newline",
    "ctrl+j newline",
    "send ⏎",
    "⏎ send",
    "↵ send",
];

fn line_is_codex_footer_hint(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    CODEX_TUI_FOOTER_HINTS
        .iter()
        .any(|hint| trimmed.contains(hint))
}

/// A composer-edge line is "mostly" Unicode box-drawing characters
/// (the rounded input box top/bottom rules). We require at least
/// [`COMPOSER_EDGE_MIN_GLYPHS`] box glyphs and that they dominate the
/// line so a single stray glyph in prose cannot match.
const COMPOSER_EDGE_MIN_GLYPHS: usize = 8;

fn line_is_codex_composer_edge(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let total = trimmed.chars().count();
    if total < COMPOSER_EDGE_MIN_GLYPHS {
        return false;
    }
    let box_glyphs = trimmed.chars().filter(|ch| is_box_drawing_char(*ch)).count();
    box_glyphs >= COMPOSER_EDGE_MIN_GLYPHS && box_glyphs * 2 >= total
}

fn is_box_drawing_char(ch: char) -> bool {
    // U+2500..U+257F Box Drawing block (covers ─ │ ╭ ╮ ╰ ╯ ┌ ┐ ┘ └ etc.)
    matches!(ch as u32, 0x2500..=0x257F)
}

fn log_prompt_ready_timeout(
    session_name: &str,
    readiness: PromptReadinessKind,
    timeout: Duration,
    snapshot: &PromptReadinessSnapshot,
) {
    tracing::debug!(
        tmux_session_name = session_name,
        readiness = readiness.label(),
        timeout_secs = timeout.as_secs(),
        composer_marker_detected = snapshot.composer_marker_detected,
        previous_tui_turn_still_running =
            snapshot.tmux_pane_alive && !snapshot.composer_marker_detected,
        tmux_pane_alive = snapshot.tmux_pane_alive,
        capture_available = snapshot.capture_available,
        pane_tail = %snapshot.pane_tail,
        "codex_tui prompt readiness timed out"
    );
}

fn prompt_ready_debug_tail(pane: &str) -> String {
    let mut lines = pane
        .lines()
        .rev()
        .take(PROMPT_READY_DEBUG_TAIL_LINES)
        .map(|line| line.trim_end_matches('\r'))
        .collect::<Vec<_>>();
    lines.reverse();
    let tail = lines.join("\n");
    crate::utils::format::safe_suffix(tail.trim(), PROMPT_READY_DEBUG_TAIL_BYTES).to_string()
}

fn validate_prompt_text(input: &str) -> Result<(), String> {
    // Block terminal control channels such as ESC bracketed-paste markers,
    // DEL, and C1 controls before either literal send or tmux paste-buffer
    // delivery can relay them into the hosted Codex TUI. Mirrors
    // claude_tui::input::validate_prompt_text.
    if input
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        return Err("prompt contains unsupported terminal control characters".to_string());
    }
    Ok(())
}

fn validate_prompt_not_empty(input: &str) -> Result<(), String> {
    if input.trim().is_empty() {
        return Err("prompt must contain non-whitespace text".to_string());
    }
    Ok(())
}

fn split_literal_chunks(input: &str, max_chars: usize) -> Vec<String> {
    if input.is_empty() {
        return Vec::new();
    }
    let max_chars = max_chars.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;
    for ch in input.chars() {
        if current_chars >= max_chars {
            chunks.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        current.push(ch);
        current_chars += 1;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // plan_prompt_submit
    // ------------------------------------------------------------------

    #[test]
    fn prompt_submit_uses_literal_chunks_then_enter() {
        let actions = plan_prompt_submit("abc").unwrap();
        assert_eq!(
            actions,
            vec![
                TuiInputAction::Literal("abc".to_string()),
                TuiInputAction::Enter
            ]
        );
    }

    #[test]
    fn prompt_submit_uses_paste_buffer_for_multiline_prompts() {
        let actions = plan_prompt_submit("line 1\nline 2").unwrap();
        assert_eq!(
            actions,
            vec![
                TuiInputAction::PasteBuffer("line 1\nline 2".to_string()),
                TuiInputAction::Enter
            ]
        );
    }

    #[test]
    fn prompt_submit_normalizes_crlf_to_lf_before_paste() {
        let actions = plan_prompt_submit("line 1\r\nline 2").unwrap();
        assert_eq!(
            actions,
            vec![
                TuiInputAction::PasteBuffer("line 1\nline 2".to_string()),
                TuiInputAction::Enter
            ]
        );
    }

    #[test]
    fn empty_prompt_is_rejected() {
        let error = plan_prompt_submit("").unwrap_err();
        assert_eq!(error, "prompt must contain non-whitespace text");
    }

    #[test]
    fn whitespace_only_prompt_is_rejected_after_normalization() {
        let error = plan_prompt_submit(" \r\n\t ").unwrap_err();
        assert_eq!(error, "prompt must contain non-whitespace text");
    }

    #[test]
    fn control_characters_are_rejected() {
        let error = plan_prompt_submit("hello\x1b[0m world").unwrap_err();
        assert_eq!(
            error,
            "prompt contains unsupported terminal control characters"
        );
    }

    #[test]
    fn split_literal_chunks_preserves_multibyte_char_boundaries() {
        let chunks = split_literal_chunks("가나다abc", 2);
        assert_eq!(chunks, vec!["가나", "다a", "bc"]);
    }

    #[test]
    fn cancel_uses_escape() {
        assert_eq!(plan_cancel(), vec![TuiInputAction::Escape]);
    }

    // ------------------------------------------------------------------
    // Readiness detector
    // ------------------------------------------------------------------

    /// Realistic Codex TUI bottom-of-pane snapshot when waiting for the
    /// user's next prompt. The composer is the rounded box; the footer
    /// hint sits under it.
    const CODEX_TUI_READY_PANE: &str = "\
some earlier output\n\
more output\n\
╭──────────────────────────────────────────────────────────────╮\n\
│ ▌                                                            │\n\
╰──────────────────────────────────────────────────────────────╯\n\
  Esc to interrupt   Ctrl+J newline   ⏎ send";

    #[test]
    fn codex_pane_with_composer_and_footer_is_ready() {
        assert!(pane_looks_ready_for_codex_prompt(CODEX_TUI_READY_PANE));
    }

    #[test]
    fn codex_pane_without_footer_hint_is_not_ready() {
        let pane = "\
some earlier output\n\
╭──────────────────────────────────────────────────────────────╮\n\
│ working...                                                   │\n\
╰──────────────────────────────────────────────────────────────╯";
        assert!(!pane_looks_ready_for_codex_prompt(pane));
    }

    #[test]
    fn codex_pane_without_composer_edge_is_not_ready() {
        // Footer hint appears in assistant prose without the box edges
        // — must not be treated as ready.
        let pane = "\
The keybinding shown in the docs is `Esc to interrupt`.\n\
Working on your request...";
        assert!(!pane_looks_ready_for_codex_prompt(pane));
    }

    #[test]
    fn assistant_output_with_box_drawing_alone_is_not_ready() {
        // Model rendered a table; no footer hint, must not be ready.
        let pane = "\
Here is a table:\n\
┌────────┬────────┐\n\
│ key    │ value  │\n\
├────────┼────────┤\n\
│ alpha  │ 1      │\n\
└────────┴────────┘\n\
done thinking, next step is...";
        assert!(!pane_looks_ready_for_codex_prompt(pane));
    }

    #[test]
    fn stale_composer_scrolled_deep_into_history_is_not_ready() {
        // Old composer frame is far above the scan window; current tail
        // shows new model output. Must not be classified as ready.
        let mut pane = String::from(
            "╭──────────────────────────────────────────────────────────────╮\n\
             │ old composer                                                 │\n\
             ╰──────────────────────────────────────────────────────────────╯\n\
             Esc to interrupt   Ctrl+J newline   ⏎ send\n",
        );
        for i in 0..30 {
            pane.push_str(&format!("model output line {i}\n"));
        }
        assert!(!pane_looks_ready_for_codex_prompt(&pane));
    }

    #[test]
    fn footer_phrase_inside_quoted_assistant_text_is_not_ready_without_box_edge() {
        let pane = "\
Assistant said:\n\
  > To stop, press Esc to interrupt at any time.\n\
  > Continuing to work on the task now.";
        assert!(!pane_looks_ready_for_codex_prompt(pane));
    }

    #[test]
    fn alternate_footer_phrasing_still_matches() {
        let pane = "\
╭──────────────────────────────────────────────────────────────╮\n\
│ ▌                                                            │\n\
╰──────────────────────────────────────────────────────────────╯\n\
  esc to interrupt · ctrl+j newline";
        assert!(pane_looks_ready_for_codex_prompt(pane));
    }

    #[test]
    fn rejects_pane_with_only_one_box_glyph() {
        // A line with a single ╭ glyph in prose must not be treated as
        // a composer edge even if the footer is present.
        let pane = "\
The diagram shows ╭ here.\n\
  Esc to interrupt   ⏎ send";
        assert!(!pane_looks_ready_for_codex_prompt(pane));
    }

    #[test]
    fn detects_box_drawing_classifier_on_pure_edge_line() {
        let edge = "╭──────────────────────────────────────────────────────────────╮";
        assert!(line_is_codex_composer_edge(edge));
    }

    #[test]
    fn rejects_box_drawing_classifier_on_mixed_prose() {
        let prose = "The diagram shows ╭ here in passing text.";
        assert!(!line_is_codex_composer_edge(prose));
    }

    #[test]
    fn rejects_box_drawing_classifier_on_short_glyph_run() {
        // Fewer than COMPOSER_EDGE_MIN_GLYPHS glyphs must not match.
        let short = "──────";
        assert!(!line_is_codex_composer_edge(short));
    }

    // ------------------------------------------------------------------
    // Timeout policy
    // ------------------------------------------------------------------

    #[test]
    fn prompt_ready_timeouts_are_split_for_fresh_and_followup_turns() {
        assert_eq!(PromptReadinessKind::FreshTurn.timeout().as_secs(), 120);
        assert_eq!(PromptReadinessKind::Followup.timeout().as_secs(), 45);
    }

    #[test]
    fn prompt_ready_timeout_error_is_classified() {
        assert!(is_prompt_ready_timeout_error(
            "timeout waiting for codex tui fresh prompt input readiness after 120s"
        ));
        // The Claude TUI prefix must NOT be classified as a Codex timeout.
        assert!(!is_prompt_ready_timeout_error(
            "timeout waiting for claude tui fresh prompt input readiness after 120s"
        ));
        assert!(!is_prompt_ready_timeout_error(
            "codex tui session died before prompt input was ready"
        ));
    }

    #[test]
    fn session_dead_error_is_classified() {
        assert!(is_session_dead_error(
            "codex tui session died before prompt input was ready"
        ));
        assert!(!is_session_dead_error(
            "timeout waiting for codex tui follow-up prompt input readiness after 45s"
        ));
    }

    // ------------------------------------------------------------------
    // Debug tail
    // ------------------------------------------------------------------

    #[test]
    fn prompt_ready_debug_tail_keeps_recent_lines_and_utf8_boundaries() {
        let pane = (0..40)
            .map(|index| format!("라인 {index}"))
            .collect::<Vec<_>>()
            .join("\n");

        let tail = prompt_ready_debug_tail(&pane);

        assert!(!tail.contains("라인 0"));
        assert!(tail.contains("라인 39"));
        assert!(std::str::from_utf8(tail.as_bytes()).is_ok());
    }
}
