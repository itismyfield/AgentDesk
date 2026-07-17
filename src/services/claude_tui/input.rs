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
/// The busy-turn `/compact` path gets exactly one passive confirmation capture.
/// It intentionally never follows generic submit retry/cleanup behavior.
const COMPACT_SUBMIT_PASSIVE_SETTLE: Duration = Duration::from_millis(120);
const PROMPT_DRAFT_CLEANUP_CANCEL_TOKEN: Option<&CancelToken> = None;
/// Upper bound for how long we wait for an interactive selector overlay (e.g.
/// `/effort`) to mount after submitting the slash command, before giving up and
/// reporting failure rather than sending navigation keys into a composer.
const SELECTOR_OPEN_TIMEOUT: Duration = Duration::from_secs(5);
const PROMPT_READY_TIMEOUT_ERROR_PREFIX: &str = "timeout waiting for claude tui";
pub const PROMPT_READY_CANCELLED_ERROR: &str = "claude tui prompt readiness wait cancelled";
/// #3889: distinct, NON-timeout error prefix returned when a cold-boot lands on
/// the MCP-authentication-required welcome screen. Kept separate from the
/// readiness-timeout prefix so the fresh-prompt retry loop does not treat it as a
/// transient timeout and reboot straight back into the same blocked screen.
const PROMPT_READY_MCP_AUTH_ERROR_PREFIX: &str = "claude tui blocked on MCP server authentication";
/// Settle delay before re-capturing to confirm an observed MCP-auth cold-boot
/// banner is a stable blocking state and not a single half-rendered boot frame.
const PROMPT_READY_MCP_AUTH_CONFIRM_SETTLE: Duration = Duration::from_millis(400);
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
    ProvenWarmFollowup,
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

/// #tui-hook-ttl-buffer (REQ-006): claim the Claude hook registry key for this
/// tmux session and report whether a fresh Stop / SubagentStop was already
/// buffered. `claim_once` drains and drops the key so the consumed Stop cannot
/// replay into a later turn (single-consumption). Returns `false` when the
/// registry is disabled by the rollback flag, when no key/event matches, or
/// when the buffered events are not Stop kinds — in which case the caller falls
/// through to the existing Notify fast path and polling fallback unchanged.
///
/// Key-match (REQ-001): for a hosted Claude launch the hook relay buffers under
/// the PROVIDER session UUID (`config.session_id`, passed to the relay as
/// `--session-id`), while the readiness layer only knows the tmux session name.
/// Claiming `(claude, tmux_name)` would therefore MISS the buffered
/// `(claude, provider_session_uuid)` Stop and the early-Stop race this block
/// exists to rescue would still fall through. We resolve the provider session id
/// for this tmux session via the launch-time mapping in `tui_prompt_dedupe` and
/// claim that SAME key the hooks buffered under; we fall back to the tmux session
/// name (REQ-001 fallback) when no mapping is known (e.g. a relay that reported
/// only the tmux name).
///
/// `claim_matching_once` consumes every buffered Stop / SubagentStop and
/// re-buffers any other fresh buffered events (e.g. a token payload a concurrent
/// `/tui/wait` until=token might still want), so the readiness wait does not
/// discard unrelated buffered events. Consumed Stops cannot replay into a later
/// turn (single-consumption).
fn claude_registry_stop_already_buffered(session_name: &str) -> bool {
    use crate::services::claude_tui::hook_registry;
    use crate::services::claude_tui::hook_server::HookEventKind;
    if !hook_registry::registry_enabled() {
        return false;
    }
    // Prefer the provider session UUID the relay actually buffers under; fall
    // back to the tmux session name when no launch mapping is recorded.
    let provider_session_id =
        crate::services::tui_prompt_dedupe::provider_session_for_tmux("claude", session_name);
    let Some(key) = hook_registry::RegistryKey::new(
        "claude",
        provider_session_id.as_deref(),
        Some(session_name),
    ) else {
        return false;
    };
    hook_registry::global()
        .claim_matching_once(key, |event| {
            matches!(
                event.kind,
                HookEventKind::Stop | HookEventKind::SubagentStop
            )
        })
        .is_some()
}

impl PromptReadinessKind {
    fn timeout(self) -> Duration {
        match self {
            Self::FreshTurn => FRESH_PROMPT_READY_TIMEOUT,
            Self::Followup | Self::ProvenWarmFollowup => followup_prompt_ready_timeout(),
        }
    }

    fn event_budget(self) -> Duration {
        match self {
            Self::FreshTurn => FRESH_PROMPT_READY_EVENT_BUDGET,
            Self::Followup | Self::ProvenWarmFollowup => FOLLOWUP_PROMPT_READY_EVENT_BUDGET,
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

/// Outcome of the compact-specific steering path. Once a tmux mutation starts,
/// an error is necessarily ambiguous: a retry could enqueue a duplicate compact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactSubmitOutcome {
    PreMutationRefused,
    AcceptedOrQueued,
    AmbiguousAfterMutation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TuiInputAction {
    Literal(String),
    PasteBuffer(String),
    Enter,
    Escape,
    CtrlU,
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

/// True for the distinct, NON-timeout error `wait_for_prompt_ready` returns when
/// a Claude Code cold-boot is stranded on the MCP-authentication-required welcome
/// screen (#3889). Held apart from `is_prompt_ready_timeout_error` so the
/// fresh-prompt retry loop does NOT treat it as a transient readiness timeout and
/// re-boot into the same blocked screen; the caller instead surfaces the
/// actionable `run /mcp` reason to the operator and stops looping.
pub fn is_mcp_auth_required_error(error: &str) -> bool {
    error.starts_with(PROMPT_READY_MCP_AUTH_ERROR_PREFIX)
}

pub fn is_prompt_ready_cancelled_error(error: &str) -> bool {
    error == PROMPT_READY_CANCELLED_ERROR
}

pub fn prompt_readiness_snapshot(session_name: &str) -> PromptReadinessSnapshot {
    let pane = crate::services::platform::tmux::capture_pane(
        session_name,
        PROMPT_READY_CAPTURE_SCROLLBACK,
    );
    prompt_readiness_snapshot_from_capture(
        pane.as_deref(),
        crate::services::tmux_diagnostics::tmux_session_has_live_pane(session_name),
    )
}

fn prompt_readiness_snapshot_from_capture(
    pane: Option<&str>,
    tmux_pane_alive: bool,
) -> PromptReadinessSnapshot {
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

fn send_prompt_with_readiness(
    session_name: &str,
    prompt: &str,
    readiness: PromptReadinessKind,
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    let actions = plan_prompt_submit(prompt)?;
    // Readiness can wait for a busy prior turn, but it must not hold the narrow
    // composer lock while doing so. Revalidate only after acquiring that lock so
    // `/compact` and a normal follow-up cannot interleave their key mutations.
    wait_for_prompt_ready(session_name, readiness, cancel_token)?;
    crate::services::claude_tui::composer_lock::with_composer_mutation_lock(session_name, || {
        let snapshot = prompt_readiness_snapshot(session_name);
        if !prompt_marker_confirms_prompt_ready(readiness, &snapshot) {
            return Err("claude tui composer changed before follow-up mutation".to_string());
        }
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
    })
}

/// Submit `/compact` into a busy Claude pane without the generic readiness wait.
/// This function deliberately contains no retry Enter, Escape, or Ctrl-U cleanup:
/// after the first tmux mutation starts, every uncertainty stays disarmed.
pub fn send_compact_while_busy(session_name: &str) -> CompactSubmitOutcome {
    let snapshot = prompt_readiness_snapshot(session_name);
    if compact_steering_decision(&snapshot).is_err() {
        return CompactSubmitOutcome::PreMutationRefused;
    }

    // The first call begins the mutation boundary. Even a transport/result error
    // can mean tmux partially typed the control, so nothing after this point is
    // retryable by this path.
    let literal = match crate::services::platform::tmux::send_literal(session_name, "/compact") {
        Ok(output) => output,
        Err(_) => return CompactSubmitOutcome::AmbiguousAfterMutation,
    };
    if ensure_tmux_success(literal, &TuiInputAction::Literal("/compact".to_string())).is_err() {
        return CompactSubmitOutcome::AmbiguousAfterMutation;
    }
    std::thread::sleep(COMPACT_SUBMIT_PASSIVE_SETTLE);

    // F4 (security-adjacent): re-check modal state under the composer lock we are
    // already holding, IMMEDIATELY before the confirming Enter. The busy pane may
    // have mounted a permission / plan / startup dialog during the settle window;
    // pressing Enter would confirm its default selection (e.g. approve an
    // unapproved tool in ask/plan mode). If a dialog is now on screen, abort
    // WITHOUT Enter and report ambiguous so the trigger re-arms and a later idle
    // turn retries once the dialog is gone. We deliberately do NOT send cleanup
    // keys here: a stray Escape/Ctrl-U while a modal is focused could itself
    // dismiss the operator's dialog, so the `/compact` literal is left as a draft
    // (the next steering attempt refuses on the non-empty composer rather than
    // stacking a second `/compact`).
    let pre_enter = prompt_readiness_snapshot(session_name);
    if compact_pre_enter_modal_guard(&pre_enter).is_err() {
        return CompactSubmitOutcome::AmbiguousAfterMutation;
    }

    let enter = match crate::services::platform::tmux::send_keys(session_name, &["Enter"]) {
        Ok(output) => output,
        Err(_) => return CompactSubmitOutcome::AmbiguousAfterMutation,
    };
    if ensure_tmux_success(enter, &TuiInputAction::Enter).is_err() {
        return CompactSubmitOutcome::AmbiguousAfterMutation;
    }
    std::thread::sleep(COMPACT_SUBMIT_PASSIVE_SETTLE);

    let confirmation = prompt_readiness_snapshot(session_name);
    if !confirmation.tmux_pane_alive || !confirmation.capture_available {
        return CompactSubmitOutcome::AmbiguousAfterMutation;
    }
    if crate::services::tmux_common::tmux_capture_indicates_claude_tui_exact_empty_composer(
        &confirmation.pane_tail,
    ) || compact_queued_message_hint(&confirmation.pane_tail)
    {
        CompactSubmitOutcome::AcceptedOrQueued
    } else {
        CompactSubmitOutcome::AmbiguousAfterMutation
    }
}

/// Strict pre-mutation steering policy. A regular busy frame is permitted only
/// when its bottom composer is provably empty; every modal/draft/auth/blind
/// capture is a refusal rather than a best-effort recovery opportunity.
pub(crate) fn compact_steering_decision(
    snapshot: &PromptReadinessSnapshot,
) -> Result<(), &'static str> {
    if !snapshot.tmux_pane_alive {
        return Err("pane dead");
    }
    if !snapshot.capture_available {
        return Err("pane capture unavailable");
    }
    if snapshot.prompt_draft_detected {
        return Err("composer draft");
    }
    if snapshot_indicates_mcp_auth_block(snapshot) {
        return Err("MCP authentication block");
    }
    if crate::services::claude_tui::startup_dialog::detect_claude_startup_dialog(
        &snapshot.pane_tail,
    )
    .is_some()
    {
        return Err("startup dialog");
    }
    if crate::services::tmux_common::tmux_capture_indicates_claude_tui_interactive_modal(
        &snapshot.pane_tail,
    ) {
        return Err("interactive modal");
    }
    if !crate::services::tmux_common::tmux_capture_indicates_claude_tui_exact_empty_composer(
        &snapshot.pane_tail,
    ) {
        return Err("exact empty composer unavailable");
    }
    Ok(())
}

/// F4: the modal / dialog subset of [`compact_steering_decision`], applied a
/// SECOND time immediately before the confirming Enter of `send_compact_while_busy`.
///
/// The full steering decision cannot be reused at the pre-Enter boundary because
/// by then the `/compact` literal has been typed, so the composer is
/// intentionally non-empty and the empty-composer gate would always fail. This
/// guard therefore checks ONLY the "a dialog is swallowing input" conditions
/// whose default-confirm an auto Enter must never trigger — it shares the exact
/// same detection primitives (`snapshot_indicates_mcp_auth_block`,
/// `detect_claude_startup_dialog`, `tmux_capture_indicates_claude_tui_interactive_modal`)
/// that are the single authority for "is a modal mounted", so the two boundaries
/// can never disagree about what counts as a dialog.
fn compact_pre_enter_modal_guard(snapshot: &PromptReadinessSnapshot) -> Result<(), &'static str> {
    if !snapshot.tmux_pane_alive {
        return Err("pane dead");
    }
    if !snapshot.capture_available {
        return Err("pane capture unavailable");
    }
    if snapshot_indicates_mcp_auth_block(snapshot) {
        return Err("MCP authentication block");
    }
    if crate::services::claude_tui::startup_dialog::detect_claude_startup_dialog(
        &snapshot.pane_tail,
    )
    .is_some()
    {
        return Err("startup dialog");
    }
    if crate::services::tmux_common::tmux_capture_indicates_claude_tui_interactive_modal(
        &snapshot.pane_tail,
    ) {
        return Err("interactive modal");
    }
    Ok(())
}

fn compact_queued_message_hint(pane_tail: &str) -> bool {
    let lower = pane_tail.to_ascii_lowercase();
    lower.contains("queued message")
        || (lower.contains("queued")
            && (lower.contains("after current") || lower.contains("will be sent")))
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
    // The potentially long readiness wait remains outside the narrow composer
    // lock. Once ready, every selector mutation (including the open→confirm
    // interval) stays serialized with auto `/compact` so their keys cannot
    // land in one another's editor/overlay.
    wait_for_prompt_ready(session_name, PromptReadinessKind::Followup, cancel_token)?;
    let result = crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
        session_name,
        || {
            let snapshot = prompt_readiness_snapshot(session_name);
            if !prompt_marker_confirms_prompt_ready(PromptReadinessKind::Followup, &snapshot) {
                return Err("claude tui composer changed before selector mutation".to_string());
            }
            // The slash command is typed into Claude as a real composer entry,
            // so the transcript relay would otherwise classify it as SSH-direct
            // input and lease a spurious external turn. Record it under the same
            // lock that protects the ensuing mutation.
            crate::services::tui_prompt_dedupe::record_discord_originated_prompt(
                "claude",
                session_name,
                nav.slash_command,
            );
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
        },
    );
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

/// #3880 (A1): settle delay between the last single-line `Literal` and the
/// `Enter` that submits it. A single-line prompt plans as `Literal…(Enter)` with
/// NO `PasteBuffer`, so — unlike the multi-line paste path that already settles
/// `POST_PASTE_BUFFER_SETTLE` — the Enter previously fired with a 0 ms gap. On a
/// cold / `/clear` composer that is still re-mounting, that Enter races the
/// composer remount and is swallowed, leaving the typed text as a stranded
/// draft (the submit never lands → 120s transcript timeout → tmux kill). A short
/// settle before the Enter closes the race; the cost is one settle per
/// single-line submit. Mirrors POST_PASTE_BUFFER_SETTLE in spirit and duration.
const POST_LITERAL_SETTLE: Duration = Duration::from_millis(200);

/// #3880 (A1): true when `current` is a `Literal` that is immediately followed
/// by `Enter` — the exact single-line submit transition that needs the
/// POST_LITERAL_SETTLE window. Consecutive `Literal` chunks (a long single line
/// split for `send-keys`) do NOT settle between themselves; only the final
/// `Literal → Enter` boundary does. Pure and lookahead-only so the settle wiring
/// is unit-testable without a live tmux pane.
fn literal_action_needs_post_settle(
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
    for (index, action) in actions.iter().enumerate() {
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
            TuiInputAction::CtrlU => {
                crate::services::platform::tmux::send_keys(session_name, &["C-u"])?
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
        // #3880 (A1): close the single-line Literal→Enter race on a re-mounting
        // composer. Only the final `Literal` before an `Enter` settles (see
        // literal_action_needs_post_settle); the PasteBuffer/Backspace arms
        // `continue` above and never reach here.
        if literal_action_needs_post_settle(action, actions.get(index + 1)) {
            check_prompt_cancel(cancel_token)?;
            std::thread::sleep(POST_LITERAL_SETTLE);
        }
    }
    Ok(())
}

fn run_actions_with_submission_confirmation(
    session_name: &str,
    actions: &[TuiInputAction],
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    let actions_contained_paste = actions_contain_paste_buffer(actions);
    let result = run_actions(session_name, actions, cancel_token)
        .and_then(|()| confirm_prompt_submission_left_editor(session_name, cancel_token));
    if should_clear_draft_on_error(actions_contained_paste, result.is_err()) {
        clear_prompt_draft_before_error(session_name);
    }
    result
}

fn actions_contain_paste_buffer(actions: &[TuiInputAction]) -> bool {
    actions
        .iter()
        .any(|action| matches!(action, TuiInputAction::PasteBuffer(_)))
}

fn should_clear_draft_on_error(actions_contained_paste: bool, result_is_err: bool) -> bool {
    actions_contained_paste && result_is_err
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

fn clear_prompt_draft_before_error(session_name: &str) {
    let snapshot = prompt_readiness_snapshot(session_name);
    let actions = prompt_draft_cleanup_actions(&snapshot);
    if let Err(error) = run_actions(session_name, &actions, PROMPT_DRAFT_CLEANUP_CANCEL_TOKEN) {
        tracing::warn!(
            tmux_session_name = session_name,
            error = %error,
            "failed to clear Claude TUI draft after prompt submit retries"
        );
    }
}

/// F1 single authority: run an out-of-turn composer cleanup through the SAME
/// `with_composer_mutation_lock` that `/compact` steering and every normal
/// prompt submit hold, so a stranded-draft clear and an auto `/compact` can never
/// interleave their key sends.
///
/// Both out-of-turn stranded-draft clearers funnel here: the readiness-timeout
/// cleanup below (which runs OUTSIDE the send's composer critical section, unlike
/// the in-send cleanup in `run_actions_with_submission_confirmation` that is
/// already lock-held), and the warm-followup stranded-draft clear in
/// `hosting::followup_support`. Callers MUST NOT already hold the composer lock —
/// every readiness/warm-followup wait acquires it only AFTER the wait returns,
/// so this is the outermost composer acquisition on those paths (no re-entry).
pub(crate) fn with_composer_cleanup_lock<R>(session_name: &str, cleanup: impl FnOnce() -> R) -> R {
    crate::services::claude_tui::composer_lock::with_composer_mutation_lock(session_name, cleanup)
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
        TuiInputAction::CtrlU => "ctrl-u",
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
    // The potentially long warm-follow-up readiness wait must not hold the
    // narrow composer lock: a busy-pane `/compact` still needs to steer while
    // this path waits for the prior turn. Once readiness returns, however,
    // re-check the live composer and keep the dedupe record plus the entire
    // submit/confirmation sequence serialized with every other mutation.
    wait_for_prompt_ready_or_idle_transcript(
        session_name,
        PromptReadinessKind::ProvenWarmFollowup,
        cancel_token,
        transcript_path,
    )?;
    crate::services::claude_tui::composer_lock::with_composer_mutation_lock(session_name, || {
        let snapshot = prompt_readiness_snapshot(session_name);
        if !proven_warm_followup_revalidates_prompt_ready(&snapshot, transcript_path) {
            return Err("claude tui composer changed before follow-up mutation".to_string());
        }
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
    })
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

    // #3889/#4528: a cold pane can land on the MCP-authentication-required
    // welcome screen, which paints composer chrome (so it reads READY) yet
    // silently drops every prompt submission until the operator runs `/mcp`.
    // Detect it up front and fail fast with an actionable, non-timeout reason.
    // Only the hosted existing-session path supplies ProvenWarmFollowup after a
    // recorded turn; a plain Followup intent is not proof that this warning is
    // stale. The check only re-captures when the banner is actually present.
    if !readiness.allows_stale_mcp_auth_warning() {
        let snapshot = prompt_readiness_snapshot(session_name);
        if let Some(confirmed) = confirm_mcp_auth_block(session_name, cancel_token, &snapshot)? {
            log_prompt_ready_mcp_auth_block(session_name, readiness, &confirmed);
            return Err(mcp_auth_required_error_message(session_name));
        }
    }

    // #tui-hook-ttl-buffer (REQ-006): consult the in-memory hook registry as an
    // additive event source for an early Stop that landed before this wait
    // began. The global `prompt_ready_notify()` used by the fast path below is
    // edge-triggered, so a Stop fired in the gap between the prior turn and this
    // wait would otherwise be lost and force a full polling fallback. We consume
    // a FRESH (unexpired) Stop keyed by the PROVIDER session UUID the hooks
    // buffer under (resolved from the tmux session, with the tmux name as the
    // REQ-001 fallback), and `claim_matching_once` removes buffered Stops so
    // they can never replay into a later turn (REQ-002/REQ-003).
    //
    // CRITICAL (verifier top regression risk — "stale Stop contaminates a new
    // turn"): a buffered Stop is treated like an edge-triggered Notify wake, NOT
    // as an unconditional ready signal. We REQUIRE the pane snapshot to confirm
    // the prompt marker (the same gate the Notify fast path applies via its
    // post-event snapshot) before returning ready. A stale Stop whose pane is
    // still mid-turn therefore does NOT short-circuit; it falls through to the
    // normal Notify + polling path. The rollback flag turns the whole block off.
    if claude_registry_stop_already_buffered(session_name) {
        check_prompt_cancel(cancel_token)?;
        let snapshot = prompt_readiness_snapshot(session_name);
        if prompt_marker_confirms_prompt_ready(readiness, &snapshot) {
            tracing::debug!(
                tmux_session_name = session_name,
                readiness = readiness.label(),
                elapsed_ms = start.elapsed().as_millis() as u64,
                "claude_tui prompt ready via buffered hook registry Stop confirmed by pane marker (early-Stop race avoided)"
            );
            return Ok(());
        }
        // Buffered Stop did not correspond to a ready pane (stale / mid-turn) —
        // fall through to the existing Notify fast path and polling fallback.
        tracing::debug!(
            tmux_session_name = session_name,
            readiness = readiness.label(),
            "claude_tui buffered hook registry Stop did not confirm pane readiness; using standard fast path"
        );
    }

    // #3889/#4528: an idle transcript must not bypass an auth block without
    // recorded-turn warm provenance. Even proven warm reuse must still reject
    // live busy chrome in the confirming pane capture.
    if transcript_idle_confirms_prompt_ready_without_capture(session_name, transcript_path)
        && pane_allows_prompt_readiness(session_name, readiness)
    {
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
    let (fast_path, post_event_snapshot) = run_prompt_ready_fast_path(
        notify,
        session_name.to_string(),
        readiness,
        readiness.event_budget(),
    );

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
        if prompt_marker_confirms_prompt_ready(readiness, &snapshot) {
            check_prompt_cancel(cancel_token)?;
            // The live broadcast woke this waiter. Drain the parallel registry
            // copy as consumed too, otherwise the same Stop can be replayed into
            // the next follow-up wait.
            let drained_buffered_stop = claude_registry_stop_already_buffered(session_name);
            tracing::debug!(
                tmux_session_name = session_name,
                readiness = readiness.label(),
                hook_event_fast_path_hit = matches!(fast_path, HookFastPathOutcome::Ready),
                drained_buffered_stop,
                elapsed_ms = start.elapsed().as_millis() as u64,
                "claude_tui prompt ready via hook event fast path"
            );
            return Ok(());
        }
        // #3889/#4528: only proven warm reuse may treat this banner as stale.
        // Every path still honors the live draft and busy state below.
        if snapshot_allows_prompt_readiness(readiness, &snapshot)
            && transcript_idle_confirms_prompt_ready(&snapshot, transcript_path)
        {
            check_prompt_cancel(cancel_token)?;
            let drained_buffered_stop = claude_registry_stop_already_buffered(session_name);
            tracing::info!(
                tmux_session_name = session_name,
                readiness = readiness.label(),
                drained_buffered_stop,
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
    readiness: PromptReadinessKind,
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
        if prompt_marker_confirms_prompt_ready(readiness, &pre_snapshot) {
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
        // #3889/#4528: preserve the auth gate unless the caller supplied
        // recorded-turn warm provenance. Short-circuit ordering keeps the pane
        // capture off the non-idle poll cadence.
        if transcript_idle_confirms_prompt_ready_without_capture(session_name, transcript_path)
            && pane_allows_prompt_readiness(session_name, readiness)
        {
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
        // #3889/#4528: plain Followup callers may point at a genuine cold pane,
        // so only recorded-turn warm provenance can bypass the stable auth block.
        if !readiness.allows_stale_mcp_auth_warning() {
            if let Some(confirmed) = confirm_mcp_auth_block(session_name, cancel_token, &snapshot)?
            {
                log_prompt_ready_mcp_auth_block(session_name, readiness, &confirmed);
                return Err(mcp_auth_required_error_message(session_name));
            }
        }
        if prompt_marker_confirms_prompt_ready(readiness, &snapshot) {
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
        // #3889/#4528: apply the same provenance-aware auth policy to the
        // transcript-idle fallback; draft and busy state still veto every kind.
        if snapshot_allows_prompt_readiness(readiness, &snapshot)
            && transcript_idle_confirms_prompt_ready(&snapshot, transcript_path)
        {
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
            if prompt_ready_timeout_should_clear_followup_draft(
                readiness,
                &snapshot,
                crate::services::claude::claude_tui_followup_requeue_enabled(),
            ) {
                tracing::warn!(
                    tmux_session_name = session_name,
                    readiness = readiness.label(),
                    "claude_tui clearing stranded follow-up prompt draft after readiness timeout so retry can re-inject cleanly"
                );
                // F1: this cleanup runs OUTSIDE the send's composer critical
                // section (the readiness wait completed with a timeout, so no
                // submit lock is held). Route it through the shared composer lock
                // so it is mutually exclusive with a busy-pane auto `/compact`.
                with_composer_cleanup_lock(session_name, || {
                    clear_prompt_draft_before_error(session_name);
                });
            }
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
    // Prompt-delivery readiness only. Session completion/readiness fallback must
    // go through `FallbackPaneReadiness`, which is disabled for ClaudeTui.
    crate::services::tmux_common::tmux_capture_indicates_claude_tui_ready_for_input(pane)
}

fn prompt_marker_confirms_prompt_ready(
    readiness: PromptReadinessKind,
    snapshot: &PromptReadinessSnapshot,
) -> bool {
    snapshot.prompt_marker_detected
        && !snapshot.prompt_draft_detected
        && snapshot_allows_prompt_readiness(readiness, snapshot)
}

/// Revalidate a hosted warm follow-up after it acquires the narrow composer
/// lock. The earlier readiness wait may validly have returned through the
/// transcript-idle fallback before a pane capture showed the `❯` marker, so a
/// marker-only recheck would incorrectly reject an otherwise ready warm pane.
///
/// This mirrors the snapshot-based success condition in
/// `wait_for_prompt_ready_inner`: the normal prompt marker is sufficient, or a
/// live, non-busy snapshot plus an idle transcript is sufficient for the
/// recorded-turn warm-reuse path.
fn proven_warm_followup_revalidates_prompt_ready(
    snapshot: &PromptReadinessSnapshot,
    transcript_path: &std::path::Path,
) -> bool {
    let readiness = PromptReadinessKind::ProvenWarmFollowup;
    prompt_marker_confirms_prompt_ready(readiness, snapshot)
        || (snapshot_allows_prompt_readiness(readiness, snapshot)
            && transcript_idle_confirms_prompt_ready(snapshot, Some(transcript_path)))
}

/// #3889/#4528: an MCP-auth warning blocks readiness unless the caller proves
/// this is hosted reuse after a recorded turn. A plain Followup intent can also
/// target a genuine cold/auth-blocked pane, so it receives no exemption. Draft
/// and live busy evidence veto every readiness kind, including proven warm reuse
/// and transcript-idle fallback paths that do not require a prompt marker.
fn snapshot_allows_prompt_readiness(
    readiness: PromptReadinessKind,
    snapshot: &PromptReadinessSnapshot,
) -> bool {
    if snapshot.prompt_draft_detected {
        return false;
    }
    if snapshot.capture_available
        && crate::services::tmux_common::tmux_capture_indicates_claude_tui_busy(&snapshot.pane_tail)
    {
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

/// #3889/#4528: apply the same provenance-aware auth policy to the transcript-
/// idle path, which has no live snapshot of its own. Callers MUST gate this
/// behind the transcript-idle check via short-circuit `&&` so the pane is
/// captured only at the confirm boundary. Only ProvenWarmFollowup ignores the
/// auth warning; draft or busy evidence still blocks it. A blind capture cannot
/// assert a draft, busy frame, or auth block.
fn pane_allows_prompt_readiness(session_name: &str, readiness: PromptReadinessKind) -> bool {
    snapshot_allows_prompt_readiness(readiness, &prompt_readiness_snapshot(session_name))
}

/// Actionable, NON-timeout error for a cold-boot stranded on the
/// MCP-authentication-required welcome screen. The `run /mcp` remediation is
/// embedded so the reason reaches the operator verbatim instead of a generic
/// "transport error".
fn mcp_auth_required_error_message(session_name: &str) -> String {
    format!(
        "{PROMPT_READY_MCP_AUTH_ERROR_PREFIX}: the Claude Code cold-boot welcome screen is waiting on MCP server authentication and is silently dropping prompt submissions; run /mcp in tmux session '{session_name}' to authenticate the server, then resend"
    )
}

/// Detect — and CONFIRM across a short settle re-capture — that the pane is
/// stranded on the MCP-authentication-required cold-boot welcome screen.
///
/// Returns the confirming snapshot when the block is stable so the caller can
/// fail fast with an actionable reason. Returns `Ok(None)` when the banner is
/// absent, or when it was a transient half-rendered boot frame that cleared on
/// the re-capture (so a session still coming up is never aborted). Cancellation
/// during the settle propagates via `?`.
fn confirm_mcp_auth_block(
    session_name: &str,
    cancel_token: Option<&CancelToken>,
    first: &PromptReadinessSnapshot,
) -> Result<Option<PromptReadinessSnapshot>, String> {
    if !snapshot_indicates_mcp_auth_block(first) {
        return Ok(None);
    }
    std::thread::sleep(PROMPT_READY_MCP_AUTH_CONFIRM_SETTLE);
    check_prompt_cancel(cancel_token)?;
    let confirm = prompt_readiness_snapshot(session_name);
    if snapshot_indicates_mcp_auth_block(&confirm) {
        Ok(Some(confirm))
    } else {
        Ok(None)
    }
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

/// Whether a readiness timeout should clear a stranded follow-up composer draft
/// before returning the error, so the requeued retry can re-inject cleanly. Only
/// fires for follow-ups when requeue is enabled and the pane visibly holds a
/// real, editable unsent draft (an idle-suggestion tail reports no backspace
/// budget via `tmux_common`, so it is excluded). Pure for unit-testing.
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

/// #3880 (A2): distinguish a transcript that is `Idle` because a turn FINISHED
/// from one that is `Idle` only because the session just STARTED (or has no
/// turns yet). The global turn-state classifier (`observe_claude_jsonl_turn_state`)
/// collapses BOTH an empty transcript AND a `system{init}`-only transcript — the
/// freshly-rotated session after a Claude-native `/clear` — to `Idle`: an empty
/// file is `Idle` by definition and `system{init}` is an Idle-CLASS session-start
/// marker (`tui_turn_state.rs`, intentionally left unchanged — it also feeds the
/// codex path and other idle consumers). So on its own that signal cannot tell
/// the warm `/clear` first turn apart from a genuinely completed turn — and the
/// original "first non-whitespace line" scan was fooled exactly because a
/// `system{init}` line is non-whitespace, letting an init-only transcript take
/// the no-capture fast-path and report ready before the `❯` composer mounts.
///
/// This predicate reuses the SAME per-envelope `type`/`subtype` classification
/// the global observer applies, but counts ONLY a genuine RECORDED turn — a
/// `user`/`assistant` message turn or an authoritative turn terminator
/// (`result` / `system{turn_duration | stop_hook_summary}`). It EXCLUDES the
/// `system{init}` session-start marker and every housekeeping/mode envelope, so
/// an init-only / empty / rotated transcript reports `false` and the no-capture
/// idle fast-path falls through to the `❯` marker polling path. The scan is
/// bounded: it returns on the first recorded-turn line.
fn transcript_has_recorded_turn(transcript_path: &std::path::Path) -> bool {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(transcript_path) else {
        return false;
    };
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else {
            return false;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            // A malformed / partial line cannot prove a recorded turn; keep
            // scanning the remaining lines.
            continue;
        };
        if claude_jsonl_envelope_is_recorded_turn(&json) {
            return true;
        }
    }
    false
}

/// Per-envelope predicate behind [`transcript_has_recorded_turn`]: is this JSONL
/// envelope a genuine turn record, as opposed to session bring-up
/// (`system{init}`) or post-turn housekeeping? Mirrors the `type`/`subtype`
/// distinctions in `tui_turn_state::claude_envelope_turn_state`, minus the
/// `system{init}` SESSION-start marker (which that classifier folds into the
/// Idle family) and the `permission-mode`/`mode` housekeeping envelopes.
fn claude_jsonl_envelope_is_recorded_turn(json: &serde_json::Value) -> bool {
    match json.get("type").and_then(serde_json::Value::as_str) {
        Some("user" | "assistant" | "result") => true,
        Some("system") => matches!(
            json.get("subtype").and_then(serde_json::Value::as_str),
            Some("turn_duration" | "stop_hook_summary")
        ),
        _ => false,
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
    // #3880 (A2): an init-only / empty / freshly-rotated transcript also
    // classifies as `Idle` (`system{init}` is an Idle-class session-start
    // marker), but after a warm `/clear` the composer is still re-mounting — so
    // declaring ready here injects before the `❯` marker exists and the Enter is
    // dropped. Require a genuine RECORDED turn before trusting this no-capture
    // idle fast-path; the init-only / empty case falls through to the Notify +
    // marker polling path that confirms the composer marker via a pane snapshot.
    // A genuine completed-turn idle still fast-paths with no added latency.
    if !transcript_has_recorded_turn(transcript_path) {
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

/// #3889: record the confirmed MCP-auth cold-boot block. Logs the full pane tail
/// (which carries the welcome box + warning banner) to `claude_tui.log` so the
/// failure is diagnosable from a forensics file, not just a first-line WARN.
fn log_prompt_ready_mcp_auth_block(
    session_name: &str,
    readiness: PromptReadinessKind,
    snapshot: &PromptReadinessSnapshot,
) {
    tracing::warn!(
        tmux_session_name = session_name,
        readiness = readiness.label(),
        prompt_marker_detected = snapshot.prompt_marker_detected,
        prompt_draft_detected = snapshot.prompt_draft_detected,
        tmux_pane_alive = snapshot.tmux_pane_alive,
        capture_available = snapshot.capture_available,
        pane_tail = %snapshot.pane_tail,
        "claude_tui cold-boot stranded on MCP server authentication welcome screen; failing fast with an actionable reason instead of blind-waiting the readiness timeout"
    );
    crate::services::claude::debug_log_to(
        "claude_tui.log",
        &format!(
            "prompt readiness mcp-auth block session={} readiness={} tmux_pane_alive={} capture_available={} pane_tail:\n{}",
            session_name,
            readiness.label(),
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
    fn empty_transcript_has_no_recorded_turn() {
        // #3880 (A2): an empty / whitespace-only transcript (the warm `/clear`
        // rotation) must NOT count as a recorded turn, so the no-capture idle
        // fast-path falls through to the pane `❯` marker confirmation instead of
        // declaring ready before the composer re-mounts. A genuine turn record
        // (user/assistant/result/turn-end) counts; a session-start `system{init}`
        // marker and a missing file do NOT.
        let dir = tempfile::tempdir().unwrap();

        let empty = dir.path().join("empty.jsonl");
        std::fs::write(&empty, "").unwrap();
        assert!(!transcript_has_recorded_turn(&empty));

        let whitespace = dir.path().join("whitespace.jsonl");
        std::fs::write(&whitespace, "\n  \n\t\n").unwrap();
        assert!(!transcript_has_recorded_turn(&whitespace));

        let missing = dir.path().join("missing.jsonl");
        assert!(!transcript_has_recorded_turn(&missing));

        // #3880 (A2): a `system{init}`-only transcript is non-whitespace (the old
        // naive scan was fooled) but is a SESSION-start marker, not a turn — it
        // must NOT count as a recorded turn.
        let init_only = dir.path().join("init.jsonl");
        std::fs::write(
            &init_only,
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-1\"}\n",
        )
        .unwrap();
        assert!(!transcript_has_recorded_turn(&init_only));

        let with_turn = dir.path().join("turn.jsonl");
        std::fs::write(&with_turn, "{\"type\":\"user\"}\n").unwrap();
        assert!(transcript_has_recorded_turn(&with_turn));

        // A turn terminator alone (`result` / `system{turn_duration}`) is also a
        // recorded turn; a housekeeping `permission-mode` envelope is not.
        let result_only = dir.path().join("result.jsonl");
        std::fs::write(&result_only, "{\"type\":\"result\"}\n").unwrap();
        assert!(transcript_has_recorded_turn(&result_only));

        let housekeeping_only = dir.path().join("mode.jsonl");
        std::fs::write(&housekeeping_only, "{\"type\":\"permission-mode\"}\n").unwrap();
        assert!(!transcript_has_recorded_turn(&housekeeping_only));
    }

    #[test]
    fn init_only_transcript_falls_through_recorded_turn_idle_fast_paths() {
        // #3880 (A2): the no-capture idle fast-path
        // (`transcript_idle_confirms_prompt_ready_without_capture`) is gated on
        // BOTH a genuine recorded turn AND the lenient `Idle` classification.
        // This pins the discriminator: an init-only (freshly-rotated `/clear`)
        // transcript classifies as lenient-`Idle` — which on its own WOULD take
        // the fast-path and report ready before the `❯` composer mounts — yet
        // has NO recorded turn, so the gate rejects it and it falls through to
        // marker polling. A genuine completed-turn transcript is BOTH Idle and a
        // recorded turn, so it still fast-paths (no new latency on the common
        // case).
        use crate::services::tui_turn_state::TuiTurnState;
        let dir = tempfile::tempdir().unwrap();

        let init_only = dir.path().join("init.jsonl");
        std::fs::write(
            &init_only,
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-1\"}\n",
        )
        .unwrap();
        // Lenient classifier says Idle (the trap)…
        assert_eq!(
            crate::services::claude_tui::transcript_tail::observe_transcript_turn_state(&init_only),
            TuiTurnState::Idle
        );
        // …but the recorded-turn gate falls through, so no fast-path.
        assert!(!transcript_has_recorded_turn(&init_only));

        let recorded_idle = dir.path().join("recorded.jsonl");
        std::fs::write(
            &recorded_idle,
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-1\"}\n\
             {\"type\":\"user\"}\n\
             {\"type\":\"assistant\"}\n\
             {\"type\":\"result\"}\n",
        )
        .unwrap();
        assert_eq!(
            crate::services::claude_tui::transcript_tail::observe_transcript_turn_state(
                &recorded_idle
            ),
            TuiTurnState::Idle
        );
        assert!(transcript_has_recorded_turn(&recorded_idle));
    }

    #[test]
    fn warm_followup_lock_revalidation_keeps_idle_transcript_fallback() {
        // The warm hosted path can become ready through the transcript-idle
        // fallback before a concurrent pane capture observes the prompt marker.
        // Acquiring the composer lock must revalidate that same valid state,
        // rather than narrowing it to a marker-only condition.
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("recorded-idle.jsonl");
        std::fs::write(
            &transcript,
            "{\"type\":\"system\",\"subtype\":\"init\"}\n\
             {\"type\":\"user\"}\n\
             {\"type\":\"assistant\"}\n\
             {\"type\":\"result\"}\n",
        )
        .unwrap();
        let snapshot = PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "completed warm turn; prompt chrome not yet captured".to_string(),
        };

        assert!(
            !prompt_marker_confirms_prompt_ready(
                PromptReadinessKind::ProvenWarmFollowup,
                &snapshot
            ),
            "fixture must exercise the markerless transcript fallback"
        );
        assert!(proven_warm_followup_revalidates_prompt_ready(
            &snapshot,
            &transcript,
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

    // #3889: the MCP-auth fail-fast error must be a DISTINCT, non-timeout error
    // so the fresh-prompt retry loop does not treat it as a transient timeout and
    // reboot straight back into the same blocked screen.
    #[test]
    fn mcp_auth_required_error_is_distinct_from_timeout() {
        let error = mcp_auth_required_error_message("AgentDesk-ch-ad");
        assert!(is_mcp_auth_required_error(&error));
        assert!(
            !is_prompt_ready_timeout_error(&error),
            "MCP-auth error must not be misread as a readiness timeout (which would retry it)"
        );
        assert!(error.contains("run /mcp"), "reason must be actionable");

        // A real readiness timeout is not an MCP-auth error.
        let timeout = format!("{PROMPT_READY_TIMEOUT_ERROR_PREFIX} fresh prompt input readiness");
        assert!(is_prompt_ready_timeout_error(&timeout));
        assert!(!is_mcp_auth_required_error(&timeout));
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
    fn prompt_submit_cleanup_on_error_is_limited_to_paste_actions() {
        let literal_actions = vec![
            TuiInputAction::Literal("abc".to_string()),
            TuiInputAction::Enter,
        ];
        assert!(!actions_contain_paste_buffer(&literal_actions));
        assert!(!should_clear_draft_on_error(false, true));

        let paste_actions = vec![
            TuiInputAction::PasteBuffer("line1\nline2".to_string()),
            TuiInputAction::Enter,
        ];
        assert!(actions_contain_paste_buffer(&paste_actions));
        assert!(!should_clear_draft_on_error(true, false));
        assert!(should_clear_draft_on_error(true, true));
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

    // #3889/#4528: transcript-idle fallback applies the same provenance-aware
    // policy as marker readiness. Plain Followup rejects an auth welcome pane;
    // only ProvenWarmFollowup may ignore that warning, and drafts still veto it.
    #[test]
    fn idle_transcript_fallback_applies_kind_aware_mcp_auth_gate() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            r#"{"type":"system","subtype":"turn_duration","sessionId":"s"}"#,
        )
        .unwrap();

        let blocked = PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "\
 \u{26a0} 1 MCP server needs authentication \u{b7} run /mcp
────────────────────────────────────────────────────
\u{276f} [Pasted text #1 +59 lines]
────────────────────────────────────────────────────
  🤖 Opus(H) │ 0% │ MCP: 2 │ ⏵⏵ bypass permissions on"
                .to_string(),
        };
        // Precondition: the transcript-idle predicate alone accepts the
        // composer-chrome welcome pane (this is exactly why the gate is needed).
        let transcript_idle = transcript_idle_confirms_prompt_ready(&blocked, Some(file.path()));
        assert!(transcript_idle);
        assert!(snapshot_indicates_mcp_auth_block(&blocked));
        assert!(
            !(snapshot_allows_prompt_readiness(PromptReadinessKind::FreshTurn, &blocked)
                && transcript_idle),
            "MCP-auth welcome pane must not confirm FreshTurn via idle transcript"
        );
        assert!(
            !(snapshot_allows_prompt_readiness(PromptReadinessKind::ProvenWarmFollowup, &blocked)
                && transcript_idle),
            "an unsent draft must block proven warm reuse despite an idle transcript"
        );

        let warm_idle = PromptReadinessSnapshot {
            prompt_draft_detected: false,
            ..blocked.clone()
        };
        assert!(
            !(snapshot_allows_prompt_readiness(PromptReadinessKind::Followup, &warm_idle)
                && transcript_idle),
            "Followup intent alone must not bypass a genuine cold auth block"
        );
        assert!(
            snapshot_allows_prompt_readiness(PromptReadinessKind::ProvenWarmFollowup, &warm_idle,)
                && transcript_idle,
            "recorded-turn warm provenance may ignore a stale warning only when idle"
        );

        // No regression: a normal recorded-turn idle pane (no welcome banner)
        // still confirms ready through the same gate.
        let ready = PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "status footer without prompt glyph".to_string(),
        };
        assert!(!snapshot_indicates_mcp_auth_block(&ready));
        assert!(
            snapshot_allows_prompt_readiness(PromptReadinessKind::FreshTurn, &ready)
                && transcript_idle_confirms_prompt_ready(&ready, Some(file.path())),
            "a normal recorded-turn idle pane must still confirm ready (no regression)"
        );
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
        assert_eq!(
            PromptReadinessKind::ProvenWarmFollowup.timeout().as_secs(),
            45
        );
    }

    #[test]
    fn event_budget_is_shorter_than_full_timeout() {
        // The event-budget is meant to fail fast and yield to the polling
        // fallback long before the legacy timeout would fire.
        for kind in [
            PromptReadinessKind::FreshTurn,
            PromptReadinessKind::Followup,
            PromptReadinessKind::ProvenWarmFollowup,
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

    #[test]
    fn prompt_ready_timeout_clears_only_retryable_followup_drafts() {
        let draft = PromptReadinessSnapshot {
            prompt_marker_detected: true,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "\u{276f} unsubmitted follow-up".to_string(),
        };

        // A real, editable follow-up draft on a live pane with a backspace
        // budget should be cleared (only when requeue is enabled).
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

    fn compact_snapshot(pane_tail: &str) -> PromptReadinessSnapshot {
        PromptReadinessSnapshot {
            prompt_marker_detected: true,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: pane_tail.to_string(),
        }
    }

    #[test]
    fn compact_steering_allows_idle_and_busy_empty_composers_only() {
        let idle = compact_snapshot("Ready for input (type message + Enter)\n❯");
        let busy = compact_snapshot("✻ Working (12s · esc to interrupt)\n❯");
        assert_eq!(compact_steering_decision(&idle), Ok(()));
        assert_eq!(compact_steering_decision(&busy), Ok(()));

        let mut draft = compact_snapshot("✻ Working\n❯ operator draft");
        draft.prompt_draft_detected = true;
        assert_eq!(compact_steering_decision(&draft), Err("composer draft"));
    }

    #[test]
    fn compact_steering_rejects_auth_modals_dead_and_blind_panes() {
        let auth = compact_snapshot("⚠ 1 MCP server needs authentication · run /mcp\n❯");
        assert_eq!(
            compact_steering_decision(&auth),
            Err("MCP authentication block")
        );

        let resume = compact_snapshot("Resume from summary\nEnter to confirm\n❯");
        assert_eq!(compact_steering_decision(&resume), Err("startup dialog"));

        let permission = compact_snapshot("Allow\nDeny\nEnter to confirm\n❯");
        assert_eq!(
            compact_steering_decision(&permission),
            Err("interactive modal")
        );

        let selector = compact_snapshot("Effort\n←/→ to adjust\n❯");
        assert_eq!(
            compact_steering_decision(&selector),
            Err("interactive modal")
        );

        let mut dead = compact_snapshot("❯");
        dead.tmux_pane_alive = false;
        assert_eq!(compact_steering_decision(&dead), Err("pane dead"));

        let mut blind = compact_snapshot("❯");
        blind.capture_available = false;
        assert_eq!(
            compact_steering_decision(&blind),
            Err("pane capture unavailable")
        );
    }

    #[test]
    fn compact_passive_confirmation_accepts_only_clear_or_queued_hint() {
        assert!(compact_queued_message_hint(
            "Your queued message will be sent after current"
        ));
        assert!(compact_queued_message_hint("Queued message"));
        assert!(!compact_queued_message_hint(
            "assistant mentioned a queue in prose"
        ));
    }

    /// F4 mutation guard: the pre-Enter modal re-check
    /// (`compact_pre_enter_modal_guard`). After `/compact` is typed the composer
    /// is intentionally non-empty, so the guard must still ALLOW the confirming
    /// Enter for a plain busy pane, yet REJECT a permission / plan / startup /
    /// MCP-auth dialog that mounted during the settle window (pressing Enter
    /// would confirm its default selection). Reverting the modal checks (or
    /// making the guard always `Ok`) makes the dialog asserts below fail — the
    /// Enter would auto-confirm the modal.
    #[test]
    fn pre_enter_modal_guard_blocks_dialogs_but_allows_the_typed_compact_draft() {
        // The `/compact` literal has been typed: composer non-empty, draft shown.
        // The pre-Enter guard allows the confirming Enter even though the FULL
        // steering decision would now refuse the same frame (non-empty composer).
        let mut typed_compact = compact_snapshot("✻ Working (12s · esc to interrupt)\n❯ /compact");
        typed_compact.prompt_draft_detected = true;
        assert_eq!(compact_pre_enter_modal_guard(&typed_compact), Ok(()));
        assert!(
            compact_steering_decision(&typed_compact).is_err(),
            "the full steering decision must refuse the non-empty composer the pre-Enter guard tolerates"
        );

        // A dialog that mounted during the settle window must block the Enter.
        let permission = compact_snapshot("Allow\nDeny\nEnter to confirm\n❯");
        assert_eq!(
            compact_pre_enter_modal_guard(&permission),
            Err("interactive modal")
        );
        let selector = compact_snapshot("Effort\n←/→ to adjust\n❯");
        assert_eq!(
            compact_pre_enter_modal_guard(&selector),
            Err("interactive modal")
        );
        let auth = compact_snapshot("⚠ 1 MCP server needs authentication · run /mcp\n❯");
        assert_eq!(
            compact_pre_enter_modal_guard(&auth),
            Err("MCP authentication block")
        );
        let resume = compact_snapshot("Resume from summary\nEnter to confirm\n❯");
        assert_eq!(
            compact_pre_enter_modal_guard(&resume),
            Err("startup dialog")
        );
        let mut dead = compact_snapshot("❯");
        dead.tmux_pane_alive = false;
        assert_eq!(compact_pre_enter_modal_guard(&dead), Err("pane dead"));
        let mut blind = compact_snapshot("❯");
        blind.capture_available = false;
        assert_eq!(
            compact_pre_enter_modal_guard(&blind),
            Err("pane capture unavailable")
        );
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
