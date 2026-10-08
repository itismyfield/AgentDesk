//! The rows Claude draws a paste into, against composers captured from Claude Code 2.1.293.

use super::screen::{Drawn, drawn};
use crate::services::tui_input::actor::gate::own_wrapped_draft;

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
        (format!("a{}", hangul(38)), vec![format!("a{}", hangul(37)), "아".into()]),
        ("😀a".repeat(25), vec!["😀a".repeat(25)]),
        (format!("{} abcdef", x(70)), vec![x(70), "abcdef".into()]),
        (format!("{} abcde", x(70)), vec![format!("{} abcde", x(70))]),
        (format!("{} ab", x(76)), vec![x(76), "ab".into()]),
        (format!("{}  ab", x(76)), vec![x(76), "ab".into()]),
        (format!("{}    ab", x(74)), vec![x(74), "ab".into()]),
        (format!("{}  abcdefgh", x(70)), vec![x(70), "abcdefgh".into()]),
        (format!("ab{}c", " ".repeat(80)), vec!["ab".into(), "c".into()]),
        (space_run, vec!["ab".into(), String::new(), "c".into()]),
        (format!("ab {}", x(100)), vec![format!("ab {}", x(73)), x(27)]),
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
                "one-two-three/four_five.six,seven;one-two-three/four_five.six,seven;one-two-".into(),
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
    ] {
        assert_eq!(rows(&text, PANE), None, "{text}");
    }
    assert_eq!(drawn(&format!("{HEADER}\nshort"), None), None);
    // Over 800 UTF-16 units or more than two line breaks, Claude folds instead.
    for text in ["z".repeat(998), "a\nb\nc".to_string()] {
        assert_eq!(drawn(&format!("{HEADER}\n{text}"), PANE), Some(Drawn::Folded));
    }
}

#[test]
fn a_wrapped_paste_is_owned_only_in_the_rows_claude_draws() {
    let frame = format!("{HEADER}\n{} 끝", words(25));
    let Some(Drawn::Rows(predicted)) = drawn(&frame, PANE) else {
        panic!("predicted rows");
    };
    let border = "─".repeat(80);
    let composer = |rows: &[&str]| format!("{border}\n❯\u{00a0}{HEADER}\n{}\n{border}\n", rows.join("\n"));
    let (one, two, three) = (
        format!("  {}", words(11)),
        format!("  {}", words(10)),
        format!("  {} 끝", words(4)),
    );
    assert!(own_wrapped_draft(&composer(&[&one, &two, &three]), &predicted));
    let typed = format!("{three}x");
    let raw: Vec<String> = frame.split('\n').map(str::to_string).collect();
    assert!(!own_wrapped_draft(&composer(&[&one, &two, &three]), &raw));
    assert!(!own_wrapped_draft(&composer(&[&one, &two, &typed]), &predicted));
    assert!(!own_wrapped_draft(&composer(&[&one, &two, &three, "  x"]), &predicted));
    assert!(!own_wrapped_draft(&composer(&[&one, &two]), &predicted));
    assert!(!own_wrapped_draft(&composer(&[&one, &two[1..], &three]), &predicted));
}
