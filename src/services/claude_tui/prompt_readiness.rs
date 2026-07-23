use crate::services::terminal_status_formatting::{
    ContextWindowUsage, format_usage_status_segments,
};
use crate::services::tui_turn_state::TuiTurnState;

pub(crate) fn render_prompt_readiness_panel_line(
    model_label: &str,
    progress_bar: &str,
    usage: Option<String>,
) -> Option<String> {
    usage.map(|usage| format!("  {model_label} │ {progress_bar} │ {usage}"))
}

fn normalize_prompt_readiness_panel_line(line: &str) -> Option<String> {
    let segments = line.trim().split(" │ ").map(str::trim).collect::<Vec<_>>();
    let model_label = *segments.first()?;
    let progress_bar = *segments.get(1)?;
    let context_percent = segments.get(2)?.strip_suffix('%')?.parse::<u64>().ok()?;
    let tokens = *segments.get(3)?;
    let (used_tokens, window_tokens) = tokens.split_once('/')?;
    let context = ContextWindowUsage {
        used_tokens: parse_compact_tokens(used_tokens)?,
        window_tokens: parse_compact_tokens(window_tokens)?,
    };
    let rendered_percent = ((u128::from(context.used_tokens.min(context.window_tokens)) * 100)
        / u128::from(context.window_tokens)) as u64;
    if rendered_percent != context_percent {
        return None;
    }
    let usage = format_usage_status_segments(segments.iter().skip(4).copied(), Some(context));
    render_prompt_readiness_panel_line(model_label, progress_bar, usage)
}

fn parse_compact_tokens(value: &str) -> Option<u64> {
    let value = value.trim();
    if let Some(value) = value.strip_suffix('K') {
        value
            .parse::<f64>()
            .ok()
            .map(|value| (value * 1_000.0) as u64)
    } else if let Some(value) = value.strip_suffix('M') {
        value
            .parse::<f64>()
            .ok()
            .map(|value| (value * 1_000_000.0) as u64)
    } else {
        value.parse().ok()
    }
}

pub(crate) fn normalize_prompt_readiness_panel_in_capture(pane: &str) -> String {
    pane.lines()
        .map(|line| normalize_prompt_readiness_panel_line(line).unwrap_or_else(|| line.to_string()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Return line indexes occupied by authenticated Claude TUI background-agent
/// chrome. The three shapes deliberately include their TUI-only placement:
/// a spinner footer, an indented management affordance, or a contiguous task
/// table headed by `⏺ main`. Text in an assistant response may use the same
/// words or `◯` bullet, but it cannot satisfy these structural anchors.
pub(crate) fn claude_tui_background_agent_status_line_indexes(pane: &str) -> Vec<usize> {
    let lines = pane.lines().collect::<Vec<_>>();
    let Some(prompt_index) = lines
        .iter()
        .rposition(|line| line.trim_start().starts_with('❯'))
    else {
        return Vec::new();
    };
    // Claude's persistent chrome is painted around the active bottom composer.
    // A waiting footer has its own separator/composer block; a management
    // affordance directly precedes the composer; and a task table is below it.
    // Require these adjacent structures so transcript text quoting exact chrome
    // cannot become a keep-alive signal.
    let mut statuses = Vec::new();
    if let Some(footer_index) =
        claude_tui_background_agent_footer_before_composer(&lines, prompt_index)
    {
        statuses.push(footer_index);
    }
    if prompt_index >= 2
        && is_claude_tui_background_agent_management_header(lines[prompt_index - 2])
        && is_claude_tui_background_agent_management_line(lines[prompt_index - 1])
    {
        statuses.push(prompt_index - 1);
    }

    let mut task_table_open = false;
    for (index, line) in lines.iter().enumerate().skip(prompt_index + 1) {
        if line.starts_with("  ⏺ main") {
            task_table_open = true;
            continue;
        }
        if task_table_open {
            if is_claude_tui_background_agent_task_row(line) {
                statuses.push(index);
                continue;
            }
            break;
        }
    }

    statuses
}

fn claude_tui_background_agent_footer_before_composer(
    lines: &[&str],
    prompt_index: usize,
) -> Option<usize> {
    let separator_index = prompt_index.checked_sub(1)?;
    if !is_claude_tui_horizontal_separator(lines[separator_index]) {
        return None;
    }
    const MAX_FOOTER_BLANK_LINES: usize = 1;
    let blank_lines = lines[..separator_index]
        .iter()
        .rev()
        .take_while(|line| line.trim().is_empty())
        .count();
    if blank_lines > MAX_FOOTER_BLANK_LINES {
        return None;
    }
    let footer_index = separator_index.checked_sub(blank_lines + 1)?;
    is_claude_tui_background_agent_footer(lines[footer_index]).then_some(footer_index)
}

fn is_claude_tui_horizontal_separator(line: &str) -> bool {
    line.chars().count() >= 8 && line.chars().all(|character| character == '─')
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

fn is_claude_tui_background_agent_management_header(line: &str) -> bool {
    line.starts_with("⏺ Agent(") && line.ends_with(')')
}

fn is_claude_tui_background_agent_management_line(line: &str) -> bool {
    let Some(affordance) = line.strip_prefix("  ⎿  Backgrounded agent (") else {
        return false;
    };
    affordance.ends_with(')') && affordance.contains("↓ to manage · ctrl+o to expand")
}

fn is_claude_tui_background_agent_task_row(line: &str) -> bool {
    line.strip_prefix("  ◯ ").is_some_and(|row| {
        row.split_whitespace().any(|value| {
            let Some(unit) = value.chars().last() else {
                return false;
            };
            matches!(unit, 's' | 'm' | 'h')
                && value.strip_suffix(unit).is_some_and(|digits| {
                    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
                })
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
    fn production_capture_normalizes_context_panel_through_shared_formatter_4822() {
        let pane = "answer\n────────────────\n❯\n────────────────\n  🤖 Opus(H) │ ███░░░░░░░ │ 26% │ 265K/1.0M │ 5h: 8% (3h0m) │ 7d: 55% (1d23h)";
        let normalized = normalize_prompt_readiness_panel_in_capture(pane);

        assert_eq!(
            normalized,
            "answer\n────────────────\n❯\n────────────────\n  🤖 Opus(H) │ ███░░░░░░░ │ 5h: 8% (3h0m) │ 7d: 55% (1d23h) │ ctw: 26% (265K/1.0M)"
        );
    }

    #[test]
    fn background_agent_chrome_requires_composer_adjacency() {
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

        for pane in [
            "❯\n✻ Waiting for 3 background agents to finish",
            "❯\n  ⎿  Backgrounded agent (↓ to manage · ctrl+o to expand)",
            "✻ Waiting for 3 background agents to finish\n❯",
        ] {
            assert!(claude_tui_background_agent_status_line_indexes(pane).is_empty());
        }
    }

    #[test]
    fn captured_background_agent_frame_with_draft_composer_is_detected() {
        let usage = format_usage_status_segments(
            ["5h: 8% (3h0m)", "7d: 55% (1d23h)", "7d-F: 34% (4d20h)"],
            Some(ContextWindowUsage {
                used_tokens: 265_000,
                window_tokens: 1_000_000,
            }),
        );
        let panel_line = render_prompt_readiness_panel_line("🤖 Opus(H)", "███░░░░░░░", usage)
            .expect("usage panel line");
        assert_eq!(
            panel_line,
            "  🤖 Opus(H) │ ███░░░░░░░ │ 5h: 8% (3h0m) │ 7d: 55% (1d23h) │ 7d-F: 34% (4d20h) │ ctw: 26% (265K/1.0M)"
        );
        let pane = format!(
            "원하는 대로 할게:\n  어느 쪽?\n\n✻ Waiting for 5 background agents to finish\n\n─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────\n❯ a로 확정짓고 4:22 타임라인 떠서 처리해\n─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────\n{panel_line}\n  ⏺ main                                                                                        ↑/↓ to select · Enter to view\n  ◯ general-purpose  Fix #3207 turn-stop + resume                                                    16m 5s · ↓ 159.5k tokens\n  ◯ general-purpose  Implement #3154 A converged design                                             10m 53s · ↓ 110.5k tokens"
        );
        assert_eq!(
            claude_tui_background_agent_status_line_indexes(&pane),
            vec![3, 10, 11],
        );
    }

    #[test]
    fn background_agent_footer_rejects_assistant_footer_separated_by_many_blanks() {
        let pane = "\
✻ Waiting for 3 background agents to finish



────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  ◆ Opus(M) │ Tools: 224 done";
        assert!(claude_tui_background_agent_status_line_indexes(pane).is_empty());
    }

    #[test]
    fn background_agent_footer_requires_separator_before_composer() {
        let pane = "\
✻ Waiting for 3 background agents to finish
not a Claude TUI separator
❯";
        assert!(claude_tui_background_agent_status_line_indexes(pane).is_empty());
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
