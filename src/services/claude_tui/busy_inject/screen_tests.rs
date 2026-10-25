use super::screen::{self, Composer};

fn boxed(body: &str, footer: &str) -> String {
    format!(
        "\n{}\n{body}\n{}\n{footer}\n",
        "─".repeat(46),
        "─".repeat(46)
    )
}

#[test]
fn pre_paste_box_does_not_depend_on_footer_or_status() {
    for footer in [
        "",
        "  ⏵⏵ bypass permissions on · 1 shell, 1",
        "unknown footer",
        "  paste again to expand",
    ] {
        assert_eq!(
            screen::input_box(&boxed("❯\u{a0}", footer)),
            Composer::Empty
        );
        assert_eq!(
            screen::input_box(&boxed("❯ draft\n  second line", footer)),
            Composer::Text(vec!["draft".into(), "second line".into()])
        );
    }
}

#[test]
fn empty_requires_a_complete_box_and_an_exact_blank_or_measured_hint() {
    assert_eq!(
        screen::input_box(&boxed("❯\u{a0}\x1b[2mTry something\x1b[0m", "")),
        Composer::Empty
    );
    for body in [
        "❯\u{a0}\x1b[2munclosed hint",
        "❯ \x1b[2mfaint with plain space\x1b[0m",
        "❯ \n  ",
        "❯ [Image #1]",
        "❯ draft\nmalformed continuation",
    ] {
        assert_eq!(
            screen::input_box(&boxed(body, "")),
            Composer::Unknown,
            "{body:?}"
        );
    }
    for capture in [
        "❯ ".into(),
        "────────────────\n❯ ".into(),
        "❯ \n────────────────".into(),
        format!("{}\n❯ \n{}", "─".repeat(46), "─".repeat(45)),
    ] {
        assert_eq!(
            screen::input_box(&capture),
            Composer::Unknown,
            "{capture:?}"
        );
    }
}

#[test]
fn menus_and_permission_cards_are_vetoed_but_literal_draft_words_are_not() {
    for menu in [
        "Run this tool? allow / deny",
        "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEnter to select · Esc to cancel",
    ] {
        assert!(screen::modal(&format!("{menu}\n{}", boxed("❯ ", ""))));
    }
    assert!(!screen::modal(&boxed(
        "❯ Run this tool? allow / deny",
        "unknown footer"
    )));
}

#[test]
fn captured_46_column_boxes_ignore_the_cut_footer() {
    let empty = include_str!("fixtures/truncated-footer-empty.ansi");
    let draft = include_str!("fixtures/truncated-footer-draft.ansi");
    assert_eq!(screen::input_box(empty), Composer::Empty);
    assert!(matches!(screen::input_box(draft), Composer::Text(_)));
    for capture in [empty, draft] {
        let plain = crate::services::codex_tui::input::strip_ansi_escape_sequences(capture);
        assert_eq!(plain.lines().count(), 37);
        assert!(
            plain
                .lines()
                .all(|row| unicode_width::UnicodeWidthStr::width(row) <= 46)
        );
        assert!(plain.ends_with("  ⏵⏵ bypass permissions on · 1 shell, 1\n"));
    }
}
