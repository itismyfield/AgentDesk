use std::process::Output;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::sync::Notify;

use crate::services::provider::{CancelToken, cancel_requested};

const DEFAULT_LITERAL_CHUNK_CHARS: usize = 1800;
const PROMPT_READY_CAPTURE_SCROLLBACK: i32 = -80;
const PROMPT_READY_DEBUG_TAIL_LINES: usize = 24;
const PROMPT_READY_DEBUG_TAIL_BYTES: usize = 4096;
pub const FRESH_PROMPT_READY_TIMEOUT: Duration = Duration::from_secs(120);
pub const FOLLOWUP_PROMPT_READY_TIMEOUT: Duration = Duration::from_secs(45);
/// Extends #2416 (don't drop the user's follow-up when the pane is busy): when
/// the transcript shows the PRIOR turn is still actively streaming
/// (`TuiTurnState::is_busy`), `wait_for_prompt_ready_polling` treats the per-kind
/// readiness `timeout` as a *stall* budget and keeps waiting up to this absolute
/// ceiling, so a long turn no longer makes the next message vanish via
/// `tui_warm_followup_busy_pre_submit`. The ceiling still bounds a genuinely
/// wedged pane that never returns to a ready prompt.
const PROMPT_READY_ACTIVE_TURN_WAIT_CEILING: Duration = Duration::from_secs(900);
/// Maximum time we let the hook-event fast path block before falling back to
/// the legacy pane-scrape polling loop. Fresh turns historically need a bit
/// more headroom (cold start, MCP load) than follow-ups.
const FRESH_PROMPT_READY_EVENT_BUDGET: Duration = Duration::from_secs(10);
const FOLLOWUP_PROMPT_READY_EVENT_BUDGET: Duration = Duration::from_secs(5);
/// Brief settle delay between hook arrival and the post-event snapshot check
/// so the TUI has time to redraw the prompt marker after Stop.
const PROMPT_READY_POST_EVENT_SETTLE: Duration = Duration::from_millis(50);
const PROMPT_SUBMIT_INITIAL_SETTLE: Duration = Duration::from_millis(120);
const PROMPT_SUBMIT_RETRY_SETTLE: Duration = Duration::from_millis(350);
const PROMPT_SUBMIT_CONFIRM_RETRIES: usize = 2;
/// Upper bound for how long we wait for an interactive selector overlay (e.g.
/// `/effort`) to mount after submitting the slash command, before giving up and
/// reporting failure rather than sending navigation keys into a composer.
const SELECTOR_OPEN_TIMEOUT: Duration = Duration::from_secs(5);
const PROMPT_READY_TIMEOUT_ERROR_PREFIX: &str = "timeout waiting for claude tui";
pub const PROMPT_READY_CANCELLED_ERROR: &str = "claude tui prompt readiness wait cancelled";
/// Cap on auto-dismiss key presses for Claude startup dialogs (resume-from-
/// summary picker, workspace trust) per readiness wait. Startup can stack at
/// most two dialogs back to back; if the pane still shows a dialog after this
/// many Enters something unexpected is on screen and we fall back to the
/// normal timeout path instead of blindly mashing Enter.
const STARTUP_DIALOG_DISMISS_MAX_ATTEMPTS: usize = 5;
/// Settle delay after dismissing a startup dialog so the TUI can redraw
/// (either the next dialog or the composer prompt) before the next snapshot.
const STARTUP_DIALOG_DISMISS_SETTLE: Duration = Duration::from_millis(500);

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

    fn event_budget(self) -> Duration {
        match self {
            Self::FreshTurn => FRESH_PROMPT_READY_EVENT_BUDGET,
            Self::Followup => FOLLOWUP_PROMPT_READY_EVENT_BUDGET,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::FreshTurn => "fresh",
            Self::Followup => "follow-up",
        }
    }
}

/// Outcome of the hook-event fast path.
///
/// `PreSnapshotReady` short-circuits the whole readiness wait (the prompt
/// marker was already visible when we checked, after enabling the Notify
/// permit so no concurrent Stop is dropped).
///
/// `PreSnapshotSessionDead` propagates a tmux-died error to the caller.
///
/// `Ready` means a Stop/SubagentStop hook fired within the event budget after
/// we enabled the permit — the polling fallback is skipped if the post-event
/// snapshot confirms the prompt marker.
///
/// `Pending` (event budget elapsed without the hook firing) falls through to
/// the legacy pane-scrape loop using the remaining budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HookFastPathOutcome {
    PreSnapshotReady,
    PreSnapshotSessionDead,
    Ready,
    Pending,
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
    Backspace(usize),
    /// Navigate an interactive Claude TUI selector with a literal arrow key.
    /// Only `Left`/`Right` are accepted so callers cannot smuggle arbitrary
    /// tmux key names through this path. Claude Code's `/effort` UI is a
    /// horizontal slider (`←/→ to adjust`), so navigation is left/right, not
    /// up/down.
    ArrowLeft,
    ArrowRight,
}

/// Description of an interactive Claude TUI selector (e.g. `/effort`) that must
/// be driven with arrow-key navigation instead of inline arguments.
///
/// Claude Code 2.1.x renders `/effort` as a *horizontal slider* whose footer
/// reads `←/→ to adjust`: the stops are laid out left-to-right and Left/Right
/// move the highlighted stop. `total_items` is the number of slider stops and
/// `target_index` is the 0-based stop we want to land on (leftmost = 0).
///
/// Because the slider opens highlighting the *currently active* stop — which
/// AgentDesk cannot observe ahead of time — navigation is made deterministic by
/// first pressing `Left` enough times to clamp the highlight onto the leftmost
/// stop, then pressing `Right` exactly `target_index` times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectorNavigation {
    pub slash_command: &'static str,
    pub total_items: usize,
    pub target_index: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptSubmitConfirmationDecision {
    Submitted,
    RetrySnapshot,
    RetryEnter,
    FailedSessionDead,
    FailedCaptureUnavailable,
    FailedDraftStuck,
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

fn validate_selector_navigation(nav: SelectorNavigation) -> Result<(), String> {
    if nav.total_items == 0 {
        return Err(format!(
            "{} selector has no selectable items",
            nav.slash_command
        ));
    }
    if nav.target_index >= nav.total_items {
        return Err(format!(
            "{} selector target index {} out of range for {} items",
            nav.slash_command, nav.target_index, nav.total_items
        ));
    }
    validate_prompt_text(nav.slash_command)?;
    validate_prompt_not_empty(nav.slash_command)?;
    Ok(())
}

/// Phase 1 of driving Claude's `/effort` slider: type the slash command and
/// `Enter` to *open* the slider overlay. Navigation keys must NOT be sent until
/// the overlay is confirmed mounted (see `wait_for_selector_open`), otherwise
/// on a fresh/slow pane the keys land in the composer and are dropped.
pub fn plan_selector_open(nav: SelectorNavigation) -> Result<Vec<TuiInputAction>, String> {
    validate_selector_navigation(nav)?;
    Ok(vec![
        TuiInputAction::Literal(nav.slash_command.to_string()),
        TuiInputAction::Enter,
    ])
}

/// Phase 2 of driving Claude's `/effort` slider — run only AFTER the overlay is
/// confirmed open.
///
/// Claude Code's `/effort` is a horizontal slider (`←/→ to adjust`):
///   1. Press `Left` `total_items - 1` times so the highlight is clamped onto
///      the leftmost stop regardless of which stop was initially highlighted
///      (the active level). Pressing `Left` past the leftmost stop is a no-op,
///      so this is a deterministic "home" move.
///   2. Press `Right` `target_index` times to land on the requested stop.
///   3. `Enter` to confirm the selection and close the overlay.
pub fn plan_selector_navigation(nav: SelectorNavigation) -> Result<Vec<TuiInputAction>, String> {
    validate_selector_navigation(nav)?;
    let mut actions = Vec::with_capacity((nav.total_items - 1) + nav.target_index + 1);
    for _ in 0..nav.total_items - 1 {
        actions.push(TuiInputAction::ArrowLeft);
    }
    for _ in 0..nav.target_index {
        actions.push(TuiInputAction::ArrowRight);
    }
    actions.push(TuiInputAction::Enter);
    Ok(actions)
}

pub fn send_fresh_prompt(
    session_name: &str,
    prompt: &str,
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    send_prompt_with_readiness(
        session_name,
        prompt,
        PromptReadinessKind::FreshTurn,
        cancel_token,
    )
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
}

pub fn is_prompt_ready_timeout_error(error: &str) -> bool {
    error.starts_with(PROMPT_READY_TIMEOUT_ERROR_PREFIX)
}

pub fn is_prompt_ready_cancelled_error(error: &str) -> bool {
    error == PROMPT_READY_CANCELLED_ERROR
}

pub fn prompt_readiness_snapshot(session_name: &str) -> PromptReadinessSnapshot {
    let pane = crate::services::platform::tmux::capture_pane(
        session_name,
        PROMPT_READY_CAPTURE_SCROLLBACK,
    );
    let prompt_marker_detected = pane.as_deref().is_some_and(pane_looks_ready_for_prompt);
    let prompt_draft_detected = pane
        .as_deref()
        .is_some_and(crate::services::tmux_common::tmux_capture_indicates_claude_tui_prompt_draft);
    let pane_tail = pane
        .as_deref()
        .map(prompt_ready_debug_tail)
        .unwrap_or_else(|| "<capture unavailable>".to_string());
    PromptReadinessSnapshot {
        prompt_marker_detected,
        prompt_draft_detected,
        tmux_pane_alive: crate::services::tmux_diagnostics::tmux_session_has_live_pane(
            session_name,
        ),
        capture_available: pane.is_some(),
        pane_tail,
    }
}

fn send_prompt_with_readiness(
    session_name: &str,
    prompt: &str,
    readiness: PromptReadinessKind,
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    let actions = plan_prompt_submit(prompt)?;
    wait_for_prompt_ready(session_name, readiness, cancel_token)?;
    crate::services::tui_prompt_dedupe::record_discord_originated_prompt(
        "claude",
        session_name,
        prompt,
    );
    match run_actions_with_submission_confirmation(session_name, &actions, cancel_token) {
        Ok(()) => Ok(()),
        Err(error) => {
            crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
                "claude",
                session_name,
                prompt,
            );
            Err(error)
        }
    }
}

/// Drive an interactive Claude TUI selector (e.g. `/effort <level>`) to a
/// confirmed selection, then validate via a post-submit pane snapshot that the
/// selector overlay actually closed.
///
/// Returns `Ok(())` only when the overlay confirms closed; otherwise returns an
/// `Err` so the caller reports a clear failure to Discord instead of false
/// success while the pane is stranded on the slider. If the overlay is
/// still open we press `Escape` to dismiss it so the pane is not left blocking
/// subsequent input.
pub fn send_selector_followup(
    session_name: &str,
    nav: SelectorNavigation,
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    let open_actions = plan_selector_open(nav)?;
    let navigate_actions = plan_selector_navigation(nav)?;
    wait_for_prompt_ready(session_name, PromptReadinessKind::Followup, cancel_token)?;
    // The slash command is typed into Claude as a real composer entry, so the
    // transcript relay would otherwise classify it as SSH-direct input and
    // lease a spurious external turn. Record it as Discord-originated (same as
    // send_prompt_with_readiness) and drop the record if the drive fails.
    crate::services::tui_prompt_dedupe::record_discord_originated_prompt(
        "claude",
        session_name,
        nav.slash_command,
    );
    let result = (|| {
        // Phase 1: open the slider overlay.
        run_actions(session_name, &open_actions, cancel_token)?;
        // Phase 2: confirm the overlay actually mounted before sending any
        // navigation keys. On a fresh/slow pane the selector is not focused
        // immediately after Enter; sending Left/Right too early drops the keys
        // into the composer and leaves the effort unchanged.
        wait_for_selector_open(session_name, nav, cancel_token)?;
        // Phase 3: home + move to the requested stop + Enter to confirm.
        run_actions(session_name, &navigate_actions, cancel_token)?;
        // Phase 4: validate the overlay closed (selection committed).
        confirm_selector_closed(session_name, nav, cancel_token)
    })();
    if result.is_err() {
        crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
            "claude",
            session_name,
            nav.slash_command,
        );
    }
    result
}

/// Poll until the `/effort` slider overlay is confirmed mounted, so navigation
/// keys are never sent into a composer that has not yet entered the selector.
///
/// Bounded by `SELECTOR_OPEN_TIMEOUT`; a dead pane or capture failure short-
/// circuits to an error. If the overlay never appears we return an error rather
/// than blindly sending navigation that would silently no-op.
fn wait_for_selector_open(
    session_name: &str,
    nav: SelectorNavigation,
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    let start = Instant::now();
    let mut wait_interval = Duration::from_millis(50);
    loop {
        check_prompt_cancel(cancel_token)?;
        let snapshot = selector_state_snapshot(session_name);
        check_prompt_cancel(cancel_token)?;
        if !snapshot.tmux_pane_alive {
            return Err(format!(
                "claude tui session died before {} selector opened",
                nav.slash_command
            ));
        }
        if snapshot.selector_open {
            return Ok(());
        }
        if start.elapsed() >= SELECTOR_OPEN_TIMEOUT {
            log_selector_never_opened(session_name, nav, &snapshot);
            return Err(format!(
                "{} selector did not open within {}s; level not applied",
                nav.slash_command,
                SELECTOR_OPEN_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(wait_interval);
        wait_interval = std::cmp::min(wait_interval * 2, Duration::from_millis(400));
    }
}

fn confirm_selector_closed(
    session_name: &str,
    nav: SelectorNavigation,
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    let mut attempt = 0usize;
    loop {
        std::thread::sleep(prompt_submit_settle_for_attempt(attempt));
        check_prompt_cancel(cancel_token)?;
        let snapshot = selector_state_snapshot(session_name);
        check_prompt_cancel(cancel_token)?;

        if !snapshot.tmux_pane_alive {
            return Err(format!(
                "claude tui session died while applying {}",
                nav.slash_command
            ));
        }
        if !snapshot.capture_available {
            if attempt >= PROMPT_SUBMIT_CONFIRM_RETRIES {
                return Err(format!(
                    "{} selector confirmation unavailable after {} retries; capture_available=false",
                    nav.slash_command, PROMPT_SUBMIT_CONFIRM_RETRIES
                ));
            }
            attempt += 1;
            continue;
        }
        if !snapshot.selector_open {
            return Ok(());
        }
        if attempt >= PROMPT_SUBMIT_CONFIRM_RETRIES {
            log_selector_left_open(session_name, nav, &snapshot);
            // Dismiss the stranded overlay so the pane is not left blocking
            // subsequent input, then report the failure to the caller.
            if let Err(error) = run_actions(session_name, &[TuiInputAction::Escape], cancel_token) {
                tracing::warn!(
                    tmux_session_name = session_name,
                    error = %error,
                    "failed to Escape stranded Claude TUI selector overlay"
                );
            }
            return Err(format!(
                "{} selector did not close after {} confirm retries; level not applied",
                nav.slash_command, PROMPT_SUBMIT_CONFIRM_RETRIES
            ));
        }
        // Overlay still open within retry budget — re-send Enter to confirm.
        run_actions(session_name, &[TuiInputAction::Enter], cancel_token)?;
        attempt += 1;
    }
}

struct SelectorStateSnapshot {
    selector_open: bool,
    tmux_pane_alive: bool,
    capture_available: bool,
    pane_tail: String,
}

fn selector_state_snapshot(session_name: &str) -> SelectorStateSnapshot {
    let pane = crate::services::platform::tmux::capture_pane(
        session_name,
        PROMPT_READY_CAPTURE_SCROLLBACK,
    );
    let selector_open = pane
        .as_deref()
        .is_some_and(crate::services::tmux_common::tmux_capture_indicates_claude_tui_selector_open);
    let pane_tail = pane
        .as_deref()
        .map(prompt_ready_debug_tail)
        .unwrap_or_else(|| "<capture unavailable>".to_string());
    SelectorStateSnapshot {
        selector_open,
        tmux_pane_alive: crate::services::tmux_diagnostics::tmux_session_has_live_pane(
            session_name,
        ),
        capture_available: pane.is_some(),
        pane_tail,
    }
}

fn log_selector_left_open(
    session_name: &str,
    nav: SelectorNavigation,
    snapshot: &SelectorStateSnapshot,
) {
    tracing::warn!(
        tmux_session_name = session_name,
        slash_command = nav.slash_command,
        target_index = nav.target_index,
        total_items = nav.total_items,
        pane_tail = %snapshot.pane_tail,
        "claude_tui selector overlay still open after confirm retries; selection not applied"
    );
    crate::services::claude::debug_log_to(
        "claude_tui.log",
        &format!(
            "selector left open session={} command={} target_index={} total_items={} pane_tail:\n{}",
            session_name, nav.slash_command, nav.target_index, nav.total_items, snapshot.pane_tail
        ),
    );
}

fn log_selector_never_opened(
    session_name: &str,
    nav: SelectorNavigation,
    snapshot: &SelectorStateSnapshot,
) {
    tracing::warn!(
        tmux_session_name = session_name,
        slash_command = nav.slash_command,
        timeout_secs = SELECTOR_OPEN_TIMEOUT.as_secs(),
        capture_available = snapshot.capture_available,
        pane_tail = %snapshot.pane_tail,
        "claude_tui selector overlay never mounted; skipping navigation to avoid false success"
    );
    crate::services::claude::debug_log_to(
        "claude_tui.log",
        &format!(
            "selector never opened session={} command={} timeout={}s capture_available={} pane_tail:\n{}",
            session_name,
            nav.slash_command,
            SELECTOR_OPEN_TIMEOUT.as_secs(),
            snapshot.capture_available,
            snapshot.pane_tail
        ),
    );
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
const POST_PASTE_BUFFER_SETTLE: Duration = Duration::from_millis(200);

fn run_actions(
    session_name: &str,
    actions: &[TuiInputAction],
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    for action in actions {
        check_prompt_cancel(cancel_token)?;
        let output = match action {
            TuiInputAction::Literal(text) => {
                crate::services::platform::tmux::send_literal(session_name, text)?
            }
            TuiInputAction::PasteBuffer(text) => {
                let buffer_name = format!("agentdesk-tui-input-{}", uuid::Uuid::new_v4());
                let load_output = crate::services::platform::tmux::load_buffer(&buffer_name, text)?;
                ensure_tmux_success(load_output, action)?;
                check_prompt_cancel(cancel_token)?;
                let paste_output = crate::services::platform::tmux::paste_buffer(
                    session_name,
                    &buffer_name,
                    true,
                )?;
                ensure_tmux_success(paste_output, action)?;
                // #2730 settle: see POST_PASTE_BUFFER_SETTLE rationale above.
                std::thread::sleep(POST_PASTE_BUFFER_SETTLE);
                check_prompt_cancel(cancel_token)?;
                continue;
            }
            TuiInputAction::Enter => {
                crate::services::platform::tmux::send_keys(session_name, &["Enter"])?
            }
            TuiInputAction::Escape => {
                crate::services::platform::tmux::send_keys(session_name, &["Escape"])?
            }
            TuiInputAction::ArrowLeft => {
                crate::services::platform::tmux::send_keys(session_name, &["Left"])?
            }
            TuiInputAction::ArrowRight => {
                crate::services::platform::tmux::send_keys(session_name, &["Right"])?
            }
            TuiInputAction::Backspace(count) => {
                let mut remaining = *count;
                while remaining > 0 {
                    let batch = remaining.min(32);
                    let keys = vec!["BSpace"; batch];
                    let output = crate::services::platform::tmux::send_keys(session_name, &keys)?;
                    ensure_tmux_success(output, action)?;
                    remaining -= batch;
                }
                continue;
            }
        };
        ensure_tmux_success(output, action)?;
    }
    Ok(())
}

fn run_actions_with_submission_confirmation(
    session_name: &str,
    actions: &[TuiInputAction],
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    run_actions(session_name, actions, cancel_token)?;
    confirm_prompt_submission_left_editor(session_name, cancel_token)
}

fn confirm_prompt_submission_left_editor(
    session_name: &str,
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    let mut attempt = 0usize;
    loop {
        std::thread::sleep(prompt_submit_settle_for_attempt(attempt));
        check_prompt_cancel(cancel_token)?;
        let snapshot = prompt_readiness_snapshot(session_name);
        check_prompt_cancel(cancel_token)?;

        match prompt_submit_confirmation_decision(&snapshot, attempt, PROMPT_SUBMIT_CONFIRM_RETRIES)
        {
            PromptSubmitConfirmationDecision::Submitted => return Ok(()),
            PromptSubmitConfirmationDecision::FailedSessionDead => {
                return Err("claude tui session died after prompt submit".to_string());
            }
            PromptSubmitConfirmationDecision::FailedCaptureUnavailable => {
                log_prompt_submit_capture_unavailable(session_name, attempt, &snapshot);
                return Err(format!(
                    "claude tui prompt submit confirmation unavailable after {} retries; capture_available=false",
                    PROMPT_SUBMIT_CONFIRM_RETRIES
                ));
            }
            PromptSubmitConfirmationDecision::FailedDraftStuck => {
                log_prompt_submit_left_draft(session_name, &snapshot);
                clear_prompt_draft_before_error(session_name, cancel_token);
                return Err(format!(
                    "claude tui prompt submit left draft after {} enter retries; prompt_marker_detected={}; prompt_draft_detected={}; capture_available={}",
                    PROMPT_SUBMIT_CONFIRM_RETRIES,
                    snapshot.prompt_marker_detected,
                    snapshot.prompt_draft_detected,
                    snapshot.capture_available
                ));
            }
            PromptSubmitConfirmationDecision::RetrySnapshot => {
                log_prompt_submit_capture_unavailable(session_name, attempt, &snapshot);
                attempt += 1;
            }
            PromptSubmitConfirmationDecision::RetryEnter => {
                tracing::warn!(
                    tmux_session_name = session_name,
                    retry = attempt + 1,
                    max_retries = PROMPT_SUBMIT_CONFIRM_RETRIES,
                    "claude_tui prompt submit left a draft after Enter; retrying Enter"
                );
                run_actions(session_name, &[TuiInputAction::Enter], cancel_token)?;
                attempt += 1;
            }
        }
    }
}

fn prompt_submit_needs_enter_retry(snapshot: &PromptReadinessSnapshot) -> bool {
    snapshot.tmux_pane_alive && snapshot.prompt_marker_detected && snapshot.prompt_draft_detected
}

fn prompt_submit_confirmation_decision(
    snapshot: &PromptReadinessSnapshot,
    attempt: usize,
    max_retries: usize,
) -> PromptSubmitConfirmationDecision {
    if !snapshot.tmux_pane_alive {
        return PromptSubmitConfirmationDecision::FailedSessionDead;
    }
    if !snapshot.capture_available {
        return if attempt >= max_retries {
            PromptSubmitConfirmationDecision::FailedCaptureUnavailable
        } else {
            PromptSubmitConfirmationDecision::RetrySnapshot
        };
    }
    if prompt_submit_needs_enter_retry(snapshot) {
        return if attempt >= max_retries {
            PromptSubmitConfirmationDecision::FailedDraftStuck
        } else {
            PromptSubmitConfirmationDecision::RetryEnter
        };
    }
    PromptSubmitConfirmationDecision::Submitted
}

fn prompt_submit_settle_for_attempt(attempt: usize) -> Duration {
    if attempt == 0 {
        PROMPT_SUBMIT_INITIAL_SETTLE
    } else {
        PROMPT_SUBMIT_RETRY_SETTLE
    }
}

fn clear_prompt_draft_before_error(session_name: &str, cancel_token: Option<&CancelToken>) {
    let snapshot = prompt_readiness_snapshot(session_name);
    let mut actions = vec![TuiInputAction::Escape];
    if let Some(count) = claude_prompt_draft_backspace_budget_from_tail(&snapshot.pane_tail) {
        actions.push(TuiInputAction::Backspace(count));
    }
    if let Err(error) = run_actions(session_name, &actions, cancel_token) {
        tracing::warn!(
            tmux_session_name = session_name,
            error = %error,
            "failed to clear Claude TUI draft after prompt submit retries"
        );
    }
}

pub(crate) fn claude_prompt_draft_backspace_budget_from_tail(pane_tail: &str) -> Option<usize> {
    crate::services::tmux_common::tmux_capture_claude_tui_prompt_draft_backspace_budget(pane_tail)
}

pub(crate) fn claude_prompt_draft_is_idle_suggestion_tail(pane_tail: &str) -> bool {
    crate::services::tmux_common::tmux_capture_indicates_claude_tui_idle_suggestion(pane_tail)
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
        TuiInputAction::ArrowLeft => "arrow-left",
        TuiInputAction::ArrowRight => "arrow-right",
        TuiInputAction::Backspace(_) => "backspace",
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

pub fn send_followup_prompt_or_idle_transcript(
    session_name: &str,
    prompt: &str,
    cancel_token: Option<&CancelToken>,
    transcript_path: &std::path::Path,
) -> Result<(), String> {
    let actions = plan_prompt_submit(prompt)?;
    wait_for_prompt_ready_or_idle_transcript(
        session_name,
        PromptReadinessKind::Followup,
        cancel_token,
        transcript_path,
    )?;
    crate::services::tui_prompt_dedupe::record_discord_originated_prompt(
        "claude",
        session_name,
        prompt,
    );
    match run_actions_with_submission_confirmation(session_name, &actions, cancel_token) {
        Ok(()) => Ok(()),
        Err(error) => {
            crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
                "claude",
                session_name,
                prompt,
            );
            Err(error)
        }
    }
}

fn wait_for_prompt_ready_inner(
    session_name: &str,
    readiness: PromptReadinessKind,
    cancel_token: Option<&CancelToken>,
    transcript_path: Option<&std::path::Path>,
) -> Result<(), String> {
    check_prompt_cancel(cancel_token)?;
    let timeout = readiness.timeout();
    let start = Instant::now();

    if transcript_idle_confirms_prompt_ready_without_capture(session_name, transcript_path) {
        tracing::info!(
            tmux_session_name = session_name,
            readiness = readiness.label(),
            elapsed_ms = start.elapsed().as_millis() as u64,
            "claude_tui prompt ready via idle transcript"
        );
        return Ok(());
    }

    if cancel_token.is_some() {
        return wait_for_prompt_ready_polling(
            session_name,
            readiness,
            timeout,
            start,
            cancel_token,
            transcript_path,
        );
    }

    // Event-driven fast path with subscribe-before-snapshot ordering.
    //
    // The pre-snapshot used to live OUTSIDE the `Notified` registration which
    // left a narrow race: a Stop hook firing between the pre-snapshot and the
    // first `notified()` poll would be dropped (`notify_waiters` does NOT
    // buffer for not-yet-enabled waiters), and the call would then wait up to
    // the full event budget before the polling fallback rescued it.
    //
    // We close that gap by calling `Notified::enable()` BEFORE the
    // pre-snapshot — once enabled, any `notify_waiters` invocation is
    // guaranteed to wake this specific permit, even if the wait has not yet
    // been polled. See issue #2445.
    let notify = crate::services::claude_tui::hook_server::prompt_ready_notify();
    let (fast_path, post_event_snapshot) =
        run_prompt_ready_fast_path(notify, session_name.to_string(), readiness.event_budget());

    match fast_path {
        HookFastPathOutcome::PreSnapshotReady => {
            check_prompt_cancel(cancel_token)?;
            tracing::debug!(
                tmux_session_name = session_name,
                readiness = readiness.label(),
                elapsed_ms = start.elapsed().as_millis() as u64,
                "claude_tui prompt ready on pre-snapshot (no event wait needed)"
            );
            return Ok(());
        }
        HookFastPathOutcome::PreSnapshotSessionDead => {
            check_prompt_cancel(cancel_token)?;
            return Err("claude tui session died before prompt input was ready".to_string());
        }
        HookFastPathOutcome::Ready | HookFastPathOutcome::Pending => {}
    }

    check_prompt_cancel(cancel_token)?;
    if let Some(snapshot) = post_event_snapshot {
        if prompt_marker_confirms_prompt_ready(&snapshot) {
            check_prompt_cancel(cancel_token)?;
            tracing::debug!(
                tmux_session_name = session_name,
                readiness = readiness.label(),
                hook_event_fast_path_hit = matches!(fast_path, HookFastPathOutcome::Ready),
                elapsed_ms = start.elapsed().as_millis() as u64,
                "claude_tui prompt ready via hook event fast path"
            );
            return Ok(());
        }
        if transcript_idle_confirms_prompt_ready(&snapshot, transcript_path) {
            check_prompt_cancel(cancel_token)?;
            tracing::info!(
                tmux_session_name = session_name,
                readiness = readiness.label(),
                elapsed_ms = start.elapsed().as_millis() as u64,
                "claude_tui prompt ready via idle transcript fallback after hook fast path"
            );
            return Ok(());
        }
        if !snapshot.tmux_pane_alive {
            check_prompt_cancel(cancel_token)?;
            return Err("claude tui session died before prompt input was ready".to_string());
        }
    }

    check_prompt_cancel(cancel_token)?;
    if !matches!(fast_path, HookFastPathOutcome::Ready) {
        // Fast path did not fire within its budget — keep the original warn
        // visibility so missing Stop hooks remain debuggable.
        tracing::warn!(
            tmux_session_name = session_name,
            readiness = readiness.label(),
            event_budget_ms = readiness.event_budget().as_millis() as u64,
            "claude_tui hook didn't fire within budget, falling back to pane-scrape polling"
        );
    }

    wait_for_prompt_ready_polling(
        session_name,
        readiness,
        timeout,
        start,
        cancel_token,
        transcript_path,
    )
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
#[allow(dead_code)]
fn wait_for_prompt_ready_event(notify: Arc<Notify>, budget: Duration) -> HookFastPathOutcome {
    let fut = async move {
        let notified = notify.notified();
        tokio::pin!(notified);
        // Register the waker before the first `.await` so any concurrent
        // `notify_waiters()` is guaranteed to wake this specific permit.
        notified.as_mut().enable();
        tokio::select! {
            _ = &mut notified => HookFastPathOutcome::Ready,
            _ = tokio::time::sleep(budget) => HookFastPathOutcome::Pending,
        }
    };

    drive_fast_path_future(fut)
}

/// Subscribe-before-snapshot fast path: register a permit on the global
/// `Notify` BEFORE taking the pre-snapshot so a Stop hook that fires during
/// the snapshot is not dropped.
///
/// Returns the fast-path outcome plus, when relevant, the post-event snapshot
/// the caller should consult before falling through to the polling loop.
fn run_prompt_ready_fast_path(
    notify: Arc<Notify>,
    session_name: String,
    budget: Duration,
) -> (HookFastPathOutcome, Option<PromptReadinessSnapshot>) {
    let fut = async move {
        let notified = notify.notified();
        tokio::pin!(notified);
        // Register the waker before taking the pre-snapshot so any Stop fired
        // after this point — including during the snapshot syscalls — is
        // guaranteed to wake us. `notify_waiters` does not buffer for
        // not-yet-enabled waiters, so enabling here is the load-bearing step
        // that closes the #2445 race.
        notified.as_mut().enable();

        let pre_snapshot = prompt_readiness_snapshot(&session_name);
        if prompt_marker_confirms_prompt_ready(&pre_snapshot) {
            return (HookFastPathOutcome::PreSnapshotReady, None);
        }
        if !pre_snapshot.tmux_pane_alive {
            return (HookFastPathOutcome::PreSnapshotSessionDead, None);
        }

        let fast_path = tokio::select! {
            _ = &mut notified => HookFastPathOutcome::Ready,
            _ = tokio::time::sleep(budget) => HookFastPathOutcome::Pending,
        };

        if matches!(fast_path, HookFastPathOutcome::Ready) {
            tokio::time::sleep(PROMPT_READY_POST_EVENT_SETTLE).await;
        }
        let post_event_snapshot = prompt_readiness_snapshot(&session_name);
        (fast_path, Some(post_event_snapshot))
    };

    drive_fast_path_future(fut)
}

/// Run an async fast-path future to completion using the caller's runtime
/// when possible, falling back to a dedicated thread with a fresh
/// current-thread runtime otherwise.
fn drive_fast_path_future<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static + FastPathFallback,
{
    match Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
            // Safe to park the worker thread because the multi-thread runtime
            // has other workers to keep driving tasks during the block.
            tokio::task::block_in_place(|| handle.block_on(fut))
        }
        _ => {
            // Either no ambient runtime, or a current-thread runtime where
            // `block_in_place` would panic and parking the only worker would
            // deadlock the runtime. Run the wait on a dedicated thread with
            // its own minimal current-thread runtime so we never block the
            // caller's runtime.
            wait_on_dedicated_thread(fut)
        }
    }
}

/// Drive `fut` to completion on a fresh OS thread with its own current-thread
/// Tokio runtime. The thread join itself is a plain blocking syscall, which is
/// safe regardless of the caller's runtime flavor (multi-thread or current-
/// thread). Returns the fallback (`Pending`-equivalent) value if we fail to
/// spawn or build the runtime so the polling fallback can take over.
fn wait_on_dedicated_thread<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static + FastPathFallback,
{
    match std::thread::Builder::new()
        .name("claude-tui-prompt-ready".to_string())
        .spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
            {
                Ok(rt) => rt.block_on(fut),
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "failed to build local runtime for prompt readiness fast path; falling back to polling"
                    );
                    T::fallback()
                }
            }
        }) {
        Ok(handle) => handle.join().unwrap_or_else(|panic| {
            tracing::warn!(
                "prompt readiness fast-path worker panicked: {:?}; falling back to polling",
                panic
            );
            T::fallback()
        }),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "failed to spawn prompt readiness fast-path worker; falling back to polling"
            );
            T::fallback()
        }
    }
}

/// Helper trait so the dedicated-thread driver can produce the right
/// "treat as Pending" sentinel regardless of which fast-path future flavor
/// we are running.
trait FastPathFallback {
    fn fallback() -> Self;
}

impl FastPathFallback for HookFastPathOutcome {
    fn fallback() -> Self {
        HookFastPathOutcome::Pending
    }
}

impl FastPathFallback for (HookFastPathOutcome, Option<PromptReadinessSnapshot>) {
    fn fallback() -> Self {
        (HookFastPathOutcome::Pending, None)
    }
}

fn wait_for_prompt_ready_polling(
    session_name: &str,
    readiness: PromptReadinessKind,
    timeout: Duration,
    start: Instant,
    cancel_token: Option<&CancelToken>,
    transcript_path: Option<&std::path::Path>,
) -> Result<(), String> {
    let mut wait_interval = Duration::from_millis(100);
    let mut dialog_dismiss_attempts = 0usize;
    let mut active_turn_extension_logged = false;
    loop {
        check_prompt_cancel(cancel_token)?;
        if transcript_idle_confirms_prompt_ready_without_capture(session_name, transcript_path) {
            tracing::info!(
                tmux_session_name = session_name,
                readiness = readiness.label(),
                elapsed_ms = start.elapsed().as_millis() as u64,
                "claude_tui prompt ready via idle transcript"
            );
            return Ok(());
        }
        let snapshot = prompt_readiness_snapshot(session_name);
        check_prompt_cancel(cancel_token)?;
        if prompt_marker_confirms_prompt_ready(&snapshot) {
            return Ok(());
        }
        // Startup dialogs (resume-from-summary picker, workspace trust) park
        // the pane on an option selector whose highlighted `❯ 1. ...` row
        // reads as a composer draft, so neither the marker check above nor
        // the transcript fallback below would ever pass. Handle them before
        // the transcript fallback so an idle transcript cannot confirm
        // readiness while a modal dialog is still swallowing input.
        if let Some(dialog) =
            crate::services::claude_tui::startup_dialog::detect_claude_startup_dialog(
                &snapshot.pane_tail,
            )
        {
            use crate::services::claude_tui::startup_dialog::StartupDialogPlan;
            match crate::services::claude_tui::startup_dialog::plan_startup_dialog_response(&dialog)
            {
                StartupDialogPlan::DismissWithEnter
                    if dialog_dismiss_attempts < STARTUP_DIALOG_DISMISS_MAX_ATTEMPTS =>
                {
                    dialog_dismiss_attempts += 1;
                    log_startup_dialog_dismiss(
                        session_name,
                        readiness,
                        dialog.label(),
                        dialog_dismiss_attempts,
                    );
                    run_actions(session_name, &[TuiInputAction::Enter], cancel_token)?;
                    std::thread::sleep(STARTUP_DIALOG_DISMISS_SETTLE);
                    check_prompt_cancel(cancel_token)?;
                    continue;
                }
                StartupDialogPlan::DismissWithEnter => {
                    // Dismiss budget exhausted with a dialog still on screen —
                    // fall through to the regular timeout bookkeeping so the
                    // canonical timeout error (with pane tail logging) fires.
                }
                StartupDialogPlan::FailUntrustedWorkspace { workspace } => {
                    check_prompt_cancel(cancel_token)?;
                    log_startup_dialog_untrusted_workspace(session_name, readiness, &workspace);
                    return Err(format!(
                        "claude tui startup blocked by workspace trust dialog for untrusted path '{workspace}'; refusing to auto-trust; fix the agent/channel workspace mapping so claude does not spawn there"
                    ));
                }
            }
        }
        if transcript_idle_confirms_prompt_ready(&snapshot, transcript_path) {
            tracing::info!(
                tmux_session_name = session_name,
                readiness = readiness.label(),
                elapsed_ms = start.elapsed().as_millis() as u64,
                "claude_tui prompt ready via idle transcript fallback"
            );
            return Ok(());
        }
        if !snapshot.tmux_pane_alive {
            check_prompt_cancel(cancel_token)?;
            return Err("claude tui session died before prompt input was ready".to_string());
        }
        if start.elapsed() >= timeout {
            check_prompt_cancel(cancel_token)?;
            // Extends #2416: a long PRIOR turn must not make us abandon this
            // follow-up. The loop already resolves ready the instant the
            // transcript goes Idle, so reaching the wall-clock `timeout` means
            // the prior turn is still mid-flight. While the transcript confirms
            // it is actively streaming (is_busy ⇒ Streaming/UserSubmitted; an
            // Idle/Unknown read does NOT extend), keep waiting up to the absolute
            // ceiling — the prompt returns once the turn finishes, so the
            // follow-up is delivered sequentially instead of dropped.
            let transcript_turn_active = transcript_path.is_some_and(|path| {
                crate::services::claude_tui::transcript_tail::observe_transcript_turn_state(path)
                    .is_busy()
            });
            if active_turn_warrants_wait_extension(
                snapshot.tmux_pane_alive,
                transcript_turn_active,
                start.elapsed(),
                PROMPT_READY_ACTIVE_TURN_WAIT_CEILING,
            ) {
                if !active_turn_extension_logged {
                    active_turn_extension_logged = true;
                    tracing::info!(
                        tmux_session_name = session_name,
                        readiness = readiness.label(),
                        base_timeout_secs = timeout.as_secs(),
                        ceiling_secs = PROMPT_READY_ACTIVE_TURN_WAIT_CEILING.as_secs(),
                        "claude_tui readiness wait extended: prior turn still streaming; waiting for it to finish instead of dropping the follow-up"
                    );
                }
                std::thread::sleep(wait_interval);
                check_prompt_cancel(cancel_token)?;
                wait_interval = std::cmp::min(wait_interval * 2, Duration::from_millis(1000));
                continue;
            }
            log_prompt_ready_timeout(session_name, readiness, timeout, &snapshot);
            return Err(format!(
                "{PROMPT_READY_TIMEOUT_ERROR_PREFIX} {} prompt input readiness after {}s; reason={}; previous_tui_turn_still_running={}; prompt_marker_detected={}; prompt_draft_detected={}; capture_available={}",
                readiness.label(),
                timeout.as_secs(),
                prompt_ready_timeout_reason(&snapshot),
                // Mirror the computed value the debug log already records
                // (`log_prompt_ready_timeout`) instead of the legacy hardcoded
                // `true`: the pane is known alive here (a dead pane returned
                // above), so this is `true` only when the pane never looked
                // ready — i.e. a turn that is plausibly still running. An unsent
                // draft means the turn ended, so it reports `false`.
                snapshot.tmux_pane_alive && !snapshot.prompt_marker_detected,
                snapshot.prompt_marker_detected,
                snapshot.prompt_draft_detected,
                snapshot.capture_available
            ));
        }
        std::thread::sleep(wait_interval);
        check_prompt_cancel(cancel_token)?;
        wait_interval = std::cmp::min(wait_interval * 2, Duration::from_millis(1000));
    }
}

/// Whether `wait_for_prompt_ready_polling` should keep waiting past the per-kind
/// readiness `timeout` instead of failing. We only extend when the pane is alive
/// AND the transcript confirms the prior turn is still actively streaming, and
/// only up to an absolute `ceiling`. Pure so the policy is unit-testable without
/// a live tmux pane.
fn active_turn_warrants_wait_extension(
    pane_alive: bool,
    transcript_turn_active: bool,
    elapsed: Duration,
    ceiling: Duration,
) -> bool {
    pane_alive && transcript_turn_active && elapsed < ceiling
}

#[cfg(test)]
mod active_turn_wait_extension_tests {
    use super::active_turn_warrants_wait_extension;
    use std::time::Duration;

    #[test]
    fn extends_only_when_pane_alive_and_turn_active_within_ceiling() {
        let ceiling = Duration::from_secs(900);
        let within = Duration::from_secs(60);

        // Prior turn still streaming, pane alive, within ceiling → keep waiting
        // (the follow-up must not be dropped just because the turn is long).
        assert!(active_turn_warrants_wait_extension(
            true, true, within, ceiling
        ));
        // Dead pane → never extend (the existing dead-pane error owns that case).
        assert!(!active_turn_warrants_wait_extension(
            false, true, within, ceiling
        ));
        // Transcript not confirmably active (Idle/Unknown) → do not extend; fail
        // as before so a genuinely stuck/uncertain pane still times out.
        assert!(!active_turn_warrants_wait_extension(
            true, false, within, ceiling
        ));
        // At/!past the absolute ceiling → stop waiting even if still streaming.
        assert!(!active_turn_warrants_wait_extension(
            true, true, ceiling, ceiling
        ));
        assert!(!active_turn_warrants_wait_extension(
            true,
            true,
            ceiling + Duration::from_secs(1),
            ceiling
        ));
    }
}

fn pane_looks_ready_for_prompt(pane: &str) -> bool {
    crate::services::tmux_common::tmux_capture_indicates_claude_tui_ready_for_input(pane)
}

fn prompt_marker_confirms_prompt_ready(snapshot: &PromptReadinessSnapshot) -> bool {
    snapshot.prompt_marker_detected && !snapshot.prompt_draft_detected
}

/// Truthful root-cause attribution for a prompt-readiness timeout, derived from
/// the final snapshot.
///
/// The legacy timeout copy hardcoded `reason=prompt_marker_not_detected` even
/// when the marker WAS seen (as an unsent composer draft) or the pane capture
/// failed outright. That sent every readiness-timeout investigation down the
/// "the prompt never came back" path even when the real cause was a stuck draft
/// or a blind capture — so the returned error disagreed with the debug log,
/// which already records the computed signals. We classify by the strongest
/// evidence present in the snapshot instead.
fn prompt_ready_timeout_reason(snapshot: &PromptReadinessSnapshot) -> &'static str {
    if !snapshot.capture_available {
        // No usable pane capture: the readiness check was blind, so we cannot
        // claim anything about the marker.
        "pane_capture_unavailable"
    } else if snapshot.prompt_draft_detected {
        // The composer holds an unsent draft (`❯ <text>`), which deliberately
        // blocks the ready match in `prompt_marker_confirms_prompt_ready`.
        "prompt_draft_present"
    } else if snapshot.prompt_marker_detected {
        // The pane looked ready at least once but never confirmed within the
        // window (e.g. the ready frame flickered behind transient chrome).
        "prompt_marker_unconfirmed"
    } else {
        "prompt_marker_not_detected"
    }
}

fn transcript_idle_confirms_prompt_ready_without_capture(
    session_name: &str,
    transcript_path: Option<&std::path::Path>,
) -> bool {
    let Some(transcript_path) = transcript_path else {
        return false;
    };
    if !crate::services::tmux_diagnostics::tmux_session_has_live_pane(session_name) {
        return false;
    }
    crate::services::claude_tui::transcript_tail::observe_transcript_turn_state(transcript_path)
        == crate::services::tui_turn_state::TuiTurnState::Idle
}

fn transcript_idle_confirms_prompt_ready(
    snapshot: &PromptReadinessSnapshot,
    transcript_path: Option<&std::path::Path>,
) -> bool {
    if !snapshot.tmux_pane_alive {
        return false;
    }
    if snapshot.prompt_draft_detected
        && !claude_prompt_draft_is_idle_suggestion_tail(&snapshot.pane_tail)
    {
        return false;
    }
    transcript_path.is_some_and(|path| {
        crate::services::claude_tui::transcript_tail::observe_transcript_turn_state(path)
            == crate::services::tui_turn_state::TuiTurnState::Idle
    })
}

fn log_startup_dialog_dismiss(
    session_name: &str,
    readiness: PromptReadinessKind,
    dialog_label: &str,
    attempt: usize,
) {
    tracing::info!(
        tmux_session_name = session_name,
        readiness = readiness.label(),
        dialog = dialog_label,
        attempt,
        max_attempts = STARTUP_DIALOG_DISMISS_MAX_ATTEMPTS,
        "claude_tui dismissing startup dialog with Enter"
    );
    crate::services::claude::debug_log_to(
        "claude_tui.log",
        &format!(
            "startup dialog dismiss session={session_name} readiness={} dialog={dialog_label} attempt={attempt}/{STARTUP_DIALOG_DISMISS_MAX_ATTEMPTS}",
            readiness.label(),
        ),
    );
}

fn log_startup_dialog_untrusted_workspace(
    session_name: &str,
    readiness: PromptReadinessKind,
    workspace: &str,
) {
    tracing::warn!(
        tmux_session_name = session_name,
        readiness = readiness.label(),
        workspace,
        "claude_tui startup blocked by workspace trust dialog for untrusted path; failing fast"
    );
    crate::services::claude::debug_log_to(
        "claude_tui.log",
        &format!(
            "startup dialog untrusted workspace session={session_name} readiness={} workspace={workspace}",
            readiness.label(),
        ),
    );
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
        prompt_marker_detected = snapshot.prompt_marker_detected,
        prompt_draft_detected = snapshot.prompt_draft_detected,
        previous_tui_turn_still_running = snapshot.tmux_pane_alive && !snapshot.prompt_marker_detected,
        tmux_pane_alive = snapshot.tmux_pane_alive,
        capture_available = snapshot.capture_available,
        pane_tail = %snapshot.pane_tail,
        "claude_tui prompt readiness timed out"
    );
    crate::services::claude::debug_log_to(
        "claude_tui.log",
        &format!(
            "prompt readiness timeout session={} readiness={} timeout={}s prompt_marker_detected={} prompt_draft_detected={} previous_tui_turn_still_running={} tmux_pane_alive={} capture_available={} pane_tail:\n{}",
            session_name,
            readiness.label(),
            timeout.as_secs(),
            snapshot.prompt_marker_detected,
            snapshot.prompt_draft_detected,
            snapshot.tmux_pane_alive && !snapshot.prompt_marker_detected,
            snapshot.tmux_pane_alive,
            snapshot.capture_available,
            snapshot.pane_tail
        ),
    );
}

fn log_prompt_submit_left_draft(session_name: &str, snapshot: &PromptReadinessSnapshot) {
    tracing::warn!(
        tmux_session_name = session_name,
        prompt_marker_detected = snapshot.prompt_marker_detected,
        prompt_draft_detected = snapshot.prompt_draft_detected,
        tmux_pane_alive = snapshot.tmux_pane_alive,
        capture_available = snapshot.capture_available,
        pane_tail = %snapshot.pane_tail,
        "claude_tui prompt submit still has a draft after Enter retries"
    );
    crate::services::claude::debug_log_to(
        "claude_tui.log",
        &format!(
            "prompt submit left draft session={} prompt_marker_detected={} prompt_draft_detected={} tmux_pane_alive={} capture_available={} pane_tail:\n{}",
            session_name,
            snapshot.prompt_marker_detected,
            snapshot.prompt_draft_detected,
            snapshot.tmux_pane_alive,
            snapshot.capture_available,
            snapshot.pane_tail
        ),
    );
}

fn log_prompt_submit_capture_unavailable(
    session_name: &str,
    attempt: usize,
    snapshot: &PromptReadinessSnapshot,
) {
    tracing::warn!(
        tmux_session_name = session_name,
        attempt,
        tmux_pane_alive = snapshot.tmux_pane_alive,
        capture_available = snapshot.capture_available,
        "claude_tui post-submit capture unavailable; cannot confirm Enter took effect"
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
    fn selector_open_phase_is_slash_command_then_enter() {
        // Phase 1 only opens the overlay — no navigation keys until the
        // overlay is confirmed mounted.
        let nav = SelectorNavigation {
            slash_command: "/effort",
            total_items: 5,
            target_index: 2,
        };

        let actions = plan_selector_open(nav).unwrap();

        assert_eq!(
            actions,
            vec![
                TuiInputAction::Literal("/effort".to_string()),
                TuiInputAction::Enter,
            ]
        );
    }

    #[test]
    fn selector_navigation_homes_left_then_moves_right_to_target() {
        // 5-stop slider, target stop index 2 (high): Left x4 (home to
        // leftmost), Right x2 (to index 2), Enter — no slash/open keys here.
        let nav = SelectorNavigation {
            slash_command: "/effort",
            total_items: 5,
            target_index: 2,
        };

        let actions = plan_selector_navigation(nav).unwrap();

        assert_eq!(
            actions,
            vec![
                TuiInputAction::ArrowLeft,
                TuiInputAction::ArrowLeft,
                TuiInputAction::ArrowLeft,
                TuiInputAction::ArrowLeft,
                TuiInputAction::ArrowRight,
                TuiInputAction::ArrowRight,
                TuiInputAction::Enter,
            ]
        );
    }

    #[test]
    fn selector_navigation_leftmost_stop_needs_no_right_moves() {
        let nav = SelectorNavigation {
            slash_command: "/effort",
            total_items: 5,
            target_index: 0,
        };

        let actions = plan_selector_navigation(nav).unwrap();

        // Left x4 (home), Enter — no Right presses.
        assert_eq!(
            actions,
            vec![
                TuiInputAction::ArrowLeft,
                TuiInputAction::ArrowLeft,
                TuiInputAction::ArrowLeft,
                TuiInputAction::ArrowLeft,
                TuiInputAction::Enter,
            ]
        );
    }

    #[test]
    fn selector_navigation_homes_past_hidden_ultracode_stop_for_effort_max() {
        // Real /effort slider has 6 physical stops (incl. ultracode). Targeting
        // `max` (index 4) must press Left 5 times to clear the full width from
        // any starting stop (including ultracode), then Right 4 times.
        let nav = SelectorNavigation {
            slash_command: "/effort",
            total_items: 6,
            target_index: 4,
        };

        let actions = plan_selector_navigation(nav).unwrap();

        let left_count = actions
            .iter()
            .filter(|a| matches!(a, TuiInputAction::ArrowLeft))
            .count();
        let right_count = actions
            .iter()
            .filter(|a| matches!(a, TuiInputAction::ArrowRight))
            .count();
        assert_eq!(
            left_count, 5,
            "must clear all 6 stops to reach the leftmost"
        );
        assert_eq!(right_count, 4, "must move right to the `max` stop");
        assert_eq!(actions.last(), Some(&TuiInputAction::Enter));
    }

    #[test]
    fn selector_navigation_rightmost_stop_moves_right_to_last_index() {
        let nav = SelectorNavigation {
            slash_command: "/effort",
            total_items: 5,
            target_index: 4,
        };

        let actions = plan_selector_navigation(nav).unwrap();

        let right_count = actions
            .iter()
            .filter(|a| matches!(a, TuiInputAction::ArrowRight))
            .count();
        let left_count = actions
            .iter()
            .filter(|a| matches!(a, TuiInputAction::ArrowLeft))
            .count();
        assert_eq!(
            left_count, 4,
            "home move must press Left total_items-1 times"
        );
        assert_eq!(right_count, 4, "must move right to the last stop");
        assert_eq!(actions.last(), Some(&TuiInputAction::Enter));
    }

    #[test]
    fn selector_plans_reject_out_of_range_target() {
        let nav = SelectorNavigation {
            slash_command: "/effort",
            total_items: 5,
            target_index: 5,
        };

        assert!(
            plan_selector_open(nav)
                .unwrap_err()
                .contains("out of range")
        );
        assert!(
            plan_selector_navigation(nav)
                .unwrap_err()
                .contains("out of range")
        );
    }

    #[test]
    fn selector_plans_reject_empty_selector() {
        let nav = SelectorNavigation {
            slash_command: "/effort",
            total_items: 0,
            target_index: 0,
        };

        assert!(
            plan_selector_open(nav)
                .unwrap_err()
                .contains("no selectable items")
        );
        assert!(
            plan_selector_navigation(nav)
                .unwrap_err()
                .contains("no selectable items")
        );
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
    fn prompt_marker_does_not_confirm_readiness_when_draft_is_present() {
        let snapshot = PromptReadinessSnapshot {
            prompt_marker_detected: true,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "\u{276f} stale draft".to_string(),
        };

        assert!(!prompt_marker_confirms_prompt_ready(&snapshot));
    }

    #[test]
    fn prompt_submit_retries_only_when_live_pane_still_has_draft() {
        let draft_snapshot = PromptReadinessSnapshot {
            prompt_marker_detected: true,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "\u{276f} draft".to_string(),
        };
        assert!(prompt_submit_needs_enter_retry(&draft_snapshot));

        let active_snapshot = PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "✳ Architecting...".to_string(),
        };
        assert!(!prompt_submit_needs_enter_retry(&active_snapshot));

        let inconsistent_snapshot = PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "stale draft heuristic without prompt marker".to_string(),
        };
        assert!(!prompt_submit_needs_enter_retry(&inconsistent_snapshot));

        let dead_snapshot = PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: true,
            tmux_pane_alive: false,
            capture_available: true,
            pane_tail: "stale draft".to_string(),
        };
        assert!(!prompt_submit_needs_enter_retry(&dead_snapshot));
    }

    #[test]
    fn prompt_submit_confirmation_decision_retries_enter_then_fails_stuck_draft() {
        let snapshot = PromptReadinessSnapshot {
            prompt_marker_detected: true,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "\u{276f} draft".to_string(),
        };

        assert_eq!(
            prompt_submit_confirmation_decision(&snapshot, 0, 2),
            PromptSubmitConfirmationDecision::RetryEnter
        );
        assert_eq!(
            prompt_submit_confirmation_decision(&snapshot, 2, 2),
            PromptSubmitConfirmationDecision::FailedDraftStuck
        );
    }

    #[test]
    fn prompt_submit_confirmation_decision_retries_capture_without_enter() {
        let snapshot = PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: false,
            pane_tail: "<capture unavailable>".to_string(),
        };

        assert_eq!(
            prompt_submit_confirmation_decision(&snapshot, 0, 2),
            PromptSubmitConfirmationDecision::RetrySnapshot
        );
        assert_eq!(
            prompt_submit_confirmation_decision(&snapshot, 2, 2),
            PromptSubmitConfirmationDecision::FailedCaptureUnavailable
        );
    }

    #[test]
    fn prompt_submit_confirmation_decision_handles_submitted_and_dead_pane() {
        let submitted = PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "✳ Architecting...".to_string(),
        };
        assert_eq!(
            prompt_submit_confirmation_decision(&submitted, 0, 2),
            PromptSubmitConfirmationDecision::Submitted
        );

        let dead = PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: false,
            tmux_pane_alive: false,
            capture_available: false,
            pane_tail: "<capture unavailable>".to_string(),
        };
        assert_eq!(
            prompt_submit_confirmation_decision(&dead, 0, 2),
            PromptSubmitConfirmationDecision::FailedSessionDead
        );
    }

    #[test]
    fn prompt_submit_settle_uses_short_initial_and_long_retry_delay() {
        assert!(prompt_submit_settle_for_attempt(0) < prompt_submit_settle_for_attempt(1));
        assert_eq!(
            prompt_submit_settle_for_attempt(1),
            PROMPT_SUBMIT_RETRY_SETTLE
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
line 13";

        assert!(!pane_looks_ready_for_prompt(pane));
    }

    #[test]
    fn idle_transcript_can_confirm_readiness_when_prompt_marker_is_absent() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            r#"{"type":"system","subtype":"turn_duration","sessionId":"s"}"#,
        )
        .unwrap();
        let snapshot = PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "status footer without prompt glyph".to_string(),
        };

        assert!(transcript_idle_confirms_prompt_ready(
            &snapshot,
            Some(file.path())
        ));
    }

    #[test]
    fn idle_transcript_does_not_override_active_prompt_draft() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            r#"{"type":"system","subtype":"turn_duration","sessionId":"s"}"#,
        )
        .unwrap();
        let snapshot = PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "\u{276f} operator draft".to_string(),
        };

        assert!(!transcript_idle_confirms_prompt_ready(
            &snapshot,
            Some(file.path())
        ));
    }

    #[test]
    fn idle_transcript_accepts_claude_suggestion_prompt_chrome() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            r#"{"type":"system","subtype":"turn_duration","sessionId":"s"}"#,
        )
        .unwrap();
        let snapshot = PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "\
✻ Worked for 2s
────────────────────────────────────────────────────────────────────────────
❯\u{00a0}좋아, 잘 동작하네
────────────────────────────────────────────────────────────────────────────
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done
  ⏵⏵ bypass permissions on"
                .to_string(),
        };

        assert!(claude_prompt_draft_is_idle_suggestion_tail(
            &snapshot.pane_tail
        ));
        assert!(transcript_idle_confirms_prompt_ready(
            &snapshot,
            Some(file.path())
        ));
    }

    #[test]
    fn idle_transcript_does_not_override_dead_pane() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            r#"{"type":"system","subtype":"turn_duration","sessionId":"s"}"#,
        )
        .unwrap();
        let snapshot = PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: false,
            tmux_pane_alive: false,
            capture_available: true,
            pane_tail: "dead pane".to_string(),
        };

        assert!(!transcript_idle_confirms_prompt_ready(
            &snapshot,
            Some(file.path())
        ));
    }

    #[test]
    fn non_idle_transcript_does_not_confirm_readiness() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"still streaming"}]}}"#,
        )
        .unwrap();
        let snapshot = PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "streaming turn".to_string(),
        };

        assert!(!transcript_idle_confirms_prompt_ready(
            &snapshot,
            Some(file.path())
        ));
    }

    #[test]
    fn prompt_ready_timeouts_are_split_for_fresh_and_followup_turns() {
        assert_eq!(PromptReadinessKind::FreshTurn.timeout().as_secs(), 120);
        assert_eq!(PromptReadinessKind::Followup.timeout().as_secs(), 45);
    }

    #[test]
    fn event_budget_is_shorter_than_full_timeout() {
        // The event-budget is meant to fail fast and yield to the polling
        // fallback long before the legacy timeout would fire.
        for kind in [
            PromptReadinessKind::FreshTurn,
            PromptReadinessKind::Followup,
        ] {
            assert!(
                kind.event_budget() < kind.timeout(),
                "event budget for {:?} must be smaller than legacy timeout",
                kind
            );
        }
        // Fresh starts get more headroom than follow-ups.
        assert!(
            PromptReadinessKind::FreshTurn.event_budget()
                > PromptReadinessKind::Followup.event_budget()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fast_path_returns_ready_when_notify_fires_within_budget() {
        let notify = Arc::new(Notify::new());
        let trigger = notify.clone();
        // Fire the signal shortly after the waiter registers.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            trigger.notify_waiters();
        });

        let outcome = tokio::task::spawn_blocking(move || {
            wait_for_prompt_ready_event(notify, Duration::from_secs(2))
        })
        .await
        .expect("blocking task panicked");

        assert_eq!(outcome, HookFastPathOutcome::Ready);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fast_path_returns_pending_when_budget_elapses_without_notify() {
        let notify = Arc::new(Notify::new());
        let outcome = tokio::task::spawn_blocking(move || {
            wait_for_prompt_ready_event(notify, Duration::from_millis(50))
        })
        .await
        .expect("blocking task panicked");

        assert_eq!(outcome, HookFastPathOutcome::Pending);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fast_path_does_not_panic_on_current_thread_runtime() {
        // Regression: `tokio::task::block_in_place` panics on current-thread
        // runtimes. Many AgentDesk worker entry points (cluster watchers,
        // turn-bridge, doctor, etc.) build current-thread runtimes, so the
        // hook fast-path must never assume multi-thread. Budget is short so
        // the test stays fast.
        let notify = Arc::new(Notify::new());
        let outcome = wait_for_prompt_ready_event(notify, Duration::from_millis(20));
        assert_eq!(outcome, HookFastPathOutcome::Pending);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fast_path_returns_ready_on_current_thread_runtime_when_notify_fires() {
        // Even on a current-thread runtime the dedicated worker thread we
        // spawn for the wait must observe a `notify_waiters` signal that
        // fires after the waiter registers.
        let notify = Arc::new(Notify::new());
        let trigger = notify.clone();
        let task = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            trigger.notify_waiters();
        });
        let outcome = wait_for_prompt_ready_event(notify, Duration::from_secs(2));
        task.join().expect("trigger thread joined");
        assert_eq!(outcome, HookFastPathOutcome::Ready);
    }

    #[test]
    fn fast_path_works_without_ambient_runtime() {
        // Exercises the fallback runtime branch that callers without tokio
        // runtime hit (e.g. plain sync test contexts). Budget is short enough
        // to keep the test cheap.
        let notify = Arc::new(Notify::new());
        let outcome = wait_for_prompt_ready_event(notify, Duration::from_millis(30));
        assert_eq!(outcome, HookFastPathOutcome::Pending);
    }

    // #2445 regression — `Notified::enable()` registers the waker before the
    // first `.await`. A `notify_waiters()` invocation that fires AFTER
    // `enable()` returned must still wake the waiter, even if the future has
    // not yet been polled into its `.await`. This is the load-bearing
    // semantic that lets us call `enable()` then take the pre-snapshot
    // without dropping a hook that races against the snapshot syscalls.
    //
    // We exercise the contract directly on `tokio::sync::Notify` rather than
    // through the full fast-path so the test is deterministic — we control
    // the exact ordering of enable → notify_waiters → poll the future.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enable_then_notify_waiters_is_observed_when_future_is_polled_later() {
        let notify = Arc::new(Notify::new());

        let notified = notify.notified();
        tokio::pin!(notified);
        // Step 1: register the waker (no .await yet).
        notified.as_mut().enable();

        // Step 2: fire notify_waiters BEFORE the future is ever polled into
        // its wait. Without `enable()` this signal would be dropped because
        // notify_waiters only wakes already-registered waiters.
        notify.notify_waiters();

        // Step 3: now poll the future. It must complete promptly, not block.
        let start = std::time::Instant::now();
        tokio::time::timeout(Duration::from_millis(500), &mut notified)
            .await
            .expect("enabled Notified must observe a prior notify_waiters");
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "enabled Notified should resolve immediately; took {:?}",
            start.elapsed()
        );
    }

    // #2445 regression — the production wait_for_prompt_ready_event wrapper
    // builds the Notified-with-enable() future. After the wrapper returns the
    // outer future starts polling, so a notify_waiters fired *just before*
    // the select is polled (but after enable() ran) must still wake the
    // waiter within the test's tight latency budget.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_for_prompt_ready_event_observes_notify_fired_during_setup() {
        let notify = Arc::new(Notify::new());
        let trigger = notify.clone();

        // Race the trigger against the spawn_blocking worker. With `enable()`
        // wired into the future body the worker registers BEFORE awaiting,
        // so even if the notify_waiters call lands very close to the select
        // we still observe it. Generous budget so the test is robust even
        // on slow CI runners.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            trigger.notify_waiters();
        });

        let outcome = tokio::task::spawn_blocking(move || {
            wait_for_prompt_ready_event(notify, Duration::from_secs(2))
        })
        .await
        .expect("blocking task panicked");

        assert_eq!(outcome, HookFastPathOutcome::Ready);
    }

    // #2445 regression — even if `notify_waiters` is invoked BEFORE the
    // wait_for_prompt_ready_event call (simulating a Stop hook that landed
    // during the caller's pre-flight work), the wait must NOT block on that
    // missed edge. We accept either outcome: returning `Pending` quickly when
    // the budget is short is acceptable, but the call MUST NOT silently
    // capture an edge that fired before subscription (that would be a bug in
    // the other direction). The point is purely that the call completes
    // within its budget.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fast_path_returns_within_budget_even_with_pre_subscription_edges() {
        let notify = Arc::new(Notify::new());

        // Simulate a missed edge before the waiter subscribes.
        notify.notify_waiters();

        let start = std::time::Instant::now();
        let outcome = tokio::task::spawn_blocking({
            let notify = notify.clone();
            move || wait_for_prompt_ready_event(notify, Duration::from_millis(80))
        })
        .await
        .expect("blocking task panicked");

        assert_eq!(outcome, HookFastPathOutcome::Pending);
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "fast path must respect its budget; took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_prompt_ready_notify_round_trips_via_hook_server_helper() {
        // End-to-end wiring sanity: hook_server::signal_prompt_ready_for_test
        // wakes the same global Notify that input.rs consumes.
        let notify = crate::services::claude_tui::hook_server::prompt_ready_notify();
        let waiter_notify = notify.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            crate::services::claude_tui::hook_server::signal_prompt_ready_for_test();
        });

        let outcome = tokio::task::spawn_blocking(move || {
            wait_for_prompt_ready_event(waiter_notify, Duration::from_secs(2))
        })
        .await
        .expect("blocking task panicked");

        assert_eq!(outcome, HookFastPathOutcome::Ready);
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

    // The returned timeout copy must reflect the ACTUAL snapshot, not the legacy
    // hardcoded `reason=prompt_marker_not_detected; previous_tui_turn_still_running=true`.
    #[test]
    fn prompt_ready_timeout_reason_reflects_snapshot() {
        let base = PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: String::new(),
        };

        // No usable capture wins over everything else.
        let blind = PromptReadinessSnapshot {
            capture_available: false,
            prompt_marker_detected: true,
            prompt_draft_detected: true,
            ..base.clone()
        };
        assert_eq!(
            prompt_ready_timeout_reason(&blind),
            "pane_capture_unavailable"
        );

        // An unsent draft is the cause, and the turn is NOT still running.
        let draft = PromptReadinessSnapshot {
            prompt_marker_detected: true,
            prompt_draft_detected: true,
            ..base.clone()
        };
        assert_eq!(prompt_ready_timeout_reason(&draft), "prompt_draft_present");
        assert!(!(draft.tmux_pane_alive && !draft.prompt_marker_detected));

        // Marker seen but never confirmed.
        let unconfirmed = PromptReadinessSnapshot {
            prompt_marker_detected: true,
            ..base.clone()
        };
        assert_eq!(
            prompt_ready_timeout_reason(&unconfirmed),
            "prompt_marker_unconfirmed"
        );

        // The genuine "prompt never came back" case — and the only one that
        // reports the turn as plausibly still running.
        assert_eq!(
            prompt_ready_timeout_reason(&base),
            "prompt_marker_not_detected"
        );
        assert!(base.tmux_pane_alive && !base.prompt_marker_detected);
    }

    // #2416: wait_for_prompt_ready must be reachable as a public API so the
    // busy-followup wait+retry paths in claude.rs and the Discord router can
    // call it. Trying to call it on a non-existent session must return Err
    // (so callers can fall back to the busy notice deterministically).
    #[test]
    fn wait_for_prompt_ready_is_public_and_returns_err_for_missing_session() {
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
}
