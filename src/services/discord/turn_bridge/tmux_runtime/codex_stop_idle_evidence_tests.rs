//! The idle-evidence line hides user input, stays bounded and survives the shipped log filter.

use super::*;
use std::sync::{Arc, Mutex};

fn sha8(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// Prompt, quoted and bash-mode rows and a prompt's indented continuation keep only a length and
// a hash; escapes and control characters are gone, and tool rows under a prompt stay readable.
#[test]
fn idle_evidence_tail_masks_user_input_and_strips_controls() {
    let screen = "❯ deploy the secret build please\n  second line of that prompt\n  ⎿  Interrupted · What should Claude do instead?\n\n> quoted user text\n! git push --force\n\x1b[1mbold\x1b[0m tab\there\x07bell\u{85}c1\r\n────────\n❯\u{a0}ghost text suggestion\u{a0}\n────────\n  status line\n";
    let (tail, below) = tail(screen);
    assert_eq!(
        tail,
        vec![
            format!(
                "❯ <masked chars=30 sha={}>",
                sha8("deploy the secret build please")
            ),
            format!(
                "<masked chars=26 sha={}>",
                sha8("second line of that prompt")
            ),
            "⎿  Interrupted · What should Claude do instead?".to_owned(),
            format!("> <masked chars=16 sha={}>", sha8("quoted user text")),
            format!("! <masked chars=16 sha={}>", sha8("git push --force")),
            "bold tabherebellc1".to_owned(),
            "─×8".to_owned(),
            format!("❯ <masked chars=21 sha={}>", sha8("ghost text suggestion")),
            "─×8".to_owned(),
            "status line".to_owned(),
        ]
    );
    assert_eq!(below, Some(2));
    assert_eq!(
        super::tail("x\n❯ \n").0,
        vec!["x", "❯"],
        "an empty composer is its marker"
    );
}

// Eight rows above the bottom composer, three below; without a composer the last twelve; rows
// are cut at 160 characters and the whole tail at 2,048 bytes, nearest the composer kept.
#[test]
fn idle_evidence_tail_is_bounded_in_rows_width_and_bytes() {
    let above: Vec<String> = (1..=20).map(|row| format!("row {row:02}")).collect();
    let below: Vec<String> = (1..=6).map(|row| format!("below {row}")).collect();
    let screen = format!("{}\n❯ \n{}\n", above.join("\n"), below.join("\n"));
    let mut expected = above[12..].to_vec();
    expected.push("❯".to_owned());
    expected.extend_from_slice(&below[..3]);
    assert_eq!(tail(&screen), (expected, Some(6)));

    assert_eq!(tail(&above.join("\n")), (above[8..].to_vec(), None));

    let wide = "ab".repeat(150);
    let (cut, _) = tail(&wide);
    assert_eq!(cut[0].chars().count(), 160);
    assert_eq!(cut[0], format!("{}…", &wide[..159]));

    let heavy: Vec<String> = (0..12)
        .map(|row| format!("{row:02}{}", "가나".repeat(80)))
        .collect();
    let (kept, _) = tail(&heavy.join("\n"));
    assert!(kept.iter().map(String::len).sum::<usize>() <= 2048);
    let starts: Vec<&str> = kept.iter().map(|line| &line[..2]).collect();
    assert_eq!(starts, ["08", "09", "10", "11"]);
}

// A secret on a tool row is redacted before it reaches the tail.
#[test]
fn idle_evidence_tail_redacts_known_secrets() {
    let screen = "⏺ Bash(deploy)\n  ⎿  $ OPENAI_API_KEY=sk-live-abc123 psql postgres://agent:hunter2@db/x (3s · timeout 2m)\n";
    let joined = tail(screen).0.join("\n");
    assert!(!joined.contains("sk-live-abc123"), "{joined}");
    assert!(!joined.contains("hunter2"), "{joined}");
    assert!(joined.contains("OPENAI_API_KEY=***"), "{joined}");
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::writer::MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

// The line is kept by the directive every dcserver installs, and stays one line whatever the
// screen held.
#[test]
fn idle_evidence_survives_the_shipped_filter_as_one_line() {
    let binding = TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        output_path: "/tmp/transcript.jsonl".to_owned(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: None,
        last_offset: 0,
        relay_last_offset: None,
    };
    let evidence = IdleEvidence::new(
        &CancelToken::new(),
        &ProviderKind::Claude,
        (42, "AgentDesk-claude-test", "nonce-1"),
        (&binding, &binding),
        (Some(TuiTurnState::Streaming), true, false, false),
        "⏺ done\n\"quoted\" line\n\n❯ \n",
    );
    let writer = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer.clone())
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            crate::logging::DEFAULT_TRACING_DIRECTIVE,
        ))
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    tracing::subscriber::with_default(subscriber, || evidence.emit());
    let logs = String::from_utf8(writer.0.lock().unwrap().clone()).unwrap();
    assert_eq!(logs.lines().count(), 1, "{logs}");
    assert!(logs.contains(" INFO agentdesk::"), "{logs}");
    assert!(
        logs.contains("event=\"herdr_stop_idle_evidence\""),
        "{logs}"
    );
    assert!(logs.contains("phase=\"ambiguous\""), "{logs}");
    assert!(logs.contains("sources_same=true"), "{logs}");
}
