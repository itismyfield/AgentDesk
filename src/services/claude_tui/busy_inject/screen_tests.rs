//! The rows Claude draws a paste into, against composers captured from Claude Code 2.1.293, and
//! the layout read under a box that carries a status line.

use super::screen::{self, Composer, Drawn, Stash, drawn};
use super::{Plan, Veto, judge_before_paste};
use crate::services::tui_input::actor::gate::{own_draft, own_wrapped_draft};
use crate::services::tui_o::shadow::ShadowProvider;

const HEADER: &str = "[📱 s · a · n1]";
const PANE: Option<(usize, usize)> = Some((80, 24));
const HANGUL: &str = "가나다라마바사아자차";

fn x(n: usize) -> String {
    "x".repeat(n)
}

fn hangul(n: usize) -> String {
    HANGUL.chars().cycle().take(n).collect()
}

fn words(n: usize) -> String {
    ["가나다"; 25][..n].join(" ")
}

/// The rows below the header as captured, indent removed.
fn rows(text: &str, size: Option<(usize, usize)>) -> Option<Vec<String>> {
    match drawn(&format!("{HEADER}\n{text}"), size)? {
        Drawn::Rows(rows) => {
            assert_eq!(rows[0], HEADER);
            Some(rows[1..].to_vec())
        }
        Drawn::Folded => None,
    }
}

#[test]
fn a_flat_paste_is_drawn_in_the_rows_measured_at_each_boundary() {
    let space_run = format!("ab{}c", " ".repeat(200));
    let cases: Vec<(String, Vec<String>)> = vec![
        (x(75), vec![x(75)]),
        (x(76), vec![x(76)]),
        (x(77), vec![x(76), x(1)]),
        (hangul(37), vec![hangul(37)]),
        (hangul(38), vec![hangul(38)]),
        (hangul(39), vec![hangul(38), "자".into()]),
        (
            format!("a{}", hangul(38)),
            vec![format!("a{}", hangul(37)), "아".into()],
        ),
        ("😀a".repeat(25), vec!["😀a".repeat(25)]),
        (format!("{} abcdef", x(70)), vec![x(70), "abcdef".into()]),
        (format!("{} abcde", x(70)), vec![format!("{} abcde", x(70))]),
        (format!("{} ab", x(76)), vec![x(76), "ab".into()]),
        (format!("{}  ab", x(76)), vec![x(76), "ab".into()]),
        (format!("{}    ab", x(74)), vec![x(74), "ab".into()]),
        (
            format!("{}  abcdefgh", x(70)),
            vec![x(70), "abcdefgh".into()],
        ),
        (
            format!("ab{}c", " ".repeat(80)),
            vec!["ab".into(), "c".into()],
        ),
        (space_run, vec!["ab".into(), String::new(), "c".into()]),
        (
            format!("ab {}", x(100)),
            vec![format!("ab {}", x(73)), x(27)],
        ),
        (format!("ab {}", x(74)), vec!["ab".into(), x(74)]),
        (format!("   {}", x(80)), vec![format!("   {}", x(73)), x(7)]),
        (
            format!("{} 끝", words(25)),
            vec![words(11), words(10), format!("{} 끝", words(4))],
        ),
        (
            format!("{} ㅠㅠ", "ㅋ".repeat(40)),
            vec!["ㅋ".repeat(38), "ㅋㅋ ㅠㅠ".into()],
        ),
        (
            "one-two-three/four_five.six,seven;".repeat(3),
            vec![
                "one-two-three/four_five.six,seven;one-two-three/four_five.six,seven;one-two-"
                    .into(),
                "three/four_five.six,seven;".into(),
            ],
        ),
        (
            format!("short line\n{}", "y".repeat(90)),
            vec!["short line".into(), "y".repeat(76), "y".repeat(14)],
        ),
    ];
    for (text, measured) in cases {
        assert_eq!(rows(&text, PANE), Some(measured), "{text}");
    }
    assert_eq!(rows(&x(96), Some((100, 30))), Some(vec![x(96)]));
    assert_eq!(rows(&x(97), Some((100, 30))), Some(vec![x(96), x(1)]));
}

#[test]
fn a_paste_whose_rows_cannot_be_predicted_or_shown_whole_has_no_rows() {
    // The composer shows (rows-10)/2 rows and scrolls past them; measured at 18, 24 and 31 rows.
    let z = |rows: usize| "z".repeat(76 * (rows - 2) + 10);
    for (shown, size) in [(4, (80, 18)), (7, (80, 24)), (10, (80, 31))] {
        assert!(rows(&z(shown), Some(size)).is_some(), "{shown}");
        assert_eq!(rows(&z(shown + 1), Some(size)), None, "{shown}");
    }
    for text in [
        "😀a".repeat(26),
        format!("{}…끝", "가".repeat(37)),
        "trailing ".to_string(),
        "tab\there".to_string(),
        // Rows the composer reader would take for a border or the prompt.
        "─".to_string(),
        "───".to_string(),
        "❯ hello".to_string(),
        format!("{} ❯ y", x(76)),
        format!("{} ───", x(76)),
        // Characters that may join into one glyph, even on a short row.
        "👨\u{200d}👩".to_string(),
        "❤\u{fe0f}".to_string(),
        "e\u{301}".to_string(),
        "🇰🇷".to_string(),
        "👍🏻".to_string(),
    ] {
        assert_eq!(rows(&text, PANE), None, "{text}");
    }
    // A tab or CR is refused before folding, since a rewrite would change the folded line count.
    for text in [
        format!("{}\tz", "z".repeat(900)),
        "a\r\nb\nc\nd".to_string(),
    ] {
        assert_eq!(drawn(&format!("{HEADER}\n{text}"), PANE), None, "{text}");
    }
    assert_eq!(drawn(&format!("{HEADER}\nshort"), None), None);
    // Over 800 UTF-16 units or more than two line breaks, Claude folds instead.
    for text in ["z".repeat(998), "a\nb\nc".to_string()] {
        assert_eq!(
            drawn(&format!("{HEADER}\n{text}"), PANE),
            Some(Drawn::Folded)
        );
    }
}

#[test]
fn a_wrapped_paste_is_owned_only_in_the_rows_claude_draws() {
    let frame = format!("{HEADER}\n{} 끝", words(25));
    let Some(Drawn::Rows(predicted)) = drawn(&frame, PANE) else {
        panic!("predicted rows");
    };
    let border = "─".repeat(80);
    let composer = |rows: &[&str]| {
        format!(
            "{border}\n❯\u{00a0}{HEADER}\n{}\n{border}\n",
            rows.join("\n")
        )
    };
    let (one, two, three) = (
        format!("  {}", words(11)),
        format!("  {}", words(10)),
        format!("  {} 끝", words(4)),
    );
    assert!(own_wrapped_draft(
        &composer(&[&one, &two, &three]),
        &predicted
    ));
    let typed = format!("{three}x");
    let raw: Vec<String> = frame.split('\n').map(str::to_string).collect();
    assert!(!own_wrapped_draft(&composer(&[&one, &two, &three]), &raw));
    assert!(!own_wrapped_draft(
        &composer(&[&one, &two, &typed]),
        &predicted
    ));
    assert!(!own_wrapped_draft(
        &composer(&[&one, &two, &three, "  x"]),
        &predicted
    ));
    assert!(!own_wrapped_draft(&composer(&[&one, &two]), &predicted));
    // Rows after a pasted border-like row are still the composer's; no reader may stop there.
    let foreign = composer(&[&one, &two, &three, "  ─", "  foreign"]);
    assert!(!own_wrapped_draft(&foreign, &predicted));
    let short = format!("{border}\n❯\u{00a0}{HEADER}\n  hello\n  ───\n  foreign\n{border}\n");
    assert!(!own_draft(
        ShadowProvider::Claude,
        &short,
        &format!("{HEADER}\nhello"),
        true
    ));
    assert!(!own_wrapped_draft(
        &composer(&[&one, &two[1..], &three]),
        &predicted
    ));
}

/// Busy 46x37 panes captured from a live Claude pane with a five-row status line under the box:
/// an empty composer, and the person's draft `js`. A toast sits right above both boxes.
const BUSY: &str = include_str!("../../../../tests/fixtures/tui_input/claude-busy-statusline.ansi");
const BUSY_DRAFT: &str =
    include_str!("../../../../tests/fixtures/tui_input/claude-busy-statusline-draft.ansi");
const TOAST: &str = "      \x1b[38;5;114m✔ Update installed · Restart to update\x1b[39m";

/// The plan for a short Discord input under a live turn: the stashed draft rows, or none for
/// a direct paste.
fn plan(capture: &str) -> Result<Option<Vec<String>>, Veto> {
    let transcript = tempfile::NamedTempFile::new().unwrap();
    let busy_turn = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"go\"}}\n\
        {\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"working\"}]}}\n";
    std::fs::write(transcript.path(), busy_turn).unwrap();
    let text = super::frame("discord", "ann", "n1", "hello");
    let plan = judge_before_paste(capture, transcript.path(), &text, Some((46, 37)))?;
    Ok(match plan {
        Plan::Direct(_) => None,
        Plan::Stash(rows) => Some(rows),
    })
}

#[test]
fn a_busy_pane_with_a_status_line_under_the_box_plans_the_paste() {
    assert_eq!(plan(BUSY), Ok(None));
    assert_eq!(
        plan(&BUSY_DRAFT.replace(TOAST, "")),
        Ok(Some(vec!["js".to_string()]))
    );
    // A toast glyph no stash row was measured with keeps the stash unread, so the draft stays.
    assert_eq!(plan(BUSY_DRAFT), Err(Veto::Draft));
}

#[test]
fn only_status_rows_and_a_whole_footer_may_follow_the_box() {
    let base = BUSY_DRAFT.replace(TOAST, "");
    let rows: Vec<&str> = base.lines().collect();
    let foot = rows.iter().rposition(|row| row.contains("⏵⏵")).unwrap();
    // The captured rows with `new` spliced in at `at` in place of `cut` rows.
    let read = |at: usize, cut: usize, new: &[&str]| {
        let mut lines = rows.clone();
        lines.splice(at..at + cut, new.iter().copied());
        let got = screen::read(&(lines.join("\n") + "\n"));
        (got.composer, got.stash)
    };
    let on = "  ⏵⏵ bypass permissions on";
    let draft = Composer::Text(vec!["js".to_string()]);
    let whole = (draft.clone(), Stash::AbsentInRecognizedLayout);
    let unknown = (Composer::Unknown, Stash::Unknown);
    assert_eq!(read(foot, 0, &[]), whole);
    assert_eq!(read(foot - 5, 5, &[]), whole, "no status line");
    for tail in [
        "· 1 monitor",
        "· 2 shells, 1 monitor",
        "(shift+tab to cycle) · 4 monitors",
    ] {
        assert_eq!(read(foot, 1, &[&format!("{on} {tail}")]), whole, "{tail}");
    }
    // A cut, miscounted or other-mode footer.
    for footer in [
        format!("{on} · 4 monit"),
        format!("{on} · 4"),
        format!("{on} · 1 monitors"),
        "  ⏵⏵ accept edits on · 4 monitors".to_string(),
    ] {
        assert_eq!(read(foot, 1, &[&footer]), unknown, "{footer}");
    }
    // A row between the box and the footer that is no status line, or a row under the footer.
    let border = "─".repeat(46);
    for (at, row) in [
        (foot, border.as_str()),
        (foot, "  ❯ x"),
        (foot, "  │ Allow? │"),
        (foot, "⏺ answer"),
        (foot, ""),
        (foot, on),
        (foot + 1, "  Context left: 9%"),
    ] {
        assert_eq!(read(at, 0, &[row]), unknown, "{row}");
    }
    assert_eq!(read(foot, 0, &["  › stashed"]), (draft, Stash::Unknown));
}
