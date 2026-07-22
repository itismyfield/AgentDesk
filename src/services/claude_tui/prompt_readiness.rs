use crate::services::tui_turn_state::TuiTurnState;

/// Return foreground live-turn evidence from a Claude TUI pane.
///
/// A completed foreground turn may leave detached background-agent chrome on
/// screen. That chrome is ignored only when the session-bound transcript has
/// authoritatively reached `Idle`; busy or unknown transcripts keep the full,
/// conservative pane classifier.
pub(super) fn pane_has_foreground_busy_evidence(
    pane: &str,
    capture_available: bool,
    transcript_turn_state: Option<TuiTurnState>,
) -> bool {
    if !capture_available {
        return false;
    }
    if transcript_turn_state != Some(TuiTurnState::Idle) {
        return crate::services::tmux_common::tmux_capture_indicates_claude_tui_busy(pane);
    }

    let foreground_only = pane
        .lines()
        .filter(|line| {
            !crate::services::tmux_common::tmux_line_is_claude_tui_background_agent_status(line)
        })
        .collect::<Vec<_>>()
        .join("\n");
    crate::services::tmux_common::tmux_capture_indicates_claude_tui_busy(&foreground_only)
}

/// Derive the timeout diagnostic from the transcript when it is conclusive.
/// Unknown/no transcript retains the legacy pane-marker fallback.
pub(super) fn previous_turn_still_running(
    pane_alive: bool,
    prompt_marker_detected: bool,
    transcript_turn_state: Option<TuiTurnState>,
) -> bool {
    pane_alive
        && match transcript_turn_state {
            Some(TuiTurnState::Idle) => false,
            Some(TuiTurnState::Streaming | TuiTurnState::UserSubmitted) => true,
            Some(TuiTurnState::Unknown) | None => !prompt_marker_detected,
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BACKGROUND_WAITING_PANE: &str = "\
⏺ Foreground answer complete
✻ Waiting for 3 background agents to finish
────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  ◆ Opus(M) │ Tools: 224 done
  ⏵⏵ bypass permissions on · 2 shells

  ⏺ main
  ◯ reviewer       Watching CI                         6m 13s
  ◯ implementer    Updating tests                      3m 52s";

    #[test]
    fn authoritative_idle_excludes_background_agent_status_from_busy_evidence() {
        assert!(!pane_has_foreground_busy_evidence(
            BACKGROUND_WAITING_PANE,
            true,
            Some(TuiTurnState::Idle),
        ));
    }

    #[test]
    fn foreground_streaming_still_vetoes_with_background_agent_status_present() {
        let pane = format!("{BACKGROUND_WAITING_PANE}\n✳ Architecting… (12s · esc to interrupt)");
        assert!(pane_has_foreground_busy_evidence(
            &pane,
            true,
            Some(TuiTurnState::Streaming),
        ));
    }

    #[test]
    fn idle_transcript_never_reports_previous_turn_running() {
        assert!(!previous_turn_still_running(
            true,
            false,
            Some(TuiTurnState::Idle),
        ));
        assert!(previous_turn_still_running(
            true,
            true,
            Some(TuiTurnState::Streaming),
        ));
    }
}
