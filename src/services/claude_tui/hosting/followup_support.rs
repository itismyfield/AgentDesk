use std::sync::mpsc::Sender;

use crate::services::agent_protocol::StreamMessage;
use crate::services::claude::debug_log;
use crate::services::provider::{CancelToken, cancel_requested};

#[cfg(unix)]
const CLAUDE_TUI_STRANDED_DRAFT_CLEAR_ATTEMPTS: usize = 2;
#[cfg(unix)]
const CLAUDE_TUI_BUSY_FOLLOWUP_NOTICE: &str = "⚠ Claude TUI가 아직 이전 터미널 턴을 처리 중이라 이 메시지를 주입하지 않았습니다. 현재 응답이 끝난 뒤 다시 보내 주세요.";
#[cfg(unix)]
pub(crate) fn emit_claude_tui_zero_harvest(
    kind: &str,
    report_channel_id: Option<u64>,
    tmux_session_name: &str,
    transcript_path: &str,
    start_offset: u64,
    transcript_len: u64,
) {
    tracing::warn!(
        tmux_session_name,
        transcript_path,
        start_offset,
        transcript_len,
        "claude_tui delivered turn harvested ZERO stream messages — the turn's response text never entered the bridge ({kind})"
    );
    crate::services::observability::emit_inflight_lifecycle_event(
        "claude",
        report_channel_id.unwrap_or(0),
        None,
        None,
        None,
        kind,
        serde_json::json!({
            "tmux_session_name": tmux_session_name,
            "transcript_path": transcript_path,
            "start_offset": start_offset,
            "transcript_len": transcript_len,
        }),
    );
}

#[cfg(unix)]
pub(super) fn emit_claude_tui_busy_followup_notice(
    sender: &Sender<StreamMessage>,
    tmux_session_name: &str,
    snapshot: &crate::services::claude_tui::input::PromptReadinessSnapshot,
    requeue_for_retry: bool,
) {
    tracing::warn!(
        tmux_session_name,
        prompt_marker_detected = snapshot.prompt_marker_detected,
        prompt_draft_detected = snapshot.prompt_draft_detected,
        prompt_draft_blocks_submission = snapshot.tmux_pane_alive && snapshot.prompt_draft_detected,
        tmux_pane_alive = snapshot.tmux_pane_alive,
        capture_available = snapshot.capture_available,
        pane_tail = %snapshot.pane_tail,
        "claude_tui follow-up blocked before prompt submission because hosted TUI is busy"
    );
    debug_log(&format!(
        "Claude TUI follow-up blocked before prompt submission: session={} prompt_marker_detected={} prompt_draft_detected={} prompt_draft_blocks_submission={} tmux_pane_alive={} capture_available={} pane_tail:\n{}",
        tmux_session_name,
        snapshot.prompt_marker_detected,
        snapshot.prompt_draft_detected,
        snapshot.tmux_pane_alive && snapshot.prompt_draft_detected,
        snapshot.tmux_pane_alive,
        snapshot.capture_available,
        snapshot.pane_tail
    ));
    if requeue_for_retry {
        return;
    }
    let _ = sender.send(StreamMessage::Text {
        content: CLAUDE_TUI_BUSY_FOLLOWUP_NOTICE.to_string(),
    });
    let _ = sender.send(StreamMessage::Done {
        result: String::new(),
        session_id: None,
    });
}

#[cfg(all(test, unix))]
mod busy_followup_notice_tests {
    use super::*;

    fn busy_snapshot() -> crate::services::claude_tui::input::PromptReadinessSnapshot {
        crate::services::claude_tui::input::PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "Thinking...".to_string(),
        }
    }

    fn emitted_text(requeue_for_retry: bool) -> Option<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        emit_claude_tui_busy_followup_notice(
            &tx,
            "AgentDesk-claude-test",
            &busy_snapshot(),
            requeue_for_retry,
        );
        match rx.try_recv() {
            Ok(StreamMessage::Text { content }) => Some(content),
            Ok(other) => panic!("expected text notice, got {other:?}"),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                panic!("notice channel disconnected")
            }
        }
    }

    #[test]
    fn busy_followup_retry_is_silent() {
        assert_eq!(
            emitted_text(true),
            None,
            "automatic retry state is represented by the source-message reaction, not a card"
        );
    }

    #[test]
    fn busy_followup_notice_reports_manual_resend_when_requeue_is_disabled() {
        let text = emitted_text(false).expect("manual resend notice");
        assert!(text.contains("다시 보내 주세요"));
    }

    #[test]
    fn busy_followup_retry_emits_no_stream_frames() {
        let (tx, rx) = std::sync::mpsc::channel();
        emit_claude_tui_busy_followup_notice(&tx, "AgentDesk-claude-test", &busy_snapshot(), true);
        assert!(
            rx.try_recv().is_err(),
            "automatic-retry path must emit neither a retry card nor Done before the retryable Error"
        );
    }

    #[test]
    fn busy_followup_notice_keeps_legacy_done_when_requeue_is_disabled() {
        let (tx, rx) = std::sync::mpsc::channel();
        emit_claude_tui_busy_followup_notice(&tx, "AgentDesk-claude-test", &busy_snapshot(), false);
        assert!(matches!(
            rx.recv().expect("notice text"),
            StreamMessage::Text { .. }
        ));
        assert!(matches!(
            rx.recv().expect("legacy done"),
            StreamMessage::Done { .. }
        ));
    }
}

#[cfg(unix)]
pub(super) fn claude_tui_followup_busy_before_submit(
    tmux_session_name: &str,
    transcript_path: Option<&std::path::Path>,
) -> Option<crate::services::claude_tui::input::PromptReadinessSnapshot> {
    let snapshot = crate::services::claude_tui::input::prompt_readiness_snapshot(tmux_session_name);
    let transcript_turn_state = transcript_path.map(|transcript_path| {
        crate::services::claude_tui::transcript_tail::observe_transcript_turn_state(transcript_path)
    });
    claude_tui_followup_busy_before_submit_from_snapshot(snapshot, transcript_turn_state)
}

#[cfg(unix)]
pub(crate) fn claude_tui_followup_busy_before_submit_from_snapshot(
    snapshot: crate::services::claude_tui::input::PromptReadinessSnapshot,
    transcript_turn_state: Option<crate::services::tui_turn_state::TuiTurnState>,
) -> Option<crate::services::claude_tui::input::PromptReadinessSnapshot> {
    if let Some(transcript_turn_state) = transcript_turn_state {
        match transcript_turn_state {
            crate::services::tui_turn_state::TuiTurnState::Idle => {
                if snapshot.tmux_pane_alive && snapshot.prompt_draft_detected {
                    return Some(snapshot);
                }
                return None;
            }
            crate::services::tui_turn_state::TuiTurnState::Unknown
                if snapshot.tmux_pane_alive && snapshot.prompt_draft_detected =>
            {
                return Some(snapshot);
            }
            state if state.is_busy() && snapshot.tmux_pane_alive => return Some(snapshot),
            _ => {}
        }
    }
    if snapshot.tmux_pane_alive && snapshot.prompt_draft_detected {
        Some(snapshot)
    } else {
        None
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClaudeTuiStrandedPromptDraftState {
    IdleTranscript,
    UnknownTranscript,
}

#[cfg(unix)]
impl ClaudeTuiStrandedPromptDraftState {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::IdleTranscript => "idle",
            Self::UnknownTranscript => "unknown",
        }
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClaudeTuiWarmFollowupSubmitPlan {
    pub(crate) submit_existing_session: bool,
    pub(crate) recheck_busy_before_submit: bool,
}

#[cfg(unix)]
pub(crate) fn claude_tui_warm_followup_submit_plan(
    recreate_before_submit: bool,
    prompt_draft_cleared_before_submit: bool,
) -> ClaudeTuiWarmFollowupSubmitPlan {
    ClaudeTuiWarmFollowupSubmitPlan {
        submit_existing_session: !recreate_before_submit,
        recheck_busy_before_submit: !prompt_draft_cleared_before_submit,
    }
}

#[cfg(unix)]
pub(crate) fn claude_tui_followup_stranded_prompt_draft_state(
    snapshot: &crate::services::claude_tui::input::PromptReadinessSnapshot,
    transcript_path: &std::path::Path,
) -> Option<ClaudeTuiStrandedPromptDraftState> {
    let transcript_turn_state =
        crate::services::claude_tui::transcript_tail::observe_transcript_turn_state(
            transcript_path,
        );
    if !claude_tui_snapshot_has_recoverable_prompt_draft(snapshot) {
        return None;
    }
    match transcript_turn_state {
        crate::services::tui_turn_state::TuiTurnState::Idle => {
            Some(ClaudeTuiStrandedPromptDraftState::IdleTranscript)
        }
        crate::services::tui_turn_state::TuiTurnState::Unknown => {
            Some(ClaudeTuiStrandedPromptDraftState::UnknownTranscript)
        }
        _ => None,
    }
}

#[cfg(unix)]
fn claude_tui_snapshot_has_recoverable_prompt_draft(
    snapshot: &crate::services::claude_tui::input::PromptReadinessSnapshot,
) -> bool {
    snapshot.tmux_pane_alive
        && snapshot.prompt_draft_detected
        && crate::services::claude_tui::input::claude_prompt_draft_backspace_budget_from_tail(
            &snapshot.pane_tail,
        )
        .is_some()
}

#[cfg(unix)]
pub(super) fn claude_tui_prompt_remained_in_input_buffer(
    snapshot: &crate::services::claude_tui::input::PromptReadinessSnapshot,
    prompt: &str,
) -> bool {
    if !snapshot.tmux_pane_alive || !snapshot.capture_available {
        return false;
    }
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return false;
    }
    snapshot.pane_tail.lines().rev().take(12).any(|line| {
        let trimmed = line.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{00a0}');
        let Some(rest) = trimmed.strip_prefix('\u{276f}') else {
            return false;
        };
        let rest = rest.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{00a0}');
        !rest
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("[User:"))
            && rest.contains(prompt)
    })
}

#[cfg(unix)]
pub(super) fn claude_tui_zero_advance_input_buffer_error(
    tmux_session_name: &str,
    transcript_path: &str,
    start_offset: u64,
    snapshot: &crate::services::claude_tui::input::PromptReadinessSnapshot,
) -> String {
    format!(
        "claude tui follow-up produced no new transcript bytes and prompt remained in TUI input buffer; tmux_session={}; transcript_path={}; start_offset={}; capture_available={}; prompt_marker_detected={}; prompt_draft_detected={}; pane_tail={}",
        tmux_session_name,
        transcript_path,
        start_offset,
        snapshot.capture_available,
        snapshot.prompt_marker_detected,
        snapshot.prompt_draft_detected,
        snapshot.pane_tail
    )
}

#[cfg(all(test, unix))]
mod claude_tui_prompt_buffer_tests {
    use super::*;

    #[test]
    fn detects_stuck_followup_prompt_even_when_generic_draft_heuristic_ignores_footer() {
        let snapshot = crate::services::claude_tui::input::PromptReadinessSnapshot {
            prompt_marker_detected: true,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "\
✻ Baked for 10m 9s
────────────────────────────────────────────────────────────────────────────
❯\u{00a0}응답에 정확히 한 줄로 [E2E:E6:AFTER] 만 출력해줘.
────────────────────────────────────────────────────────────────────────────
  CLAUDE.md: 1, MCP: 2 │ Tools: 5 done
  ⏵⏵ bypass permissions on"
                .to_string(),
        };

        assert!(claude_tui_prompt_remained_in_input_buffer(
            &snapshot,
            "응답에 정확히 한 줄로 [E2E:E6:AFTER] 만 출력해줘."
        ));
    }

    #[test]
    fn ignores_submitted_discord_history_prompt() {
        let snapshot = crate::services::claude_tui::input::PromptReadinessSnapshot {
            prompt_marker_detected: false,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "\
❯ [User: 명령봇 (ID: 1)] 응답에 정확히 한 줄로 [E2E:E6:AFTER] 만 출력해줘.
⏺ [E2E:E6:AFTER]"
                .to_string(),
        };

        assert!(!claude_tui_prompt_remained_in_input_buffer(
            &snapshot,
            "응답에 정확히 한 줄로 [E2E:E6:AFTER] 만 출력해줘."
        ));
    }
}

#[cfg(unix)]
pub(crate) fn claude_tui_unknown_transcript_draft_recreate_allowed(
    snapshot: &crate::services::claude_tui::input::PromptReadinessSnapshot,
) -> bool {
    if !snapshot.tmux_pane_alive || !snapshot.prompt_draft_detected || !snapshot.capture_available {
        return false;
    }
    let tail = snapshot.pane_tail.as_str();
    let tail_lower = tail.to_ascii_lowercase();
    if tail_lower.contains("esc to interrupt")
        || tail_lower.contains("processing")
        || tail_lower.contains("thinking")
        || tail_lower.contains("running")
    {
        return false;
    }
    tail.contains("Baked for")
        || tail.contains("Brewed for")
        || (tail.contains("Tools:") && tail.contains(" done"))
}

#[cfg(unix)]
fn ensure_tmux_key_send_success(
    output: std::process::Output,
    action_name: &str,
) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        Err(format!("tmux send {action_name} failed: {}", output.status))
    } else {
        Err(format!("tmux send {action_name} failed: {stderr}"))
    }
}

#[cfg(unix)]
fn clear_claude_tui_draft_with_backspaces(
    tmux_session_name: &str,
    snapshot: &mut crate::services::claude_tui::input::PromptReadinessSnapshot,
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    let Some(mut remaining) =
        crate::services::claude_tui::input::claude_prompt_draft_backspace_budget_from_tail(
            &snapshot.pane_tail,
        )
    else {
        return Ok(());
    };
    let output = crate::services::platform::tmux::send_keys(tmux_session_name, &["C-e"])?;
    ensure_tmux_key_send_success(output, "draft-clear-cursor-end")?;
    while remaining > 0 {
        if cancel_requested(cancel_token) {
            return Err(
                crate::services::claude_tui::input::PROMPT_READY_CANCELLED_ERROR.to_string(),
            );
        }
        let batch = remaining.min(32);
        let keys = vec!["BSpace"; batch];
        let output = crate::services::platform::tmux::send_keys(tmux_session_name, &keys)?;
        ensure_tmux_key_send_success(output, "draft-clear-backspace")?;
        remaining -= batch;
    }
    std::thread::sleep(std::time::Duration::from_millis(120));
    *snapshot = crate::services::claude_tui::input::prompt_readiness_snapshot(tmux_session_name);
    Ok(())
}

#[cfg(unix)]
pub(super) fn clear_claude_tui_stranded_prompt_draft(
    tmux_session_name: &str,
    cancel_token: Option<&CancelToken>,
) -> Result<crate::services::claude_tui::input::PromptReadinessSnapshot, String> {
    let clear_key_plans: [&[&str]; 3] = [&["C-e", "C-u"], &["Escape"], &["C-e", "C-u"]];
    let mut snapshot =
        crate::services::claude_tui::input::prompt_readiness_snapshot(tmux_session_name);
    if !snapshot.prompt_draft_detected || !snapshot.tmux_pane_alive {
        return Ok(snapshot);
    }

    for attempt in 1..=CLAUDE_TUI_STRANDED_DRAFT_CLEAR_ATTEMPTS {
        if cancel_requested(cancel_token) {
            return Err(
                crate::services::claude_tui::input::PROMPT_READY_CANCELLED_ERROR.to_string(),
            );
        }
        for keys in clear_key_plans {
            let output = crate::services::platform::tmux::send_keys(tmux_session_name, keys)?;
            ensure_tmux_key_send_success(output, "clear-draft")?;
            std::thread::sleep(std::time::Duration::from_millis(120));
            if cancel_requested(cancel_token) {
                return Err(
                    crate::services::claude_tui::input::PROMPT_READY_CANCELLED_ERROR.to_string(),
                );
            }
            snapshot =
                crate::services::claude_tui::input::prompt_readiness_snapshot(tmux_session_name);
            if !snapshot.prompt_draft_detected || !snapshot.tmux_pane_alive {
                return Ok(snapshot);
            }
        }
        clear_claude_tui_draft_with_backspaces(tmux_session_name, &mut snapshot, cancel_token)?;
        if !snapshot.prompt_draft_detected || !snapshot.tmux_pane_alive {
            return Ok(snapshot);
        }
        tracing::warn!(
            tmux_session_name,
            attempt,
            prompt_marker_detected = snapshot.prompt_marker_detected,
            prompt_draft_detected = snapshot.prompt_draft_detected,
            tmux_pane_alive = snapshot.tmux_pane_alive,
            capture_available = snapshot.capture_available,
            pane_tail = %snapshot.pane_tail,
            "claude_tui stranded prompt draft still present after clear attempt"
        );
    }

    Ok(snapshot)
}

#[cfg(unix)]
pub(super) fn gently_clear_claude_tui_prompt_draft(
    tmux_session_name: &str,
    cancel_token: Option<&CancelToken>,
) -> Result<crate::services::claude_tui::input::PromptReadinessSnapshot, String> {
    let mut snapshot =
        crate::services::claude_tui::input::prompt_readiness_snapshot(tmux_session_name);
    if !snapshot.prompt_draft_detected || !snapshot.tmux_pane_alive {
        return Ok(snapshot);
    }

    for attempt in 1..=CLAUDE_TUI_STRANDED_DRAFT_CLEAR_ATTEMPTS {
        if cancel_requested(cancel_token) {
            return Err(
                crate::services::claude_tui::input::PROMPT_READY_CANCELLED_ERROR.to_string(),
            );
        }
        let output =
            crate::services::platform::tmux::send_keys(tmux_session_name, &["C-e", "C-u"])?;
        ensure_tmux_key_send_success(output, "gentle-clear-draft")?;
        std::thread::sleep(std::time::Duration::from_millis(120));
        snapshot = crate::services::claude_tui::input::prompt_readiness_snapshot(tmux_session_name);
        if !snapshot.prompt_draft_detected || !snapshot.tmux_pane_alive {
            return Ok(snapshot);
        }
        clear_claude_tui_draft_with_backspaces(tmux_session_name, &mut snapshot, cancel_token)?;
        if !snapshot.prompt_draft_detected || !snapshot.tmux_pane_alive {
            return Ok(snapshot);
        }
        tracing::warn!(
            tmux_session_name,
            attempt,
            prompt_marker_detected = snapshot.prompt_marker_detected,
            prompt_draft_detected = snapshot.prompt_draft_detected,
            tmux_pane_alive = snapshot.tmux_pane_alive,
            capture_available = snapshot.capture_available,
            pane_tail = %snapshot.pane_tail,
            "claude_tui prompt draft still present after gentle clear attempt"
        );
    }

    Ok(snapshot)
}
