use crate::services::tui_turn_state::TuiTurnState;

/// Return line indexes occupied by authenticated Claude TUI background-agent
/// chrome. The three shapes deliberately include their TUI-only placement:
/// a spinner footer, an indented management affordance, or a contiguous task
/// table headed by `⏺ main`. Text in an assistant response may use the same
/// words or `◯` bullet, but it cannot satisfy these structural anchors.
pub(crate) fn claude_tui_background_agent_status_line_indexes(pane: &str) -> Vec<usize> {
    let lines = pane.lines().collect::<Vec<_>>();
    let Some(prompt_index) = lines.iter().rposition(|line| line.trim() == "❯") else {
        return Vec::new();
    };
    // Claude's persistent chrome is painted around the active bottom composer:
    // its waiting/management footer is immediately above the separator and
    // composer, while its task table is below it. Restrict matching to that
    // bottom zone so identical lines in the assistant transcript are never
    // promoted to live status.
    let chrome_start = prompt_index.saturating_sub(2);
    let mut statuses = Vec::new();
    let mut task_table_open = false;

    for (index, line) in lines.iter().enumerate().skip(chrome_start) {
        if is_claude_tui_background_agent_footer(line)
            || is_claude_tui_background_agent_management_line(line)
        {
            statuses.push(index);
            task_table_open = false;
            continue;
        }

        if line == &"  ⏺ main" {
            task_table_open = true;
            continue;
        }

        if task_table_open && is_claude_tui_background_agent_task_row(line) {
            statuses.push(index);
            continue;
        }

        task_table_open = false;
    }

    statuses
}

fn is_claude_tui_background_agent_footer(line: &str) -> bool {
    let Some(count) = line.strip_prefix("✻ Waiting for ") else {
        return false;
    };
    let count = count
        .strip_suffix(" background agent to finish")
        .or_else(|| count.strip_suffix(" background agents to finish"));
    count.is_some_and(|count| !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit()))
}

fn is_claude_tui_background_agent_management_line(line: &str) -> bool {
    let Some(affordance) = line.strip_prefix("  ⎿  Backgrounded agent (") else {
        return false;
    };
    affordance.ends_with(')') && affordance.contains("↓ to manage · ctrl+o to expand")
}

fn is_claude_tui_background_agent_task_row(line: &str) -> bool {
    let Some(columns) = line.strip_prefix("  ◯ ") else {
        return false;
    };
    let columns = columns
        .split("  ")
        .filter(|column| !column.trim().is_empty())
        .collect::<Vec<_>>();
    columns.len() == 3 && is_claude_tui_elapsed_duration(columns[2].trim())
}

fn is_claude_tui_elapsed_duration(value: &str) -> bool {
    let components = value.split_whitespace().collect::<Vec<_>>();
    !components.is_empty()
        && components.iter().all(|component| {
            let Some(unit) = component.chars().last() else {
                return false;
            };
            matches!(unit, 's' | 'm' | 'h')
                && component.strip_suffix(unit).is_some_and(|digits| {
                    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
                })
        })
}

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

    let background_status_lines = claude_tui_background_agent_status_line_indexes(pane);
    let foreground_only = pane
        .lines()
        .enumerate()
        .filter_map(|(index, line)| (!background_status_lines.contains(&index)).then_some(line))
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
    fn background_agent_chrome_requires_bottom_prompt_zone() {
        assert_eq!(
            claude_tui_background_agent_status_line_indexes(BACKGROUND_WAITING_PANE),
            vec![1, 9, 10],
        );
        assert_eq!(
            claude_tui_background_agent_status_line_indexes(
                "⏺ Agent(read story)\n  ⎿  Backgrounded agent (↓ to manage · ctrl+o to expand)\n❯"
            ),
            vec![1],
        );

        let quoted_assistant_output = "\
```text
✻ Waiting for 3 background agents to finish
  ⎿  Backgrounded agent (↓ to manage · ctrl+o to expand)
  ⏺ main
  ◯ reviewer       Watching CI                         6m 13s
```
────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  ◆ Opus(M) │ Tools: 224 done";
        assert!(
            claude_tui_background_agent_status_line_indexes(quoted_assistant_output).is_empty()
        );
    }

    #[test]
    fn background_agent_footer_rejects_indented_assistant_text() {
        let pane = "  ✻ Waiting for 3 background agents to finish\n\
────────────────────────────────────────────────────\n\
❯\n\
────────────────────────────────────────────────────\n\
  ◆ Opus(M) │ Tools: 224 done";
        assert!(claude_tui_background_agent_status_line_indexes(pane).is_empty());
    }

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
