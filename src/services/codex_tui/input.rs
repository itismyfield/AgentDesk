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
//! The wait path combines rollout envelopes, provider hook events, and pane
//! verification:
//!
//! 1. **Rollout composer-ready fast path.** `codex_tui::rollout_tail`
//!    synthesizes an `event_msg/composer_ready` envelope after observing the
//!    Codex rollout terminal signal for the tmux session. This is the primary
//!    readiness signal because it comes from the JSONL turn lifecycle rather
//!    than brittle pane scraping.
//!
//! 2. **Provider hook Stop/SubagentStop fast path.** Codex hook events wake
//!    the same prompt-ready notify used by Claude. The hook only shortens the
//!    wait; we still take a post-event pane snapshot before returning ready.
//!
//! 3. **Bottom-anchored composer frame.** The Codex TUI
//!    composer renders at the *bottom* of the pane. We require that
//!    a composer-edge line (mostly Unicode box-drawing chars) appear
//!    within the last [`COMPOSER_EDGE_BOTTOM_WINDOW`] non-empty lines
//!    AND that a footer-hint line (`Esc to interrupt`, `Ctrl+J newline`,
//!    or similar) appear within [`FOOTER_HINT_BOTTOM_WINDOW`] of the
//!    pane bottom. Bottom-anchoring kills the false positive where a
//!    model-rendered table several screens up still has glyphs in
//!    the scan tail.
//!
//! 4. **Compact prompt marker.** After context compaction or hook-review
//!    prompts, Codex may draw the prompt as `› ...` plus a model/status
//!    footer instead of the rounded box. That compact marker is treated as
//!    ready only when it is bottom-anchored and paired with the status line
//!    beneath it.
//!
//! 5. **Adjacency.** The footer hint and the composer edge must
//!    co-occur within [`COMPOSER_FOOTER_ADJACENCY_LINES`] of each
//!    other. A copied UI frame in assistant prose will not satisfy
//!    this because it lacks the live footer underneath, and a real
//!    footer never lives more than a few rows below the composer.
//!
//! 6. **Live pane fallback gate.** When no rollout composer-ready envelope
//!    is available, a dead pane cannot be ready; the capture fallback fails
//!    fast with a structured error instead of waiting out the full timeout,
//!    so the caller can decide to recreate the session.
//!
//! ## Cancellation contract
//!
//! [`wait_until_codex_tui_input_ready`] accepts an optional
//! [`CancelToken`]. The wait checks the token before each capture
//! and after each sleep so a `/stop` arriving while the TUI is hung
//! (live pane, never-arriving composer) crosses the boundary inside
//! ~one wait-interval rather than waiting out the 45s/120s budget.
//! Prompt delivery also checks the same token between tmux input
//! actions, so a long literal prompt split into multiple chunks can
//! be interrupted before the next chunk is sent.
//! Cancellation returns a distinct
//! [`PROMPT_READY_CANCELLED_ERROR`] string so the caller can release
//! the turn without recreating the session — this matches the cancel
//! boundary contract in PR #2284 where user-cancel beats deadline.
//!
//! ## Timeout / fail-safe
//!
//! Fresh launches get a longer budget than follow-ups, matching the
//! Claude TUI split. The timeout returns a structured error prefixed
//! with [`PROMPT_READY_TIMEOUT_ERROR_PREFIX`] so callers can decide
//! whether to recreate the session or surface a user-visible error
//! — same contract as `claude_tui::input::is_prompt_ready_timeout_error`.
//! Combined with the Codex TUI cancel boundary (PR #2284), a hung TUI
//! has three independent escape hatches: cancel (above), this
//! readiness timeout (caller recreates), and the rollout deadline
//! (caller emits `Done`).

use std::collections::HashSet;
use std::process::Output;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::services::provider::{CancelToken, cancel_requested};
use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::sync::Notify;

// #4411: warm follow-up uses the action-plan layer in production. Keeping the
// final Enter as a distinct action is what lets submission confirmation reason
// about a visible, still-stranded composer draft without ever double-submitting.
const DEFAULT_LITERAL_CHUNK_CHARS: usize = 1800;

static CODEX_COMPOSER_MUTATION_LOCKS: LazyLock<dashmap::DashMap<String, Arc<Mutex<()>>>> =
    LazyLock::new(dashmap::DashMap::new);

fn with_composer_mutation_lock<R>(session_name: &str, operation: impl FnOnce() -> R) -> R {
    let composer_lock = CODEX_COMPOSER_MUTATION_LOCKS
        .entry(session_name.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    let _composer_guard = composer_lock
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    operation()
}
const PROMPT_INPUT_BEFORE_ENTER_SETTLE: Duration = Duration::from_millis(200);
const PROMPT_SUBMIT_INITIAL_SETTLE: Duration = Duration::from_millis(150);
const PROMPT_SUBMIT_DRAFT_RECHECK_SETTLE: Duration = Duration::from_millis(250);
const PROMPT_READY_CAPTURE_SCROLLBACK: i32 = -80;
const PROMPT_READY_DEBUG_TAIL_LINES: usize = 24;
const PROMPT_READY_DEBUG_TAIL_BYTES: usize = 4096;

/// Number of trailing non-empty lines scanned for *any* part of the
/// composer pattern. Sets the outer search window.
const PROMPT_READY_SCAN_LINES: usize = 14;
/// A composer-edge line must appear within this many trailing non-empty
/// lines (counted from pane bottom). Bottom-anchoring rejects stale
/// composer frames scrolled deep into history.
const COMPOSER_EDGE_BOTTOM_WINDOW: usize = 6;
/// A footer hint must appear within this many trailing non-empty lines.
/// Codex TUI prints `Esc to interrupt` etc. immediately under the
/// composer; in practice it sits in the last 1–3 visible rows.
const FOOTER_HINT_BOTTOM_WINDOW: usize = 5;
/// Compact Codex prompt (`› ...`) must be very near the pane bottom.
const COMPACT_PROMPT_BOTTOM_WINDOW: usize = 4;
/// Composer edge and footer hint must co-occur within this many lines
/// of each other so a screenshot of the TUI in assistant prose cannot
/// pair with a real footer further down the buffer.
///
/// Kept strictly tighter than [`COMPOSER_EDGE_BOTTOM_WINDOW`] so the
/// adjacency gate is not redundant with the bottom-anchor windows.
const COMPOSER_FOOTER_ADJACENCY_LINES: usize = 3;

pub const FRESH_PROMPT_READY_TIMEOUT: Duration = Duration::from_secs(120);
pub const FOLLOWUP_PROMPT_READY_TIMEOUT: Duration = Duration::from_secs(45);
const FRESH_PROMPT_READY_EVENT_BUDGET: Duration = Duration::from_millis(1500);
const FOLLOWUP_PROMPT_READY_EVENT_BUDGET: Duration = Duration::from_millis(1500);
/// Post-turn handoff probe budget. Sized to fit inside the turn-bridge
/// `terminal_control_drain_until` window (250ms) so any
/// `StreamMessage::RuntimeReady` / failure `Done` we emit after this
/// probe still reaches the bridge before it finalises the inflight on
/// the rollout-tail `Done`. See #2325 / Codex review.
pub const POST_TURN_HANDOFF_PROBE_TIMEOUT: Duration = Duration::from_millis(200);
const POST_TURN_HANDOFF_EVENT_BUDGET: Duration = Duration::from_millis(150);
const PROMPT_READY_POST_EVENT_SETTLE: Duration = Duration::from_millis(25);
const PROMPT_READY_TIMEOUT_ERROR_PREFIX: &str = "timeout waiting for codex tui";
const PROMPT_READY_SESSION_DEAD_ERROR: &str =
    "codex tui session died before prompt input was ready";
pub const PROMPT_READY_CANCELLED_ERROR: &str = "codex tui prompt readiness wait cancelled";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptReadinessKind {
    // #3034: constructed only by tests now (the fresh-turn send path was
    // retired); kept for taxonomy completeness — `timeout`/`label` match it.
    #[allow(dead_code)]
    FreshTurn,
    Followup,
    /// Bounded post-turn probe used by the Codex TUI launch frame to
    /// gate the `RuntimeReady` handoff on a live composer without
    /// racing the turn-bridge drain window. See [`POST_TURN_HANDOFF_PROBE_TIMEOUT`].
    PostTurnHandoff,
}

/// Resolve the Follow-up readiness budget from the live config snapshot,
/// falling back to the compiled-in 45s default. `config_live_reload::current()`
/// returns `None` before boot install and in unit tests, so the const stays the
/// safe fallback. A configured `0` is treated as unset to avoid an
/// immediate-timeout footgun. See `RuntimeSettingsConfig::followup_prompt_ready_timeout_secs`.
fn followup_prompt_ready_timeout() -> Duration {
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
            Self::Followup => followup_prompt_ready_timeout(),
            Self::PostTurnHandoff => POST_TURN_HANDOFF_PROBE_TIMEOUT,
        }
    }

    fn event_budget(self) -> Duration {
        match self {
            Self::FreshTurn => FRESH_PROMPT_READY_EVENT_BUDGET,
            Self::Followup => FOLLOWUP_PROMPT_READY_EVENT_BUDGET,
            Self::PostTurnHandoff => POST_TURN_HANDOFF_EVENT_BUDGET,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::FreshTurn => "fresh",
            Self::Followup => "follow-up",
            Self::PostTurnHandoff => "post-turn-handoff",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptDraftPolicy {
    RejectDraft,
    // #3034: the accept-for-clear policy fed the retired draft-clear path;
    // retained as the second policy state pinned by `accepts`.
    #[allow(dead_code)]
    AcceptDraftForClear,
}

impl PromptDraftPolicy {
    fn accepts(self, snapshot: &PromptReadinessSnapshot) -> bool {
        snapshot.composer_marker_detected
            && (matches!(self, Self::AcceptDraftForClear) || !snapshot.prompt_draft_detected)
    }

    fn should_block_rollout_ready(self, snapshot: &PromptReadinessSnapshot) -> bool {
        matches!(self, Self::RejectDraft)
            && snapshot.capture_available
            && snapshot.prompt_draft_detected
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RolloutReadySnapshotDecision {
    Accept,
    Ignore,
    SessionDead,
}

fn decide_rollout_ready_snapshot(
    snapshot: &PromptReadinessSnapshot,
    draft_policy: PromptDraftPolicy,
    warm_followup_enabled: bool,
) -> RolloutReadySnapshotDecision {
    if !warm_followup_enabled {
        // Exact pre-#4411 ordering for the kill-switch path: a visible draft
        // blocks first, then a captured dead pane fails, and an otherwise
        // uncorroborated rollout-ready envelope is accepted.
        if draft_policy.should_block_rollout_ready(snapshot) {
            return RolloutReadySnapshotDecision::Ignore;
        }
        if snapshot.capture_available && !snapshot.tmux_pane_alive {
            return RolloutReadySnapshotDecision::SessionDead;
        }
        return RolloutReadySnapshotDecision::Accept;
    }
    if snapshot.capture_available && !snapshot.tmux_pane_alive {
        return RolloutReadySnapshotDecision::SessionDead;
    }
    if snapshot.capture_available && !draft_policy.accepts(snapshot) {
        return RolloutReadySnapshotDecision::Ignore;
    }
    RolloutReadySnapshotDecision::Accept
}

/// Outcome of the provider hook-event fast path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HookFastPathOutcome {
    /// Rollout tail observed an explicit/synthetic composer-ready envelope for
    /// this tmux session. This is accepted without pane-capture corroboration.
    RolloutComposerReady,
    /// Prompt marker was already visible in the subscribe-before-snapshot check.
    PreSnapshotReady,
    /// Pane disappeared before a prompt-ready event could help.
    PreSnapshotSessionDead,
    /// The caller cancelled while the hook fast path was waiting.
    Cancelled,
    /// Stop/SubagentStop arrived inside the hook budget.
    Ready,
    /// No hook arrived inside the hook budget; fall back to pane polling.
    Pending,
}

struct RolloutComposerReadyState {
    notify: Arc<Notify>,
    ready_sessions: Mutex<HashSet<String>>,
}

static ROLLOUT_COMPOSER_READY_STATE: OnceLock<RolloutComposerReadyState> = OnceLock::new();

fn rollout_composer_ready_state() -> &'static RolloutComposerReadyState {
    ROLLOUT_COMPOSER_READY_STATE.get_or_init(|| RolloutComposerReadyState {
        notify: Arc::new(Notify::new()),
        ready_sessions: Mutex::new(HashSet::new()),
    })
}

pub(crate) fn record_rollout_composer_ready(session_name: &str) {
    if session_name.trim().is_empty() {
        return;
    }
    let state = rollout_composer_ready_state();
    state
        .ready_sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(session_name.to_string());
    state.notify.notify_waiters();
}

#[cfg(test)]
fn mark_rollout_composer_busy(session_name: &str) {
    rollout_composer_ready_state()
        .ready_sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(session_name);
}

fn rollout_composer_ready_observed(session_name: &str) -> bool {
    rollout_composer_ready_state()
        .ready_sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(session_name)
}

fn take_rollout_composer_ready(session_name: &str) -> bool {
    rollout_composer_ready_state()
        .ready_sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(session_name)
}

fn observe_rollout_composer_ready_for_prompt_wait(session_name: &str) -> bool {
    if super::warm_followup::codex_tui_warm_followup_enabled() {
        take_rollout_composer_ready(session_name)
    } else {
        // Kill-switch parity: preserve the pre-#4411 non-consuming readiness
        // observation together with the legacy cleanup/relaunch path.
        rollout_composer_ready_observed(session_name)
    }
}

fn rollout_composer_ready_notify() -> Arc<Notify> {
    rollout_composer_ready_state().notify.clone()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptReadinessSnapshot {
    pub composer_marker_detected: bool,
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

#[allow(dead_code)] // #3034: test-only (see header note).
pub fn plan_cancel() -> Vec<TuiInputAction> {
    vec![TuiInputAction::Escape]
}

pub fn is_prompt_ready_timeout_error(error: &str) -> bool {
    error.starts_with(PROMPT_READY_TIMEOUT_ERROR_PREFIX)
}

pub fn is_session_dead_error(error: &str) -> bool {
    error == PROMPT_READY_SESSION_DEAD_ERROR
}

pub fn is_prompt_ready_cancelled_error(error: &str) -> bool {
    error == PROMPT_READY_CANCELLED_ERROR
}

/// Capture the current pane and classify whether the Codex composer
/// is visible. Returned regardless of timing so callers can log the
/// state at decision points.
pub fn prompt_readiness_snapshot(session_name: &str) -> PromptReadinessSnapshot {
    // ANSI capture is the canonical snapshot. Its deterministic plain-text
    // projection keeps marker and draft classification tied to the same pane
    // revision, while retaining the dim-placeholder signal plain capture loses.
    let pane_with_escapes = crate::services::platform::tmux::capture_pane_with_escapes(
        session_name,
        PROMPT_READY_CAPTURE_SCROLLBACK,
    );
    let (composer_marker_detected, prompt_draft_detected, pane_tail) = pane_with_escapes
        .as_deref()
        .map(prompt_readiness_from_ansi_pane)
        .unwrap_or_else(|| (false, false, "<capture unavailable>".to_string()));
    PromptReadinessSnapshot {
        composer_marker_detected,
        prompt_draft_detected,
        tmux_pane_alive: crate::services::tmux_diagnostics::tmux_session_has_live_pane(
            session_name,
        ),
        capture_available: pane_with_escapes.is_some(),
        pane_tail,
    }
}

fn prompt_readiness_from_ansi_pane(pane_with_escapes: &str) -> (bool, bool, String) {
    let pane = strip_ansi_escape_sequences(pane_with_escapes);
    let composer_marker_detected = pane_looks_ready_for_codex_prompt_with_ansi(pane_with_escapes);
    let dim_placeholder_detected = pane_has_dim_legacy_codex_prompt_in_pane(pane_with_escapes);
    let prompt_draft_detected = !dim_placeholder_detected && pane_has_codex_prompt_draft(&pane);
    (
        composer_marker_detected,
        prompt_draft_detected,
        prompt_ready_debug_tail(&pane),
    )
}

/// Block until the Codex TUI composer is visible or `timeout` elapses.
/// Returns `Ok(())` on success, a session-dead error if the tmux pane
/// disappears, a cancelled error if `cancel_token` flips, or a timeout
/// error prefixed with [`PROMPT_READY_TIMEOUT_ERROR_PREFIX`].
///
/// Cancellation is checked before each pane capture and after each
/// sleep so a `/stop` arriving while the TUI is hung (live pane,
/// never-arriving composer) crosses the boundary inside ~one
/// wait-interval.
///
/// #2399 HIGH 1 — hard deadline contract:
///
/// The loop computes an absolute `deadline = start + timeout`, checks it
/// before each capture, and only ever sleeps for `min(wait_interval,
/// deadline - now)`. Without this, the legacy loop could capture at
/// `start + 100ms`, see the composer not ready, and then sleep the full
/// `wait_interval` (up to 1s) before re-checking — overshooting the
/// caller's budget by up to ~1s. For `PromptReadinessKind::PostTurnHandoff`
/// (200ms budget that must fit inside the bridge's 250ms terminal drain,
/// see codex.rs) that overshoot meant the `RuntimeReady` / failure `Done`
/// was emitted AFTER the bridge had already finalised the inflight.
pub fn wait_until_codex_tui_input_ready(
    session_name: &str,
    readiness: PromptReadinessKind,
    cancel_token: Option<&Arc<CancelToken>>,
) -> Result<(), String> {
    wait_until_codex_tui_input_ready_with_policy(
        session_name,
        readiness,
        cancel_token,
        PromptDraftPolicy::RejectDraft,
    )
}

fn wait_until_codex_tui_input_ready_with_policy(
    session_name: &str,
    readiness: PromptReadinessKind,
    cancel_token: Option<&Arc<CancelToken>>,
    draft_policy: PromptDraftPolicy,
) -> Result<(), String> {
    let timeout = readiness.timeout();
    let start = Instant::now();
    let deadline = start + timeout;
    let mut wait_interval = Duration::from_millis(100);
    let token_ref = cancel_token.map(Arc::as_ref);
    // Cancel-takes-precedence helper: any error path must consult the
    // token first so a /stop arriving during the capture or between
    // checks gets reported as cancellation, not timeout/session-dead.
    // This matches the cancel-boundary contract in PR #2284 (user
    // cancel > deadline > session death).
    let cancel_check = || -> Option<String> {
        if cancel_requested(token_ref) {
            Some(PROMPT_READY_CANCELLED_ERROR.to_string())
        } else {
            None
        }
    };

    // Emit the typed timeout error string. Threaded in two places (pre-
    // capture deadline check and post-capture deadline check) so the
    // formatting stays identical and a future copy refactor only has to
    // touch one spot.
    let timeout_error = |snapshot: &PromptReadinessSnapshot| -> String {
        log_prompt_ready_timeout(session_name, readiness, timeout, snapshot);
        format!(
            "{PROMPT_READY_TIMEOUT_ERROR_PREFIX} {} prompt input readiness after {}s; reason=composer_not_detected; previous_tui_turn_still_running=true; capture_available={}",
            readiness.label(),
            timeout.as_secs(),
            snapshot.capture_available
        )
    };

    let notify = crate::services::claude_tui::hook_server::prompt_ready_notify();
    let (fast_path, post_event_snapshot) = run_prompt_ready_fast_path(
        notify,
        session_name.to_string(),
        readiness.event_budget(),
        deadline,
        cancel_token.cloned(),
        draft_policy,
    );

    match fast_path {
        HookFastPathOutcome::RolloutComposerReady => {
            if let Some(err) = cancel_check() {
                return Err(err);
            }
            if Instant::now() >= deadline {
                let snapshot = prompt_readiness_snapshot(session_name);
                return Err(timeout_error(&snapshot));
            }
            let snapshot = prompt_readiness_snapshot(session_name);
            match decide_rollout_ready_snapshot(
                &snapshot,
                draft_policy,
                super::warm_followup::codex_tui_warm_followup_enabled(),
            ) {
                RolloutReadySnapshotDecision::SessionDead => {
                    return Err(PROMPT_READY_SESSION_DEAD_ERROR.to_string());
                }
                RolloutReadySnapshotDecision::Ignore => tracing::warn!(
                    tmux_session_name = session_name,
                    readiness = readiness.label(),
                    pane_tail = %snapshot.pane_tail,
                    "codex_tui rollout composer_ready ignored because the captured pane is not input-ready"
                ),
                RolloutReadySnapshotDecision::Accept => {
                    tracing::debug!(
                        tmux_session_name = session_name,
                        readiness = readiness.label(),
                        elapsed_ms = start.elapsed().as_millis() as u64,
                        "codex_tui prompt ready via rollout composer_ready envelope"
                    );
                    return Ok(());
                }
            }
        }
        HookFastPathOutcome::PreSnapshotReady => {
            if let Some(err) = cancel_check() {
                return Err(err);
            }
            if Instant::now() >= deadline {
                let snapshot = prompt_readiness_snapshot(session_name);
                return Err(timeout_error(&snapshot));
            }
            tracing::debug!(
                tmux_session_name = session_name,
                readiness = readiness.label(),
                elapsed_ms = start.elapsed().as_millis() as u64,
                "codex_tui prompt ready on pre-snapshot (no event wait needed)"
            );
            return Ok(());
        }
        HookFastPathOutcome::PreSnapshotSessionDead => {
            if let Some(err) = cancel_check() {
                return Err(err);
            }
            if Instant::now() >= deadline {
                let snapshot = prompt_readiness_snapshot(session_name);
                return Err(timeout_error(&snapshot));
            }
            return Err(PROMPT_READY_SESSION_DEAD_ERROR.to_string());
        }
        HookFastPathOutcome::Cancelled => return Err(PROMPT_READY_CANCELLED_ERROR.to_string()),
        HookFastPathOutcome::Ready | HookFastPathOutcome::Pending => {}
    }

    if let Some(err) = cancel_check() {
        return Err(err);
    }
    if let Some(snapshot) = post_event_snapshot {
        if draft_policy.accepts(&snapshot) {
            if let Some(err) = cancel_check() {
                return Err(err);
            }
            if Instant::now() >= deadline {
                return Err(timeout_error(&snapshot));
            }
            tracing::debug!(
                tmux_session_name = session_name,
                readiness = readiness.label(),
                hook_event_fast_path_hit = matches!(fast_path, HookFastPathOutcome::Ready),
                elapsed_ms = start.elapsed().as_millis() as u64,
                "codex_tui prompt ready via hook event fast path"
            );
            return Ok(());
        }
        if !snapshot.tmux_pane_alive {
            if let Some(err) = cancel_check() {
                return Err(err);
            }
            if Instant::now() >= deadline {
                return Err(timeout_error(&snapshot));
            }
            return Err(PROMPT_READY_SESSION_DEAD_ERROR.to_string());
        }
    }

    if !matches!(fast_path, HookFastPathOutcome::Ready) {
        tracing::warn!(
            tmux_session_name = session_name,
            readiness = readiness.label(),
            event_budget_ms = readiness.event_budget().as_millis() as u64,
            "codex_tui hook didn't fire within budget, falling back to pane-scrape polling"
        );
    }

    loop {
        if let Some(err) = cancel_check() {
            return Err(err);
        }
        if observe_rollout_composer_ready_for_prompt_wait(session_name) {
            let snapshot = prompt_readiness_snapshot(session_name);
            match decide_rollout_ready_snapshot(
                &snapshot,
                draft_policy,
                super::warm_followup::codex_tui_warm_followup_enabled(),
            ) {
                RolloutReadySnapshotDecision::SessionDead => {
                    return Err(PROMPT_READY_SESSION_DEAD_ERROR.to_string());
                }
                RolloutReadySnapshotDecision::Ignore => tracing::warn!(
                    tmux_session_name = session_name,
                    readiness = readiness.label(),
                    pane_tail = %snapshot.pane_tail,
                    "codex_tui fallback rollout composer_ready ignored because the captured pane is not input-ready"
                ),
                RolloutReadySnapshotDecision::Accept => {
                    tracing::debug!(
                        tmux_session_name = session_name,
                        readiness = readiness.label(),
                        elapsed_ms = start.elapsed().as_millis() as u64,
                        "codex_tui prompt ready via rollout composer_ready envelope during fallback loop"
                    );
                    return Ok(());
                }
            }
        }
        // #2399 HIGH 1: deadline check BEFORE the capture so an
        // already-elapsed budget cannot waste another ~tmux capture-pane
        // round trip on its way out.
        if Instant::now() >= deadline {
            let snapshot = prompt_readiness_snapshot(session_name);
            if let Some(err) = cancel_check() {
                return Err(err);
            }
            return Err(timeout_error(&snapshot));
        }
        let snapshot = prompt_readiness_snapshot(session_name);
        if let Some(err) = cancel_check() {
            return Err(err);
        }
        // Codex review HIGH on PR #2457: deadline check must run BEFORE
        // marker detection so a snapshot that arrives post-deadline is
        // converted to timeout instead of silently emitting RuntimeReady
        // past the bridge's 250ms drain window. The previous order
        // (marker check first → deadline check after) let a slow tmux
        // capture-pane succeed minutes late.
        if Instant::now() >= deadline {
            return Err(timeout_error(&snapshot));
        }
        if draft_policy.accepts(&snapshot) {
            return Ok(());
        }
        if !snapshot.tmux_pane_alive {
            if let Some(err) = cancel_check() {
                return Err(err);
            }
            return Err(PROMPT_READY_SESSION_DEAD_ERROR.to_string());
        }
        // #2399 HIGH 1: cap the sleep to the remaining budget so the
        // backoff never overshoots `deadline`. `saturating_sub` returns
        // zero past the deadline, which means the next loop iteration
        // observes the timeout immediately.
        let remaining = deadline.saturating_duration_since(Instant::now());
        let sleep_for = std::cmp::min(wait_interval, remaining);
        if sleep_for.is_zero() {
            return Err(timeout_error(&snapshot));
        }
        std::thread::sleep(sleep_for);
        if let Some(err) = cancel_check() {
            return Err(err);
        }
        wait_interval = std::cmp::min(wait_interval * 2, Duration::from_millis(1000));
    }
}

/// Subscribe-before-snapshot fast path backed by provider hook Stop/SubagentStop
/// events. The post-event snapshot still verifies the Codex composer marker, so
/// a hook from another provider/session can only shorten the wait when this
/// tmux pane is actually ready.
fn run_prompt_ready_fast_path(
    notify: Arc<Notify>,
    session_name: String,
    budget: Duration,
    deadline: Instant,
    cancel_token: Option<Arc<CancelToken>>,
    draft_policy: PromptDraftPolicy,
) -> (HookFastPathOutcome, Option<PromptReadinessSnapshot>) {
    let fut = async move {
        let rollout_ready_notify = rollout_composer_ready_notify();
        let rollout_ready_notified = rollout_ready_notify.notified();
        tokio::pin!(rollout_ready_notified);
        rollout_ready_notified.as_mut().enable();

        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        if cancel_requested(cancel_token.as_deref()) {
            return (HookFastPathOutcome::Cancelled, None);
        }
        if observe_rollout_composer_ready_for_prompt_wait(&session_name) {
            return (HookFastPathOutcome::RolloutComposerReady, None);
        }
        let pre_snapshot = prompt_readiness_snapshot(&session_name);
        if cancel_requested(cancel_token.as_deref()) {
            return (HookFastPathOutcome::Cancelled, None);
        }
        if draft_policy.accepts(&pre_snapshot) {
            return (HookFastPathOutcome::PreSnapshotReady, None);
        }
        if !pre_snapshot.tmux_pane_alive {
            return (HookFastPathOutcome::PreSnapshotSessionDead, None);
        }
        // Keep the hook fast path inside the same absolute deadline enforced
        // by the pane-polling path. This is especially important for the
        // 200ms post-turn handoff probe that must fit inside the bridge drain.
        let wait_budget = std::cmp::min(budget, deadline.saturating_duration_since(Instant::now()));
        if wait_budget.is_zero() {
            return (HookFastPathOutcome::Pending, Some(pre_snapshot));
        }

        let cancel_wait = async {
            loop {
                if cancel_requested(cancel_token.as_deref()) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::pin!(cancel_wait);

        let fast_path = tokio::select! {
            _ = &mut rollout_ready_notified => {
                if observe_rollout_composer_ready_for_prompt_wait(&session_name) {
                    HookFastPathOutcome::RolloutComposerReady
                } else {
                    HookFastPathOutcome::Pending
                }
            },
            _ = &mut notified => HookFastPathOutcome::Ready,
            _ = tokio::time::sleep(wait_budget) => HookFastPathOutcome::Pending,
            _ = &mut cancel_wait => HookFastPathOutcome::Cancelled,
        };

        if matches!(
            fast_path,
            HookFastPathOutcome::Cancelled | HookFastPathOutcome::RolloutComposerReady
        ) {
            return (fast_path, None);
        }
        if matches!(fast_path, HookFastPathOutcome::Ready) {
            tokio::time::sleep(PROMPT_READY_POST_EVENT_SETTLE).await;
        }
        if cancel_requested(cancel_token.as_deref()) {
            return (HookFastPathOutcome::Cancelled, None);
        }
        let post_event_snapshot = prompt_readiness_snapshot(&session_name);
        (fast_path, Some(post_event_snapshot))
    };

    drive_fast_path_future(fut)
}

/// Run an async hook fast-path future to completion using the caller's runtime
/// when possible, falling back to a dedicated current-thread runtime otherwise.
fn drive_fast_path_future<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static + FastPathFallback,
{
    match Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(fut))
        }
        _ => wait_on_dedicated_thread(fut),
    }
}

fn wait_on_dedicated_thread<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static + FastPathFallback,
{
    match std::thread::Builder::new()
        .name("codex-tui-prompt-ready".to_string())
        .spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
            {
                Ok(rt) => rt.block_on(fut),
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "failed to build local runtime for codex prompt readiness fast path; falling back to polling"
                    );
                    T::fallback()
                }
            }
        }) {
        Ok(handle) => handle.join().unwrap_or_else(|panic| {
            tracing::warn!(
                "codex prompt readiness fast-path worker panicked: {:?}; falling back to polling",
                panic
            );
            T::fallback()
        }),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "failed to spawn codex prompt readiness fast-path worker; falling back to polling"
            );
            T::fallback()
        }
    }
}

trait FastPathFallback {
    fn fallback() -> Self;
}

impl FastPathFallback for (HookFastPathOutcome, Option<PromptReadinessSnapshot>) {
    fn fallback() -> Self {
        (HookFastPathOutcome::Pending, None)
    }
}

trait TuiActionExecutor {
    fn send_literal(&mut self, session_name: &str, text: &str) -> Result<Output, String>;
    fn load_buffer(&mut self, buffer_name: &str, text: &str) -> Result<Output, String>;
    fn paste_buffer(
        &mut self,
        session_name: &str,
        buffer_name: &str,
        delete: bool,
    ) -> Result<Output, String>;
    fn send_keys(&mut self, session_name: &str, keys: &[&str]) -> Result<Output, String>;
}

#[derive(Default)]
struct TmuxTuiActionExecutor {
    composer_mutated: bool,
    enter_attempted: bool,
}

impl TuiActionExecutor for TmuxTuiActionExecutor {
    fn send_literal(&mut self, session_name: &str, text: &str) -> Result<Output, String> {
        let output = crate::services::platform::tmux::send_literal(session_name, text)?;
        self.composer_mutated |= output.status.success();
        Ok(output)
    }

    fn load_buffer(&mut self, buffer_name: &str, text: &str) -> Result<Output, String> {
        crate::services::platform::tmux::load_buffer(buffer_name, text)
    }

    fn paste_buffer(
        &mut self,
        session_name: &str,
        buffer_name: &str,
        delete: bool,
    ) -> Result<Output, String> {
        let output =
            crate::services::platform::tmux::paste_buffer(session_name, buffer_name, delete)?;
        self.composer_mutated |= output.status.success();
        Ok(output)
    }

    fn send_keys(&mut self, session_name: &str, keys: &[&str]) -> Result<Output, String> {
        self.enter_attempted |= keys.contains(&"Enter");
        crate::services::platform::tmux::send_keys(session_name, keys)
    }
}

fn run_actions_with_executor(
    session_name: &str,
    actions: &[TuiInputAction],
    cancel_token: Option<&CancelToken>,
    executor: &mut impl TuiActionExecutor,
) -> Result<(), String> {
    for action in actions {
        check_prompt_cancel(cancel_token)?;
        if matches!(action, TuiInputAction::Enter) {
            // Match the Claude TUI precedent: let the composer apply the last
            // literal/paste mutation before Enter so a re-mount cannot drop or
            // reorder the submit key. Re-check cancellation after the settle
            // so /stop cannot arrive inside this window and still submit.
            std::thread::sleep(PROMPT_INPUT_BEFORE_ENTER_SETTLE);
            check_prompt_cancel(cancel_token)?;
        }
        let output = match action {
            TuiInputAction::Literal(text) => executor.send_literal(session_name, text)?,
            TuiInputAction::PasteBuffer(text) => {
                let buffer_name = format!("agentdesk-codex-tui-input-{}", uuid::Uuid::new_v4());
                let load_output = executor.load_buffer(&buffer_name, text)?;
                ensure_tmux_success(load_output, action)?;
                check_prompt_cancel(cancel_token)?;
                executor.paste_buffer(session_name, &buffer_name, true)?
            }
            TuiInputAction::Enter => executor.send_keys(session_name, &["Enter"])?,
            TuiInputAction::Escape => executor.send_keys(session_name, &["Escape"])?,
        };
        ensure_tmux_success(output, action)?;
    }
    Ok(())
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
    };
    if stderr.is_empty() {
        Err(format!("tmux send {action_name} failed: {}", output.status))
    } else {
        Err(format!("tmux send {action_name} failed: {stderr}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptSubmitConfirmation {
    /// Continue by tailing the pinned rollout. This also covers historical
    /// prompt text with no active-composer draft; it never authorizes replay.
    Submitted,
    RetrySafeDraft,
    Unconfirmed,
}

/// Result of the one-shot warm-follow-up submit attempt. Only
/// `RetrySafeDraft` authorizes the caller to relaunch and submit again: it means
/// two post-Enter pane snapshots both showed the live composer holding a draft.
/// `Unconfirmed` is deliberately terminal so an Enter that may have reached
/// Codex is never replayed.
#[derive(Debug)]
pub(crate) enum CodexFollowupPromptSubmitOutcome {
    Submitted,
    /// Planning or delivery failed before any Enter attempt. The caller may
    /// cold-fallback only if the pinned rollout also remained unchanged.
    NotSubmitted {
        error: String,
    },
    RetrySafeDraft {
        first: PromptReadinessSnapshot,
        second: PromptReadinessSnapshot,
    },
    Cancelled,
    Unconfirmed {
        error: String,
        snapshot: PromptReadinessSnapshot,
    },
}

fn snapshot_has_retry_safe_prompt_draft(snapshot: &PromptReadinessSnapshot) -> bool {
    snapshot.tmux_pane_alive
        && snapshot.capture_available
        && snapshot.composer_marker_detected
        && snapshot.prompt_draft_detected
        && active_composer_visible_prompt_draft(snapshot).is_some()
}

pub(crate) fn prompt_draft_matches(
    snapshot: &PromptReadinessSnapshot,
    expected_prompt: &str,
) -> bool {
    if !snapshot_has_retry_safe_prompt_draft(snapshot) {
        return false;
    }
    active_composer_visible_prompt_draft(snapshot)
        .is_some_and(|draft| draft.trim() == expected_prompt.trim())
}

/// Return draft text only when it is structurally anchored inside the active,
/// bottom-most composer. A historical `› prompt` line in scrollback is never
/// submission-retry evidence.
fn active_composer_visible_prompt_draft(snapshot: &PromptReadinessSnapshot) -> Option<&str> {
    active_composer_visible_prompt_draft_in_pane(&snapshot.pane_tail)
}

fn active_composer_visible_prompt_draft_in_pane(pane: &str) -> Option<&str> {
    let recent: Vec<&str> = pane
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(PROMPT_READY_SCAN_LINES)
        .collect();

    if recent_has_codex_compact_composer(&recent) {
        return recent
            .get(1)
            .and_then(|line| codex_visible_prompt_draft_text(line));
    }

    let footer_idx = recent
        .iter()
        .take(FOOTER_HINT_BOTTOM_WINDOW)
        .position(|line| line_is_codex_footer_hint(line))?;
    let bottom_edge_idx = recent
        .iter()
        .take(COMPOSER_EDGE_BOTTOM_WINDOW)
        .position(|line| line_is_codex_composer_edge(line))?;
    if footer_idx > bottom_edge_idx
        || bottom_edge_idx - footer_idx > COMPOSER_FOOTER_ADJACENCY_LINES
    {
        return None;
    }
    let body_start = bottom_edge_idx + 1;
    let top_edge_offset = recent
        .iter()
        .skip(body_start)
        .position(|line| line_is_codex_composer_edge(line))?;
    let body_end = body_start + top_edge_offset;
    recent[body_start..body_end]
        .iter()
        .find_map(|line| codex_composer_body_draft_text(line))
}

fn clear_cancelled_partial_prompt_draft(
    session_name: &str,
    expected_prompt: &str,
    executor: &TmuxTuiActionExecutor,
) {
    if !executor.composer_mutated || executor.enter_attempted {
        return;
    }
    let snapshot = prompt_readiness_snapshot(session_name);
    let matches_partial = snapshot_has_retry_safe_prompt_draft(&snapshot)
        && active_composer_visible_prompt_draft(&snapshot).is_some_and(|draft| {
            !draft.trim().is_empty() && expected_prompt.trim().starts_with(draft.trim())
        });
    if !matches_partial {
        return;
    }
    match crate::services::platform::tmux::send_keys(session_name, &["C-u"]) {
        Ok(output) if output.status.success() => {}
        Ok(output) => tracing::warn!(
            tmux_session_name = session_name,
            status = %output.status,
            "failed to clear cancelled Codex TUI partial prompt draft"
        ),
        Err(error) => tracing::warn!(
            tmux_session_name = session_name,
            error,
            "failed to clear cancelled Codex TUI partial prompt draft"
        ),
    }
}

fn snapshot_allows_tail_only_no_replay(snapshot: &PromptReadinessSnapshot) -> bool {
    snapshot.tmux_pane_alive
        && snapshot.capture_available
        && !snapshot_has_retry_safe_prompt_draft(snapshot)
}

fn classify_prompt_submit_confirmation(
    first: &PromptReadinessSnapshot,
    second: &PromptReadinessSnapshot,
) -> PromptSubmitConfirmation {
    if snapshot_has_retry_safe_prompt_draft(first) && snapshot_has_retry_safe_prompt_draft(second) {
        PromptSubmitConfirmation::RetrySafeDraft
    } else if snapshot_allows_tail_only_no_replay(first)
        || snapshot_allows_tail_only_no_replay(second)
    {
        PromptSubmitConfirmation::Submitted
    } else {
        PromptSubmitConfirmation::Unconfirmed
    }
}

fn snapshot_allows_warm_followup_submit(snapshot: &PromptReadinessSnapshot) -> bool {
    snapshot.tmux_pane_alive
        && snapshot.capture_available
        && snapshot.composer_marker_detected
        && !snapshot.prompt_draft_detected
}

fn codex_snapshot_indicates_interactive_modal(snapshot: &PromptReadinessSnapshot) -> bool {
    let lower = snapshot.pane_tail.to_ascii_lowercase();
    [
        "approval required",
        "allow command",
        "allow this action",
        "confirm to continue",
        "do you trust",
        "trust this folder",
        "sign in",
        "log in",
        "authentication required",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

pub(crate) fn steering_snapshot_decision(
    snapshot: &PromptReadinessSnapshot,
) -> Result<(), &'static str> {
    if !snapshot.tmux_pane_alive {
        return Err("pane dead");
    }
    if !snapshot.capture_available {
        return Err("pane capture unavailable");
    }
    if !snapshot.composer_marker_detected {
        return Err("composer not present");
    }
    if snapshot.prompt_draft_detected {
        return Err("composer draft");
    }
    // Codex has no shared modal detector yet. Keep this conservative blacklist
    // behind the positive canonical composer marker, which is the primary gate.
    if codex_snapshot_indicates_interactive_modal(snapshot) {
        return Err("interactive modal");
    }
    Ok(())
}

fn steering_submit_outcome_to_result(
    outcome: CodexFollowupPromptSubmitOutcome,
) -> Result<(), String> {
    match outcome {
        CodexFollowupPromptSubmitOutcome::Submitted => Ok(()),
        CodexFollowupPromptSubmitOutcome::NotSubmitted { error }
        | CodexFollowupPromptSubmitOutcome::Unconfirmed { error, .. } => Err(error),
        CodexFollowupPromptSubmitOutcome::RetrySafeDraft { .. } => {
            Err("codex tui steering prompt remained in the composer after submit".to_string())
        }
        CodexFollowupPromptSubmitOutcome::Cancelled => {
            Err("codex tui steering prompt submission was cancelled".to_string())
        }
    }
}

fn inject_steering_prompt_using<C, R, U, S>(
    session_name: &str,
    prompt: &str,
    mut capture: C,
    mut record_prompt: R,
    mut remove_prompt: U,
    mut submit: S,
) -> Result<(), String>
where
    C: FnMut(&str) -> PromptReadinessSnapshot,
    R: FnMut(&str, &str, &str),
    U: FnMut(&str, &str, &str),
    S: FnMut(&str, &str) -> CodexFollowupPromptSubmitOutcome,
{
    plan_prompt_submit(prompt)?;
    with_composer_mutation_lock(session_name, || {
        let final_snapshot = capture(session_name);
        steering_snapshot_decision(&final_snapshot).map_err(str::to_string)?;
        record_prompt("codex", session_name, prompt);
        let result = steering_submit_outcome_to_result(submit(session_name, prompt));
        if result.is_err() {
            remove_prompt("codex", session_name, prompt);
        }
        result
    })
}

pub(crate) fn inject_steering_prompt(session_name: &str, prompt: &str) -> Result<(), String> {
    inject_steering_prompt_using(
        session_name,
        prompt,
        prompt_readiness_snapshot,
        crate::services::tui_prompt_dedupe::record_discord_originated_prompt,
        crate::services::tui_prompt_dedupe::remove_discord_originated_prompt,
        |session_name, prompt| submit_codex_followup_prompt_under_lock(session_name, prompt, None),
    )
}

/// Submit one Discord follow-up to an already-live Codex composer.
///
/// The Enter key is sent at most once. A possible delivery error never causes
/// another Enter. After the action plan finishes, the pane is sampled twice
/// when the first sample still contains a live draft (or cannot be captured).
/// This makes relaunch/replay legal only with positive evidence that the prompt
/// remained in the editor.
pub(crate) fn submit_codex_followup_prompt(
    session_name: &str,
    prompt: &str,
    cancel_token: Option<&CancelToken>,
) -> CodexFollowupPromptSubmitOutcome {
    with_composer_mutation_lock(session_name, || {
        submit_codex_followup_prompt_under_lock(session_name, prompt, cancel_token)
    })
}

fn submit_codex_followup_prompt_under_lock(
    session_name: &str,
    prompt: &str,
    cancel_token: Option<&CancelToken>,
) -> CodexFollowupPromptSubmitOutcome {
    let actions = match plan_prompt_submit(prompt) {
        Ok(actions) => actions,
        Err(error) => {
            return CodexFollowupPromptSubmitOutcome::NotSubmitted { error };
        }
    };
    // The warm-flow post-wait probe cannot make the send atomic with tmux.
    // Take one final canonical snapshot immediately before mutating the
    // composer so a just-arrived user draft or active turn is never appended
    // to or submitted as the Discord follow-up.
    let final_snapshot = prompt_readiness_snapshot(session_name);
    if !snapshot_allows_warm_followup_submit(&final_snapshot) {
        return CodexFollowupPromptSubmitOutcome::NotSubmitted {
            error: "Codex TUI warm follow-up final pane snapshot rejected submit".to_string(),
        };
    }
    let mut executor = TmuxTuiActionExecutor::default();
    let action_result =
        run_actions_with_executor(session_name, &actions, cancel_token, &mut executor);
    if action_result
        .as_ref()
        .err()
        .is_some_and(|error| is_prompt_ready_cancelled_error(error))
    {
        clear_cancelled_partial_prompt_draft(session_name, prompt, &executor);
        return CodexFollowupPromptSubmitOutcome::Cancelled;
    }
    if let Err(error) = action_result.as_ref()
        && !executor.enter_attempted
    {
        return CodexFollowupPromptSubmitOutcome::NotSubmitted {
            error: error.clone(),
        };
    }

    std::thread::sleep(PROMPT_SUBMIT_INITIAL_SETTLE);
    if cancel_requested(cancel_token) {
        return CodexFollowupPromptSubmitOutcome::Cancelled;
    }
    let first = prompt_readiness_snapshot(session_name);
    let needs_recheck = snapshot_has_retry_safe_prompt_draft(&first)
        || !first.tmux_pane_alive
        || !first.capture_available;
    let second = if needs_recheck {
        std::thread::sleep(PROMPT_SUBMIT_DRAFT_RECHECK_SETTLE);
        if cancel_requested(cancel_token) {
            return CodexFollowupPromptSubmitOutcome::Cancelled;
        }
        prompt_readiness_snapshot(session_name)
    } else {
        first.clone()
    };

    match (
        action_result,
        classify_prompt_submit_confirmation(&first, &second),
    ) {
        (_, PromptSubmitConfirmation::RetrySafeDraft) => {
            CodexFollowupPromptSubmitOutcome::RetrySafeDraft { first, second }
        }
        (Ok(()), PromptSubmitConfirmation::Submitted) => {
            CodexFollowupPromptSubmitOutcome::Submitted
        }
        (action_result, confirmation) => CodexFollowupPromptSubmitOutcome::Unconfirmed {
            error: action_result.err().unwrap_or_else(|| {
                format!("codex tui prompt submit confirmation was {confirmation:?}")
            }),
            snapshot: second,
        },
    }
}

/// Pane-capture classifier: returns true when the recent tail looks
/// like the Codex composer waiting for input.
///
/// Four independent gates, all required:
///
/// 1. **Bottom-anchored footer hint** — a footer phrase line within
///    the last [`FOOTER_HINT_BOTTOM_WINDOW`] non-empty lines.
/// 2. **Bottom-anchored composer edge** — a mostly box-drawing line
///    within the last [`COMPOSER_EDGE_BOTTOM_WINDOW`] non-empty lines.
/// 3. **Footer-below-edge ordering** — the footer must sit *below*
///    the composer edge in the pane, matching the Codex TUI layout
///    (the composer is drawn first, the hint row underneath).
/// 4. **Tight adjacency** — the composer edge and the footer hint
///    co-occur within [`COMPOSER_FOOTER_ADJACENCY_LINES`] of each
///    other, which is strictly smaller than either bottom window.
///
/// These together reject:
/// - stale composer frames scrolled deep into pane history;
/// - assistant prose that happens to mention `Esc to interrupt`;
/// - assistant output rendering a box-drawing table separately from
///   the live footer;
/// - a screenshot of a Codex TUI frame quoted inside model output;
/// - any bottom-of-pane snippet where a box-drawing line and a
///   footer phrase appear together but in the wrong order or with
///   visible status output between them.
pub(crate) fn pane_looks_ready_for_codex_prompt(pane: &str) -> bool {
    // recent[0] is the bottom-most non-empty line; index increases
    // moving upward (away from the live composer).
    let recent: Vec<&str> = pane
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(PROMPT_READY_SCAN_LINES)
        .collect();
    if recent.is_empty() || recent_has_codex_active_turn(&recent) {
        return false;
    }
    if recent_has_codex_compact_prompt(&recent) {
        return true;
    }

    let footer_idx = recent
        .iter()
        .take(FOOTER_HINT_BOTTOM_WINDOW)
        .position(|line| line_is_codex_footer_hint(line));
    let edge_idx = recent
        .iter()
        .take(COMPOSER_EDGE_BOTTOM_WINDOW)
        .position(|line| line_is_codex_composer_edge(line));

    let (Some(f), Some(e)) = (footer_idx, edge_idx) else {
        return false;
    };
    // Footer must be at or below the composer edge in pane coords.
    // Because we indexed from the bottom (recent[0] = bottom-most),
    // "below the edge" means a smaller index.
    if f > e {
        return false;
    }
    // Strict adjacency: composer and footer must be within a few rows
    // of each other. This is the actual gate — the bottom windows are
    // just outer search bounds.
    e - f <= COMPOSER_FOOTER_ADJACENCY_LINES
}

fn pane_looks_ready_for_codex_prompt_with_ansi(pane: &str) -> bool {
    // ANSI-preserving tmux capture lets us distinguish Codex's dim placeholder
    // suggestions from real user drafts in the compact prompt.
    let plain = strip_ansi_escape_sequences(pane);
    if pane_has_codex_active_turn_in_pane(&plain) {
        return false;
    }
    if pane_has_dim_legacy_codex_prompt_in_pane(pane) {
        return true;
    }
    pane_looks_ready_for_codex_prompt(&plain)
}

fn pane_has_codex_active_turn_in_pane(pane: &str) -> bool {
    let recent: Vec<&str> = pane
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(PROMPT_READY_SCAN_LINES)
        .collect();
    recent_has_codex_active_turn(&recent)
}

fn pane_has_dim_legacy_codex_prompt_in_pane(pane: &str) -> bool {
    let recent: Vec<&str> = pane
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(PROMPT_READY_SCAN_LINES)
        .collect();
    pane_has_dim_legacy_codex_prompt(&recent)
}

fn pane_has_dim_legacy_codex_prompt(recent_bottom_up: &[&str]) -> bool {
    const LEGACY_PROMPT_BOTTOM_WINDOW: usize = 4;
    const LEGACY_STATUS_BOTTOM_WINDOW: usize = 3;

    let prompt_idx = recent_bottom_up
        .iter()
        .take(LEGACY_PROMPT_BOTTOM_WINDOW)
        .position(|line| line_is_codex_compact_prompt_marker(&strip_ansi_escape_sequences(line)));
    let status_idx = recent_bottom_up
        .iter()
        .take(LEGACY_STATUS_BOTTOM_WINDOW)
        .position(|line| line_is_codex_status_with_ansi(line));
    let (Some(prompt_idx), Some(status_idx)) = (prompt_idx, status_idx) else {
        return false;
    };

    status_idx < prompt_idx
        && !recent_bottom_up[status_idx + 1..prompt_idx]
            .iter()
            .any(|line| {
                line_is_codex_status_with_ansi(line)
                    || line_is_codex_compact_prompt_marker(&strip_ansi_escape_sequences(line))
            })
        && line_is_dim_legacy_codex_prompt(recent_bottom_up[prompt_idx])
}

fn line_is_legacy_codex_prompt(line: &str) -> bool {
    let trimmed = line.trim();
    let Some(rest) = trimmed.strip_prefix('›') else {
        return false;
    };
    rest.trim().is_empty()
}

fn line_is_dim_legacy_codex_prompt(line: &str) -> bool {
    let plain = strip_ansi_escape_sequences(line);
    let trimmed = plain.trim();
    let Some(rest) = trimmed.strip_prefix('›') else {
        return false;
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return true;
    }
    line.find('›')
        .map(|idx| contains_dim_sgr(&line[idx + '›'.len_utf8()..]))
        .unwrap_or(false)
}

fn line_is_codex_fast_context_status(line: &str) -> bool {
    let parts: Vec<&str> = line.split('·').map(str::trim).collect();
    parts.len() == 3
        && matches!(parts[0], "Fast on" | "Fast off")
        && !parts[1].is_empty()
        && parts[2]
            .strip_prefix("Context ")
            .and_then(|value| value.strip_suffix("% left"))
            .is_some_and(|percent| {
                !percent.is_empty() && percent.chars().all(|ch| ch.is_ascii_digit())
            })
}

fn recent_has_codex_active_turn(recent_bottom_up: &[&str]) -> bool {
    const ACTIVE_TURN_BOTTOM_WINDOW: usize = 6;

    recent_bottom_up
        .iter()
        .take(ACTIVE_TURN_BOTTOM_WINDOW)
        .any(|line| {
            let trimmed = line.trim_start();
            (trimmed.starts_with('•') || trimmed.starts_with('◦'))
                && (trimmed.contains("esc to interrupt") || trimmed.contains("Esc to interrupt"))
        })
}

fn line_is_codex_status_with_ansi(line: &str) -> bool {
    line_is_codex_compact_status_line(&strip_ansi_escape_sequences(line))
        || line_is_codex_fast_context_status(&strip_ansi_escape_sequences(line))
}

fn contains_dim_sgr(input: &str) -> bool {
    let mut rest = input;
    while let Some(start) = rest.find("\x1b[") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find('m') else {
            return false;
        };
        let codes = &rest[..end];
        if codes
            .split(';')
            .filter_map(|code| {
                let code = code.trim();
                if code.is_empty() {
                    None
                } else {
                    code.parse::<u16>().ok()
                }
            })
            .any(|code| code == 2)
        {
            return true;
        }
        rest = &rest[end + 1..];
    }
    false
}

fn strip_ansi_escape_sequences(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && matches!(chars.peek(), Some(&'[')) {
            chars.next();
            for code_ch in chars.by_ref() {
                if ('@'..='~').contains(&code_ch) {
                    break;
                }
            }
            continue;
        }
        if ch != '\r' {
            output.push(ch);
        }
    }
    output
}

fn recent_has_codex_compact_prompt(recent: &[&str]) -> bool {
    recent_has_codex_compact_composer(recent)
        && recent
            .get(1)
            .is_some_and(|line| line_is_legacy_codex_prompt(line))
}

fn recent_has_codex_compact_composer(recent: &[&str]) -> bool {
    let Some(prompt_idx) = recent
        .iter()
        .take(COMPACT_PROMPT_BOTTOM_WINDOW)
        .position(|line| line_is_codex_compact_prompt_marker(line))
    else {
        return false;
    };
    prompt_idx == 1
        && (line_is_codex_compact_status_line(recent[0])
            || line_is_codex_fast_context_status(recent[0]))
}

fn pane_has_codex_prompt_draft(pane: &str) -> bool {
    active_composer_visible_prompt_draft_in_pane(pane).is_some()
        || pane
            .lines()
            .rev()
            .map(str::trim_end)
            .find(|line| !line.trim().is_empty())
            .and_then(codex_visible_prompt_draft_text)
            .is_some()
}

#[allow(dead_code)] // #3034: test-only (draft-clear path retired).
fn codex_visible_prompt_draft_backspace_budget(
    snapshot: &PromptReadinessSnapshot,
) -> Option<usize> {
    if !snapshot.prompt_draft_detected || !snapshot.tmux_pane_alive {
        return None;
    }
    let visible_chars = snapshot
        .pane_tail
        .lines()
        .filter_map(codex_visible_prompt_draft_text)
        .map(|text| text.chars().count())
        .sum::<usize>();
    (visible_chars > 0).then_some(visible_chars.saturating_add(16).min(512))
}

fn codex_visible_prompt_draft_text(line: &str) -> Option<&str> {
    let trimmed = line.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{00a0}');
    if let Some(rest) = trimmed.strip_prefix('›') {
        let text = rest.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{00a0}');
        return (!text.is_empty()).then_some(text);
    }
    codex_composer_body_draft_text(line)
}

fn line_is_codex_compact_prompt_marker(line: &str) -> bool {
    let trimmed = line.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{00a0}');
    trimmed == "›" || trimmed.starts_with("› ")
}

fn line_is_codex_compact_status_line(line: &str) -> bool {
    let trimmed = line.trim();
    let parts: Vec<&str> = trimmed.split('·').map(str::trim).collect();
    if parts.len() < 2 || !parts[0].starts_with("gpt-") {
        return false;
    }
    let has_effort = parts.iter().any(|part| {
        part.split_whitespace().any(|word| {
            matches!(
                word,
                "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
            )
        })
    });
    let has_path = parts
        .iter()
        .skip(1)
        .any(|part| part.starts_with("~/") || part.starts_with('/'));
    has_effort && has_path
}

fn codex_composer_body_draft_text(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if !(trimmed.starts_with('│') && trimmed.ends_with('│')) {
        return None;
    }
    let inner = trimmed
        .trim_matches('│')
        .trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{00a0}');
    let Some(cursor_index) = inner.rfind('▌') else {
        return None;
    };
    let before_cursor =
        inner[..cursor_index].trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{00a0}');
    if before_cursor.is_empty() || codex_composer_placeholder_text(before_cursor) {
        None
    } else {
        Some(before_cursor)
    }
}

fn codex_composer_placeholder_text(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "send a message"
            | "send a message..."
            | "send a message…"
            | "type / for commands"
            | "type a message"
            | "message"
    )
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
    let box_glyphs = trimmed
        .chars()
        .filter(|ch| is_box_drawing_char(*ch))
        .count();
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
        prompt_draft_detected = snapshot.prompt_draft_detected,
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
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;
    use std::sync::atomic::Ordering;

    #[test]
    fn followup_timeout_falls_back_to_default_without_live_config() {
        // current() is None in unit tests -> 45s default holds; the other arms
        // (FreshTurn 120s, PostTurnHandoff 200ms) must be unchanged.
        assert_eq!(
            followup_prompt_ready_timeout(),
            FOLLOWUP_PROMPT_READY_TIMEOUT
        );
        assert_eq!(PromptReadinessKind::Followup.timeout().as_secs(), 45);
        assert_eq!(PromptReadinessKind::FreshTurn.timeout().as_secs(), 120);
        assert_eq!(
            PromptReadinessKind::PostTurnHandoff.timeout().as_millis(),
            200
        );
    }

    #[cfg(unix)]
    fn successful_exit_status() -> std::process::ExitStatus {
        std::process::ExitStatus::from_raw(0)
    }

    #[cfg(windows)]
    fn successful_exit_status() -> std::process::ExitStatus {
        std::process::ExitStatus::from_raw(0)
    }

    fn successful_tmux_output() -> Output {
        Output {
            status: successful_exit_status(),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    #[derive(Default)]
    struct RecordingExecutor {
        calls: Vec<String>,
        cancel_after_calls: Option<usize>,
        cancel_token: Option<Arc<CancelToken>>,
    }

    impl RecordingExecutor {
        fn maybe_cancel(&self) {
            if self
                .cancel_after_calls
                .is_some_and(|cancel_after| self.calls.len() >= cancel_after)
            {
                if let Some(token) = &self.cancel_token {
                    token.cancelled.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    impl TuiActionExecutor for RecordingExecutor {
        fn send_literal(&mut self, _session_name: &str, text: &str) -> Result<Output, String> {
            self.calls.push(format!("literal:{text}"));
            self.maybe_cancel();
            Ok(successful_tmux_output())
        }

        fn load_buffer(&mut self, _buffer_name: &str, text: &str) -> Result<Output, String> {
            self.calls.push(format!("load-buffer:{text}"));
            self.maybe_cancel();
            Ok(successful_tmux_output())
        }

        fn paste_buffer(
            &mut self,
            _session_name: &str,
            _buffer_name: &str,
            _delete: bool,
        ) -> Result<Output, String> {
            self.calls.push("paste-buffer".to_string());
            self.maybe_cancel();
            Ok(successful_tmux_output())
        }

        fn send_keys(&mut self, _session_name: &str, keys: &[&str]) -> Result<Output, String> {
            self.calls.push(format!("keys:{}", keys.join("+")));
            self.maybe_cancel();
            Ok(successful_tmux_output())
        }
    }

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

    // U-3+ Bare ESC / BEL / FF must all be rejected by the validator before
    // any tmux delivery — mirrors claude_tui::input coverage.
    #[test]
    fn bare_control_bytes_are_rejected() {
        for prompt in [
            "hello\u{1b}stop", // bare ESC
            "ring\u{07}bell",  // BEL
            "form\u{0c}feed",  // FF
            "hello\u{7f}",     // DEL
            "hello\u{85}",     // C1 NEXT LINE
        ] {
            let error = plan_prompt_submit(prompt).unwrap_err();
            assert_eq!(
                error,
                "prompt contains unsupported terminal control characters"
            );
        }
    }

    // U-1+ Codex codeblock-style multiline (3 lines) is delivered as one
    // PasteBuffer + Enter.
    #[test]
    fn codeblock_multiline_paste_buffer() {
        let prompt = "fn main() {\n    println!(\"hi\");\n}";

        let actions = plan_prompt_submit(prompt).unwrap();

        assert_eq!(
            actions,
            vec![
                TuiInputAction::PasteBuffer(prompt.to_string()),
                TuiInputAction::Enter,
            ]
        );
    }

    // U-2+ Emoji + Korean on a single line stays in the Literal path and
    // is not UTF-8-split.
    #[test]
    fn emoji_and_korean_stay_literal() {
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

    // U-5 8 KiB single-line prompt → Literal chunks bounded by
    // DEFAULT_LITERAL_CHUNK_CHARS, followed by Enter, reassembling
    // to the original input.
    #[test]
    fn chunks_8kib_single_line_into_literals_then_enter() {
        let prompt: String = std::iter::repeat('B').take(8 * 1024).collect();

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
    fn run_actions_stops_before_first_tmux_action_when_token_is_pre_cancelled() {
        let token = Arc::new(CancelToken::new());
        token.cancelled.store(true, Ordering::Relaxed);
        let mut executor = RecordingExecutor::default();

        let error = run_actions_with_executor(
            "agentdesk-codex-tui-input-test",
            &[TuiInputAction::Escape],
            Some(&token),
            &mut executor,
        )
        .expect_err("pre-cancelled token must stop before tmux send");

        assert_eq!(error, PROMPT_READY_CANCELLED_ERROR);
        assert!(executor.calls.is_empty());
    }

    #[test]
    fn run_actions_stops_between_literal_chunks_when_token_flips() {
        let token = Arc::new(CancelToken::new());
        let mut executor = RecordingExecutor {
            cancel_after_calls: Some(1),
            cancel_token: Some(token.clone()),
            ..RecordingExecutor::default()
        };

        let error = run_actions_with_executor(
            "agentdesk-codex-tui-input-test",
            &[
                TuiInputAction::Literal("first".to_string()),
                TuiInputAction::Literal("second".to_string()),
                TuiInputAction::Enter,
            ],
            Some(&token),
            &mut executor,
        )
        .expect_err("cancelled token must stop before next literal chunk");

        assert_eq!(error, PROMPT_READY_CANCELLED_ERROR);
        assert_eq!(executor.calls, vec!["literal:first"]);
    }

    #[test]
    fn run_actions_rechecks_cancel_after_pre_enter_settle() {
        let token = Arc::new(CancelToken::new());
        let cancel_token = token.clone();
        let cancel_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            cancel_token.cancelled.store(true, Ordering::Relaxed);
        });
        let mut executor = RecordingExecutor::default();

        let error = run_actions_with_executor(
            "agentdesk-codex-tui-input-test",
            &[
                TuiInputAction::Literal("draft".to_string()),
                TuiInputAction::Enter,
            ],
            Some(&token),
            &mut executor,
        )
        .expect_err("cancel arriving during settle must stop before Enter");
        cancel_thread.join().unwrap();

        assert_eq!(error, PROMPT_READY_CANCELLED_ERROR);
        assert_eq!(executor.calls, vec!["literal:draft"]);
    }

    #[test]
    fn run_actions_stops_after_load_buffer_before_paste_when_token_flips() {
        let token = Arc::new(CancelToken::new());
        let mut executor = RecordingExecutor {
            cancel_after_calls: Some(1),
            cancel_token: Some(token.clone()),
            ..RecordingExecutor::default()
        };

        let error = run_actions_with_executor(
            "agentdesk-codex-tui-input-test",
            &[
                TuiInputAction::PasteBuffer("multi\nline".to_string()),
                TuiInputAction::Enter,
            ],
            Some(&token),
            &mut executor,
        )
        .expect_err("cancelled token must stop before paste-buffer");

        assert_eq!(error, PROMPT_READY_CANCELLED_ERROR);
        assert_eq!(executor.calls, vec!["load-buffer:multi\nline"]);
    }

    fn submit_snapshot(
        pane_alive: bool,
        capture_available: bool,
        composer_marker_detected: bool,
        prompt_draft_detected: bool,
    ) -> PromptReadinessSnapshot {
        PromptReadinessSnapshot {
            composer_marker_detected,
            prompt_draft_detected,
            tmux_pane_alive: pane_alive,
            capture_available,
            pane_tail: String::new(),
        }
    }

    fn active_box_draft_snapshot(prompt: &str) -> PromptReadinessSnapshot {
        let mut snapshot = submit_snapshot(true, true, true, true);
        snapshot.pane_tail = format!(
            "old output\n\
             ╭────────────────────────────────────────╮\n\
             │ {prompt} ▌                              │\n\
             ╰────────────────────────────────────────╯\n\
               Esc to interrupt   Ctrl+J newline   ⏎ send"
        );
        snapshot
    }

    #[test]
    fn steering_snapshot_requires_composer_and_rejects_modal() {
        let missing_composer = submit_snapshot(true, true, false, false);
        assert_eq!(
            steering_snapshot_decision(&missing_composer),
            Err("composer not present")
        );

        let mut modal = submit_snapshot(true, true, true, false);
        modal.pane_tail = "Approval required: allow command?".to_string();
        assert_eq!(steering_snapshot_decision(&modal), Err("interactive modal"));
    }

    #[test]
    fn codex_injection_records_before_confirmed_submit() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let record_events = events.clone();
        let remove_events = events.clone();
        let submit_events = events.clone();

        inject_steering_prompt_using(
            "agentdesk-codex-steering-dedupe-test",
            "steer now",
            |_| submit_snapshot(true, true, true, false),
            move |provider, session_name, prompt| {
                record_events
                    .lock()
                    .unwrap()
                    .push(format!("record:{provider}:{session_name}:{prompt}"));
            },
            move |provider, session_name, prompt| {
                remove_events
                    .lock()
                    .unwrap()
                    .push(format!("remove:{provider}:{session_name}:{prompt}"));
            },
            move |_, _| {
                submit_events.lock().unwrap().push("submit".to_string());
                CodexFollowupPromptSubmitOutcome::Submitted
            },
        )
        .unwrap();

        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "record:codex:agentdesk-codex-steering-dedupe-test:steer now",
                "submit"
            ]
        );
    }

    #[test]
    fn codex_steering_production_path_uses_confirmed_submit() {
        let module_src = include_str!("input.rs");
        let steering_entry = module_src
            .find("pub(crate) fn inject_steering_prompt(")
            .expect("Codex steering entry point exists");
        let warm_followup_entry = module_src[steering_entry..]
            .find("pub(crate) fn submit_codex_followup_prompt(")
            .map(|offset| steering_entry + offset)
            .expect("warm-followup entry follows steering entry");
        let steering_body = &module_src[steering_entry..warm_followup_entry];

        assert!(steering_body.contains("submit_codex_followup_prompt_under_lock("));
        assert!(!steering_body.contains("run_actions_with_executor("));
    }

    #[test]
    fn codex_injection_rejects_stranded_draft_and_rolls_back_dedupe() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let record_events = events.clone();
        let remove_events = events.clone();
        let first = active_box_draft_snapshot("steer now");
        let second = first.clone();

        let error = inject_steering_prompt_using(
            "agentdesk-codex-steering-confirmation-test",
            "steer now",
            |_| submit_snapshot(true, true, true, false),
            move |provider, session_name, prompt| {
                record_events
                    .lock()
                    .unwrap()
                    .push(format!("record:{provider}:{session_name}:{prompt}"));
            },
            move |provider, session_name, prompt| {
                remove_events
                    .lock()
                    .unwrap()
                    .push(format!("remove:{provider}:{session_name}:{prompt}"));
            },
            move |_, _| CodexFollowupPromptSubmitOutcome::RetrySafeDraft {
                first: first.clone(),
                second: second.clone(),
            },
        )
        .expect_err("a stranded draft must not report steering success");

        assert!(error.contains("remained in the composer"));
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "record:codex:agentdesk-codex-steering-confirmation-test:steer now",
                "remove:codex:agentdesk-codex-steering-confirmation-test:steer now"
            ]
        );
    }

    #[test]
    fn codex_injection_rejects_unconfirmed_submit_and_rolls_back_dedupe() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let record_events = events.clone();
        let remove_events = events.clone();

        let error = inject_steering_prompt_using(
            "agentdesk-codex-steering-unconfirmed-test",
            "steer now",
            |_| submit_snapshot(true, true, true, false),
            move |provider, session_name, prompt| {
                record_events
                    .lock()
                    .unwrap()
                    .push(format!("record:{provider}:{session_name}:{prompt}"));
            },
            move |provider, session_name, prompt| {
                remove_events
                    .lock()
                    .unwrap()
                    .push(format!("remove:{provider}:{session_name}:{prompt}"));
            },
            move |_, _| CodexFollowupPromptSubmitOutcome::Unconfirmed {
                error: "submit could not be confirmed".to_string(),
                snapshot: submit_snapshot(true, false, false, false),
            },
        )
        .expect_err("an unconfirmed submit must not report steering success");

        assert_eq!(error, "submit could not be confirmed");
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "record:codex:agentdesk-codex-steering-unconfirmed-test:steer now",
                "remove:codex:agentdesk-codex-steering-unconfirmed-test:steer now"
            ]
        );
    }

    #[test]
    fn rollout_ready_snapshot_tightening_is_disabled_with_warm_kill_switch() {
        let marker_missing = submit_snapshot(true, true, false, false);
        assert_eq!(
            decide_rollout_ready_snapshot(&marker_missing, PromptDraftPolicy::RejectDraft, false,),
            RolloutReadySnapshotDecision::Accept
        );
        assert_eq!(
            decide_rollout_ready_snapshot(&marker_missing, PromptDraftPolicy::RejectDraft, true,),
            RolloutReadySnapshotDecision::Ignore
        );

        let dead_with_draft = submit_snapshot(false, true, true, true);
        assert_eq!(
            decide_rollout_ready_snapshot(&dead_with_draft, PromptDraftPolicy::RejectDraft, false,),
            RolloutReadySnapshotDecision::Ignore,
            "legacy order blocks a draft before checking pane death"
        );
        assert_eq!(
            decide_rollout_ready_snapshot(&dead_with_draft, PromptDraftPolicy::RejectDraft, true,),
            RolloutReadySnapshotDecision::SessionDead
        );
    }

    #[test]
    fn submit_confirmation_allows_fallback_only_after_two_live_draft_snapshots() {
        let draft = active_box_draft_snapshot("Discord follow-up");
        let mut submitted = submit_snapshot(true, true, false, true);
        submitted.pane_tail = "working\n› Discord follow-up".to_string();
        let mut historical_over_blank_composer = submit_snapshot(true, true, true, true);
        historical_over_blank_composer.pane_tail = "\
› Discord follow-up\n\
╭────────────────────────────────────────╮\n\
│ ▌                                      │\n\
╰────────────────────────────────────────╯\n\
  Esc to interrupt   Ctrl+J newline   ⏎ send"
            .to_string();
        let capture_missing = submit_snapshot(true, false, false, false);
        let pane_dead = submit_snapshot(false, false, false, false);

        assert_eq!(
            classify_prompt_submit_confirmation(&draft, &draft),
            PromptSubmitConfirmation::RetrySafeDraft
        );
        assert_eq!(
            classify_prompt_submit_confirmation(&draft, &submitted),
            PromptSubmitConfirmation::Submitted
        );
        assert_eq!(
            classify_prompt_submit_confirmation(&capture_missing, &pane_dead),
            PromptSubmitConfirmation::Unconfirmed
        );
        assert_eq!(
            classify_prompt_submit_confirmation(
                &historical_over_blank_composer,
                &historical_over_blank_composer,
            ),
            PromptSubmitConfirmation::Submitted,
            "historical prompt text without an active draft is tail-only and never replay-safe"
        );
    }

    #[test]
    fn retry_safe_draft_must_match_the_discord_prompt() {
        let snapshot = active_box_draft_snapshot("exact Discord prompt");

        assert!(prompt_draft_matches(&snapshot, "exact Discord prompt"));
        assert!(!prompt_draft_matches(&snapshot, "different prompt"));

        let mut historical_only = submit_snapshot(true, true, true, true);
        historical_only.pane_tail = "\
› exact Discord prompt\n\
╭────────────────────────────────────────╮\n\
│ ▌                                      │\n\
╰────────────────────────────────────────╯\n\
  Esc to interrupt   Ctrl+J newline   ⏎ send"
            .to_string();
        assert!(
            !prompt_draft_matches(&historical_only, "exact Discord prompt"),
            "a submitted prompt left in scrollback is never fallback evidence"
        );
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
        assert!(!pane_has_codex_prompt_draft(CODEX_TUI_READY_PANE));
    }

    #[test]
    fn codex_prompt_marker_with_text_is_detected_as_draft() {
        let pane = "\
• previous response

› Run /review on my current changes

  gpt-5.5 · gpt-5.5 xhigh · ~/.adk/release/workspaces/agentdesk · agentdesk · main";

        assert!(!pane_looks_ready_for_codex_prompt(pane));
        assert!(pane_has_codex_prompt_draft(pane));
    }

    #[test]
    fn codex_compact_prompt_marker_without_text_is_ready() {
        let pane = "\
• previous response

›

  gpt-5.5 · gpt-5.5 xhigh · ~/.adk/release/workspaces/agentdesk · agentdesk · main";

        assert!(pane_looks_ready_for_codex_prompt(pane));
        assert!(!pane_has_codex_prompt_draft(pane));
    }

    #[test]
    fn quoted_compact_prompt_without_status_is_not_ready() {
        let pane = "\
The documentation example ends with:

› Run /review on my current changes";

        assert!(!pane_looks_ready_for_codex_prompt(pane));
        assert!(pane_has_codex_prompt_draft(pane));
    }

    #[test]
    fn quoted_compact_prompt_with_status_footer_is_not_ready() {
        let pane = "\
The documentation example ends with:

> › Run /review on my current changes

  gpt-5.5 · gpt-5.5 xhigh · ~/.adk/release/workspaces/agentdesk · agentdesk · main";

        assert!(!pane_looks_ready_for_codex_prompt(pane));
        assert!(!pane_has_codex_prompt_draft(pane));
    }

    #[test]
    fn codex_box_composer_with_text_is_detected_as_draft() {
        let pane = "\
╭──────────────────────────────────────────────────────────────╮
│ hello world ▌                                                │
╰──────────────────────────────────────────────────────────────╯
  Esc to interrupt   Ctrl+J newline   ⏎ send";

        assert!(pane_has_codex_prompt_draft(pane));
    }

    #[test]
    fn codex_box_composer_with_multiple_cursor_lines_is_detected_as_draft() {
        let pane = "\
╭──────────────────────────────────────────────────────────────╮
│ first wrapped segment ▌                                      │
│ second wrapped segment ▌                                     │
╰──────────────────────────────────────────────────────────────╯
  Esc to interrupt   Ctrl+J newline   ⏎ send";

        assert!(pane_has_codex_prompt_draft(pane));
        assert!(active_composer_visible_prompt_draft_in_pane(pane).is_some());
    }

    #[test]
    fn codex_box_placeholder_is_not_detected_as_draft() {
        let pane = "\
╭──────────────────────────────────────────────────────────────╮
│ Send a message… ▌                                            │
╰──────────────────────────────────────────────────────────────╯
  Esc to interrupt   Ctrl+J newline   ⏎ send";

        assert!(!pane_has_codex_prompt_draft(pane));
    }

    #[test]
    fn codex_visible_draft_backspace_budget_counts_prompt_text() {
        let snapshot = PromptReadinessSnapshot {
            composer_marker_detected: true,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "› abc".to_string(),
        };

        assert_eq!(
            codex_visible_prompt_draft_backspace_budget(&snapshot),
            Some(19)
        );
    }

    #[test]
    fn current_codex_idle_pane_uses_dim_evidence_to_override_plain_draft() {
        let pane = concat!(
            "╭─────────────────────────────────────────╮\n",
            "│ >_ OpenAI Codex (v0.144.4)              │\n",
            "╰─────────────────────────────────────────╯\n",
            "\n",
            "\x1b[0;1m›\x1b[0m \x1b[2mUse /skills to list available skills\x1b[0m\n",
            "\n",
            "  Fast off · fix/4411-codex-warm-pane-reuse · Context 100% left",
        );
        let plain = strip_ansi_escape_sequences(pane);
        let (marker, draft, _) = prompt_readiness_from_ansi_pane(pane);

        assert!(pane_looks_ready_for_codex_prompt_with_ansi(pane));
        assert!(pane_has_codex_prompt_draft(&plain));
        assert!(marker);
        assert!(!draft);
    }

    #[test]
    fn current_codex_busy_pane_is_not_ready() {
        let pane = "\
• Working (0s • esc to interrupt)\n\
\n\
› Use /skills to list available skills\n\
\n\
  Fast off · fix/4411-codex-warm-pane-reuse · Context 100% left";

        assert!(!pane_looks_ready_for_codex_prompt(pane));
    }

    #[test]
    fn compact_codex_dim_placeholder_is_ready_without_plain_allowlist() {
        let pane = concat!(
            "─ Worked for 3m 08s ────────────────────────────────────────────\n",
            "\n",
            "\x1b[0;1m›\x1b[0m \x1b[2mUse /skills to list available skills\n",
            "\n",
            "\x1b[0m  \x1b[38;2;246;226;183mgpt-5.5 xhigh\x1b[2m\x1b[39m",
            " · \x1b[0m\x1b[38;2;171;223;167m~/.adk/release/workspaces/baby\n",
        );
        let (marker, draft, _) = prompt_readiness_from_ansi_pane(pane);

        assert!(marker);
        assert!(!draft);
    }

    #[test]
    fn compact_codex_non_dim_exact_placeholder_text_is_a_draft() {
        let pane = "\
─ Worked for 3m 08s ────────────────────────────────────────────\n\
\n\
\x1b[0;1m›\x1b[0m Use /skills to list available skills\n\
\n\
\x1b[0m  \x1b[38;2;246;226;183mgpt-5.5 xhigh\x1b[2m\x1b[39m · \x1b[0m\x1b[38;2;171;223;167m~/.adk/release/workspaces/baby";
        let (marker, draft, _) = prompt_readiness_from_ansi_pane(pane);

        assert!(!marker);
        assert!(draft);
    }

    #[test]
    fn compact_codex_current_non_dim_draft_overrides_scrollback_dim_placeholder() {
        let pane = concat!(
            "\x1b[0;1m›\x1b[0m \x1b[2mUse /skills to list available skills\x1b[0m\n",
            "\n",
            "\x1b[0;1m›\x1b[0m keep my unsent draft\n",
            "\n",
            "  Fast off · fix/4411-codex-warm-pane-reuse · Context 100% left",
        );
        let (marker, draft, _) = prompt_readiness_from_ansi_pane(pane);

        assert!(!marker);
        assert!(draft);
    }

    #[test]
    fn compact_codex_interleaved_status_rejects_stale_dim_placeholder() {
        let pane = concat!(
            "\x1b[0;1m›\x1b[0m \x1b[2mUse /skills to list available skills\x1b[0m\n",
            "  gpt-5.5 xhigh · ~/.adk/release/workspaces/baby\n",
            "  Fast off · fix/4411-codex-warm-pane-reuse · Context 100% left",
        );
        let (marker, draft, _) = prompt_readiness_from_ansi_pane(pane);

        assert!(!marker);
        assert!(!draft);
    }

    #[test]
    fn compact_codex_partial_and_multiline_user_drafts_are_not_placeholders() {
        let partial = "\
─ Worked for 3m 08s ────────────────────────────────────────────\n\
\n\
› Use /skills to list available\n\
\n\
  gpt-5.5 xhigh · ~/.adk/release/workspaces/baby";
        let multiline = "\
╭──────────────────────────────────────────────────────────────╮\n\
│ Use /skills to list available skills                          │\n\
│ then summarize the result ▌                                   │\n\
╰──────────────────────────────────────────────────────────────╯\n\
  Esc to interrupt   Ctrl+J newline   ⏎ send";

        assert!(pane_has_codex_prompt_draft(partial));
        assert!(pane_has_codex_prompt_draft(multiline));
        assert!(!pane_looks_ready_for_codex_prompt(partial));
    }

    #[test]
    fn canonical_ansi_snapshot_draft_and_busy_state_block_reuse() {
        let draft = "\
› Use /skills to list available skills\n\
\n\
  gpt-5.5 xhigh · ~/.adk/release/workspaces/baby";
        let busy = "\
• Working (0s • esc to interrupt)\n\
\n\
\x1b[0;1m›\x1b[0m \x1b[2mUse /skills to list available skills\x1b[0m\n\
\n\
  gpt-5.5 xhigh · ~/.adk/release/workspaces/baby";

        let (draft_marker, draft_detected, _) = prompt_readiness_from_ansi_pane(draft);
        let (busy_marker, busy_detected, _) = prompt_readiness_from_ansi_pane(busy);
        assert!(!draft_marker);
        assert!(draft_detected);
        assert!(!busy_marker);
        assert!(!busy_detected);
    }

    #[test]
    fn compact_codex_history_text_is_not_a_composer_without_bottom_layout() {
        let pane = "\
The assistant quoted this old Codex frame:\n\
› Use /skills to list available skills\n\
  gpt-5.5 xhigh · ~/.adk/release/workspaces/baby\n\
then continued with a response.";

        assert!(!pane_looks_ready_for_codex_prompt(pane));
        assert!(!pane_has_codex_prompt_draft(pane));
    }

    #[test]
    fn final_submit_gate_requires_a_live_empty_canonical_snapshot() {
        let ready = submit_snapshot(true, true, true, false);
        assert!(snapshot_allows_warm_followup_submit(&ready));

        for mutate in [
            |snapshot: &mut PromptReadinessSnapshot| snapshot.tmux_pane_alive = false,
            |snapshot: &mut PromptReadinessSnapshot| snapshot.capture_available = false,
            |snapshot: &mut PromptReadinessSnapshot| snapshot.composer_marker_detected = false,
            |snapshot: &mut PromptReadinessSnapshot| snapshot.prompt_draft_detected = true,
        ] {
            let mut rejected = ready.clone();
            mutate(&mut rejected);
            assert!(
                !snapshot_allows_warm_followup_submit(&rejected),
                "final submit must reject every mutated readiness guard"
            );
        }
    }

    #[test]
    fn compact_codex_non_dim_unlisted_draft_is_not_ready_with_ansi() {
        let pane = "\
─ Worked for 3m 08s ────────────────────────────────────────────\n\
\n\
\x1b[0;1m›\x1b[0m run the pending draft\n\
\n\
\x1b[0m  \x1b[38;2;246;226;183mgpt-5.5 xhigh\x1b[2m\x1b[39m · \x1b[0m\x1b[38;2;171;223;167m~/.adk/release/workspaces/baby";
        assert!(!pane_looks_ready_for_codex_prompt_with_ansi(pane));
        assert!(pane_has_codex_prompt_draft(&strip_ansi_escape_sequences(
            pane
        )));
    }

    #[test]
    fn compact_codex_prompt_with_user_draft_is_not_ready() {
        let pane = "\
─ Worked for 4m 03s ────────────────────────────────────────────\n\
\n\
› run the pending draft\n\
\n\
  gpt-5.5 xhigh · ~/.adk/release/workspaces/baby";
        assert!(!pane_looks_ready_for_codex_prompt(pane));
        assert!(pane_has_codex_prompt_draft(pane));
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
    fn post_turn_handoff_probe_fits_inside_bridge_drain_window() {
        // #2325 round-3: the post-turn probe must fit inside the
        // turn-bridge `terminal_control_drain_until` window (250ms)
        // so any RuntimeReady / failure Done emitted after the probe
        // still reaches the bridge before it finalises the inflight.
        // If this assertion fails, the post-turn handoff race
        // documented in `execute_streaming_local_tui_tmux` will
        // silently drop frames — keep the probe strictly under
        // 250ms or revisit the bridge drain window first.
        assert!(
            PromptReadinessKind::PostTurnHandoff.timeout() < Duration::from_millis(250),
            "post-turn handoff probe must stay strictly under the 250ms bridge drain window"
        );
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

    #[test]
    fn cancelled_error_is_classified_and_distinct_from_timeout_and_session_dead() {
        assert!(is_prompt_ready_cancelled_error(
            PROMPT_READY_CANCELLED_ERROR
        ));
        assert!(!is_prompt_ready_timeout_error(PROMPT_READY_CANCELLED_ERROR));
        assert!(!is_session_dead_error(PROMPT_READY_CANCELLED_ERROR));
    }

    #[test]
    fn rollout_composer_ready_signal_beats_dead_pane_capture() {
        let session = format!(
            "agentdesk-codex-tui-rollout-ready-{}",
            uuid::Uuid::new_v4().simple()
        );
        record_rollout_composer_ready(&session);

        let result =
            wait_until_codex_tui_input_ready(&session, PromptReadinessKind::PostTurnHandoff, None);

        assert!(
            result.is_ok(),
            "explicit rollout composer_ready must be accepted before pane fallback, got {result:?}"
        );
        assert!(
            !rollout_composer_ready_observed(&session),
            "composer-ready must be one-shot so a later warm turn cannot consume stale readiness"
        );
    }

    #[test]
    fn marking_composer_busy_clears_rollout_ready_signal() {
        let session = format!(
            "agentdesk-codex-tui-rollout-busy-{}",
            uuid::Uuid::new_v4().simple()
        );
        record_rollout_composer_ready(&session);
        assert!(rollout_composer_ready_observed(&session));

        mark_rollout_composer_busy(&session);

        assert!(!rollout_composer_ready_observed(&session));
    }

    #[test]
    fn taking_composer_ready_signal_is_one_shot() {
        let session = format!(
            "agentdesk-codex-tui-rollout-take-{}",
            uuid::Uuid::new_v4().simple()
        );
        record_rollout_composer_ready(&session);

        assert!(take_rollout_composer_ready(&session));
        assert!(!take_rollout_composer_ready(&session));
    }

    // ------------------------------------------------------------------
    // Cancellation contract (no tmux required — uses dead session name
    // and a pre-cancelled token to drive the wait loop deterministically).
    // ------------------------------------------------------------------

    #[test]
    fn wait_returns_cancelled_immediately_when_token_is_pre_cancelled() {
        let token = Arc::new(CancelToken::new());
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let result = wait_until_codex_tui_input_ready(
            "agentdesk-codex-tui-input-test-cancelled-pre",
            PromptReadinessKind::Followup,
            Some(&token),
        );
        let error = result.expect_err("pre-cancelled token must short-circuit the wait");
        assert!(is_prompt_ready_cancelled_error(&error), "got: {error}");
        assert!(!is_prompt_ready_timeout_error(&error));
    }

    #[test]
    fn wait_returns_cancelled_when_token_flips_mid_wait_even_with_no_pane() {
        // No tmux session of this name exists. Without cancellation the
        // wait would observe `tmux_pane_alive=false` and return the
        // session-dead error. With cancellation pre-set, the cancel
        // check fires first.
        let token = Arc::new(CancelToken::new());
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let error = wait_until_codex_tui_input_ready(
            "agentdesk-codex-tui-input-test-cancelled-mid",
            PromptReadinessKind::Followup,
            Some(&token),
        )
        .expect_err("cancelled wait must return Err");
        assert!(is_prompt_ready_cancelled_error(&error), "got: {error}");
    }

    #[test]
    fn wait_reports_cancel_not_session_dead_when_token_is_set_before_first_probe() {
        // Same dead-session setup as above but stressing the priority
        // contract: a /stop arriving before the first probe MUST be
        // reported as cancelled, not as session-dead, so callers do
        // not recreate a tmux session for a user-cancelled turn.
        let token = Arc::new(CancelToken::new());
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let error = wait_until_codex_tui_input_ready(
            "agentdesk-codex-tui-input-test-cancel-beats-session-dead",
            PromptReadinessKind::Followup,
            Some(&token),
        )
        .expect_err("cancelled wait must return Err");
        assert!(
            is_prompt_ready_cancelled_error(&error),
            "cancel must beat session-dead and timeout, got: {error}"
        );
        assert!(!is_session_dead_error(&error));
        assert!(!is_prompt_ready_timeout_error(&error));
    }

    // ------------------------------------------------------------------
    // Adversarial false-positive fixtures
    // ------------------------------------------------------------------

    #[test]
    fn copied_tui_frame_in_assistant_output_during_active_turn_is_not_ready() {
        // Model output literally copies a Codex TUI frame for documentation
        // purposes while the turn is still active. In a live Codex TUI the
        // composer/footer always anchor at the bottom of the pane; during an
        // active turn the bottom rows show the working/thinking status
        // instead. The detector must NOT confuse the embedded frame for
        // readiness when the bottom is occupied by status output.
        let pane = "\
Here's what the prompt looks like in Codex TUI:\n\
╭──────────────────────────────────────────────────────────────╮\n\
│ ▌ example prompt                                             │\n\
╰──────────────────────────────────────────────────────────────╯\n\
  Esc to interrupt   Ctrl+J newline   ⏎ send\n\
\n\
Continuing to work on your task — running tests now.\n\
⠙ Working...   tokens 1234   ctx 12%\n\
running cargo test ...\n\
test result: ok. 5 passed\n\
⠹ Working...   tokens 1456   ctx 13%\n\
finalising response";
        assert!(!pane_looks_ready_for_codex_prompt(pane));
    }

    #[test]
    fn footer_at_bottom_without_nearby_box_edge_is_not_ready() {
        // Footer hint at very bottom, but the only box-drawing line is
        // far away (a model-rendered table 20+ lines up). Adjacency
        // check must reject this.
        let mut pane = String::new();
        pane.push_str("┌────────┬────────┐\n│ a      │ b      │\n└────────┴────────┘\n");
        for i in 0..20 {
            pane.push_str(&format!("plain prose line {i}\n"));
        }
        pane.push_str("  Esc to interrupt · Ctrl+J newline");
        assert!(!pane_looks_ready_for_codex_prompt(&pane));
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

    // ------------------------------------------------------------------
    // #2399 HIGH 1: post-turn handoff probe hard deadline
    // ------------------------------------------------------------------

    /// Asserts that `wait_until_codex_tui_input_ready` with the
    /// `PostTurnHandoff` budget cannot return after `1.5 × budget`,
    /// covering the case where the legacy loop slept the full backoff
    /// past the deadline and overshot the bridge's 250ms drain.
    ///
    /// The wait is driven against a deliberately-dead tmux session so
    /// `prompt_readiness_snapshot` always reports `tmux_pane_alive=false`
    /// on its first capture. That short-circuits to a session-dead Err
    /// inside one capture call — well under the 200ms budget. But the
    /// real value of the test is the wall-clock ceiling: even if the
    /// capture were slower (e.g. CI under load), the deadline check
    /// guarantees we never overshoot by more than one capture round.
    #[test]
    fn post_turn_handoff_wait_returns_within_one_budget_overshoot() {
        let session = format!(
            "agentdesk-codex-tui-deadline-{}",
            uuid::Uuid::new_v4().simple()
        );
        let budget = PromptReadinessKind::PostTurnHandoff.timeout();
        let started = Instant::now();
        let result =
            wait_until_codex_tui_input_ready(&session, PromptReadinessKind::PostTurnHandoff, None);
        let elapsed = started.elapsed();
        assert!(result.is_err(), "expected Err for dead tmux session");
        // Generous ceiling: 1× budget + one extra capture round (~500ms
        // for tmux + a single sleep window). Without the #2399 HIGH 1
        // fix this could be `start + budget + wait_interval` = up to
        // ~1.2s; we keep the bound at 1s so CI noise doesn't false-fail
        // while still catching a regression that re-introduces the
        // multi-second overshoot.
        assert!(
            elapsed < budget + Duration::from_millis(800),
            "post-turn-handoff wait must return inside ~budget + capture-jitter; took {:?}",
            elapsed
        );
    }

    /// Asserts the loop cannot oversleep its deadline when the budget is
    /// extremely short. Uses `PostTurnHandoff` (200ms) — the legacy code
    /// would sleep 100ms after the first capture, observe the deadline
    /// has elapsed only on the second iteration, and return around
    /// 300ms. With #2399 HIGH 1 the second capture / sleep is capped to
    /// the remaining budget.
    #[test]
    fn post_turn_handoff_wait_caps_sleep_to_remaining_budget() {
        // Without a real tmux session the first capture short-circuits
        // to session-dead. To exercise the sleep cap we instead use a
        // pre-cancelled token that fires AFTER the first capture — but
        // since we cannot intercept the snapshot capture from here, we
        // settle for the wall-clock ceiling check that the legacy
        // `+1s wait_interval` overshoot is gone.
        let session = format!(
            "agentdesk-codex-tui-sleep-cap-{}",
            uuid::Uuid::new_v4().simple()
        );
        let budget = PromptReadinessKind::PostTurnHandoff.timeout();
        let started = Instant::now();
        let _ =
            wait_until_codex_tui_input_ready(&session, PromptReadinessKind::PostTurnHandoff, None);
        let elapsed = started.elapsed();
        // Hard ceiling: legacy could hit ~budget + 1000ms (max
        // wait_interval). #2399 HIGH 1 keeps us under budget + 500ms.
        assert!(
            elapsed < budget + Duration::from_millis(500),
            "post-turn-handoff wait sleep must be capped to remaining budget; took {:?}",
            elapsed
        );
    }
}
