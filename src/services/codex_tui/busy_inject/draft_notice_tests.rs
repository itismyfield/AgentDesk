use super::draft_may_be_present;
use std::{
    io::Write,
    sync::{Arc, Mutex},
};

const HINT: &str = "\x1b[2mAsk Codex to do anything\x1b[0m";

fn compact(body: &str, footer: &str) -> String {
    format!("Fixture output\n\n› {body}\n\n  {footer}\n")
}

#[test]
fn fresh_and_empty_codex_composers_do_not_request_a_draft_notice() {
    let idle = include_str!("../../../../tests/fixtures/tui_input/codex-0.160.0-idle.ansi");
    let fresh = include_str!("../../../../tests/fixtures/tui_input/codex-idle-dim.ansi");
    for (name, capture) in [
        ("fresh dim composer", fresh.to_owned()),
        ("completed turn", idle.to_owned()),
        (
            "bare empty composer",
            compact("", "gpt-5.5 xhigh · /fixture/workspace"),
        ),
        (
            "whitespace composer",
            compact("   ", "gpt-5.5 xhigh · /fixture/workspace"),
        ),
    ] {
        assert!(!draft_may_be_present(&capture), "{name}");
    }
}

#[test]
fn dim_suggestions_stay_empty_with_an_opaque_or_not_yet_drawn_footer() {
    for (name, capture) in [
        (
            "footer without context marker",
            compact(HINT, "gpt-5.5 xhigh · /fixture/workspace"),
        ),
        (
            "footer not yet drawn",
            format!("Fixture startup\n\n› {HINT}\n"),
        ),
        (
            "different dim suggestion",
            compact(
                "\x1b[2mUse /skills to list available skills\x1b[0m",
                "opaque footer",
            ),
        ),
        (
            "wrapped dim suggestion",
            compact(
                "\x1b[2mAsk Codex to do\x1b[0m\n  \x1b[2manything\x1b[0m",
                "gpt-5.5 xhigh · /fixture/workspace",
            ),
        ),
        (
            "dim style carried across rows",
            compact(
                "\x1b[2mAsk Codex to do\n  anything\x1b[0m",
                "gpt-5.5 xhigh · /fixture/workspace",
            ),
        ),
    ] {
        assert!(!draft_may_be_present(&capture), "{name}");
    }
}

#[test]
fn readable_typed_text_requests_one_notice_even_when_it_matches_a_hint() {
    let captured = include_str!("../../../../tests/fixtures/tui_input/codex-0.160.0-draft.ansi");
    let footer = "gpt-5.5 xhigh · /fixture/workspace";
    for (name, capture) in [
        ("captured draft", captured.to_owned()),
        ("typed hint", compact("Ask Codex to do anything", footer)),
        (
            "partly dim hint",
            compact("\x1b[2mAsk Codex\x1b[0m to do anything", footer),
        ),
        (
            "truecolor text with a color channel of two",
            compact("\x1b[38;2;2;80;120mtyped text\x1b[0m", footer),
        ),
        (
            "multiline draft",
            compact("first row\n  second row", footer),
        ),
        (
            "typed continuation after a dim first row",
            compact(&format!("{HINT}\n  typed continuation"), footer),
        ),
    ] {
        assert_eq!(usize::from(draft_may_be_present(&capture)), 1, "{name}");
    }
}

#[test]
fn an_attachment_without_readable_text_does_not_request_a_draft_notice() {
    let capture = format!(
        "Fixture output\n\n  [Image #1]\n\n› {HINT}\n\n  gpt-5.5 xhigh · 87% context left\n"
    );
    assert!(!draft_may_be_present(&capture));
}

#[test]
fn an_unread_capture_skips_the_notice_and_records_one_body_free_decision() {
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    for (branch, capture, expected_notice) in [
        (
            "unread",
            "  private unsent words\n\n  gpt-5.5 xhigh · 87% context left\n".to_owned(),
            false,
        ),
        (
            "readable_text",
            compact("private unsent words", "gpt-5.5 xhigh · /fixture/workspace"),
            true,
        ),
    ] {
        let sink = Sink::default();
        let writer = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_env_filter(tracing_subscriber::EnvFilter::new(
                crate::logging::DEFAULT_TRACING_DIRECTIVE,
            ))
            .with_writer(move || writer.clone())
            .finish();
        crate::logging::test_capture::pin_callsite_interest();
        let notice =
            tracing::subscriber::with_default(subscriber, || draft_may_be_present(&capture));
        assert_eq!(notice, expected_notice, "{branch}");
        let logs = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            logs.matches("Codex draft notice decision").count(),
            1,
            "{logs}"
        );
        assert!(
            logs.contains(branch) && logs.contains(&format!("notify={expected_notice}")),
            "{logs}"
        );
        assert!(!logs.contains("private unsent words"), "{logs}");
    }
}
