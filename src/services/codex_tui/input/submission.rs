//! Complete current-composer evidence used by guarded prompt submission.

use super::*;

pub(super) fn prompt_readiness_snapshot_with_pane(
    session_name: &str,
    bounded: bool,
) -> (PromptReadinessSnapshot, Option<String>) {
    // One ANSI revision preserves the dim suggestion signal and its plain-text classification.
    let (pane_with_escapes, tmux_pane_alive) = if bounded {
        host_input::observe_prompt_submission(session_name, PROMPT_READY_CAPTURE_SCROLLBACK)
    } else {
        host_input::observe_legacy(session_name, PROMPT_READY_CAPTURE_SCROLLBACK)
    };
    let (composer_marker_detected, prompt_draft_detected, pane_tail) = pane_with_escapes
        .as_deref()
        .map(prompt_readiness_from_ansi_pane)
        .unwrap_or_else(|| (false, false, "<capture unavailable>".to_string()));
    (
        PromptReadinessSnapshot {
            composer_marker_detected,
            prompt_draft_detected,
            tmux_pane_alive,
            capture_available: pane_with_escapes.is_some(),
            pane_tail,
        },
        pane_with_escapes,
    )
}

/// The whole visible composer body; a partial row or unknown layout proves no ownership.
pub(crate) fn submission_draft_in_pane(pane: &str) -> Option<String> {
    let lines: Vec<&str> = pane.lines().collect();
    let footer = lines.iter().rposition(|line| {
        line_is_codex_footer_hint(line)
            || line_is_codex_model_first_status(line)
            || line_is_codex_fast_context_status(line)
            || line_is_codex_compact_status_line(line)
    })?;
    if lines[footer + 1..]
        .iter()
        .any(|line| !line.trim().is_empty())
    {
        return None;
    }
    let end = (0..footer).rfind(|&at| !lines[at].trim().is_empty())?;
    let border = |line: &str, left: char, right: char| {
        line.trim()
            .strip_prefix(left)
            .and_then(|body| body.strip_suffix(right))
            .is_some_and(|body| {
                body.chars().count() + 2 >= COMPOSER_EDGE_MIN_GLYPHS
                    && body.chars().all(|ch| ch == '─')
            })
    };
    if border(lines[end], '╰', '╯') {
        let start = (0..end).rfind(|&at| border(lines[at], '╭', '╮'))?;
        let mut rows = Vec::new();
        let mut cursors = 0;
        for line in &lines[start + 1..end] {
            let body = line
                .trim_start()
                .strip_prefix("│ ")?
                .strip_suffix('│')?
                .trim_end();
            let body = if let Some(body) = body.strip_suffix('▌') {
                cursors += 1;
                body.strip_suffix(' ').unwrap_or(body)
            } else {
                body
            };
            if body.contains('▌') {
                return None;
            }
            rows.push(body);
        }
        return (cursors == 1).then(|| rows.join("\n"));
    }
    let start = (0..=end).rfind(|&at| lines[at].starts_with('›'))?;
    let first = lines[start].strip_prefix('›')?;
    let mut rows = vec![first.strip_prefix(' ').unwrap_or(first)];
    for line in &lines[start + 1..=end] {
        rows.push(line.strip_prefix("  ")?);
    }
    Some(rows.join("\n"))
}

/// Modal headings are control chrome; model names and sentence fragments in history are not.
pub(crate) fn submission_modal_in_pane(pane: &str) -> bool {
    let lines: Vec<&str> = pane.lines().collect();
    let composer = lines
        .iter()
        .rposition(|line| line.starts_with('›') || line.trim_start().starts_with('╭'));
    let header = composer.and_then(|at| lines[..at].iter().rfind(|line| !line.trim().is_empty()));
    let choice = |line: &str| {
        let line = line.trim().strip_prefix('›').unwrap_or(line.trim()).trim();
        line.split_once(". ")
            .or_else(|| line.split_once(") "))
            .is_some_and(|(number, _)| {
                !number.is_empty() && number.chars().all(|ch| ch.is_ascii_digit())
            })
    };
    let controls: Vec<&str> = composer
        .map(|at| lines[..at].iter().rev().copied().collect())
        .unwrap_or_default();
    let has_heading = controls.iter().any(|line| {
        let lower = line.trim().to_ascii_lowercase();
        CODEX_INTERACTIVE_MODAL_MARKERS
            .iter()
            .any(|marker| lower.starts_with(marker))
    });
    if header.is_some_and(|line| choice(line))
        || (has_heading && controls.iter().any(|line| choice(line)))
        || controls.iter().any(|line| {
            let lower = line.to_ascii_lowercase();
            [
                "enter to confirm",
                "esc to cancel",
                "press enter to continue",
                "select an option",
            ]
            .iter()
            .any(|legend| lower.contains(legend))
        })
    {
        return true;
    }
    if header.is_some_and(|line| {
        let lower = line.trim().to_ascii_lowercase();
        CODEX_INTERACTIVE_MODAL_MARKERS
            .iter()
            .any(|marker| lower.contains(marker))
    }) {
        return true;
    }
    pane_has_codex_interactive_modal_in_pane(pane)
        || pane
            .lines()
            .rev()
            .take(PROMPT_READY_SCAN_LINES)
            .any(|line| {
                let lower = line.trim().to_ascii_lowercase();
                CODEX_INTERACTIVE_MODAL_MARKERS.iter().any(|marker| {
                    lower == *marker
                        || lower.strip_prefix(marker).is_some_and(|suffix| {
                            suffix.starts_with('?') || suffix.starts_with('!')
                        })
                })
            })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::codex_tui::host_input::spy::{SpyGuard, SpyState};

    #[test]
    fn idle_submission_preserves_box_rows_and_modal_choices() {
        let ready = "╭────────────────────────────────────────────╮\n│ ▌                                          │\n╰────────────────────────────────────────────╯\nEsc to interrupt   Ctrl+J newline   ⏎ send";
        let own = ready.replace("│ ▌", "│ hello ▌");
        let glyph_body = own.replace(
            "│ hello ▌",
            "│ ─────────────────foreign────────────────── │\n│ hello ▌",
        );
        let panes = [
            glyph_body.clone(),
            format!("Approval required: Allow command echo hello?\n  1. Yes\n  2. No\n{own}"),
            format!(
                "Approval required: Allow command echo hello?\nCommand: echo hello\nEnter to confirm · esc to cancel\n{own}"
            ),
            format!("1. Yes\n2. No\n{own}"),
            format!(
                "Approval required: Allow command echo hello?\n1. Yes\n2. No\n{}{own}",
                "Command detail\n".repeat(PROMPT_READY_SCAN_LINES + 1)
            ),
            format!(
                "Enter to confirm · esc to cancel\n{}{own}",
                "Command detail\n".repeat(PROMPT_READY_SCAN_LINES + 1)
            ),
        ];
        let mut failures = Vec::new();
        for (index, pane) in panes.into_iter().enumerate() {
            let spy = SpyGuard::install(SpyState {
                captures: [Some(ready.into()), Some(pane), Some(ready.into())].into(),
                ..SpyState::default()
            });
            let outcome = submit_codex_followup_prompt("idle-box-modal", "hello", None);
            if !matches!(outcome, CodexFollowupPromptSubmitOutcome::Refused { .. })
                || spy.calls().iter().any(|call| call.starts_with("keys:"))
            {
                failures.push(format!("case {index}: {outcome:?}; {:?}", spy.calls()));
            }
        }
        assert!(failures.is_empty(), "{failures:?}");
        let spy = SpyGuard::install(SpyState {
            captures: [Some(ready.into()), Some(glyph_body), Some(ready.into())].into(),
            ..SpyState::default()
        });
        assert!(matches!(
            submit_codex_followup_prompt(
                "idle-box-owned",
                "─────────────────foreign──────────────────\nhello",
                None
            ),
            CodexFollowupPromptSubmitOutcome::Submitted
        ));
        assert_eq!(
            spy.calls()
                .iter()
                .filter(|call| *call == "keys:Enter")
                .count(),
            1
        );
    }

    #[test]
    fn idle_submission_does_not_own_an_unchanged_dim_suggestion() {
        let suggestion = "› \x1b[2mhello\x1b[0m\n\n  gpt-5.4 · Fast off · Context 100% left";
        let spy = SpyGuard::install(SpyState {
            captures: [suggestion, suggestion].map(|p| Some(p.into())).into(),
            ..SpyState::default()
        });
        assert!(matches!(
            submit_codex_followup_prompt("idle-dim-suggestion", "hello", None),
            CodexFollowupPromptSubmitOutcome::Refused { .. }
        ));
        assert!(!spy.calls().iter().any(|call| call.starts_with("keys:")));
    }
}
