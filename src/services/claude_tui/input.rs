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
const PROMPT_READY_TIMEOUT_ERROR_PREFIX: &str = "timeout waiting for claude tui";
pub const PROMPT_READY_CANCELLED_ERROR: &str = "claude tui prompt readiness wait cancelled";

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

pub fn plan_cancel() -> Vec<TuiInputAction> {
    vec![TuiInputAction::Escape]
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

pub fn send_prompt(
    session_name: &str,
    prompt: &str,
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    send_followup_prompt(session_name, prompt, cancel_token)
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

pub fn send_cancel(session_name: &str) -> Result<(), String> {
    run_actions(session_name, &plan_cancel(), None)
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
    if let Err(error) = run_actions(session_name, &[TuiInputAction::Escape], cancel_token) {
        tracing::warn!(
            tmux_session_name = session_name,
            error = %error,
            "failed to clear Claude TUI draft after prompt submit retries"
        );
    }
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
    loop {
        check_prompt_cancel(cancel_token)?;
        let snapshot = prompt_readiness_snapshot(session_name);
        check_prompt_cancel(cancel_token)?;
        if prompt_marker_confirms_prompt_ready(&snapshot) {
            return Ok(());
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
            log_prompt_ready_timeout(session_name, readiness, timeout, &snapshot);
            return Err(format!(
                "{PROMPT_READY_TIMEOUT_ERROR_PREFIX} {} prompt input readiness after {}s; reason=prompt_marker_not_detected; previous_tui_turn_still_running=true; capture_available={}",
                readiness.label(),
                timeout.as_secs(),
                snapshot.capture_available
            ));
        }
        std::thread::sleep(wait_interval);
        check_prompt_cancel(cancel_token)?;
        wait_interval = std::cmp::min(wait_interval * 2, Duration::from_millis(1000));
    }
}

fn pane_looks_ready_for_prompt(pane: &str) -> bool {
    crate::services::tmux_common::tmux_capture_indicates_claude_tui_ready_for_input(pane)
}

fn prompt_marker_confirms_prompt_ready(snapshot: &PromptReadinessSnapshot) -> bool {
    snapshot.prompt_marker_detected && !snapshot.prompt_draft_detected
}

fn transcript_idle_confirms_prompt_ready(
    snapshot: &PromptReadinessSnapshot,
    transcript_path: Option<&std::path::Path>,
) -> bool {
    if !snapshot.tmux_pane_alive || snapshot.prompt_draft_detected {
        return false;
    }
    transcript_path.is_some_and(|path| {
        crate::services::claude_tui::transcript_tail::observe_transcript_turn_state(path)
            == crate::services::tui_turn_state::TuiTurnState::Idle
    })
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

fn validate_prompt_text(input: &str) -> Result<(), String> {
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
            "{PROMPT_READY_TIMEOUT_ERROR_PREFIX} follow-up prompt input readiness after 45s; reason=prompt_marker_not_detected; previous_tui_turn_still_running=true; capture_available=true"
        );
        assert!(is_prompt_ready_timeout_error(&synthetic));
        assert!(synthetic.contains("follow-up"));
        assert!(synthetic.contains("45s"));
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
