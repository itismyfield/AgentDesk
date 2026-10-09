//! Complete current-composer evidence used by guarded prompt submission.

use super::*;

const MAX_UNFOLDED_SUBMISSION_CHARS: usize = 1000;
const SUBMISSION_RESERVED_ROWS: usize = 10;
const MAX_SUBMISSION_ROWS: usize = 40;

/// A conservative screen budget; the core checks frame bytes before this payload callback.
pub(crate) fn prompt_submission_fits_pane(
    frame: &str,
    uses_paste: bool,
    size: Option<(usize, usize)>,
) -> bool {
    use unicode_width::UnicodeWidthStr;

    if frame.contains('\t')
        || (uses_paste && frame.chars().nth(MAX_UNFOLDED_SUBMISSION_CHARS).is_some())
    {
        return false;
    }
    let Some((width, height)) =
        size.filter(|(width, height)| *width > 4 && *height > SUBMISSION_RESERVED_ROWS)
    else {
        return false;
    };
    let usable = width - 4;
    let mut remaining = (height - SUBMISSION_RESERVED_ROWS).min(MAX_SUBMISSION_ROWS);
    // This read-only line view preserves the original transmitted frame.
    let lines = frame.replace("\r\n", "\n").replace('\r', "\n");
    for line in lines.split('\n') {
        let columns = line.width();
        let rows = if columns <= usable {
            1
        } else {
            2 * columns.div_ceil(usable) + 1
        };
        if rows > remaining {
            return false;
        }
        remaining -= rows;
    }
    true
}

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

/// Preflight alone uses submission semantics; post-Enter observations retain legacy evidence.
pub(super) fn submission_snapshot_with_pane(
    session_name: &str,
) -> (PromptReadinessSnapshot, Option<String>) {
    let (mut snapshot, pane) = prompt_readiness_snapshot_with_pane(session_name, true);
    if let Some(raw) = pane.as_deref() {
        let (marker, draft, tail) = submission_readiness_from_ansi_pane(raw);
        snapshot.composer_marker_detected = marker;
        snapshot.prompt_draft_detected = draft;
        snapshot.pane_tail = tail;
    }
    (snapshot, pane)
}

/// The whole visible composer body; a partial row or unknown layout proves no ownership.
pub(crate) fn submission_draft_in_pane(pane: &str) -> Option<String> {
    submission_draft_with_start_in_pane(pane).map(|(_, body)| body)
}

fn submission_draft_with_start_in_pane(pane: &str) -> Option<(usize, String)> {
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
        return (cursors == 1).then(|| (start, rows.join("\n")));
    }
    let start = (0..=end).rfind(|&at| lines[at].starts_with('›'))?;
    let first = lines[start].strip_prefix('›')?;
    let mut rows = vec![first.strip_prefix(' ').unwrap_or(first)];
    for line in &lines[start + 1..=end] {
        rows.push(line.strip_prefix("  ").or(line.is_empty().then_some(""))?);
    }
    Some((start, rows.join("\n")))
}

/// Submission requires a complete empty body or one raw dim compact suggestion row.
pub(crate) fn submission_readiness_from_ansi_pane(pane_with_escapes: &str) -> (bool, bool, String) {
    let plain = strip_ansi_escape_sequences(pane_with_escapes);
    let draft = submission_draft_in_pane(&plain);
    let dim_hint = draft
        .as_deref()
        .is_some_and(|body| !body.is_empty() && !body.contains('\n'))
        && submission_composer_start_in_pane(&plain)
            .and_then(|at| pane_with_escapes.lines().nth(at))
            .is_some_and(submission_compact_body_is_fully_dim);
    let empty = draft.as_deref().is_some_and(str::is_empty) || dim_hint;
    (
        empty && !submission_modal_in_pane(&plain) && !submission_busy_in_pane(&plain),
        draft.as_deref().is_some_and(|body| !body.is_empty()) && !dim_hint,
        prompt_ready_debug_tail(&plain),
    )
}

// Track raw current-row SGR state so a dim span cannot hide ordinary typed characters.
fn submission_compact_body_is_fully_dim(line: &str) -> bool {
    let mut remaining = line;
    let (mut dim, mut marker, mut body) = (false, false, false);
    while !remaining.is_empty() {
        if let Some(sequence) = remaining.strip_prefix("\x1b[") {
            let Some((parameters, after)) = sequence.split_once('m') else {
                return false;
            };
            if !parameters
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b';')
            {
                return false;
            }
            let Some(codes) = parameters
                .split(';')
                .map(|code| {
                    if code.is_empty() {
                        Some(0)
                    } else {
                        code.parse::<u16>().ok()
                    }
                })
                .collect::<Option<Vec<_>>>()
            else {
                return false;
            };
            let mut at = 0;
            while at < codes.len() {
                match codes[at] {
                    0 | 22 => dim = false,
                    2 => dim = true,
                    38 | 48 | 58 => {
                        let channels = match codes.get(at + 1) {
                            Some(2) => 3,
                            Some(5) => 1,
                            _ => return false,
                        };
                        if !codes
                            .get(at + 2..at + 2 + channels)
                            .is_some_and(|values| values.iter().all(|value| *value <= 255))
                        {
                            return false;
                        }
                        at += channels + 2;
                        continue;
                    }
                    1 | 3..=9 | 23..=37 | 39..=47 | 49 | 53 | 55 | 59 | 90..=97 | 100..=107 => {}
                    _ => return false,
                }
                at += 1;
            }
            remaining = after;
            continue;
        }
        let mut characters = remaining.chars();
        let Some(character) = characters.next() else {
            return false;
        };
        remaining = characters.as_str();
        if character.is_control() {
            return false;
        }
        if !marker {
            if character != '›' {
                return false;
            }
            marker = true;
        } else if !character.is_whitespace() {
            if !dim {
                return false;
            }
            body = true;
        }
    }
    marker && body
}

// The same complete body proof supplies the current composer boundary.
fn submission_composer_start_in_pane(pane: &str) -> Option<usize> {
    submission_draft_with_start_in_pane(pane).map(|(at, _)| at)
}

// Only the contiguous control block directly above the composer can establish a modal.
fn submission_control_rows(pane: &str) -> Option<Vec<&str>> {
    let composer = submission_composer_start_in_pane(pane)?;
    let lines: Vec<&str> = pane.lines().collect();
    let start = lines[..composer]
        .iter()
        .rposition(|line| line.trim().is_empty())
        .map_or(0, |at| at + 1);
    Some(lines[start..composer].to_vec())
}

/// Modal controls are outside the complete current composer, not inside its prompt body.
pub(crate) fn submission_modal_in_pane(pane: &str) -> bool {
    let Some(controls) = submission_control_rows(pane) else {
        return pane_has_codex_interactive_modal_in_pane(pane)
            || pane
                .trim_end()
                .to_ascii_lowercase()
                .ends_with("press enter to continue");
    };
    let header = controls.iter().rev().find(|line| !line.trim().is_empty());
    let choice = |line: &str| {
        let line = line.trim().strip_prefix('›').unwrap_or(line.trim()).trim();
        line.split_once(". ")
            .or_else(|| line.split_once(") "))
            .is_some_and(|(number, _)| {
                !number.is_empty() && number.chars().all(|ch| ch.is_ascii_digit())
            })
    };
    let has_choice = controls.iter().any(|line| choice(line));
    header.is_some_and(|line| choice(line) || pane_shows_codex_interactive_modal(line))
        || controls.iter().any(|line| {
            let lower = line.trim().to_ascii_lowercase();
            [
                "enter to confirm",
                "esc to cancel",
                "press enter to continue",
                "select an option",
            ]
            .iter()
            .any(|legend| lower.contains(legend))
                || CODEX_INTERACTIVE_MODAL_MARKERS.iter().any(|marker| {
                    lower.strip_prefix(marker).is_some_and(|suffix| {
                        has_choice
                            || suffix.is_empty()
                            || suffix.starts_with('?')
                            || suffix.starts_with('!')
                    })
                })
        })
}

/// Only active-turn control rows outside the current composer veto a guarded submission.
pub(crate) fn submission_busy_in_pane(pane: &str) -> bool {
    let Some(composer) = submission_composer_start_in_pane(pane) else {
        return false;
    };
    pane_has_codex_active_turn_in_pane(&pane.lines().take(composer).collect::<Vec<_>>().join("\n"))
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

    const COMPACT_READY: &str = "› \n\n  gpt-5.4 · Fast off · Context 100% left";

    fn compact_rows(rows: &[&str]) -> String {
        let (first, rest) = rows.split_first().expect("a composer has a first row");
        let mut pane = format!("› {first}");
        for row in rest {
            pane.push_str("\n  ");
            pane.push_str(row);
        }
        pane.push_str("\n\n  gpt-5.4 · Fast off · Context 100% left");
        pane
    }

    fn assert_no_submission_effects(calls: &[String]) {
        assert!(
            !calls.iter().any(|call| {
                ["load:", "paste:", "literal:", "keys:", "kill", "retire"]
                    .iter()
                    .any(|prefix| call.starts_with(prefix))
            }),
            "preflight refusal mutated the pane: {calls:?}"
        );
    }

    fn assert_one_enter_without_cleanup(calls: &[String]) {
        assert_eq!(calls.iter().filter(|call| *call == "keys:Enter").count(), 1);
        assert!(
            !calls.iter().any(|call| {
                (call.starts_with("keys:") && call != "keys:Enter")
                    || call.starts_with("kill")
                    || call.starts_with("retire")
            }),
            "successful submission sent cleanup keys: {calls:?}"
        );
    }

    #[test]
    fn idle_entry_submits_exact_ascii_wrap_at_eighty_columns() {
        // Constructed 80-column compact profile: 76 payload cells, not a native capture.
        let first = "x".repeat(76);
        let second = "x".repeat(44);
        let prompt = "x".repeat(120);
        let own = compact_rows(&[&first, &second]);
        assert_eq!(
            submission_draft_in_pane(&own),
            Some(format!("{first}\n{second}"))
        );
        let spy = SpyGuard::install(SpyState {
            pane_size: Some((80, 24)),
            captures: [
                Some(COMPACT_READY.into()),
                Some(own),
                Some(COMPACT_READY.into()),
            ]
            .into(),
            ..SpyState::default()
        });
        let outcome = submit_codex_followup_prompt("idle-ascii-wrap-fixture", &prompt, None);
        assert!(
            matches!(outcome, CodexFollowupPromptSubmitOutcome::Submitted),
            "{outcome:?}"
        );
        let calls = spy.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.starts_with("literal:"))
                .collect::<Vec<_>>(),
            vec![&format!("literal:{prompt}")]
        );
        assert!(
            !calls
                .iter()
                .any(|call| call.starts_with("load:") || call.starts_with("paste:"))
        );
        assert_one_enter_without_cleanup(&calls);
    }

    #[test]
    fn idle_entry_submits_exact_hangul_wrap_at_eighty_columns() {
        // Precomposed Hangul occupies two cells in the constructed compact profile.
        let first = "가".repeat(38);
        let second = "가".repeat(2);
        let prompt = "가".repeat(40);
        let own = compact_rows(&[&first, &second]);
        assert_eq!(
            submission_draft_in_pane(&own),
            Some(format!("{first}\n{second}"))
        );
        let spy = SpyGuard::install(SpyState {
            pane_size: Some((80, 24)),
            captures: [
                Some(COMPACT_READY.into()),
                Some(own),
                Some(COMPACT_READY.into()),
            ]
            .into(),
            ..SpyState::default()
        });
        let outcome = submit_codex_followup_prompt("idle-hangul-wrap-fixture", &prompt, None);
        assert!(
            matches!(outcome, CodexFollowupPromptSubmitOutcome::Submitted),
            "{outcome:?}"
        );
        let calls = spy.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.starts_with("literal:"))
                .collect::<Vec<_>>(),
            vec![&format!("literal:{prompt}")]
        );
        assert!(
            !calls
                .iter()
                .any(|call| call.starts_with("load:") || call.starts_with("paste:"))
        );
        assert_one_enter_without_cleanup(&calls);
    }

    #[test]
    fn idle_entry_preserves_an_interior_blank_composer_row() {
        let prompt = "alpha\n\nomega";
        let own = compact_rows(&["alpha", "", "omega"]);
        assert_eq!(submission_draft_in_pane(&own), Some(prompt.into()));
        let spy = SpyGuard::install(SpyState {
            captures: [
                Some(COMPACT_READY.into()),
                Some(own),
                Some(COMPACT_READY.into()),
            ]
            .into(),
            ..SpyState::default()
        });
        let outcome = submit_codex_followup_prompt("idle-blank-row-fixture", prompt, None);
        assert!(
            matches!(outcome, CodexFollowupPromptSubmitOutcome::Submitted),
            "{outcome:?}"
        );
        let calls = spy.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.starts_with("load:"))
                .collect::<Vec<_>>(),
            vec![&format!("load:{prompt}")]
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| *call == "paste:delete=true")
                .count(),
            1
        );
        assert!(!calls.iter().any(|call| call.starts_with("literal:")));
        assert_one_enter_without_cleanup(&calls);
        drop(spy);

        let foreign = compact_rows(&["alpha", "", "foreign"]);
        let spy = SpyGuard::install(SpyState {
            captures: [Some(COMPACT_READY.into()), Some(foreign)].into(),
            ..SpyState::default()
        });
        let outcome = submit_codex_followup_prompt("idle-blank-row-foreign-fixture", prompt, None);
        assert!(
            matches!(outcome, CodexFollowupPromptSubmitOutcome::Refused { .. }),
            "{outcome:?}"
        );
        assert!(!spy.calls().iter().any(|call| call.starts_with("keys:")));
    }

    #[test]
    fn idle_entry_refuses_a_foldable_multiline_paste_before_payload() {
        let prompt = format!("{}\ny", "x".repeat(1000));
        assert!(prompt.chars().count() > 1000);
        // The wide geometry isolates folding from the 80-column capacity limit.
        for size in [(80, 24), (2000, 24)] {
            let spy = SpyGuard::install(SpyState {
                pane_size: Some(size),
                captures: [Some(COMPACT_READY.into())].into(),
                ..SpyState::default()
            });
            let outcome = submit_codex_followup_prompt("idle-folded-paste-fixture", &prompt, None);
            assert!(
                matches!(
                    outcome,
                    CodexFollowupPromptSubmitOutcome::NotSubmitted { .. }
                ),
                "{size:?}: {outcome:?}"
            );
            assert_no_submission_effects(&spy.calls());
        }
    }

    #[test]
    fn idle_entry_refuses_a_composer_that_outgrows_pane_height_before_payload() {
        let prompt = (0..15)
            .map(|row| format!("row-{row}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(prompt.chars().count() < 1000);
        let spy = SpyGuard::install(SpyState {
            pane_size: Some((80, 24)),
            captures: [Some(COMPACT_READY.into())].into(),
            ..SpyState::default()
        });
        let outcome = submit_codex_followup_prompt("idle-overheight-fixture", &prompt, None);
        assert!(
            matches!(
                outcome,
                CodexFollowupPromptSubmitOutcome::NotSubmitted { .. }
            ),
            "{outcome:?}"
        );
        assert_no_submission_effects(&spy.calls());
    }

    #[test]
    fn idle_entry_refuses_unknown_geometry_before_payload() {
        let spy = SpyGuard::install(SpyState {
            pane_size: None,
            captures: [Some(COMPACT_READY.into())].into(),
            ..SpyState::default()
        });
        let outcome = submit_codex_followup_prompt("idle-unknown-size-fixture", "hello", None);
        assert!(
            matches!(
                outcome,
                CodexFollowupPromptSubmitOutcome::NotSubmitted { .. }
            ),
            "{outcome:?}"
        );
        assert_no_submission_effects(&spy.calls());
    }

    #[test]
    fn idle_entry_accepts_historical_modal_prose_and_lists() {
        for history in [
            "Sign in to continue",
            "Enter to confirm · esc to cancel",
            "1. Yes\n2. No",
            "update available",
        ] {
            let prefix = format!("{history}\n\n");
            let empty = format!("{prefix}{COMPACT_READY}");
            let own = format!("{prefix}{}", compact_rows(&["echo"]));
            let spy = SpyGuard::install(SpyState {
                captures: [Some(empty.clone()), Some(own), Some(empty)].into(),
                ..SpyState::default()
            });
            let outcome =
                submit_codex_followup_prompt("idle-historical-controls-fixture", "echo", None);
            assert!(
                matches!(outcome, CodexFollowupPromptSubmitOutcome::Submitted),
                "{history}: {outcome:?}"
            );
            let calls = spy.calls();
            assert_eq!(
                calls.iter().filter(|call| *call == "literal:echo").count(),
                1
            );
            assert_one_enter_without_cleanup(&calls);
        }
    }

    #[test]
    fn idle_entry_accepts_owned_working_and_sign_in_text() {
        let prompt = "echo\n• Working (1s • esc to interrupt)\nSign in";
        let own = compact_rows(&["echo", "• Working (1s • esc to interrupt)", "Sign in"]);
        let spy = SpyGuard::install(SpyState {
            captures: [
                Some(COMPACT_READY.into()),
                Some(own),
                Some(COMPACT_READY.into()),
            ]
            .into(),
            ..SpyState::default()
        });
        let outcome =
            submit_codex_followup_prompt("idle-owned-control-words-fixture", prompt, None);
        assert!(
            matches!(outcome, CodexFollowupPromptSubmitOutcome::Submitted),
            "{outcome:?}"
        );
        let calls = spy.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.starts_with("load:"))
                .collect::<Vec<_>>(),
            vec![&format!("load:{prompt}")]
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| *call == "paste:delete=true")
                .count(),
            1
        );
        assert_one_enter_without_cleanup(&calls);
    }

    #[test]
    fn idle_entry_keeps_a_current_working_marker_above_a_blank_separator_terminal() {
        let own = compact_rows(&["hello"]);
        let late_busy = format!("• Working (1s • esc to interrupt)\n\n{own}");
        let spy = SpyGuard::install(SpyState {
            captures: [Some(COMPACT_READY.into()), Some(late_busy)].into(),
            ..SpyState::default()
        });
        let outcome = submit_codex_followup_prompt("idle-current-working-fixture", "hello", None);
        assert!(
            matches!(outcome, CodexFollowupPromptSubmitOutcome::Refused { .. }),
            "{outcome:?}"
        );
        let calls = spy.calls();
        assert_eq!(
            calls.iter().filter(|call| *call == "literal:hello").count(),
            1
        );
        assert!(!calls.iter().any(|call| call.starts_with("keys:")
            || call.starts_with("kill")
            || call.starts_with("retire")));
        drop(spy);

        let current_busy = format!("• Working (1s • esc to interrupt)\n\n{COMPACT_READY}");
        let spy = SpyGuard::install(SpyState {
            captures: [Some(current_busy)].into(),
            ..SpyState::default()
        });
        let outcome =
            submit_codex_followup_prompt("idle-current-working-pre-fixture", "hello", None);
        assert!(
            matches!(
                outcome,
                CodexFollowupPromptSubmitOutcome::NotSubmitted { .. }
            ),
            "{outcome:?}"
        );
        assert_no_submission_effects(&spy.calls());
    }

    #[test]
    fn idle_entry_refuses_a_double_width_literal_that_outgrows_pane_height() {
        // 800 cells exceed the conservative 14-row budget; 400 cells would fit it.
        let prompt = "가".repeat(400);
        let spy = SpyGuard::install(SpyState {
            pane_size: Some((80, 24)),
            captures: [Some(COMPACT_READY.into())].into(),
            ..SpyState::default()
        });
        let outcome =
            submit_codex_followup_prompt("idle-double-width-capacity-fixture", &prompt, None);
        assert!(
            matches!(
                outcome,
                CodexFollowupPromptSubmitOutcome::NotSubmitted { .. }
            ),
            "{outcome:?}"
        );
        assert_no_submission_effects(&spy.calls());
    }

    #[test]
    fn idle_entry_preserves_guard_and_cancellation_outcomes_with_unknown_geometry() {
        use std::sync::atomic::Ordering;

        let foreign = compact_rows(&["a person's draft"]);
        let spy = SpyGuard::install(SpyState {
            pane_size: None,
            captures: [Some(foreign)].into(),
            ..SpyState::default()
        });
        let outcome =
            submit_codex_followup_prompt("idle-foreign-unknown-size-fixture", "hello", None);
        assert!(
            matches!(outcome, CodexFollowupPromptSubmitOutcome::NotSubmitted { error }
            if error == "Codex TUI warm follow-up final pane snapshot rejected submit")
        );
        assert_no_submission_effects(&spy.calls());
        assert!(!spy.calls().iter().any(|call| call == "pane_size"));
        drop(spy);

        let modal = format!("Approval required\n1. Yes\n2. No\n{COMPACT_READY}");
        let spy = SpyGuard::install(SpyState {
            pane_size: None,
            captures: [Some(modal)].into(),
            ..SpyState::default()
        });
        let outcome =
            submit_codex_followup_prompt("idle-modal-unknown-size-fixture", "hello", None);
        assert!(
            matches!(outcome, CodexFollowupPromptSubmitOutcome::NotSubmitted { error }
            if error == "Codex TUI warm follow-up final pane snapshot rejected submit")
        );
        assert_no_submission_effects(&spy.calls());
        assert!(!spy.calls().iter().any(|call| call == "pane_size"));
        drop(spy);

        let token = crate::services::provider::CancelToken::new();
        token.cancelled.store(true, Ordering::Relaxed);
        let spy = SpyGuard::install(SpyState {
            pane_size: None,
            captures: [Some(COMPACT_READY.into())].into(),
            ..SpyState::default()
        });
        let outcome = submit_codex_followup_prompt(
            "idle-cancelled-unknown-size-fixture",
            "hello",
            Some(&token),
        );
        assert!(
            matches!(outcome, CodexFollowupPromptSubmitOutcome::Cancelled),
            "{outcome:?}"
        );
        assert_no_submission_effects(&spy.calls());
        assert!(!spy.calls().iter().any(|call| call == "pane_size"));
    }

    #[test]
    fn idle_entry_preserves_cancellation_during_an_unknown_geometry_read() {
        use std::sync::atomic::Ordering;

        let token = std::sync::Arc::new(crate::services::provider::CancelToken::new());
        assert!(!token.cancelled.load(Ordering::Relaxed));
        let spy = SpyGuard::install(SpyState {
            pane_size: None,
            cancel_after_size: Some(token.clone()),
            captures: [Some(COMPACT_READY.into())].into(),
            ..SpyState::default()
        });
        let outcome =
            submit_codex_followup_prompt("idle-cancelled-size-read-fixture", "hello", Some(&token));
        assert!(
            matches!(outcome, CodexFollowupPromptSubmitOutcome::Cancelled),
            "{outcome:?}"
        );
        assert!(token.cancelled.load(Ordering::Relaxed));
        let calls = spy.calls();
        assert_eq!(calls.iter().filter(|call| *call == "pane_size").count(), 1);
        assert_no_submission_effects(&calls);
    }

    #[test]
    fn idle_entry_only_treats_fully_dim_body_as_native_empty() {
        let pane =
            |body: &str| format!("› {body}\x1b[0m\n\n  gpt-5.4 · Fast off · Context 100% left");
        let dim_hint = pane("\x1b[2mAsk Codex to do anything\x1b[0m");
        let own = compact_rows(&["hello"]);
        let spy = SpyGuard::install(SpyState {
            captures: [Some(dim_hint.clone()), Some(own), Some(dim_hint)].into(),
            ..SpyState::default()
        });
        let outcome = submit_codex_followup_prompt("idle-entirely-dim-body-fixture", "hello", None);
        assert!(
            matches!(outcome, CodexFollowupPromptSubmitOutcome::Submitted),
            "{outcome:?}"
        );
        let calls = spy.calls();
        assert_eq!(
            calls.iter().filter(|call| *call == "literal:hello").count(),
            1
        );
        assert_one_enter_without_cleanup(&calls);
        drop(spy);

        for (name, body) in [
            (
                "plain-prefix",
                "typed prefix \x1b[2mAsk Codex to do anything\x1b[0m",
            ),
            (
                "reset-zero-suffix",
                "\x1b[2mAsk Codex to do anything\x1b[0m typed foreign suffix",
            ),
            (
                "reset-twenty-two-suffix",
                "\x1b[2mAsk Codex to do anything\x1b[22m typed foreign suffix",
            ),
            (
                "truecolor-is-not-dim",
                "\x1b[38;2;120;80;40mAsk Codex to do anything\x1b[0m",
            ),
        ] {
            let spy = SpyGuard::install(SpyState {
                captures: [Some(pane(body))].into(),
                ..SpyState::default()
            });
            let outcome =
                submit_codex_followup_prompt("idle-partly-dim-body-fixture", "hello", None);
            assert!(
                matches!(outcome, CodexFollowupPromptSubmitOutcome::NotSubmitted { error }
                if error == "Codex TUI warm follow-up final pane snapshot rejected submit"),
                "{name}"
            );
            let calls = spy.calls();
            assert!(
                !calls.iter().any(|call| call == "pane_size"),
                "{name}: {calls:?}"
            );
            assert_no_submission_effects(&calls);
        }
    }

    #[test]
    fn idle_entry_submits_a_bare_blank_composer_row() {
        let prompt = "alpha\n\nomega";
        let indented = compact_rows(&["alpha", "", "omega"]);
        let bare = indented.replace("\n  \n", "\n\n");
        assert_ne!(bare, indented);
        let spy = SpyGuard::install(SpyState {
            captures: [
                Some(COMPACT_READY.into()),
                Some(bare),
                Some(COMPACT_READY.into()),
            ]
            .into(),
            ..SpyState::default()
        });
        let outcome = submit_codex_followup_prompt("idle-bare-blank-row-fixture", prompt, None);
        assert!(
            matches!(outcome, CodexFollowupPromptSubmitOutcome::Submitted),
            "{outcome:?}"
        );
        let calls = spy.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.starts_with("load:"))
                .collect::<Vec<_>>(),
            vec![&format!("load:{prompt}")]
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| *call == "paste:delete=true")
                .count(),
            1
        );
        assert!(!calls.iter().any(|call| call.starts_with("literal:")));
        assert_one_enter_without_cleanup(&calls);
        drop(spy);

        let indented = compact_rows(&["alpha", "", "foreign"]);
        let foreign = indented.replace("\n  \n", "\n\n");
        assert_ne!(foreign, indented);
        assert_eq!(
            submission_draft_in_pane(&foreign),
            Some("alpha\n\nforeign".into())
        );
        let spy = SpyGuard::install(SpyState {
            captures: [Some(COMPACT_READY.into()), Some(foreign)].into(),
            ..SpyState::default()
        });
        let outcome =
            submit_codex_followup_prompt("idle-bare-blank-row-foreign-fixture", prompt, None);
        assert!(
            matches!(outcome, CodexFollowupPromptSubmitOutcome::Refused { .. }),
            "{outcome:?}"
        );
        let calls = spy.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.starts_with("load:"))
                .collect::<Vec<_>>(),
            vec![&format!("load:{prompt}")]
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| *call == "paste:delete=true")
                .count(),
            1
        );
        assert!(!calls.iter().any(|call| {
            ["literal:", "keys:", "kill", "retire"]
                .iter()
                .any(|prefix| call.starts_with(prefix))
        }));
    }
}
