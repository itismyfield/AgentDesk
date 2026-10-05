use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::busy_inject::*;

const NONCE: &str = "abcd1234";
const BORDER: &str = "────────────────────────────────────────────────────────────";
const FOOTER: &str = "  ⏵⏵ bypass permissions on (shift+tab to cycle)";
const SPINNER: &str = "✻ Thinking… (12s · esc to interrupt)";

const FAST: Timing = Timing {
    lock_retries: &[
        Duration::ZERO,
        Duration::from_millis(5),
        Duration::from_millis(10),
    ],
    settle: Duration::ZERO,
    rechecks: 3,
    recheck_interval: Duration::from_millis(5),
    confirm_window: Duration::from_millis(300),
    confirm_poll: Duration::from_millis(20),
};

fn text() -> String {
    frame("iMessage", "ann", NONCE, "are you there?")
}

fn screen(composer: &str, extra: &str) -> String {
    format!("⏺ Working on it.\n\n{SPINNER}\n{extra}\n{BORDER}\n❯ {composer}\n{BORDER}\n{FOOTER}")
}

fn busy_empty() -> String {
    format!("⏺ Working on it.\n\n{SPINNER}\n\n{BORDER}\n❯\u{00a0}\n{BORDER}\n{FOOTER}")
}

fn idle_empty() -> String {
    format!("⏺ Done.\n\n{BORDER}\n❯\u{00a0}\n{BORDER}\n{FOOTER}")
}

fn modal_screen() -> String {
    format!(
        "⏺ Working on it.\n\n{SPINNER}\n Run this tool? allow / deny\n\n{BORDER}\n❯\u{00a0}\n{BORDER}\n{FOOTER}"
    )
}

/// One transcript line with the envelope fields Claude writes on every record.
fn real(mut record: serde_json::Value) -> String {
    record["sessionId"] = "6245-session".into();
    record["timestamp"] = "2026-10-06T00:00:00.000Z".into();
    record.to_string()
}

fn enqueue_record(content: &str) -> String {
    real(serde_json::json!({"type": "queue-operation", "operation": "enqueue", "content": content}))
        + "\n"
}

/// A scripted `tmux`. `keyattach` is the attach count when the server applies a key;
/// `if-shell -F` applies its command only while that count is 0, as the real server does.
const FAKE_TMUX: &str = r#"#!/bin/sh
d='@D@'
next() { n=$(cat "$d/$1.n" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$d/$1.n"; f="$d/$1.$n"; [ -f "$f" ] || f="$d/$1"; cat "$f"; }
effects() {
  [ "$1" = send-keys ] && [ -f "$d/on_enter" ] || return 0
  if [ -f "$d/on_enter_delay" ]; then
    (sleep "$(cat "$d/on_enter_delay")"; cat "$d/on_enter" >> '@T@') </dev/null >/dev/null 2>&1 &
  else
    cat "$d/on_enter" >> '@T@'
  fi
}
echo "$*" >> "$d/log"
[ -f "$d/fail.$2" ] && exit 1
case "$2" in
display-message) next attach ;;
capture-pane) next cap ;;
load-buffer) for last do :; done; cp "$last" "$d/buffer" ;;
send-keys|paste-buffer)
  [ "$(next keyattach)" = 0 ] || echo "$*" >> "$d/keys_while_attached"
  effects "$2" ;;
if-shell)
  if [ "$(next keyattach)" = 0 ]; then
    echo "-u $7" >> "$d/log"
    [ -f "$d/fail.${7%% *}" ] && exit 1
    effects "${7%% *}"
  else
    echo "agentdesk-busy-inject-vetoed 1"
  fi ;;
esac
exit 0
"#;

/// Per-subcommand answer sequences, failures and a call log for the scripted `tmux`.
struct Fake {
    dir: tempfile::TempDir,
    transcript: PathBuf,
}

impl Fake {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/busy-inject-tmp");
        fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        let transcript = dir.path().join("transcript.jsonl");
        let busy_turn = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"go\"}}\n\
            {\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"working\"}]}}\n";
        fs::write(&transcript, busy_turn).unwrap();
        let script = FAKE_TMUX
            .replace("@D@", &dir.path().display().to_string())
            .replace("@T@", &transcript.display().to_string());
        let program = dir.path().join("tmux");
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let fake = Self { dir, transcript };
        fake.answers("attach", &["0"]);
        fake.answers("keyattach", &["0"]);
        fake
    }

    /// Answers in call order; the last one repeats.
    fn answers(&self, name: &str, values: &[&str]) {
        for (i, value) in values.iter().enumerate() {
            fs::write(self.dir.path().join(format!("{name}.{}", i + 1)), value).unwrap();
        }
        fs::write(self.dir.path().join(name), values.last().unwrap()).unwrap();
    }

    fn caps(&self, values: &[String]) {
        let refs: Vec<&str> = values.iter().map(String::as_str).collect();
        self.answers("cap", &refs);
    }

    fn fail(&self, subcommand: &str) {
        fs::write(self.dir.path().join(format!("fail.{subcommand}")), "").unwrap();
    }

    fn accept_on_enter(&self) {
        fs::write(self.dir.path().join("on_enter"), enqueue_record(&text())).unwrap();
    }

    /// The transcript records the input `seconds` after the Enter.
    fn accept_on_enter_after(&self, seconds: &str) {
        self.accept_on_enter();
        fs::write(self.dir.path().join("on_enter_delay"), seconds).unwrap();
    }

    fn session(&self) -> String {
        let name = self.dir.path().file_name().unwrap().to_string_lossy();
        let name: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        format!("busy-{name}")
    }

    fn run(&self, timing: &Timing) -> Outcome {
        let session = self.session();
        let pane = Pane::with_program(&session, self.dir.path().join("tmux"));
        let request = Request {
            session: &session,
            transcript: &self.transcript,
            source: "iMessage",
            author: "ann",
            nonce: NONCE,
            text: "are you there?",
        };
        inject(&pane, &request, timing)
    }

    fn calls(&self, subcommand: &str) -> Vec<String> {
        fs::read_to_string(self.dir.path().join("log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.split_whitespace().nth(1) == Some(subcommand))
            .map(str::to_string)
            .collect()
    }

    /// Keys the server applied while a client was attached.
    fn keys_while_attached(&self) -> usize {
        fs::read_to_string(self.dir.path().join("keys_while_attached"))
            .map(|log| log.lines().count())
            .unwrap_or(0)
    }

    /// (pastes, Enter keys, any other key) — the only keys busy_inject may send are one Enter.
    fn keys(&self) -> (usize, usize, usize) {
        let sends = self.calls("send-keys");
        let enters = sends.iter().filter(|line| line.ends_with(" Enter")).count();
        (
            self.calls("paste-buffer").len(),
            enters,
            sends.len() - enters,
        )
    }
}

#[test]
fn scanner_counts_only_complete_human_input_after_the_offset_by_containment() {
    let fake = Fake::new();
    let path = &fake.transcript;
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    // Evidence before the offset never counts.
    file.write_all(enqueue_record(&text()).as_bytes()).unwrap();
    let offset = fs::metadata(path).unwrap().len();
    let marker_text = text();
    let ignored = [
        serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": marker_text}]}}),
        serde_json::json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": marker_text}]}}),
        serde_json::json!({"type": "user", "isMeta": true, "message": {"content": marker_text}}),
        serde_json::json!({"type": "queue-operation", "operation": "remove", "content": marker_text}),
        serde_json::json!({"type": "queue-operation", "operation": "dequeue"}),
        serde_json::json!({"type": "queue-operation", "operation": "enqueue", "content": frame("iMessage", "ann", "ffff0000", "x")}),
    ];
    for record in ignored {
        writeln!(file, "{}", real(record)).unwrap();
    }
    assert!(!transcript_carries(path, offset, NONCE));
    // A record still being written has no newline yet.
    let merged = serde_json::json!({"type": "attachment", "attachment": {"type": "queued_command", "commandMode": "prompt", "prompt": format!("first\n{}", text())}});
    write!(file, "{}", real(merged)).unwrap();
    assert!(!transcript_carries(path, offset, NONCE));
    writeln!(file).unwrap();
    assert!(transcript_carries(path, offset, NONCE));

    for record in [
        serde_json::json!({"type": "user", "message": {"content": format!("The user sent a new message while you were working:\n{}", text())}}),
        serde_json::json!({"type": "user", "message": {"content": [{"type": "text", "text": text()}]}}),
        serde_json::json!({"type": "queue-operation", "operation": "enqueue", "content": text()}),
    ] {
        let fresh = Fake::new();
        let offset = fs::metadata(&fresh.transcript).unwrap().len();
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&fresh.transcript)
            .unwrap();
        writeln!(file, "{}", real(record.clone())).unwrap();
        assert!(
            transcript_carries(&fresh.transcript, offset, NONCE),
            "{record}"
        );
    }
    assert!(!transcript_carries(
        &fake.dir.path().join("missing"),
        0,
        NONCE
    ));
}

#[test]
fn the_header_names_the_source_and_cannot_start_a_command_or_close_early() {
    let framed = frame("iMessage", "a]b\nc", NONCE, "/clear");
    assert_eq!(framed, "[📱 iMessage · a b c · abcd1234]\n/clear");
}

#[test]
fn every_pre_paste_veto_leaves_the_pane_untouched() {
    type Setup = fn(&Fake);
    let cases: [(&str, Setup, Veto); 8] = [
        (
            "attached",
            |f| f.answers("attach", &["1"]),
            Veto::HumanAttached,
        ),
        (
            "attach query fails",
            |f| f.fail("display-message"),
            Veto::AttachUnknown,
        ),
        (
            "capture fails",
            |f| f.fail("capture-pane"),
            Veto::PaneUnavailable,
        ),
        ("modal", |f| f.caps(&[modal_screen()]), Veto::Modal),
        (
            "draft",
            |f| f.caps(&[screen("half typed", "")]),
            Veto::Draft,
        ),
        ("idle pane", |f| f.caps(&[idle_empty()]), Veto::NotBusy),
        (
            "idle transcript",
            |f| {
                fs::write(
                    &f.transcript,
                    "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
                )
                .unwrap()
            },
            Veto::NotBusy,
        ),
        ("load fails", |f| f.fail("load-buffer"), Veto::LoadFailed),
    ];
    for (name, setup, veto) in cases {
        let fake = Fake::new();
        fake.caps(&[busy_empty()]);
        setup(&fake);
        assert_eq!(fake.run(&FAST), Outcome::NotSent(veto), "{name}");
        assert_eq!(fake.keys(), (0, 0, 0), "{name}");
    }
}

#[test]
fn a_held_composer_lock_is_tried_three_times_without_any_tmux_call() {
    let fake = Fake::new();
    fake.caps(&[busy_empty()]);
    let (held, release) = (std::sync::mpsc::channel(), std::sync::mpsc::channel::<()>());
    let session = fake.session();
    let holder = std::thread::spawn(move || {
        super::composer_lock::with_composer_mutation_lock(&session, || {
            held.0.send(()).unwrap();
            release.1.recv().unwrap();
        })
    });
    held.1.recv().unwrap();
    assert_eq!(fake.run(&FAST), Outcome::NotSent(Veto::LockContended));
    release.0.send(()).unwrap();
    holder.join().unwrap();
    assert!(fs::read_to_string(fake.dir.path().join("log")).is_err());

    // Attempts land at fixed offsets from the first try; the waits do not add up.
    let clock = std::cell::Cell::new(Duration::ZERO);
    let mut tries = Vec::new();
    let never: Option<()> = at_offsets(
        TIMING.lock_retries,
        || clock.get(),
        |wait| clock.set(clock.get() + wait),
        || {
            tries.push(clock.get());
            clock.set(clock.get() + Duration::from_millis(3));
            None
        },
    );
    let offsets = [0, 80, 160].map(Duration::from_millis).to_vec();
    assert_eq!((never, tries), (None, offsets));
}

#[test]
fn an_owned_draft_is_entered_once_and_confirmed_from_the_transcript() {
    let folded = "[Pasted text #1 +1 lines]".to_string();
    for owned in [text(), folded] {
        let fake = Fake::new();
        fake.caps(&[busy_empty(), screen(&owned, "")]);
        fake.accept_on_enter();
        assert_eq!(fake.run(&FAST), Outcome::Injected, "{owned}");
        assert_eq!(fake.keys(), (1, 1, 0));
        assert_eq!(
            fs::read_to_string(fake.dir.path().join("buffer")).unwrap(),
            text()
        );
        let paste = &fake.calls("paste-buffer")[0];
        assert!(paste.contains(" -p ") && paste.contains(&format!("={}:", fake.session())));
    }
}

/// After the paste nothing is NotSent; every path ends with at most one Enter and no deletion.
#[test]
fn schedules_after_the_paste_never_fall_back_to_not_sent_or_delete() {
    let prefix = |n: usize| text().chars().take(n).collect::<String>();
    let cases: Vec<(&str, Vec<String>, bool, Outcome, (usize, usize, usize))> = vec![
        (
            "stale capture",
            vec![busy_empty()],
            true,
            Outcome::Unconfirmed(Unconfirmed::DraftNotOwned),
            (1, 0, 0),
        ),
        (
            "bytewise paste completes",
            vec![
                busy_empty(),
                screen(&prefix(5), ""),
                screen(&prefix(20), ""),
                screen(&text(), ""),
            ],
            true,
            Outcome::Injected,
            (1, 1, 0),
        ),
        (
            "bytewise paste stalls",
            vec![
                busy_empty(),
                screen(&prefix(5), ""),
                screen(&prefix(20), ""),
            ],
            true,
            Outcome::Unconfirmed(Unconfirmed::DraftNotOwned),
            (1, 0, 0),
        ),
        (
            "modal after paste",
            vec![busy_empty(), modal_screen()],
            true,
            Outcome::Unconfirmed(Unconfirmed::ModalAfterPaste),
            (1, 0, 0),
        ),
    ];
    for (name, caps, accept, outcome, keys) in cases {
        let fake = Fake::new();
        fake.caps(&caps);
        if accept {
            fake.accept_on_enter();
        }
        assert_eq!(fake.run(&FAST), outcome, "{name}");
        assert_eq!(fake.keys(), keys, "{name}");
    }
    // The input is taken but recorded 1s after Enter: past an 80ms window, before a 1.5s poll.
    let late = Timing {
        confirm_window: Duration::from_millis(80),
        confirm_poll: Duration::from_millis(1500),
        ..FAST
    };
    let fake = Fake::new();
    fake.caps(&[busy_empty(), screen(&text(), "")]);
    fake.accept_on_enter_after("1");
    let outcome = fake.run(&late);
    // The record that lands afterwards changes nothing and asks for nothing.
    std::thread::sleep(Duration::from_millis(1300));
    assert_eq!(
        (
            outcome,
            fake.keys(),
            transcript_carries(&fake.transcript, 0, NONCE)
        ),
        (
            Outcome::Unconfirmed(Unconfirmed::NotObserved),
            (1, 1, 0),
            true
        ),
        "late transcript"
    );
    for (name, subcommand, outcome) in [
        ("paste fails", "paste-buffer", Unconfirmed::PasteFailed),
        ("enter fails", "send-keys", Unconfirmed::EnterFailed),
    ] {
        let fake = Fake::new();
        fake.caps(&[busy_empty(), screen(&text(), "")]);
        fake.fail(subcommand);
        assert_eq!(fake.run(&FAST), Outcome::Unconfirmed(outcome), "{name}");
    }
}

/// A person's keys mixed into the composer stop the Enter; nothing is ever deleted.
#[test]
fn human_typing_or_attaching_around_the_paste_sends_no_enter_and_no_delete() {
    let owned = || vec![busy_empty(), screen(&text(), "")];
    // (name, captures, attach seen by checks, attach when the server applies each key, ...)
    type Case<'a> = (
        &'a str,
        Vec<String>,
        Vec<&'a str>,
        Vec<&'a str>,
        Outcome,
        (usize, usize, usize),
    );
    let cases: Vec<Case> = vec![
        (
            "typed before the paste",
            vec![busy_empty(), screen(&format!("hi {}", text()), "")],
            vec!["0"],
            vec!["0"],
            Outcome::Unconfirmed(Unconfirmed::DraftNotOwned),
            (1, 0, 0),
        ),
        (
            "typed after the paste",
            vec![busy_empty(), screen(&format!("{} and more", text()), "")],
            vec!["0"],
            vec!["0"],
            Outcome::Unconfirmed(Unconfirmed::DraftNotOwned),
            (1, 0, 0),
        ),
        (
            "attached after the paste",
            owned(),
            vec!["0", "1"],
            vec!["0"],
            Outcome::Unconfirmed(Unconfirmed::AttachedAfterPaste),
            (1, 0, 0),
        ),
        (
            "attached between the last check and Enter",
            owned(),
            vec!["0"],
            vec!["0", "1"],
            Outcome::Unconfirmed(Unconfirmed::AttachedAfterPaste),
            (1, 0, 0),
        ),
        (
            "attached between the first check and the paste",
            owned(),
            vec!["0"],
            vec!["1"],
            Outcome::NotSent(Veto::HumanAttached),
            (0, 0, 0),
        ),
    ];
    let mut seen = Vec::new();
    let mut expected = Vec::new();
    for (name, caps, attach, keyattach, outcome, keys) in cases {
        let fake = Fake::new();
        fake.caps(&caps);
        fake.answers("attach", &attach);
        fake.answers("keyattach", &keyattach);
        fake.accept_on_enter();
        let got = fake.run(&FAST);
        seen.push((name, got, fake.keys(), fake.keys_while_attached()));
        expected.push((name, outcome, keys, 0));
    }
    assert_eq!(seen, expected);
    // Unconfirmed after Enter while a person types: busy_inject sends no cleanup keys.
    let fake = Fake::new();
    fake.caps(&[
        busy_empty(),
        screen(&text(), ""),
        screen(&format!("{} typed", text()), ""),
    ]);
    assert_eq!(
        fake.run(&FAST),
        Outcome::Unconfirmed(Unconfirmed::NotObserved)
    );
    assert_eq!(fake.keys(), (1, 1, 0));
}
