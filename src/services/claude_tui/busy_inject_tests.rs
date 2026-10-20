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
    restore_window: Duration::from_millis(100),
};

fn text() -> String {
    frame("iMessage", "200", "ann", NONCE, "are you there?")
}

fn v1_text() -> String {
    crate::services::tui_input::human_frame::inject(
        "iMessage",
        "200",
        "ann",
        NONCE,
        "are you there?",
    )
}

// Claude draws continuation rows two columns in.
fn screen(composer: &str, extra: &str) -> String {
    let composer = composer.replace('\n', "\n  ");
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
  [ "$1" = send-keys ] || return 0
  if [ ! -f "$d/on_enter" ]; then :
  elif [ -f "$d/reply_gap" ]; then
    # A slow after-send-keys hook: the key is applied, the record lands, then the reply follows.
    sleep "$(cat "$d/on_enter_delay")"; cat "$d/on_enter" >> '@T@'; sleep "$(cat "$d/reply_gap")"
  elif [ -f "$d/on_enter_delay" ]; then
    (sleep "$(cat "$d/on_enter_delay")"; cat "$d/on_enter" >> '@T@') </dev/null >/dev/null 2>&1 &
  else
    cat "$d/on_enter" >> '@T@'
  fi
  return 0
}
echo "$*" >> "$d/log"
[ -f "$d/fail.$2" ] && exit 1
case "$2" in
display-message)
  # A bare count gets a never-attached generation and a roomy pane.
  a=$(next attach); case "$a" in *,*) echo "$a" ;; *) echo "$a,,200,50" ;; esac ;;
capture-pane) next cap ;;
load-buffer) for last do :; done; cp "$last" "$d/buffer" ;;
send-keys|paste-buffer)
  [ "$(next keyattach)" = 0 ] || echo "$*" >> "$d/keys_while_attached"
  effects "$2" ;;
if-shell)
  k=$(next keyattach)
  if [ "$k" = 0 ]; then
    echo "-u $7" >> "$d/log"
    [ -f "$d/fail.${7%% *}" ] && exit 1
    effects "${7%% *}"
  elif [ "$k" = gone ]; then
    echo "agentdesk-busy-inject-vetoed"
  else
    echo "agentdesk-busy-inject-vetoed $k"
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

    /// The Enter command replies `seconds` after the delayed record has landed.
    fn reply_after_record(&self, seconds: &str) {
        fs::write(self.dir.path().join("reply_gap"), seconds).unwrap();
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
        self.run_text("are you there?", timing)
    }

    fn run_text(&self, text: &str, timing: &Timing) -> Outcome {
        let session = self.session();
        let pane = Pane::with_program(&session, self.dir.path().join("tmux"));
        let request = Request {
            session: &session,
            transcript: &self.transcript,
            source: "iMessage",
            author: "200",
            name: "ann",
            nonce: NONCE,
            text,
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

    /// Resolve late evidence using the identity and exact bytes actually pasted.
    fn pasted_attempt_submission(&self) -> Option<bool> {
        let loads = self.calls("load-buffer");
        assert_eq!(loads.len(), 1, "one pasted attempt owns the late receipt");
        let mut args = loads[0].split_whitespace();
        args.find(|arg| *arg == "-b").unwrap();
        let id = args
            .next()
            .unwrap()
            .strip_prefix("agentdesk-busy-inject-")
            .unwrap();
        let pasted = fs::read_to_string(self.dir.path().join("buffer")).unwrap();
        let registered = crate::services::tui_input::human_frame::history::registered;
        assert!(
            registered(id, &self.transcript, &pasted),
            "late receipt must retain its pasted attempt ID and exact frame hash"
        );
        assert!(!registered("foreign-attempt", &self.transcript, &pasted));
        assert!(!registered(
            id,
            &self.transcript,
            &format!("{pasted}\nforeign")
        ));
        transcript_submission(&self.transcript, 0, id, Some(&pasted))
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
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let fake = Fake::new();
    let path = &fake.transcript;
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    // Evidence before the offset never counts.
    file.write_all(enqueue_record(&v1_text()).as_bytes())
        .unwrap();
    let offset = fs::metadata(path).unwrap().len();
    let marker_text = v1_text();
    let ignored = [
        serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": marker_text}]}}),
        serde_json::json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": marker_text}]}}),
        serde_json::json!({"type": "user", "isMeta": true, "message": {"content": marker_text}}),
        serde_json::json!({"type": "queue-operation", "operation": "remove", "content": marker_text}),
        serde_json::json!({"type": "queue-operation", "operation": "dequeue"}),
        serde_json::json!({"type": "queue-operation", "operation": "enqueue", "content": crate::services::tui_input::human_frame::inject("iMessage", "200", "ann", "ffff0000", "x")}),
    ];
    for record in ignored {
        writeln!(file, "{}", real(record)).unwrap();
    }
    assert!(!transcript_carries(path, offset, NONCE));
    // A record still being written has no newline yet.
    let merged = serde_json::json!({"type": "attachment", "attachment": {"type": "queued_command", "commandMode": "prompt", "prompt": format!("first\n{}", v1_text())}});
    write!(file, "{}", real(merged)).unwrap();
    assert!(!transcript_carries(path, offset, NONCE));
    writeln!(file).unwrap();
    assert!(transcript_carries(path, offset, NONCE));

    for record in [
        serde_json::json!({"type": "user", "message": {"content": format!("The user sent a new message while you were working:\n{}", v1_text())}}),
        serde_json::json!({"type": "user", "message": {"content": [{"type": "text", "text": v1_text()}]}}),
        serde_json::json!({"type": "queue-operation", "operation": "enqueue", "content": v1_text()}),
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
fn scanner_confirms_the_full_frame_and_rejects_same_nonce_contamination() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let expected = v1_text();
    for body in [
        expected.clone(),
        format!("{expected}\noperator draft X"),
        expected.lines().next().unwrap().to_string(),
    ] {
        let exact = body == expected;
        let records = [
            serde_json::json!({"type": "queue-operation", "operation": "enqueue", "content": body}),
            serde_json::json!({"type": "attachment", "attachment": {"type": "queued_command", "commandMode": "prompt", "prompt": body}}),
            serde_json::json!({"type": "user", "message": {"content": body}}),
            serde_json::json!({"type": "user", "message": {"content": [{"type": "text", "text": body}]}}),
        ];
        for record in records {
            let fake = Fake::new();
            let offset = fs::metadata(&fake.transcript).unwrap().len();
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&fake.transcript)
                .unwrap();
            writeln!(file, "{}", real(record.clone())).unwrap();
            assert_eq!(
                transcript_submission(&fake.transcript, offset, NONCE, Some(&expected)),
                Some(exact),
                "same nonce must distinguish the submitted body: {record}"
            );
            assert_eq!(
                transcript_submission(&fake.transcript, offset, NONCE, None),
                Some(true),
                "nonce-only callers retain their existing receipt contract: {record}"
            );
        }
    }
}

#[test]
fn scanner_rejects_nontext_blocks_without_changing_text_or_nonce_receipts() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let expected = v1_text();
    let text_block = serde_json::json!({"type": "text", "text": expected});
    let mut cases = vec![
        (serde_json::json!(expected), true),
        (serde_json::json!([text_block]), true),
        (
            serde_json::json!(
                expected
                    .lines()
                    .map(|line| serde_json::json!({"type": "text", "text": line}))
                    .collect::<Vec<_>>()
            ),
            true,
        ),
    ];
    for extra in [
        serde_json::json!({"type": "image", "source": {"type": "base64", "data": "AA=="}}),
        serde_json::json!({"type": "tool_result", "content": "person input"}),
        serde_json::json!({"type": "unknown"}),
        serde_json::Value::Null,
    ] {
        cases.push((serde_json::json!([text_block, extra]), false));
        cases.push((serde_json::json!([extra, text_block]), false));
    }
    for (content, exact) in cases {
        for record in [
            serde_json::json!({"type": "user", "message": {"content": content}}),
            serde_json::json!({"type": "queue-operation", "operation": "enqueue", "content": content}),
            serde_json::json!({"type": "attachment", "attachment": {"type": "queued_command", "commandMode": "prompt", "prompt": content}}),
        ] {
            let fake = Fake::new();
            let offset = fs::metadata(&fake.transcript).unwrap().len();
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&fake.transcript)
                .unwrap();
            writeln!(file, "{}", real(record.clone())).unwrap();
            assert_eq!(
                transcript_submission(&fake.transcript, offset, NONCE, Some(&expected)),
                Some(exact),
                "{record}"
            );
            assert!(
                transcript_carries(&fake.transcript, offset, NONCE),
                "{record}"
            );
        }
    }
}

#[test]
fn scanner_ignores_nonprompt_evidence_and_incomplete_or_prior_records() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let expected = v1_text();
    let fake = Fake::new();
    let path = &fake.transcript;
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(enqueue_record(&expected).as_bytes())
        .unwrap();
    let offset = fs::metadata(path).unwrap().len();
    for record in [
        serde_json::json!({"type": "assistant", "message": {"content": expected}}),
        serde_json::json!({"type": "user", "isMeta": true, "message": {"content": expected}}),
        serde_json::json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": expected}]}}),
        serde_json::json!({"type": "queue-operation", "operation": "remove", "content": expected}),
        serde_json::json!({"type": "attachment", "attachment": {"type": "queued_command", "commandMode": "bash", "prompt": expected}}),
        serde_json::json!({"type": "queue-operation", "operation": "enqueue", "content": crate::services::tui_input::human_frame::inject("iMessage", "200", "ann", "ffff0000", "are you there?")}),
    ] {
        writeln!(file, "{}", real(record.clone())).unwrap();
        assert_eq!(
            transcript_submission(path, offset, NONCE, Some(&expected)),
            None,
            "this record cannot acknowledge or contaminate a prompt: {record}"
        );
    }
    write!(file, "{}", enqueue_record(&expected).trim_end_matches('\n')).unwrap();
    assert_eq!(
        transcript_submission(path, offset, NONCE, Some(&expected)),
        None
    );
    writeln!(file).unwrap();
    assert_eq!(
        transcript_submission(path, offset, NONCE, Some(&expected)),
        Some(true)
    );
    assert_eq!(
        transcript_submission(
            path,
            fs::metadata(path).unwrap().len() + 1,
            NONCE,
            Some(&expected)
        ),
        None
    );
    assert_eq!(
        transcript_submission(&fake.dir.path().join("missing"), 0, NONCE, Some(&expected)),
        None
    );
}

#[test]
fn the_header_names_the_person_and_cannot_start_a_command_or_close_early() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let framed = crate::services::tui_input::human_frame::inject(
        "iMessage", "200", "a]b\nc", NONCE, "/clear",
    );
    let header = "[User: a)b c (ID: 200) · 📱 v1 mode=inject source=iMessage · abcd1234]";
    assert_eq!(framed, format!("{header}\n/clear"));
}

/// A running turn's input left in the composer, flat, wrapped or after a person's keys, is
/// AgentDesk's frame: never a person's draft to stash, nor the stranded new-turn prompt a recovery may clear.
#[test]
fn an_inject_frame_left_in_the_composer_is_neither_a_draft_nor_a_clearable_prompt() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    use crate::services::claude_tui::composer_lock::DraftSighting;
    let source = "discord:1479671298497183835:1561196979327869080";
    let framed = crate::services::tui_input::human_frame::inject(
        source,
        "343742347365974026",
        "0hbujang",
        NONCE,
        "status?",
    );
    let header = framed.lines().next().unwrap();
    let (top, rest) = header.split_at(header.find("mode=").unwrap() + 2);
    let wrapped = format!("{}\n{rest}\nstatus?", top.trim_end());
    let typed_first = format!("person typed {wrapped}");
    for shown in [&framed, &wrapped, &typed_first].map(|composer| screen(composer, "")) {
        assert_eq!(draft_sighting(&shown), DraftSighting::Unsettled, "{shown}");
        assert_eq!(composer_owner(&shown), ComposerOwner::Person, "{shown}");
    }
    let idle = screen("[User: 0hbujang (ID: 343742347365974026)] status?", "");
    assert_eq!(composer_owner(&idle), ComposerOwner::AgentDesk);
    let person = screen("[User: my own draft", "");
    assert_eq!(draft_sighting(&person), DraftSighting::PersonDraft);
}

#[test]
fn every_pre_paste_veto_leaves_the_pane_untouched() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    type Setup = fn(&Fake);
    let cases: [(&str, Setup, Veto); 10] = [
        (
            "attached",
            |f| f.answers("attach", &["1"]),
            Veto::HumanAttached,
        ),
        (
            "last attached in the current second",
            |f| {
                // One second ahead, so a clock tick during the test cannot age it out.
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                f.answers("attach", &[&format!("0,{},200,50", now + 1)])
            },
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
            "draft beside a stash",
            |f| f.caps(&[screen("half typed", &format!("{:>58}", "› stashed"))]),
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
        (
            "session gone at the paste",
            |f| f.answers("keyattach", &["gone"]),
            Veto::PaneUnavailable,
        ),
    ];
    for (name, setup, veto) in cases {
        let fake = Fake::new();
        fake.caps(&[busy_empty()]);
        setup(&fake);
        assert_eq!(fake.run(&FAST), Outcome::NotSent(veto), "{name}");
        assert_eq!(fake.keys(), (0, 0, 0), "{name}");
    }
    // A session or nonce that could leave a tmux command string is refused before any call.
    let fake = Fake::new();
    let pane = Pane::with_program("x'y", fake.dir.path().join("tmux"));
    for (session, nonce) in [("x'y", NONCE), ("plain", "ab cd")] {
        let request = Request {
            session,
            transcript: &fake.transcript,
            source: "iMessage",
            author: "200",
            name: "ann",
            nonce,
            text: "are you there?",
        };
        let outcome = inject(&pane, &request, &FAST);
        assert_eq!(outcome, Outcome::NotSent(Veto::InvalidInput), "{session}");
    }
    assert!(fs::read_to_string(fake.dir.path().join("log")).is_err());
}

#[test]
fn a_held_composer_lock_is_tried_three_times_without_any_tmux_call() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
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
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    // A person's stash beside an empty composer needs no C-s; Claude hands it back on the submit.
    let stashed = format!("{:>58}", "› stashed");
    for status in ["", stashed.as_str()] {
        let owned = text();
        let fake = Fake::new();
        let empty =
            busy_empty().replace(&format!("\n\n{BORDER}"), &format!("\n{status}\n{BORDER}"));
        fake.caps(&[empty, screen(&owned, status)]);
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

// Fake tmux subprocesses can be delayed by compiler load; use the production receipt window.
const MULTILINE_TIMING: Timing = Timing {
    confirm_window: Duration::from_secs(3),
    ..FAST
};

fn multiline_payload() -> String {
    format!(
        "Please inspect these files:\n{}\n\n  Preserve this indentation.\nReport one result per file.",
        (0..24)
            .map(|i| format!("/tmp/downloads/design-reference-{i:02}.txt"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn paste_block(inner: &str) -> String {
    format!("<pasted_content id=\"dfd9\">\n{inner}\n</pasted_content id=\"dfd9\">")
}

#[test]
fn multiline_folded_paste_confirms_the_exact_wrapped_or_bare_transcript() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let payload = multiline_payload();
    let expected = frame("iMessage", "200", "ann", NONCE, &payload);
    assert!(expected.encode_utf16().count() > 800);
    let folded = format!("[Pasted text #1 +{} lines]", expected.matches('\n').count());
    let wrapped = paste_block(&expected);
    for body in [
        expected.clone(),
        wrapped.clone(),
        format!("\n\n{wrapped}\n"),
    ] {
        for record in [
            serde_json::json!({"type": "queue-operation", "operation": "enqueue", "content": body}),
            serde_json::json!({"type": "attachment", "attachment": {"type": "queued_command", "commandMode": "prompt", "prompt": body}}),
            serde_json::json!({"type": "user", "message": {"content": body}}),
        ] {
            let fake = Fake::new();
            fake.caps(&[busy_empty(), screen(&folded, "")]);
            fs::write(
                fake.dir.path().join("on_enter"),
                real(record.clone()) + "\n",
            )
            .unwrap();
            assert_eq!(
                fake.run_text(&payload, &MULTILINE_TIMING),
                Outcome::Injected,
                "{record}"
            );
            assert_eq!(fake.keys(), (1, 1, 0), "{record}");
            assert_eq!(
                fs::read_to_string(fake.dir.path().join("buffer")).unwrap(),
                expected
            );
        }
    }
}

#[test]
fn multiline_folded_paste_keeps_foreign_input_contaminated() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let payload = multiline_payload();
    let expected = frame("iMessage", "200", "ann", NONCE, &payload);
    let folded = format!("[Pasted text #1 +{} lines]", expected.matches('\n').count());
    let wrapped = paste_block(&expected);
    for body in [
        format!("{expected}\noperator input"),
        paste_block(&format!("{expected}\noperator input")),
        format!("operator input\n{wrapped}"),
        format!("{wrapped}\noperator input"),
        wrapped.replace(
            "</pasted_content id=\"dfd9\">",
            "</pasted_content id=\"abcd\">",
        ),
        paste_block(&wrapped),
        format!("{wrapped}\n{}", paste_block("operator input")),
        format!("\n{wrapped}\n"),
        format!("\n\n\n{wrapped}\n"),
        format!("\n\n{wrapped}"),
        format!("\n\n{wrapped}\n\n"),
        format!("\n\n{expected}\n"),
        format!("\r\n\r\n{wrapped}\r\n"),
    ] {
        for body in [body.clone(), format!("\n\n{body}\n")] {
            let fake = Fake::new();
            fake.caps(&[busy_empty(), screen(&folded, "")]);
            fs::write(fake.dir.path().join("on_enter"), enqueue_record(&body)).unwrap();
            assert_eq!(
                fake.run_text(&payload, &MULTILINE_TIMING),
                Outcome::Unconfirmed(Unconfirmed::ContaminatedSubmit),
                "submitted body: {body:?}"
            );
            assert_eq!(
                fake.keys(),
                (1, 1, 0),
                "a contaminated receipt never resubmits"
            );
        }
    }
}

/// After the paste nothing is NotSent; every path ends with at most one Enter and no deletion.
#[test]
fn schedules_after_the_paste_never_fall_back_to_not_sent_or_delete() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let prefix = |n: usize| text().chars().take(n).collect::<String>();
    // (name, captures, transcript accepts on Enter, outcome, (pastes, Enter keys, other keys))
    type Case<'a> = (&'a str, Vec<String>, bool, Outcome, (usize, usize, usize));
    let cases: Vec<Case> = vec![
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
            fake.pasted_attempt_submission() == Some(true)
        ),
        (
            Outcome::Unconfirmed(Unconfirmed::NotObserved),
            (1, 1, 0),
            true
        ),
        "late transcript belongs to the pasted attempt and frame hash"
    );
    // Production window: 3s window < record at 3.2s <= reply at 3.3s < 5s tmux call budget.
    // The Enter takes effect at once; the window must not start from the late reply.
    let production = Timing {
        confirm_window: TIMING.confirm_window,
        confirm_poll: TIMING.confirm_poll,
        ..FAST
    };
    let fake = Fake::new();
    fake.caps(&[busy_empty(), screen(&text(), "")]);
    fake.accept_on_enter_after("3.2");
    fake.reply_after_record("0.1");
    let outcome = fake.run(&production);
    assert_eq!(
        (
            outcome,
            fake.keys(),
            fake.pasted_attempt_submission() == Some(true)
        ),
        (
            Outcome::Unconfirmed(Unconfirmed::NotObserved),
            (1, 1, 0),
            true
        ),
        "slow Enter reply retains the pasted attempt receipt"
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
    let _runtime = crate::config::TestRuntimeRootGuard::new();
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

#[test]
fn identical_plain_frames_need_their_own_fresh_enqueue_and_not_a_late_command_or_user() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native.jsonl");
    fs::write(&path, "").unwrap();
    let session = format!("receipt-{}", uuid::Uuid::new_v4());
    let framed = frame("discord", "720102", "ann", "ca720104", "same words");
    let register = crate::services::tui_input::human_frame::history::register;
    register(
        ("claude", &session, "ca720104", "720102"),
        &path,
        0,
        &framed,
    )
    .unwrap();
    let append = |record: serde_json::Value| {
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{record}").unwrap();
    };
    append(
        serde_json::json!({"type":"queue-operation", "operation":"enqueue", "uuid":"enqueue-a", "content":framed}),
    );
    assert_eq!(
        transcript_submission(&path, 0, "ca720104", Some(&framed)),
        Some(true)
    );
    let second = fs::metadata(&path).unwrap().len();
    register(
        ("claude", &session, "ca720105", "720102"),
        &path,
        second,
        &framed,
    )
    .unwrap();
    append(
        serde_json::json!({"type":"attachment", "attachment":{"type":"queued_command", "commandMode":"prompt", "source_uuid":"command-a", "prompt":framed}}),
    );
    append(serde_json::json!({"type":"user", "uuid":"late-user-a", "message":{"content":framed}}));
    assert_eq!(
        transcript_submission(&path, second, "ca720105", Some(&framed)),
        None
    );
    append(
        serde_json::json!({"type":"queue-operation", "operation":"enqueue", "uuid":"enqueue-b", "content":framed}),
    );
    assert_eq!(
        transcript_submission(&path, second, "ca720105", Some(&framed)),
        Some(true)
    );
}
