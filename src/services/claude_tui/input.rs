use std::process::Output;
use std::time::Duration;

#[cfg(test)]
pub use super::guarded_outcome::{send_fresh_prompt, send_warm_followup};
use super::{
    guarded_outcome::{self, SubmissionError},
    host_input,
};

mod literal_chunks;
mod preparation_hold;
pub(crate) mod submission;
mod user_receipt;
use literal_chunks::split_literal_chunks;
#[cfg(unix)]
pub(crate) use preparation_hold::EffectPhase;
pub(crate) use preparation_hold::PreparationHold;
pub(crate) use preparation_hold::readiness_failure;
#[cfg(all(test, unix))]
pub(crate) use user_receipt::native_read_byte_total;
pub(crate) use user_receipt::revalidate_native_pin;
pub(crate) use user_receipt::{PaneIdentity, UserReceipt};

mod draft_hold;
use draft_hold::admit_automatic_write;
pub(super) use draft_hold::dismiss_startup_dialog;
pub(crate) use draft_hold::{composer_refusal, stranded_draft_is_ours};
#[cfg(test)]
mod final_ready_tests;
#[cfg(test)]
mod hold_lifecycle_tests;
#[cfg(test)]
mod timeout_draft_tests;

use crate::services::provider::{CancelToken, cancel_requested};

const DEFAULT_LITERAL_CHUNK_CHARS: usize = 1800;
const PROMPT_READY_CAPTURE_SCROLLBACK: i32 = -80;
const PROMPT_READY_DEBUG_TAIL_LINES: usize =
    crate::services::tmux_common::CLAUDE_TUI_READINESS_SCAN_LINES;
pub const FRESH_PROMPT_READY_TIMEOUT: Duration = Duration::from_secs(120);
pub const FOLLOWUP_PROMPT_READY_TIMEOUT: Duration = Duration::from_secs(45);
const PROMPT_DRAFT_CLEANUP_CANCEL_TOKEN: Option<&CancelToken> = None;
const PROMPT_READY_TIMEOUT_ERROR_PREFIX: &str = "timeout waiting for claude tui";
pub const PROMPT_READY_CANCELLED_ERROR: &str = "claude tui prompt readiness wait cancelled";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptReadinessKind {
    FreshTurn,
    Followup,
    ProvenWarmFollowup,
}

#[cfg(test)]
thread_local! {
    /// The follow-up readiness timeout this test thread uses instead of the configured one.
    pub(crate) static FOLLOWUP_TIMEOUT_FOR_TESTS: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// Resolve the Follow-up readiness budget from the live config snapshot,
/// falling back to the compiled-in 45s default. `config_live_reload::current()`
/// returns `None` before boot install and in unit tests, so the const stays the
/// safe fallback. A configured `0` is treated as unset to avoid an
/// immediate-timeout footgun. See `RuntimeSettingsConfig::followup_prompt_ready_timeout_secs`.
fn followup_prompt_ready_timeout() -> Duration {
    #[cfg(test)]
    if let Some(timeout) = FOLLOWUP_TIMEOUT_FOR_TESTS.with(std::cell::Cell::get) {
        return timeout;
    }
    crate::config_live_reload::current()
        .and_then(|cfg| cfg.runtime.followup_prompt_ready_timeout_secs)
        .filter(|secs| *secs > 0)
        // Clamp to a generous upper bound (24h) so a bad hot-reload value (e.g.
        // u64::MAX) cannot overflow `Instant + Duration` at the deadline
        // computation and panic the readiness wait.
        .map(|secs| Duration::from_secs(secs.min(86_400)))
        .unwrap_or(FOLLOWUP_PROMPT_READY_TIMEOUT)
}

impl PromptReadinessKind {
    fn timeout(self) -> Duration {
        match self {
            Self::FreshTurn => FRESH_PROMPT_READY_TIMEOUT,
            Self::Followup | Self::ProvenWarmFollowup => followup_prompt_ready_timeout(),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::FreshTurn => "fresh",
            Self::Followup | Self::ProvenWarmFollowup => "follow-up",
        }
    }

    fn is_followup(self) -> bool {
        matches!(self, Self::Followup | Self::ProvenWarmFollowup)
    }

    fn allows_stale_mcp_auth_warning(self) -> bool {
        matches!(self, Self::ProvenWarmFollowup)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptReadinessSnapshot {
    pub prompt_marker_detected: bool,
    pub prompt_draft_detected: bool,
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
    CtrlU,
    Backspace(usize),
    Left,
    Right,
}

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

// #3034: test-only — pins the Escape cancel-key plan; production cancel now
// routes through the orchestrator, not this helper.
#[allow(dead_code)]
pub fn plan_cancel() -> Vec<TuiInputAction> {
    vec![TuiInputAction::Escape]
}

pub fn send_followup_prompt(
    session_name: &str,
    prompt: &str,
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    send_prompt_with_readiness(
        session_name,
        prompt,
        PromptReadinessKind::Followup,
        cancel_token,
    )
    .map_err(SubmissionError::into_legacy)
}

pub fn is_prompt_ready_timeout_error(error: &str) -> bool {
    error.starts_with(PROMPT_READY_TIMEOUT_ERROR_PREFIX)
}

pub fn is_prompt_ready_cancelled_error(error: &str) -> bool {
    error == PROMPT_READY_CANCELLED_ERROR
}

pub fn prompt_readiness_snapshot(session_name: &str) -> PromptReadinessSnapshot {
    let (pane, alive) = host_input::observe_legacy(session_name, PROMPT_READY_CAPTURE_SCROLLBACK);
    prompt_readiness_snapshot_from_capture(pane.as_deref(), alive)
}

pub(crate) fn prompt_readiness_snapshot_from_capture(
    pane: Option<&str>,
    tmux_pane_alive: bool,
) -> PromptReadinessSnapshot {
    let normalized_pane = pane.map(
        crate::services::claude_tui::prompt_readiness::normalize_prompt_readiness_panel_in_capture,
    );
    let pane = normalized_pane.as_deref();
    let prompt_marker_detected = pane.is_some_and(pane_looks_ready_for_prompt);
    let prompt_draft_detected = pane
        .is_some_and(crate::services::tmux_common::tmux_capture_indicates_claude_tui_prompt_draft);
    let pane_tail = pane
        .map(prompt_ready_debug_tail)
        .unwrap_or_else(|| "<capture unavailable>".to_string());
    PromptReadinessSnapshot {
        prompt_marker_detected,
        prompt_draft_detected,
        tmux_pane_alive,
        capture_available: pane.is_some(),
        pane_tail,
    }
}

pub(crate) fn send_prompt_with_readiness(
    session_name: &str,
    prompt: &str,
    readiness: PromptReadinessKind,
    cancel_token: Option<&CancelToken>,
) -> Result<(), SubmissionError> {
    submission::send(session_name, prompt, readiness, cancel_token)
}

/// #2730: settle delay between a PasteBuffer action and any follow-up
/// (typically `Enter`). Claude TUI 2.1.x parses bracketed-paste sequences
/// byte-by-byte from the pane; without a brief settle window the Enter sent
/// immediately after `tmux paste-buffer -p -r` can race the paste-end marker.
/// When that happens Claude TUI commits the pre-paste input state (often a
/// single-space placeholder) as a standalone user turn, then submits the
/// actual paste content as a separate turn — which the Anthropic API rejects
/// with `400 messages: text content blocks must contain non-whitespace text`
/// because one of the blocks is whitespace-only. A short post-paste settle
/// eliminates the race in practice; the cost is one settle per multi-line
/// turn.
pub(super) const POST_PASTE_BUFFER_SETTLE: Duration = Duration::from_millis(200);

/// #3880 (A1): settle delay between the last single-line `Literal` and the
/// `Enter` that submits it. A single-line prompt plans as `Literal…(Enter)` with
/// NO `PasteBuffer`, so — unlike the multi-line paste path that already settles
/// `POST_PASTE_BUFFER_SETTLE` — the Enter previously fired with a 0 ms gap. On a
/// cold / `/clear` composer that is still re-mounting, that Enter races the
/// composer remount and is swallowed, leaving the typed text as a stranded
/// draft (the submit never lands → 120s transcript timeout → tmux kill). A short
/// settle before the Enter closes the race; the cost is one settle per
/// single-line submit. Mirrors POST_PASTE_BUFFER_SETTLE in spirit and duration.
pub(super) const POST_LITERAL_SETTLE: Duration = Duration::from_millis(200);

/// #3880 (A1): true when `current` is a `Literal` that is immediately followed
/// by `Enter` — the exact single-line submit transition that needs the
/// POST_LITERAL_SETTLE window. Consecutive `Literal` chunks (a long single line
/// split for `send-keys`) do NOT settle between themselves; only the final
/// `Literal → Enter` boundary does. Pure and lookahead-only so the settle wiring
/// is unit-testable without a live tmux pane.
pub(super) fn literal_action_needs_post_settle(
    current: &TuiInputAction,
    next: Option<&TuiInputAction>,
) -> bool {
    matches!(
        (current, next),
        (TuiInputAction::Literal(_), Some(TuiInputAction::Enter))
    )
}

fn run_actions(
    session_name: &str,
    actions: &[TuiInputAction],
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    host_input::run_legacy(session_name, actions, cancel_token).into_legacy()
}

/// F1 single authority: run an out-of-turn composer cleanup through the SAME
/// `with_composer_mutation_lock` that `/compact` steering and every normal
/// prompt submit hold, so a stranded-draft clear and an auto `/compact` can never
/// interleave their key sends.
///
/// Both out-of-turn stranded-draft clearers acquire this fence before checking ownership.
/// Callers must not hold the composer lock. In-send errors retain their uncertain payload.
/// A pane held for draft recovery takes no cleanup key; `None` reports the skipped cleanup.
pub(crate) fn with_composer_cleanup_lock<R>(
    session_name: &str,
    cleanup: impl FnOnce() -> R,
) -> Option<R> {
    crate::services::claude_tui::composer_lock::with_composer_mutation_lock(session_name, || {
        let admitted = admit_automatic_write(session_name, PromptReadinessKind::Followup);
        admitted.ok().map(|()| cleanup())
    })
}

fn prompt_draft_cleanup_actions(snapshot: &PromptReadinessSnapshot) -> Vec<TuiInputAction> {
    let mut actions = vec![
        TuiInputAction::CtrlU,
        TuiInputAction::Escape,
        TuiInputAction::CtrlU,
    ];
    if let Some(count) = claude_prompt_draft_backspace_budget_from_tail(&snapshot.pane_tail) {
        actions.push(TuiInputAction::Backspace(count));
    }
    actions
}

pub(crate) fn claude_prompt_draft_backspace_budget_from_tail(pane_tail: &str) -> Option<usize> {
    crate::services::tmux_common::tmux_capture_claude_tui_prompt_draft_backspace_budget(pane_tail)
}

// #3034: test-only thin wrapper retained to pin the tmux_common backspace
// budget contract from this module's test suite.
#[allow(dead_code)]
pub(crate) fn claude_prompt_draft_backspace_budget_from_line(line: &str) -> Option<usize> {
    crate::services::tmux_common::claude_tui_prompt_draft_backspace_budget_from_line(line)
}

fn check_prompt_cancel(cancel_token: Option<&CancelToken>) -> Result<(), String> {
    if cancel_requested(cancel_token) {
        Err(PROMPT_READY_CANCELLED_ERROR.to_string())
    } else {
        Ok(())
    }
}

pub(super) fn ensure_tmux_success(output: Output, action: &TuiInputAction) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let action_name = match action {
        TuiInputAction::Literal(_) => "literal",
        TuiInputAction::PasteBuffer(_) => "paste-buffer",
        TuiInputAction::Enter => "enter",
        TuiInputAction::Escape => "escape",
        TuiInputAction::CtrlU => "ctrl-u",
        TuiInputAction::Backspace(_) => "backspace",
        TuiInputAction::Left | TuiInputAction::Right => "horizontal",
    };
    if stderr.is_empty() {
        Err(format!("tmux send {action_name} failed: {}", output.status))
    } else {
        Err(format!("tmux send {action_name} failed: {stderr}"))
    }
}

pub fn wait_for_prompt_ready(
    session_name: &str,
    readiness: PromptReadinessKind,
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    wait_for_prompt_ready_inner(session_name, readiness, cancel_token, None)
}

pub fn wait_for_prompt_ready_or_idle_transcript(
    session_name: &str,
    readiness: PromptReadinessKind,
    cancel_token: Option<&CancelToken>,
    transcript_path: &std::path::Path,
) -> Result<(), String> {
    wait_for_prompt_ready_inner(session_name, readiness, cancel_token, Some(transcript_path))
}

pub(crate) fn send_warm_followup_owned(
    session_name: &str,
    prompt: &str,
    cancel_token: Option<&CancelToken>,
) -> Result<(), SubmissionError> {
    submission::send(
        session_name,
        prompt,
        PromptReadinessKind::ProvenWarmFollowup,
        cancel_token,
    )
}

fn wait_for_prompt_ready_inner(
    session_name: &str,
    readiness: PromptReadinessKind,
    cancel_token: Option<&CancelToken>,
    transcript_path: Option<&std::path::Path>,
) -> Result<(), String> {
    check_prompt_cancel(cancel_token)?;
    // A warm owner already checked liveness; the writer still validates its pane before keys.
    if readiness != PromptReadinessKind::ProvenWarmFollowup
        && !host_input::legacy_pane_alive(session_name)
    {
        return Err("claude tui session died before prompt input was ready".into());
    }
    match super::readiness::prepare(
        session_name,
        transcript_path,
        cancel_token,
        readiness.timeout(),
        readiness,
    ) {
        Ok(_) => Ok(()),
        Err(error) if is_prompt_ready_cancelled_error(&error) => Err(error),
        Err(error) if !is_prompt_ready_timeout_error(&error) => Err(error),
        Err(error) => {
            let error = format!("{error}; readiness={}", readiness.label());
            if !host_input::legacy_pane_alive(session_name) {
                return Err("claude tui session died before prompt input was ready".into());
            }
            if readiness == PromptReadinessKind::ProvenWarmFollowup {
                super::readiness::watch_current(session_name, transcript_path);
                return Err(SubmissionError::Held(PreparationHold::before(
                    session_name,
                    "idle_evidence_pending",
                    error,
                ))
                .into_legacy());
            }
            let snapshot = prompt_readiness_snapshot(session_name);
            if prompt_ready_timeout_should_clear_followup_draft(
                readiness,
                &snapshot,
                crate::services::claude::claude_tui_followup_requeue_enabled(),
            ) {
                with_composer_cleanup_lock(session_name, || {
                    if super::readiness::observe(session_name, transcript_path).is_some()
                        && stranded_draft_is_ours(session_name, readiness) == Ok(true)
                    {
                        draft_hold::clear_prompt_draft(session_name);
                    }
                });
            }
            let capture = host_input::observe_draft(session_name);
            crate::services::claude::debug_log_to(
                "claude_tui.log",
                &format!("idle evidence held session={session_name}"),
            );
            if let Some(reason) = composer_refusal(session_name, capture.as_deref()) {
                super::readiness::watch_draft(session_name, transcript_path);
                return Err(SubmissionError::Held(PreparationHold::before(
                    session_name,
                    reason,
                    format!("{error}; reason={reason}"),
                ))
                .into_legacy());
            }
            super::readiness::watch_current(session_name, transcript_path);
            Err(SubmissionError::Held(PreparationHold::before(
                session_name,
                "idle_evidence_pending",
                error,
            ))
            .into_legacy())
        }
    }
}

/// Sync wrapper that awaits the global prompt-ready notify with a bounded
/// budget. Returns `Ready` if the hook fired in time, `Pending` otherwise.
///
/// Uses `Notified::enable()` to register the waker BEFORE the future is
/// polled, so a `notify_waiters` call that fires between this function being
/// invoked and the select being polled is still observed. Combined with the
/// subscribe-before-snapshot ordering in `run_prompt_ready_fast_path`, this
/// closes the residual race flagged in #2445.
// #3034: test-only — the #2445 race-closing wrapper is exercised directly by
// the regression suite; production drives readiness through the fast-path
// caller. Retained as the canonical pinned implementation of the wake ordering.
fn pane_looks_ready_for_prompt(pane: &str) -> bool {
    // Adjacent UI and lifecycle callers still share this marker snapshot.
    // Ordinary input uses current-source readiness evidence.
    crate::services::tmux_common::tmux_capture_indicates_claude_tui_ready_for_input(pane)
}

pub(super) fn prompt_marker_confirms_prompt_ready(
    readiness: PromptReadinessKind,
    snapshot: &PromptReadinessSnapshot,
) -> bool {
    snapshot.prompt_marker_detected
        && !snapshot.prompt_draft_detected
        && snapshot_allows_prompt_readiness(readiness, snapshot)
}

/// A live, captured pane whose marker passes the follow-up send's draft, busy and auth vetoes.
pub(crate) fn final_prompt_ready(snapshot: &PromptReadinessSnapshot) -> bool {
    snapshot.tmux_pane_alive
        && snapshot.capture_available
        && prompt_marker_confirms_prompt_ready(PromptReadinessKind::Followup, snapshot)
}

/// #3889/#4528: an MCP-auth warning blocks readiness unless the caller proves
/// this is hosted reuse after a recorded turn. A plain Followup intent can also
/// target a genuine cold/auth-blocked pane, so it receives no exemption. Draft
/// and live busy evidence still veto this shared renderer used by adjacent UI paths.
fn snapshot_allows_prompt_readiness(
    readiness: PromptReadinessKind,
    snapshot: &PromptReadinessSnapshot,
) -> bool {
    if snapshot.prompt_draft_detected {
        return false;
    }
    if crate::services::claude_tui::prompt_readiness::pane_has_foreground_busy_evidence(
        &snapshot.pane_tail,
        snapshot.capture_available,
        None,
    ) {
        return false;
    }
    readiness.allows_stale_mcp_auth_warning() || !snapshot_indicates_mcp_auth_block(snapshot)
}

/// Whether the snapshot's captured pane shows the MCP-authentication-required
/// cold-boot welcome banner (`⚠ N MCP server(s) need authentication · run
/// /mcp`). Derived from `pane_tail`, which always includes the bottom chrome
/// where the warning renders (just above the composer); a blind capture
/// (`capture_available == false`) can claim nothing and reports `false`.
fn snapshot_indicates_mcp_auth_block(snapshot: &PromptReadinessSnapshot) -> bool {
    snapshot.capture_available
        && crate::services::tmux_common::tmux_capture_indicates_claude_tui_mcp_auth_required(
            &snapshot.pane_tail,
        )
}

/// Whether a readiness timeout considers clearing a stranded follow-up composer draft
/// before returning the error, so the requeued retry can re-inject cleanly. Only
/// fires for follow-ups when requeue is enabled and the pane visibly holds a
/// real, editable unsent draft (an idle-suggestion tail reports no backspace
/// budget via `tmux_common`, so it is excluded). Pure for unit-testing; the clear
/// itself still needs the attributed read to show a prompt AgentDesk typed.
fn prompt_ready_timeout_should_clear_followup_draft(
    readiness: PromptReadinessKind,
    snapshot: &PromptReadinessSnapshot,
    requeue_enabled: bool,
) -> bool {
    readiness.is_followup()
        && requeue_enabled
        && snapshot.tmux_pane_alive
        && snapshot.capture_available
        && snapshot.prompt_draft_detected
        && claude_prompt_draft_backspace_budget_from_tail(&snapshot.pane_tail).is_some()
}

fn prompt_ready_debug_tail(pane: &str) -> String {
    let mut lines = pane
        .lines()
        .rev()
        .take(PROMPT_READY_DEBUG_TAIL_LINES)
        .map(|line| line.trim_end_matches('\r'))
        .collect::<Vec<_>>();
    lines.reverse();
    lines.join("\n").trim().to_string()
}

pub(crate) fn validate_prompt_text(input: &str) -> Result<(), String> {
    // Block terminal control channels such as ESC bracketed-paste markers,
    // DEL, and C1 controls before either literal send or tmux paste-buffer
    // delivery can relay them into the hosted TUI.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn followup_timeout_falls_back_to_default_without_live_config() {
        // No config_live_reload::install() runs in unit tests, so current() is
        // None and the compiled-in 45s default must hold byte-for-byte. The
        // fresh-turn budget must stay independent.
        assert_eq!(
            followup_prompt_ready_timeout(),
            FOLLOWUP_PROMPT_READY_TIMEOUT
        );
        assert_eq!(PromptReadinessKind::Followup.timeout().as_secs(), 45);
        assert_eq!(
            PromptReadinessKind::ProvenWarmFollowup.timeout().as_secs(),
            45
        );
        assert_eq!(PromptReadinessKind::FreshTurn.timeout().as_secs(), 120);
    }

    #[test]
    fn prompt_submit_uses_literal_chunks_then_enter() {
        let actions = plan_prompt_submit("abc");

        assert_eq!(
            actions.unwrap(),
            vec![
                TuiInputAction::Literal("abc".to_string()),
                TuiInputAction::Enter
            ]
        );
    }

    #[test]
    fn single_line_literal_settles_before_enter() {
        // #3880 (A1): a single-line prompt plans as Literal…(Enter). The final
        // Literal must carry POST_LITERAL_SETTLE before the Enter so the submit
        // does not race a re-mounting `/clear` composer. Assert the settle wiring
        // and that the plan still terminates in Enter.
        let actions = plan_prompt_submit("abc").unwrap();
        assert_eq!(actions.last(), Some(&TuiInputAction::Enter));
        assert!(literal_action_needs_post_settle(
            &actions[actions.len() - 2],
            actions.last(),
        ));
        assert!(POST_LITERAL_SETTLE > Duration::ZERO);
    }

    #[test]
    fn literal_settle_applies_only_on_literal_then_enter() {
        // Consecutive Literals (a long single line split for send-keys) do NOT
        // settle between themselves; only the final Literal → Enter boundary
        // does. A non-Literal current action never settles here.
        assert!(literal_action_needs_post_settle(
            &TuiInputAction::Literal("a".to_string()),
            Some(&TuiInputAction::Enter),
        ));
        assert!(!literal_action_needs_post_settle(
            &TuiInputAction::Literal("a".to_string()),
            Some(&TuiInputAction::Literal("b".to_string())),
        ));
        assert!(!literal_action_needs_post_settle(
            &TuiInputAction::Literal("a".to_string()),
            None,
        ));
        assert!(!literal_action_needs_post_settle(
            &TuiInputAction::Enter,
            Some(&TuiInputAction::Enter),
        ));
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
    fn split_literal_chunks_preserves_multibyte_char_boundaries() {
        let chunks = split_literal_chunks("가나다abc", 2);

        assert_eq!(chunks, vec!["가나", "다a", "bc"]);
    }

    #[test]
    fn cancel_uses_escape() {
        assert_eq!(plan_cancel(), vec![TuiInputAction::Escape]);
    }

    #[test]
    fn claude_prompt_draft_backspace_budget_ignores_submitted_discord_prompt_history() {
        assert_eq!(
            claude_prompt_draft_backspace_budget_from_line("❯ [User: 명령봇 (ID: 1)] hello"),
            None
        );
    }

    #[test]
    fn claude_prompt_draft_backspace_budget_handles_nbsp_prompt_line() {
        let budget = claude_prompt_draft_backspace_budget_from_line("❯\u{00a0}commit this");

        assert_eq!(budget, Some("commit this".chars().count() + 4));
    }

    #[test]
    fn claude_prompt_draft_backspace_budget_ignores_completed_history_tail() {
        let tail = "\
❯ write a plan
계획만 적고 보류 — 1개
  CLAUDE.md: 1, MCP: 2 │ Tools: 5 done";

        assert_eq!(claude_prompt_draft_backspace_budget_from_tail(tail), None);
    }

    #[test]
    fn pane_ready_detection_matches_claude_prompt_marker() {
        let pane = "Claude Code v2.1.141\n\n\u{276f} \nstatus";

        assert!(pane_looks_ready_for_prompt(pane));
    }

    #[test]
    fn readiness_snapshot_derives_marker_draft_and_tail_from_one_capture() {
        let pane = "\
❯ completed prompt
⏺ earlier completed output
✻ Baked for 2s
────────────────────────────────────────────────────
❯ pending draft
────────────────────────────────────────────────────
  🤖 Opus(H) │ 7% │ MCP: 2";

        let snapshot = prompt_readiness_snapshot_from_capture(Some(pane), true);

        assert!(snapshot.prompt_marker_detected);
        assert!(snapshot.prompt_draft_detected);
        assert!(snapshot.capture_available);
        assert!(snapshot.tmux_pane_alive);
        assert_eq!(snapshot.pane_tail, pane);
    }

    #[test]
    fn prompt_marker_does_not_confirm_readiness_when_draft_is_present() {
        let snapshot = PromptReadinessSnapshot {
            prompt_marker_detected: true,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "\u{276f} stale draft".to_string(),
        };

        assert!(!prompt_marker_confirms_prompt_ready(
            PromptReadinessKind::Followup,
            &snapshot,
        ));
    }

    // #3889: the MCP-authentication-required cold-boot welcome screen paints
    // composer chrome that the legacy readiness predicate reads as ready, yet
    // Claude Code drops every submission into it. The readiness gate must refuse
    // such a snapshot as ready-to-submit (so we never false-submit and then
    // blind-wait/retry), while a genuine empty composer still confirms ready.
    #[test]
    fn mcp_auth_cold_boot_welcome_is_not_ready_but_normal_composer_is() {
        let mcp_auth_pane = "\
╭─── Claude Code v2.1.195 ───────────────────────────╮
│            Welcome back 오부장!                    │
│   Opus 4.8 (1M context) · Claude Max               │
╰────────────────────────────────────────────────────

 ⚠ 1 MCP server needs authentication · run /mcp

────────────────────────────────────────────────────
❯ [Pasted text #1 +59 lines]
────────────────────────────────────────────────────
  🤖 Opus(H) │ 0% │ MCP: 2 │ ⏵⏵ bypass permissions on";

        // Marker/draft signals as the live snapshot would compute them on this
        // pane (composer chrome ⇒ marker true, draft folded into idle chrome ⇒
        // draft false): without the gate this is the exact false-ready that made
        // the fresh turn submit into a dead screen.
        let blocked = PromptReadinessSnapshot {
            prompt_marker_detected: true,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: mcp_auth_pane.to_string(),
        };
        assert!(snapshot_indicates_mcp_auth_block(&blocked));
        assert!(
            !prompt_marker_confirms_prompt_ready(PromptReadinessKind::FreshTurn, &blocked),
            "MCP-auth cold-boot welcome screen must not be classified ready-to-submit"
        );

        // A genuine, connected, empty composer is unaffected and still ready.
        let ready = PromptReadinessSnapshot {
            prompt_marker_detected: true,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "\
────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  🤖 Opus(H) │ 0% │ MCP: 2 │ ⏵⏵ bypass permissions on"
                .to_string(),
        };
        assert!(!snapshot_indicates_mcp_auth_block(&ready));
        assert!(
            prompt_marker_confirms_prompt_ready(PromptReadinessKind::FreshTurn, &ready),
            "a normal empty composer must still confirm ready"
        );

        // A blind capture cannot assert the banner is present, so it must not be
        // flagged as auth-blocked (the unavailable-capture path owns that case).
        let blind = PromptReadinessSnapshot {
            capture_available: false,
            pane_tail: "<capture unavailable>".to_string(),
            ..blocked.clone()
        };
        assert!(!snapshot_indicates_mcp_auth_block(&blind));
    }

    // #4528: Claude Code v2.1.209 can keep the MCP-auth warning visible after a
    // successful turn. Only a caller with recorded-turn warm provenance may
    // accept the idle composer; plain Followup remains a cold/auth-blocked pane.
    // Positive busy chrome vetoes every kind.
    #[test]
    fn mcp_auth_warning_requires_warm_provenance_and_idle_pane() {
        let idle_pane = "\
╭─── Claude Code v2.1.209 ───────────────────────────╮
│            Welcome back 오부장!                    │
╰────────────────────────────────────────────────────

 ⚠ 1 MCP server needs authentication · run /mcp

────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  🤖 Opus(H) │ 7% │ MCP: 2 │ ⏵⏵ bypass permissions on";
        let idle = PromptReadinessSnapshot {
            prompt_marker_detected: pane_looks_ready_for_prompt(idle_pane),
            prompt_draft_detected:
                crate::services::tmux_common::tmux_capture_indicates_claude_tui_prompt_draft(
                    idle_pane,
                ),
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: idle_pane.to_string(),
        };

        assert!(snapshot_indicates_mcp_auth_block(&idle));
        assert!(
            !prompt_marker_confirms_prompt_ready(PromptReadinessKind::Followup, &idle),
            "Followup intent alone must not bypass a genuine cold auth block"
        );
        assert!(
            prompt_marker_confirms_prompt_ready(PromptReadinessKind::ProvenWarmFollowup, &idle,),
            "recorded-turn warm provenance may ignore a persistent warning"
        );
        assert!(
            !prompt_marker_confirms_prompt_ready(PromptReadinessKind::FreshTurn, &idle),
            "the same warning must keep the #3889 cold-boot FreshTurn guard"
        );

        let busy_pane = "\
 ⚠ 1 MCP server needs authentication · run /mcp
 ⏺ Running 1 shell command…
 · Actioning… (4m 7s · esc to interrupt)
────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  🤖 Opus(H) │ 7% │ MCP: 2 │ ⏵⏵ bypass permissions on";
        let busy = PromptReadinessSnapshot {
            // Exercise the readiness boundary directly with the stale marker
            // bit observed in the #4528 trace; the pane body must still veto it.
            prompt_marker_detected: true,
            prompt_draft_detected:
                crate::services::tmux_common::tmux_capture_indicates_claude_tui_prompt_draft(
                    busy_pane,
                ),
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: busy_pane.to_string(),
        };

        assert!(snapshot_indicates_mcp_auth_block(&busy));
        assert!(crate::services::tmux_common::tmux_capture_indicates_claude_tui_busy(busy_pane));
        for readiness in [
            PromptReadinessKind::FreshTurn,
            PromptReadinessKind::Followup,
            PromptReadinessKind::ProvenWarmFollowup,
        ] {
            assert!(
                !prompt_marker_confirms_prompt_ready(readiness, &busy),
                "positive generating chrome must veto readiness for {readiness:?}"
            );
        }
    }

    #[test]
    fn mcp_auth_warning_unknown_duration_spinner_keeps_warm_followup_not_ready() {
        // Claude Code 2.1.209 ships spinner phrases beyond the legacy verb
        // allowlist. The shared structural classifier must veto the stale prompt
        // marker even when warm provenance permits the auth warning itself.
        let pane = "\
 ⚠ 1 MCP server needs authentication · run /mcp
 ✳ Architecting… (12s)
────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  🤖 Opus(H) │ 7% │ MCP: 2 │ ⏵⏵ bypass permissions on";
        let snapshot = PromptReadinessSnapshot {
            prompt_marker_detected: true,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: pane.to_string(),
        };

        assert!(snapshot_indicates_mcp_auth_block(&snapshot));
        assert!(
            crate::services::tmux_common::tmux_capture_indicates_claude_tui_busy(pane),
            "shared live-turn classification must include unknown structured spinner phrases"
        );
        assert!(
            !prompt_marker_confirms_prompt_ready(
                PromptReadinessKind::ProvenWarmFollowup,
                &snapshot,
            ),
            "a duration-only unknown-phrase spinner must veto proven warm readiness"
        );
    }

    #[test]
    fn prompt_draft_cleanup_actions_are_cancel_agnostic() {
        let snapshot = PromptReadinessSnapshot {
            prompt_marker_detected: true,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "❯\u{00a0}commit this".to_string(),
        };

        assert!(PROMPT_DRAFT_CLEANUP_CANCEL_TOKEN.is_none());
        assert_eq!(
            prompt_draft_cleanup_actions(&snapshot),
            vec![
                TuiInputAction::CtrlU,
                TuiInputAction::Escape,
                TuiInputAction::CtrlU,
                TuiInputAction::Backspace("commit this".chars().count() + 4),
            ]
        );
    }

    #[test]
    fn pane_ready_detection_ignores_non_prompt_status_text() {
        let pane = "Claude Code v2.1.141\nloading plugins\nbypass permissions on";

        assert!(!pane_looks_ready_for_prompt(pane));
    }

    #[test]
    fn pane_ready_detection_ignores_prompt_marker_with_command_text() {
        let pane = "Claude Code v2.1.141\nexample:\n\u{276f} npm run build\nstatus";

        assert!(!pane_looks_ready_for_prompt(pane));
    }

    #[test]
    fn pane_ready_detection_ignores_stale_prompt_marker_outside_recent_tail() {
        let pane = "\
Claude Code v2.1.141
\u{276f}
line 1
line 2
line 3
line 4
line 5
line 6
line 7
line 8
line 9
line 10
line 11
line 12
line 13
line 14
line 15
line 16
line 17
line 18
line 19
line 20
line 21
line 22
line 23
line 24
line 25
line 26
line 27
line 28
line 29
line 30
line 31
line 32
line 33
line 34
line 35
line 36
line 37";

        assert!(!pane_looks_ready_for_prompt(pane));
    }

    #[test]
    fn suffix_free_spinner_at_depth_31_vetoes_prompt_readiness_4888() {
        let mut lines = vec![
            "✳ Compacting conversation…".to_string(),
            "────────────────────────────────────────────────────".to_string(),
            "❯".to_string(),
        ];
        lines.extend((0..29).map(|index| {
            format!(
                "  footer/status row {index:02} {}",
                "wide-pane-padding".repeat(9)
            )
        }));
        let pane = lines.join("\n");
        let snapshot = prompt_readiness_snapshot_from_capture(Some(&pane), true);

        assert!(
            pane.len() > 4096,
            "fixture must exceed the removed byte suffix so restoring it loses the spinner"
        );
        assert_eq!(
            pane.lines()
                .rev()
                .position(|line| line.starts_with('✳'))
                .expect("spinner depth"),
            31,
            "fixture must place the suffix-free spinner beyond the former 24-line tail"
        );
        assert!(snapshot.prompt_marker_detected);
        assert!(snapshot.pane_tail.contains("✳ Compacting conversation…"));
        assert!(!prompt_marker_confirms_prompt_ready(
            PromptReadinessKind::Followup,
            &snapshot,
        ));
    }

    #[test]
    fn prompt_ready_timeouts_are_split_for_fresh_and_followup_turns() {
        assert_eq!(PromptReadinessKind::FreshTurn.timeout().as_secs(), 120);
        assert_eq!(PromptReadinessKind::Followup.timeout().as_secs(), 45);
        assert_eq!(
            PromptReadinessKind::ProvenWarmFollowup.timeout().as_secs(),
            45
        );
    }

    #[test]
    fn prompt_ready_timeout_error_is_classified() {
        assert!(is_prompt_ready_timeout_error(
            "timeout waiting for claude tui fresh prompt input readiness after 120s"
        ));
        assert!(!is_prompt_ready_timeout_error(
            "claude tui session died before prompt input was ready"
        ));
    }

    #[test]
    fn cancelled_error_is_classified_and_distinct_from_timeout() {
        assert!(is_prompt_ready_cancelled_error(
            PROMPT_READY_CANCELLED_ERROR
        ));
        assert!(!is_prompt_ready_cancelled_error(
            "timeout waiting for claude tui fresh prompt input readiness after 120s"
        ));
        assert!(!is_prompt_ready_timeout_error(PROMPT_READY_CANCELLED_ERROR));
    }

    #[test]
    fn wait_for_prompt_ready_returns_cancelled_for_pre_cancelled_token() {
        let token = CancelToken::new();
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let session_name = format!("agentdesk-test-missing-cancelled-{}", std::process::id());

        let error =
            wait_for_prompt_ready(&session_name, PromptReadinessKind::Followup, Some(&token))
                .expect_err("pre-cancelled token must short-circuit readiness wait");

        assert!(is_prompt_ready_cancelled_error(&error), "got: {error}");
    }

    // #2416: when the busy-followup wait path bails because the tmux session
    // never came alive, the dead-session error must NOT be classified as a
    // timeout. The caller relies on this split to decide between "wait again"
    // and "emit the busy notice".
    #[test]
    fn busy_followup_wait_session_died_error_is_not_a_timeout() {
        let err = "claude tui session died before prompt input was ready".to_string();
        assert!(!is_prompt_ready_timeout_error(&err));
    }

    // #2416: the Followup timeout copy is what the busy-followup wait path
    // surfaces to the caller. Lock the prefix so both call sites
    // (claude.rs and discord/router/message_handler.rs) can rely on
    // `is_prompt_ready_timeout_error` to identify the timeout branch.
    #[test]
    fn busy_followup_wait_timeout_message_uses_followup_label() {
        let synthetic = format!(
            "{PROMPT_READY_TIMEOUT_ERROR_PREFIX} follow-up prompt input readiness after 45s; reason=prompt_marker_not_detected; previous_tui_turn_still_running=true; prompt_marker_detected=false; prompt_draft_detected=false; capture_available=true"
        );
        assert!(is_prompt_ready_timeout_error(&synthetic));
        assert!(synthetic.contains("follow-up"));
        assert!(synthetic.contains("45s"));
    }

    #[test]
    fn prompt_ready_timeout_clear_candidates_are_only_retryable_followup_drafts() {
        let draft = PromptReadinessSnapshot {
            prompt_marker_detected: true,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "\u{276f} unsubmitted follow-up".to_string(),
        };

        // A real, editable follow-up draft on a live pane with a backspace budget is a
        // candidate (only when requeue is enabled); whose text it is decides the clear.
        assert!(prompt_ready_timeout_should_clear_followup_draft(
            PromptReadinessKind::Followup,
            &draft,
            true
        ));
        assert!(prompt_ready_timeout_should_clear_followup_draft(
            PromptReadinessKind::ProvenWarmFollowup,
            &draft,
            true
        ));
        assert!(!prompt_ready_timeout_should_clear_followup_draft(
            PromptReadinessKind::FreshTurn,
            &draft,
            true
        ));
        assert!(!prompt_ready_timeout_should_clear_followup_draft(
            PromptReadinessKind::Followup,
            &draft,
            false
        ));

        // An idle-suggestion tail is not an editable draft (no backspace
        // budget), so it must not be cleared.
        let suggestion_like_draft = PromptReadinessSnapshot {
            pane_tail: "\
✻ Worked for 2s
────────────────────────────────────────────────────────────────────────────
❯\u{00a0}좋아, 잘 동작하네
────────────────────────────────────────────────────────────────────────────
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done
  ⏵⏵ bypass permissions on"
                .to_string(),
            ..draft.clone()
        };
        assert!(!prompt_ready_timeout_should_clear_followup_draft(
            PromptReadinessKind::Followup,
            &suggestion_like_draft,
            true
        ));

        // A blind capture cannot confirm an editable draft, so do not clear.
        let no_capture = PromptReadinessSnapshot {
            capture_available: false,
            ..draft.clone()
        };
        assert!(!prompt_ready_timeout_should_clear_followup_draft(
            PromptReadinessKind::Followup,
            &no_capture,
            true
        ));
    }

    // #2416: wait_for_prompt_ready must be reachable as a public API so the
    // busy-followup wait+retry paths in claude.rs and the Discord router can
    // call it. Trying to call it on a non-existent session must return Err
    // (so callers can fall back to the busy notice deterministically).
    #[test]
    fn wait_for_prompt_ready_is_public_and_returns_err_for_missing_session() {
        // A fixture's PATH tmux would answer for any session, so none may run alongside.
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let session_name = format!("agentdesk-test-missing-{}", std::process::id());
        let result = wait_for_prompt_ready(&session_name, PromptReadinessKind::Followup, None);
        assert!(
            result.is_err(),
            "wait_for_prompt_ready on a missing session must return Err; got {result:?}"
        );
    }

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

    #[test]
    fn multiline_prompt_uses_paste_buffer_before_enter() {
        let actions = plan_prompt_submit("line1\nline2").unwrap();

        assert_eq!(
            actions,
            vec![
                TuiInputAction::PasteBuffer("line1\nline2".to_string()),
                TuiInputAction::Enter
            ]
        );
    }

    #[test]
    fn multiline_prompt_normalizes_carriage_returns_before_paste() {
        let actions = plan_prompt_submit("line1\r\nline2\rline3").unwrap();

        assert_eq!(
            actions,
            vec![
                TuiInputAction::PasteBuffer("line1\nline2\nline3".to_string()),
                TuiInputAction::Enter
            ]
        );
    }

    #[test]
    fn prompt_rejects_terminal_control_characters() {
        for prompt in [
            "hello\u{1b}[201~", // ESC bracketed-paste end marker
            "hello\u{7f}",      // DEL
            "hello\u{85}",      // C1 control NEXT LINE
        ] {
            let error = plan_prompt_submit(prompt).unwrap_err();

            assert_eq!(
                error,
                "prompt contains unsupported terminal control characters"
            );
        }
    }

    // U-1+ Codeblock-style multiline (3 lines) is delivered as a single
    // PasteBuffer with newlines preserved, so the TUI receives one atomic paste
    // followed by Enter — not interleaved Literal chunks per line.
    #[test]
    fn prompt_submit_treats_codeblock_multiline_as_single_paste_buffer() {
        let prompt = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}";

        let actions = plan_prompt_submit(prompt).unwrap();

        assert_eq!(
            actions,
            vec![
                TuiInputAction::PasteBuffer(prompt.to_string()),
                TuiInputAction::Enter,
            ]
        );
    }

    // U-2+ Emoji + Korean (multi-byte UTF-8) without a newline must stay in
    // the Literal path and not be UTF-8-split. A single chunk is expected
    // because the input fits inside DEFAULT_LITERAL_CHUNK_CHARS.
    #[test]
    fn prompt_submit_preserves_emoji_and_korean_in_literal_chunk() {
        let prompt = "안녕👋 코드 분석";

        let actions = plan_prompt_submit(prompt).unwrap();

        assert_eq!(
            actions,
            vec![
                TuiInputAction::Literal(prompt.to_string()),
                TuiInputAction::Enter,
            ]
        );
    }

    // U-3+ Bare ESC and BEL must be rejected — these can collide with the
    // tmux paste-buffer escape state machine if passed through.
    #[test]
    fn prompt_rejects_bare_escape_and_bell() {
        for prompt in [
            "hello\u{1b}stop", // bare ESC
            "ring\u{07}bell",  // BEL
            "form\u{0c}feed",  // FF
        ] {
            let error = plan_prompt_submit(prompt).unwrap_err();

            assert_eq!(
                error,
                "prompt contains unsupported terminal control characters"
            );
        }
    }

    // U-5 An 8 KiB single-line prompt is chunked into Literal segments
    // bounded by DEFAULT_LITERAL_CHUNK_CHARS, followed by a single Enter.
    // Concatenating the Literal payloads reproduces the original input
    // verbatim — no truncation, reordering, or boundary corruption.
    #[test]
    fn prompt_submit_chunks_8kib_single_line_into_literals_then_enter() {
        let prompt: String = std::iter::repeat('A').take(8 * 1024).collect();

        let actions = plan_prompt_submit(&prompt).unwrap();

        let (literal_actions, terminator) = actions.split_at(actions.len() - 1);
        assert_eq!(terminator, &[TuiInputAction::Enter]);
        assert!(!literal_actions.is_empty());

        let mut reassembled = String::with_capacity(prompt.len());
        for action in literal_actions {
            match action {
                TuiInputAction::Literal(chunk) => {
                    assert!(
                        chunk.chars().count() <= DEFAULT_LITERAL_CHUNK_CHARS,
                        "chunk over DEFAULT_LITERAL_CHUNK_CHARS: {} chars",
                        chunk.chars().count()
                    );
                    reassembled.push_str(chunk);
                }
                other => panic!("expected Literal chunk, got {other:?}"),
            }
        }
        assert_eq!(reassembled, prompt);
    }

    /// F1 mutation guard: `with_composer_cleanup_lock` must acquire the SAME
    /// per-pane composer mutation lock `/compact` steering holds, so an
    /// out-of-turn stranded-draft cleanup cannot interleave its key sends with a
    /// busy-pane auto `/compact`. While a simulated `/compact` holds the composer
    /// lock, the cleanup must NOT enter its critical section; it proceeds only
    /// once the lock releases. Reverting the routing (running the cleanup without
    /// the lock) lets it enter immediately, failing the `recv_timeout(..).is_err()`
    /// assertion.
    #[cfg(unix)]
    #[test]
    fn composer_cleanup_serializes_with_compact_composer_lock() {
        use std::sync::mpsc;
        use std::time::Duration;

        let session = format!("claude-4591-cleanup-{}", uuid::Uuid::new_v4());
        let (compact_holding_tx, compact_holding_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (cleanup_entered_tx, cleanup_entered_rx) = mpsc::channel();

        let compact_session = session.clone();
        std::thread::spawn(move || {
            crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
                &compact_session,
                || {
                    compact_holding_tx
                        .send(())
                        .expect("signal compact holding composer lock");
                    release_rx.recv().expect("await release");
                },
            );
        });
        compact_holding_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("compact must acquire the composer lock");

        let cleanup_session = session.clone();
        std::thread::spawn(move || {
            with_composer_cleanup_lock(&cleanup_session, || {
                cleanup_entered_tx
                    .send(())
                    .expect("signal cleanup entered critical section");
            });
        });
        assert!(
            cleanup_entered_rx
                .recv_timeout(Duration::from_millis(40))
                .is_err(),
            "an out-of-turn composer cleanup must wait behind the /compact composer lock"
        );
        release_tx.send(()).expect("release compact");
        cleanup_entered_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("cleanup proceeds once /compact releases the composer lock");
    }
}
