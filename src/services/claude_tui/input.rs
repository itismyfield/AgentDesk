use std::process::Output;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::runtime::Handle;
use tokio::sync::Notify;

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
const PROMPT_READY_TIMEOUT_ERROR_PREFIX: &str = "timeout waiting for claude tui";

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

/// Outcome of the hook-event fast path. `Ready` short-circuits the polling
/// fallback; `Pending` (timeout or post-event snapshot still not ready) falls
/// through to the legacy pane-scrape loop using the remaining budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HookFastPathOutcome {
    Ready,
    Pending,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptReadinessSnapshot {
    pub prompt_marker_detected: bool,
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

pub fn send_fresh_prompt(session_name: &str, prompt: &str) -> Result<(), String> {
    send_prompt_with_readiness(session_name, prompt, PromptReadinessKind::FreshTurn)
}

pub fn send_followup_prompt(session_name: &str, prompt: &str) -> Result<(), String> {
    send_prompt_with_readiness(session_name, prompt, PromptReadinessKind::Followup)
}

pub fn send_prompt(session_name: &str, prompt: &str) -> Result<(), String> {
    send_followup_prompt(session_name, prompt)
}

pub fn is_prompt_ready_timeout_error(error: &str) -> bool {
    error.starts_with(PROMPT_READY_TIMEOUT_ERROR_PREFIX)
}

pub fn prompt_readiness_snapshot(session_name: &str) -> PromptReadinessSnapshot {
    let pane = crate::services::platform::tmux::capture_pane(
        session_name,
        PROMPT_READY_CAPTURE_SCROLLBACK,
    );
    let prompt_marker_detected = pane.as_deref().is_some_and(pane_looks_ready_for_prompt);
    let pane_tail = pane
        .as_deref()
        .map(prompt_ready_debug_tail)
        .unwrap_or_else(|| "<capture unavailable>".to_string());
    PromptReadinessSnapshot {
        prompt_marker_detected,
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
) -> Result<(), String> {
    let actions = plan_prompt_submit(prompt)?;
    wait_for_prompt_ready(session_name, readiness)?;
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
                let buffer_name = format!("agentdesk-tui-input-{}", uuid::Uuid::new_v4());
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
        Err(format!("tmux send {action_name} failed: {}", output.status))
    } else {
        Err(format!("tmux send {action_name} failed: {stderr}"))
    }
}

fn wait_for_prompt_ready(session_name: &str, readiness: PromptReadinessKind) -> Result<(), String> {
    let timeout = readiness.timeout();
    let start = Instant::now();

    // 1) Event-driven fast path. Register a waiter on the global Notify *before*
    //    checking the snapshot, so an Stop event that arrives in the gap below
    //    is not missed (Notify::notified() is permit-buffered for the holder).
    let notify = crate::services::claude_tui::hook_server::prompt_ready_notify();
    let fast_path = wait_for_prompt_ready_event(notify, readiness.event_budget());

    // Re-check the snapshot once regardless of fast-path outcome — the marker
    // may already be visible (cheap success), and after a Stop the TUI needs
    // a brief moment to redraw the prompt.
    if matches!(fast_path, HookFastPathOutcome::Ready) {
        std::thread::sleep(PROMPT_READY_POST_EVENT_SETTLE);
    }
    let snapshot = prompt_readiness_snapshot(session_name);
    if snapshot.prompt_marker_detected {
        tracing::debug!(
            tmux_session_name = session_name,
            readiness = readiness.label(),
            hook_event_fast_path_hit = matches!(fast_path, HookFastPathOutcome::Ready),
            elapsed_ms = start.elapsed().as_millis() as u64,
            "claude_tui prompt ready via hook event fast path"
        );
        return Ok(());
    }
    if !snapshot.tmux_pane_alive {
        return Err("claude tui session died before prompt input was ready".to_string());
    }

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

    wait_for_prompt_ready_polling(session_name, readiness, timeout, start)
}

/// Sync wrapper that awaits the global prompt-ready notify with a bounded
/// budget. Returns `Ready` if the hook fired in time, `Pending` otherwise.
///
/// The caller must obtain the `notify` handle *before* triggering whatever
/// might race against the hook arrival, otherwise an early Stop signal is
/// dropped (`notify_waiters` only wakes already-registered waiters).
fn wait_for_prompt_ready_event(notify: Arc<Notify>, budget: Duration) -> HookFastPathOutcome {
    let fut = async move {
        tokio::select! {
            _ = notify.notified() => HookFastPathOutcome::Ready,
            _ = tokio::time::sleep(budget) => HookFastPathOutcome::Pending,
        }
    };

    match Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => {
            // No ambient runtime (e.g. unit tests outside `#[tokio::test]`).
            // Spin up a minimal current-thread runtime for the wait.
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
                    HookFastPathOutcome::Pending
                }
            }
        }
    }
}

fn wait_for_prompt_ready_polling(
    session_name: &str,
    readiness: PromptReadinessKind,
    timeout: Duration,
    start: Instant,
) -> Result<(), String> {
    let mut wait_interval = Duration::from_millis(100);
    loop {
        let snapshot = prompt_readiness_snapshot(session_name);
        if snapshot.prompt_marker_detected {
            return Ok(());
        }
        if !snapshot.tmux_pane_alive {
            return Err("claude tui session died before prompt input was ready".to_string());
        }
        if start.elapsed() >= timeout {
            log_prompt_ready_timeout(session_name, readiness, timeout, &snapshot);
            return Err(format!(
                "{PROMPT_READY_TIMEOUT_ERROR_PREFIX} {} prompt input readiness after {}s; reason=prompt_marker_not_detected; previous_tui_turn_still_running=true; capture_available={}",
                readiness.label(),
                timeout.as_secs(),
                snapshot.capture_available
            ));
        }
        std::thread::sleep(wait_interval);
        wait_interval = std::cmp::min(wait_interval * 2, Duration::from_millis(1000));
    }
}

fn pane_looks_ready_for_prompt(pane: &str) -> bool {
    crate::services::tmux_common::tmux_capture_indicates_claude_tui_ready_for_input(pane)
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
        previous_tui_turn_still_running = snapshot.tmux_pane_alive && !snapshot.prompt_marker_detected,
        tmux_pane_alive = snapshot.tmux_pane_alive,
        capture_available = snapshot.capture_available,
        pane_tail = %snapshot.pane_tail,
        "claude_tui prompt readiness timed out"
    );
    crate::services::claude::debug_log_to(
        "claude_tui.log",
        &format!(
            "prompt readiness timeout session={} readiness={} timeout={}s prompt_marker_detected={} previous_tui_turn_still_running={} tmux_pane_alive={} capture_available={} pane_tail:\n{}",
            session_name,
            readiness.label(),
            timeout.as_secs(),
            snapshot.prompt_marker_detected,
            snapshot.tmux_pane_alive && !snapshot.prompt_marker_detected,
            snapshot.tmux_pane_alive,
            snapshot.capture_available,
            snapshot.pane_tail
        ),
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
    fn prompt_ready_timeouts_are_split_for_fresh_and_followup_turns() {
        assert_eq!(PromptReadinessKind::FreshTurn.timeout().as_secs(), 120);
        assert_eq!(PromptReadinessKind::Followup.timeout().as_secs(), 45);
    }

    #[test]
    fn event_budget_is_shorter_than_full_timeout() {
        // The event-budget is meant to fail fast and yield to the polling
        // fallback long before the legacy timeout would fire.
        for kind in [PromptReadinessKind::FreshTurn, PromptReadinessKind::Followup] {
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

    #[test]
    fn fast_path_works_without_ambient_runtime() {
        // Exercises the fallback runtime branch that callers without tokio
        // runtime hit (e.g. plain sync test contexts). Budget is short enough
        // to keep the test cheap.
        let notify = Arc::new(Notify::new());
        let outcome = wait_for_prompt_ready_event(notify, Duration::from_millis(30));
        assert_eq!(outcome, HookFastPathOutcome::Pending);
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
}
