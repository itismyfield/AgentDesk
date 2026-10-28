use std::cell::Cell;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;

#[path = "expanded_contract_tests.rs"]
mod expanded_contract_tests;

/// The model turn the captured `model_turn.jsonl` keeps open until its line 19.
const T: &str = "01a11d88-ef31-7860-9d48-88926c4b1809";
/// The request whose frame `busy_frame_v1.ansi`, the captured `busy_frame.ansi` with the v1 header
/// in place of the older one, shows in the composer.
const SOURCE: &str = "discord";
const AUTHOR: &str = "200";
const NAME: &str = "fixture";
const NONCE: &str = "c0ffee01";
const BODY: &str = "fixture body line one\nsecond line of the injected text";

const FAST: Timing = Timing {
    lock_retries: &[Duration::ZERO, Duration::from_millis(5)],
    settle: Duration::ZERO,
    confirm_window: Duration::from_millis(300),
    confirm_poll: Duration::from_millis(20),
};

fn fixture(name: &str) -> String {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_busy_inject");
    fs::read_to_string(dir.join(name)).unwrap()
}

/// The first `count` records of a captured rollout slice.
fn records(name: &str, count: usize) -> String {
    let text = fixture(name);
    text.lines()
        .take(count)
        .map(|line| format!("{line}\n"))
        .collect()
}

fn started(turn: &str, root: &str) -> String {
    let record = serde_json::json!({"type": "event_msg",
        "payload": {"type": "task_started", "turn_id": turn, "root_turn_id": root}});
    format!("{record}\n")
}

fn context(turn: &str, sandbox: &str) -> String {
    let record = serde_json::json!({"type": "turn_context", "payload": {"turn_id": turn,
        "root_turn_id": turn, "approval_policy": "never", "sandbox_policy": {"type": sandbox}}});
    format!("{record}\n")
}

fn item(turn: &str, kind: &str, text: &str) -> String {
    let record = serde_json::json!({"type": "event_msg", "payload": {"type": "item_completed",
        "turn_id": turn, "item": {"type": kind, "id": "i1",
        "content": [{"type": "text", "text": text, "text_elements": []}]}}});
    format!("{record}\n")
}

fn ended(kind: &str, turn: Option<&str>) -> String {
    let record =
        serde_json::json!({"type": "event_msg", "payload": {"type": kind, "turn_id": turn}});
    format!("{record}\n")
}

/// A model turn that Enter steers: started, its context, its first user message.
fn model(turn: &str) -> String {
    started(turn, turn) + &context(turn, "danger-full-access") + &item(turn, "UserMessage", "go")
}

fn verdict(rollout: &str) -> TurnVerdict {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    fs::write(&path, rollout).unwrap();
    read_turn(&path).unwrap()
}

fn steerable(turn: &str) -> TurnVerdict {
    TurnVerdict::KnownModelTurn(turn.to_string())
}

fn non_steerable(turn: &str) -> TurnVerdict {
    TurnVerdict::NonSteerable(turn.to_string())
}

/// Captured model, shell, compact and review turns read the way Enter met them in the cell:
/// only the model turn steers, and each turn reads as ended once its own end is recorded.
#[test]
fn captured_turns_read_as_enter_met_them() {
    assert_eq!(verdict(&records("model_turn.jsonl", 11)), steerable(T));
    assert_eq!(
        verdict(&records("model_turn.jsonl", 20)),
        TurnVerdict::NotBusy
    );
    let shell = "01a11d87-fbcf-7501-9a16-4da2e35d3857";
    assert_eq!(
        verdict(&records("shell_turn.jsonl", 2)),
        non_steerable(shell)
    );
    assert_eq!(
        verdict(&records("shell_turn.jsonl", 3)),
        TurnVerdict::NotBusy
    );
    let compact = "01a11dcc-3da7-7a23-9f47-5067834fa221";
    assert_eq!(
        verdict(&records("compact_turn.jsonl", 6)),
        steerable(compact)
    );
    // The review's inner turn names the outer one as its root; the outer end closes both.
    let inner = "01a11dcd-2d80-7d51-9f27-c599b4df17e6";
    assert_eq!(
        verdict(&records("review_turn.jsonl", 3)),
        non_steerable(inner)
    );
    assert_eq!(
        verdict(&records("review_turn.jsonl", 10)),
        TurnVerdict::NotBusy
    );
    assert_eq!(verdict(""), TurnVerdict::NotBusy);
}

/// A model turn needs its own context and user message, a root turn, no review and full access.
#[test]
fn a_model_turn_needs_its_own_context_and_user_message() {
    let user = |turn| item(turn, "UserMessage", "go");
    let full = |turn| context(turn, "danger-full-access");
    assert_eq!(verdict(&model("t1")), steerable("t1"));
    assert_eq!(
        verdict(&(started("t1", "t1") + &user("t1"))),
        non_steerable("t1")
    );
    assert_eq!(
        verdict(&(started("t1", "t1") + &full("t1"))),
        TurnVerdict::Unknown
    );
    // The previous turn's context or user message never counts for the next turn.
    let earlier_context = started("t0", "t0") + &full("t0") + &started("t1", "t1") + &user("t1");
    assert_eq!(verdict(&earlier_context), non_steerable("t1"));
    let earlier_user = started("t0", "t0") + &user("t0") + &started("t1", "t1") + &full("t1");
    assert_eq!(verdict(&earlier_user), TurnVerdict::Unknown);
    let sub_turn = started("s1", "r1") + &full("s1") + &user("s1");
    assert_eq!(verdict(&sub_turn), non_steerable("s1"));
    let in_review = item("r1", "EnteredReviewMode", "") + &model("t1");
    assert_eq!(verdict(&in_review), non_steerable("t1"));
    let read_only = started("t1", "t1") + &context("t1", "read-only") + &user("t1");
    assert_eq!(verdict(&read_only), non_steerable("t1"));
    assert_eq!(
        verdict(&(model("t1") + &ended("turn_aborted", Some("t1")))),
        TurnVerdict::NotBusy
    );
    assert_eq!(
        verdict(&(model("t1") + &ended("task_complete", Some("t0")))),
        steerable("t1")
    );
}

#[test]
fn a_model_turn_with_completed_compaction_still_injects() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let rollout = model(T) + &item(T, "ContextCompaction", "");
    assert_eq!(verdict(&rollout), steerable(T));
    let fake = Fake::new();
    fs::write(&fake.rollout, rollout).unwrap();
    assert_eq!(fake.run(&FakeLedger::default()), Outcome::Injected);
    assert_eq!((fake.calls("paste-buffer"), fake.enters()), (1, 1));
}

#[test]
fn a_model_turn_with_in_progress_compaction_still_injects() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let compaction = serde_json::json!({"type": "event_msg", "payload": {
        "type": "item_started", "turn_id": T,
        "item": {"type": "ContextCompaction", "id": "compacting"}}});
    let rollout = model(T) + &format!("{compaction}\n");
    assert_eq!(verdict(&rollout), steerable(T));
    let fake = Fake::new();
    fs::write(&fake.rollout, rollout).unwrap();
    let shown = format!("• Compacting context\n{}", fixture("busy_empty.ansi"));
    fake.answers("cap", &[&shown]);
    assert_eq!(fake.run(&FakeLedger::default()), Outcome::Injected);
    assert_eq!((fake.calls("paste-buffer"), fake.enters()), (1, 1));
}

#[test]
fn compaction_without_a_user_message_accepts_native_queue_admission() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let rollout =
        started(T, T) + &context(T, "danger-full-access") + &item(T, "ContextCompaction", "");
    assert_eq!(verdict(&rollout), steerable(T));
    let fake = Fake::new();
    fs::write(&fake.rollout, rollout).unwrap();
    assert_eq!(fake.run(&FakeLedger::default()), Outcome::Injected);
    assert_eq!((fake.calls("paste-buffer"), fake.enters()), (1, 1));
}

/// What the reader cannot prove reads as Unknown: an unparsed or unnamed record after the start,
/// or a start that lies before the tail window.
#[test]
fn unreadable_tails_read_as_unknown() {
    assert_eq!(
        verdict(&(model("t1") + "{not json\n")),
        TurnVerdict::Unknown
    );
    assert_eq!(
        verdict(&(model("t1") + &ended("task_complete", None))),
        TurnVerdict::Unknown
    );
    // A record still being written is not judged.
    assert_eq!(verdict(&(model("t1") + "{\"type\":")), steerable("t1"));
    let padding = item("t1", "AgentMessage", &"x".repeat(1024));
    let long = model("t1") + &padding.repeat((4 * 1024 * 1024) / padding.len() + 2);
    assert_eq!(verdict(&long), steerable("t1"));
}

/// The confirm scan finds the nonce only in a complete user record after the offset, and names
/// the turn that record belongs to.
#[test]
fn the_nonce_is_confirmed_only_by_a_later_user_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    let before = records("model_turn.jsonl", 11);
    fs::write(&path, records("model_turn.jsonl", 20)).unwrap();
    let offset = before.len() as u64;
    let nonce = "845246b2";
    assert_eq!(
        rollout::submitted(&path, offset, nonce),
        Some(Some(T.into()))
    );
    let after = records("model_turn.jsonl", 13).len() as u64;
    assert_eq!(rollout::submitted(&path, after, nonce), None);
    // An answer that quotes the frame is not the input.
    let quoted = serde_json::json!({"type": "response_item", "payload": {"type": "message",
        "role": "assistant", "content": [{"type": "output_text", "text": "[x · y · feedf00d]"}]}});
    fs::write(
        &path,
        format!("{quoted}\n") + &item("t2", "UserMessage", "[x · y · feedf00d]"),
    )
    .unwrap();
    assert_eq!(
        rollout::submitted(&path, 0, "feedf00d"),
        Some(Some("t2".into()))
    );
    let unfinished = item("t2", "UserMessage", "[x · y · feedf00d]");
    fs::write(&path, unfinished.trim_end()).unwrap();
    assert_eq!(rollout::submitted(&path, 0, "feedf00d"), None);
}

fn frame_text() -> String {
    frame(SOURCE, AUTHOR, NAME, NONCE, BODY)
}

/// Captured screens read as the cell showed them: the composer's state, Codex's own queue, and
/// the overlay and review screens that hide the composer.
#[test]
fn captured_screens_read_as_shown() {
    let shown = screen::read(&fixture("busy_empty.ansi"));
    assert_eq!(shown.composer, screen::Composer::Empty);
    assert!(!shown.modal && !shown.queued);
    let shown = screen::read(&fixture("busy_draft.ansi"));
    assert_eq!(
        shown.composer,
        screen::Composer::Text("person draft text".into())
    );
    let shown = screen::read(&fixture("busy_frame_v1.ansi"));
    assert_eq!(
        shown.composer,
        screen::Composer::Text(crate::services::tui_input::human_frame::inject(
            SOURCE, AUTHOR, NAME, NONCE, BODY
        ))
    );
    let legacy = crate::services::tui_input::human_frame::legacy_inject(SOURCE, NAME, NONCE, BODY);
    let shown = screen::read(&fixture("busy_frame.ansi"));
    assert_eq!(shown.composer, screen::Composer::Text(legacy));
    assert!(screen::read(&fixture("busy_queued.ansi")).queued);
    assert!(screen::read(&fixture("compact_queued.txt")).queued);
    for hidden in ["overlay.ansi", "review_picker.ansi", "review_custom.txt"] {
        let shown = screen::read(&fixture(hidden));
        assert!(shown.modal, "{hidden}");
        assert_eq!(shown.composer, screen::Composer::Unread, "{hidden}");
    }
    // A colour argument of 2 is not dim, so coloured text is a draft.
    let status = "  GPT-6-Luna xhigh · weekly 76% left";
    let coloured = format!("\n\x1b[1m›\x1b[0m \x1b[38;2;2;2;2mtyped\x1b[0m\n\n{status}\n");
    let shown = screen::read(&coloured);
    assert_eq!(shown.composer, screen::Composer::Text("typed".into()));
}

/// Scripted tmux records actual writes and can lose the pane or fail each command.
const FAKE_TMUX: &str = r#"#!/bin/sh
d='@D@'
next() { n=$(cat "$d/$1.n" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$d/$1.n"; f="$d/$1.$n"; [ -f "$f" ] || f="$d/$1"; cat "$f"; }
echo "$*" >> "$d/log"
[ -f "$d/fail.$2" ] && exit 1
case "$2" in
display-message) [ -f "$d/gone" ] && exit 1; if [ -f "$d/loaded" ] && [ -f "$d/replace_pane" ]; then echo pane-other; else echo pane-7504; fi ;;
capture-pane)
  [ -f "$d/pasted" ] && [ -f "$d/fail_post_capture" ] && exit 1
  next cap ;;
load-buffer)
  for last do :; done; cp "$last" "$d/buffer"
  touch "$d/loaded"
  [ -f "$d/on_load" ] && cat "$d/on_load" >> '@T@'
  [ -f "$d/gone_after_load" ] && touch "$d/gone" ;;
paste-buffer)
  [ -f "$d/gone" ] && exit 1
  touch "$d/pasted"
  [ -f "$d/gone_after_paste" ] && touch "$d/gone" ;;
send-keys)
  [ -f "$d/gone" ] && exit 1
  [ -f "$d/on_enter" ] && cat "$d/on_enter" >> '@T@'
  [ -f "$d/remove_rollout" ] && rm '@T@' ;;
esac
[ -f "$d/fail_after.$2" ] && exit 1
exit 0
"#;

#[derive(Default)]
struct FakeLedger {
    refuse: bool,
    registered: Cell<usize>,
    withdrawn: Cell<usize>,
}

impl Ledger for FakeLedger {
    fn register(&self) -> bool {
        self.registered.set(self.registered.get() + 1);
        !self.refuse
    }

    fn withdraw(&self) {
        self.withdrawn.set(self.withdrawn.get() + 1);
    }
}

struct Fake {
    dir: tempfile::TempDir,
    rollout: PathBuf,
}

impl Fake {
    /// An unattended 80x24 pane over an open model turn T, an empty composer before the paste and
    /// the captured frame after it.
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/codex-busy-inject-tmp");
        fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        fs::write(&rollout, records("model_turn.jsonl", 11)).unwrap();
        let script = FAKE_TMUX
            .replace("@D@", &dir.path().display().to_string())
            .replace("@T@", &rollout.display().to_string());
        let program = dir.path().join("tmux");
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let fake = Self { dir, rollout };
        fake.answers("attach", &["0,,80,24"]);
        fake.caps(&["busy_empty.ansi", "busy_frame_v1.ansi"]);
        fake
    }

    /// Answers in call order; the last one repeats.
    fn answers(&self, name: &str, values: &[&str]) {
        for (i, value) in values.iter().enumerate() {
            fs::write(self.dir.path().join(format!("{name}.{}", i + 1)), value).unwrap();
        }
        fs::write(self.dir.path().join(name), values.last().unwrap()).unwrap();
    }

    fn caps(&self, names: &[&str]) {
        let shown: Vec<String> = names
            .iter()
            .map(|name| {
                let shown = fixture(name);
                if *name == "busy_frame_v1.ansi" {
                    let old = crate::services::tui_input::human_frame::inject(
                        SOURCE, AUTHOR, NAME, NONCE, "",
                    );
                    let new = crate::services::tui_input::human_frame::idle(NAME, AUTHOR, "");
                    shown.replace(old.lines().next().unwrap(), &new)
                } else {
                    shown
                }
            })
            .collect();
        let refs: Vec<&str> = shown.iter().map(String::as_str).collect();
        self.answers("cap", &refs);
    }

    fn flag(&self, name: &str) {
        fs::write(self.dir.path().join(name), "").unwrap();
    }

    /// Enter makes Codex record the frame as user input of `turn`.
    fn accept_into(&self, turn: &str) {
        fs::write(
            self.dir.path().join("on_enter"),
            format!("{}\n{}\n",
                serde_json::json!({"type":"response_item", "payload": {
                    "type":"message", "role":"user", "content":[{"type":"input_text", "text": frame_text()}],
                    "internal_chat_message_metadata_passthrough":{"turn_id":turn}}}),
                serde_json::json!({"type":"event_msg", "payload": {
                    "type":"item_completed", "turn_id":turn, "item": {
                        "type":"UserMessage", "id":"current-frame", "content":[{"type":"text", "text":frame_text()}]}}})),
        )
        .unwrap();
    }

    fn run(&self, ledger: &FakeLedger) -> Outcome {
        self.run_with(ledger, BODY)
    }

    fn run_with(&self, ledger: &FakeLedger, text: &str) -> Outcome {
        self.run_with_nonce(ledger, text, NONCE)
    }

    fn run_with_nonce(&self, ledger: &FakeLedger, text: &str, nonce: &str) -> Outcome {
        let name = self.dir.path().file_name().unwrap().to_string_lossy();
        let name: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let session = format!("codex-busy-{name}");
        let pane = Pane::with_program(&session, self.dir.path().join("tmux"));
        let request = Request {
            session: &session,
            rollout: &self.rollout,
            source: SOURCE,
            author: AUTHOR,
            name: NAME,
            nonce,
            text,
            target_turn: T,
        };
        inject(&pane, &request, ledger, &FAST)
    }

    /// Subcommands the server ran, guarded ones included.
    fn calls(&self, subcommand: &str) -> usize {
        let log = fs::read_to_string(self.dir.path().join("log")).unwrap_or_default();
        log.lines()
            .filter(|line| line.split_whitespace().nth(1) == Some(subcommand))
            .count()
    }

    fn enters(&self) -> usize {
        let log = fs::read_to_string(self.dir.path().join("log")).unwrap_or_default();
        log.lines()
            .filter(|line| line.starts_with("-u send-keys") && line.ends_with("Enter"))
            .count()
    }
}

/// Local success does not claim a native turn; the receipt observer identifies it later.
#[test]
fn one_enter_completes_and_the_observer_names_the_turn() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for turn in [T, "01b-next-turn"] {
        let fake = Fake::new();
        fake.accept_into(turn);
        let ledger = FakeLedger::default();
        assert_eq!(fake.run(&ledger), Outcome::Injected);
        assert_eq!((fake.calls("paste-buffer"), fake.enters()), (1, 1));
        assert_eq!((ledger.registered.get(), ledger.withdrawn.get()), (1, 0));
        wait_receipt(&fake);
        assert_eq!(
            rollout::submitted(&fake.rollout, 0, NONCE),
            Some(Some(turn.into()))
        );
    }
}

fn wait_receipt(fake: &Fake) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while crate::services::tui_input::human_frame::history::receipt_row(NONCE, &fake.rollout)
        .is_none()
    {
        assert!(
            Instant::now() < deadline,
            "diagnostic observer missed the receipt"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Missing receipt evidence cannot fail local submission; a failed Enter is never repeated.
#[test]
fn missing_receipt_succeeds_and_enter_is_never_repeated() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let fake = Fake::new();
    let ledger = FakeLedger::default();
    assert_eq!(fake.run(&ledger), Outcome::Injected);
    assert_eq!((fake.enters(), ledger.withdrawn.get()), (1, 0));
    let fake = Fake::new();
    fake.accept_into(T);
    fake.flag("fail_after.send-keys");
    assert_eq!(
        fake.run(&FakeLedger::default()),
        Outcome::Unconfirmed(Unconfirmed::EnterFailed)
    );
    assert_eq!(fake.enters(), 1);
}

/// Screens a paste would land wrong on are refused before any buffer, paste or Enter, and the
/// ledger never hears of them.
#[test]
fn draft_queue_and_hidden_composer_are_refused_before_the_paste() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let cases = [
        ("busy_draft.ansi", Veto::DraftNavigationUnproven),
        ("overlay.ansi", Veto::Modal),
        ("review_picker.ansi", Veto::Modal),
    ];
    for (shown, veto) in cases {
        let fake = Fake::new();
        fake.caps(&[shown]);
        let ledger = FakeLedger::default();
        assert_eq!(fake.run(&ledger), Outcome::NotSent(veto), "{shown}");
        let writes = fake.calls("load-buffer") + fake.calls("paste-buffer") + fake.enters();
        assert_eq!((writes, ledger.registered.get()), (0, 0), "{shown}");
    }
}

/// The rollout verdict is read under the lock, after the screen: a turn that ended, cannot steer
/// or is not the caller's target refuses the paste.
#[test]
fn the_turn_is_rechecked_before_the_paste() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let cases = [
        (records("model_turn.jsonl", 20), Veto::NotBusy),
        (records("shell_turn.jsonl", 2), Veto::NotSteerable),
        (model("t-other"), Veto::TurnChanged),
        (
            started(T, T) + &context(T, "danger-full-access"),
            Veto::TurnUnknown,
        ),
    ];
    for (rollout, veto) in cases {
        let fake = Fake::new();
        fs::write(&fake.rollout, rollout).unwrap();
        let ledger = FakeLedger::default();
        assert_eq!(fake.run(&ledger), Outcome::NotSent(veto), "{veto:?}");
        assert_eq!(
            (fake.calls("paste-buffer"), ledger.registered.get()),
            (0, 0)
        );
    }
}

/// Folded or oversized composer renders and unknown geometry still get exactly one Enter.
#[test]
fn unpredictable_renders_still_send_once() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let rows = (0..14)
        .map(|row| format!("row {row}"))
        .collect::<Vec<_>>()
        .join("\n");
    for body in [rows, "y".repeat(1000)] {
        let fake = Fake::new();
        fake.answers("attach", &["0,,"]);
        assert_eq!(
            fake.run_with(&FakeLedger::default(), &body),
            Outcome::Injected
        );
        assert_eq!((fake.calls("paste-buffer"), fake.enters()), (1, 1));
        assert_eq!(fake.calls("capture-pane"), 1);
    }
}

/// A lost pane withdraws an unwritten reservation; an attempted paste retains its evidence.
#[test]
fn the_ledger_brackets_the_paste() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let fake = Fake::new();
    let full = FakeLedger {
        refuse: true,
        ..FakeLedger::default()
    };
    assert_eq!(fake.run(&full), Outcome::NotSent(Veto::LedgerFull));
    assert_eq!(
        (fake.calls("paste-buffer"), fake.calls("delete-buffer")),
        (0, 1)
    );
    let fake = Fake::new();
    fake.flag("gone_after_load");
    let ledger = FakeLedger::default();
    assert_eq!(fake.run(&ledger), Outcome::NotSent(Veto::PaneUnavailable));
    assert_eq!((ledger.registered.get(), ledger.withdrawn.get()), (1, 1));
    let fake = Fake::new();
    fake.flag("fail.paste-buffer");
    let ledger = FakeLedger::default();
    assert_eq!(
        fake.run(&ledger),
        Outcome::Unconfirmed(Unconfirmed::PasteFailed)
    );
    assert_eq!((ledger.withdrawn.get(), fake.enters()), (0, 0));
}

/// Attach changes and foreign post-paste text never introduce another UI check.
#[test]
fn attached_people_and_post_paste_text_do_not_withhold_enter() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for attach in ["1,,80,24", "0,,80,24"] {
        let fake = Fake::new();
        fake.answers("attach", &[attach, "1,1791496900,12,10"]);
        fake.caps(&["busy_empty.ansi", "busy_draft.ansi"]);
        assert_eq!(fake.run(&FakeLedger::default()), Outcome::Injected);
        assert_eq!((fake.calls("paste-buffer"), fake.enters()), (1, 1));
        assert_eq!(fake.calls("capture-pane"), 1);
        let log = fs::read_to_string(fake.dir.path().join("log")).unwrap();
        assert!(!log.contains("session_attached") && !log.contains("pane_width"));
    }
}

/// The capture with a remote image row drawn above its textarea, as Codex shows a restored draft.
fn with_image(capture: &str) -> String {
    let mut rows: Vec<&str> = capture.lines().collect();
    let plain = crate::services::codex_tui::input::strip_ansi_escape_sequences;
    let prompt = rows.iter().rposition(|row| plain(row).starts_with('›'));
    rows.splice(prompt.unwrap()..prompt.unwrap(), ["  [Image #1]", ""]);
    rows.join("\n")
}

/// A visible image draft is refused before the paste; later screen changes are not inspected.
#[test]
fn an_image_draft_is_refused_only_before_the_paste() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let shown = screen::read(&with_image(&fixture("busy_empty.ansi")));
    assert_eq!(
        (shown.composer, shown.attachments),
        (screen::Composer::Empty, true)
    );
    // Only the rows next to the composer count: an image named further up is transcript, while
    // an adjacent row naming one in another shape is not read at all.
    let far = fixture("busy_empty.ansi").replacen("Working", "[Image #1] Working", 1);
    assert_eq!(screen::read(&far).composer, screen::Composer::Empty);
    let odd = with_image(&fixture("busy_empty.ansi")).replace("[Image #1]", "[Image #1] (x)");
    assert_eq!(screen::read(&odd).composer, screen::Composer::Unread);
    let fake = Fake::new();
    fake.answers("cap", &[&with_image(&fixture("busy_empty.ansi"))]);
    let ledger = FakeLedger::default();
    assert_eq!(
        fake.run(&ledger),
        Outcome::NotSent(Veto::DraftImagePositionUnproven)
    );
    let writes = fake.calls("load-buffer") + fake.calls("paste-buffer") + fake.enters();
    assert_eq!((writes, ledger.registered.get()), (0, 0));
    let fake = Fake::new();
    let after = with_image(&fixture("busy_frame_v1.ansi"));
    fake.answers("cap", &[&fixture("busy_empty.ansi"), &after]);
    let outcome = fake.run(&FakeLedger::default());
    assert_eq!(outcome, Outcome::Injected);
    assert_eq!((fake.calls("paste-buffer"), fake.enters()), (1, 1));
    assert_eq!(fake.calls("capture-pane"), 1);
}

/// Blank lines and tall pasted text need no post-paste rendering proof.
#[test]
fn bodies_the_reader_cannot_prove_are_submitted_once() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let tall = (0..40)
        .map(|row| format!("row {row}"))
        .collect::<Vec<_>>()
        .join("\n");
    for body in [
        "first\n\nsecond".into(),
        "first\n   \nsecond".into(),
        "first\n".into(),
        tall,
    ] {
        let fake = Fake::new();
        let ledger = FakeLedger::default();
        assert_eq!(fake.run_with(&ledger, &body), Outcome::Injected, "{body:?}");
        assert_eq!((fake.calls("paste-buffer"), fake.enters()), (1, 1));
        assert_eq!(ledger.registered.get(), 1);
    }
}

/// A start without a usable root, a review entry without a name, or a review exit that is not the
/// open review's own never reads as a steerable model turn; the open review's exit still does.
#[test]
fn unproven_roots_and_reviews_never_read_as_steerable() {
    let raw = |payload: serde_json::Value| {
        format!(
            "{}\n",
            serde_json::json!({"type": "event_msg", "payload": payload})
        )
    };
    let rest = context("t1", "danger-full-access") + &item("t1", "UserMessage", "go");
    for root in [
        serde_json::Value::Null,
        serde_json::json!(7),
        serde_json::json!(" "),
    ] {
        let start =
            raw(serde_json::json!({"type": "task_started", "turn_id": "t1", "root_turn_id": root}));
        assert_eq!(verdict(&(start + &rest)), TurnVerdict::Unknown, "{root}");
    }
    let rootless = raw(serde_json::json!({"type": "task_started", "turn_id": "t1"}));
    assert_eq!(verdict(&(rootless + &rest)), TurnVerdict::Unknown);
    let unnamed = raw(serde_json::json!({"type": "item_completed",
        "item": {"type": "EnteredReviewMode", "id": "i1"}}));
    assert_eq!(verdict(&(unnamed + &model("t1"))), TurnVerdict::Unknown);
    let entered = item("r1", "EnteredReviewMode", "");
    let foreign = entered.clone() + &item("q9", "ExitedReviewMode", "") + &model("t1");
    assert_eq!(verdict(&foreign), non_steerable("t1"));
    let own = entered + &item("r1", "ExitedReviewMode", "") + &model("t1");
    assert_eq!(verdict(&own), steerable("t1"));
}

#[test]
fn plain_receipts_do_not_use_a_late_completed_alias_for_the_second_attempt() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native.jsonl");
    fs::write(&path, "").unwrap();
    let framed = frame("discord", "720103", "ann", "ca720106", "same words");
    let session = format!("canon-{}", uuid::Uuid::new_v4());
    let register = crate::services::tui_input::human_frame::history::register;
    register(("codex", &session, "ca720106", "720103"), &path, 0, &framed).unwrap();
    let response = |id: &str| serde_json::json!({"type":"response_item", "payload":{"type":"message", "role":"user", "id":id, "content":[{"type":"input_text", "text":framed}], "internal_chat_message_metadata_passthrough":{"turn_id":"target"}}});
    let completed = |id: &str| serde_json::json!({"type":"event_msg", "payload":{"type":"item_completed", "turn_id":"target", "item":{"type":"UserMessage", "id":id, "content":[{"type":"text", "text":framed}]}}});
    let legacy_frame = |nonce| {
        crate::services::tui_input::human_frame::inject(
            "discord",
            "720103",
            "ann",
            nonce,
            "same words",
        )
    };
    let mut legacy = response("legacy-row");
    legacy["payload"]["content"][0]["text"] = serde_json::json!(legacy_frame("ca720106"));
    fs::write(&path, format!("{legacy}\n")).unwrap();
    assert_eq!(
        rollout::submitted(&path, 0, "ca720106"),
        None,
        "a current plain attempt cannot be confirmed by its nonce inside an old frame"
    );
    let mut unregistered = legacy;
    unregistered["payload"]["content"][0]["text"] = serde_json::json!(legacy_frame("ca720110"));
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(file, "{unregistered}").unwrap();
    assert_eq!(
        rollout::submitted(&path, 0, "ca720110"),
        Some(Some("target".into())),
        "an unregistered historical attempt keeps the legacy reader"
    );
    let mut opening = response("unused-response-id");
    opening["payload"].as_object_mut().unwrap().remove("id");
    writeln!(file, "{opening}").unwrap();
    assert_eq!(rollout::submitted(&path, 0, "ca720106"), None);
    writeln!(file, "{}", completed("row-a")).unwrap();
    drop(file);
    assert_eq!(
        rollout::submitted(&path, 0, "ca720106"),
        Some(Some("target".into()))
    );
    let second = fs::metadata(&path).unwrap().len();
    register(
        ("codex", &session, "ca720107", "720103"),
        &path,
        second,
        &framed,
    )
    .unwrap();
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(file, "{}", completed("row-a")).unwrap();
    assert_eq!(
        rollout::submitted(&path, second, "ca720107"),
        None,
        "a late completion keeps the first attempt's native identity"
    );
    for has_id in [false, true] {
        let mut alias = response("response-alias");
        if !has_id {
            alias["payload"].as_object_mut().unwrap().remove("id");
        }
        writeln!(file, "{alias}").unwrap();
        assert_eq!(rollout::submitted(&path, second, "ca720107"), None);
    }
    let mut anonymous = completed("row-anonymous");
    anonymous["payload"]["item"]
        .as_object_mut()
        .unwrap()
        .remove("id");
    writeln!(file, "{anonymous}").unwrap();
    assert_eq!(rollout::submitted(&path, second, "ca720107"), None);
    for extra in [
        serde_json::json!({"type":"input_image", "image_url":"unexpected.png"}),
        serde_json::json!({"type":"tool_result", "text":""}),
        serde_json::json!({"type":"input_text", "text":null}),
    ] {
        let mut mixed = completed("row-mixed");
        mixed["payload"]["item"]["content"]
            .as_array_mut()
            .unwrap()
            .push(extra);
        writeln!(file, "{mixed}").unwrap();
        assert_eq!(rollout::submitted(&path, second, "ca720107"), None);
    }
    writeln!(file, "{opening}").unwrap();
    writeln!(file, "{}", completed("row-b")).unwrap();
    assert_eq!(
        rollout::submitted(&path, second, "ca720107"),
        Some(Some("target".into()))
    );
}

#[test]
fn common_glyphs_submit_without_a_width_allowlist() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let payload = "한글 English 123 “둥근” ‘따옴표’ … — (괄호) 😀 🚀 👍🏻 👨‍👩‍👧‍👦 ❤️ 🇰🇷 漢字 かな カナ （全角）、。！？".repeat(3);
    let expected = frame(SOURCE, AUTHOR, NAME, NONCE, &payload);
    let shown = format!(
        "\n› {}\n\n  90% context left\n",
        expected.replace('\n', "\n  ")
    );
    let fake = Fake::new();
    fake.answers("attach", &["0,,80,50"]);
    fake.answers("cap", &[&fixture("busy_empty.ansi"), &shown]);
    fake.accept_into(T);
    let on_enter = fake.dir.path().join("on_enter");
    let record = fs::read_to_string(&on_enter).unwrap().replace(
        &serde_json::to_string(&frame_text()).unwrap(),
        &serde_json::to_string(&expected).unwrap(),
    );
    fs::write(on_enter, record).unwrap();
    assert_eq!(
        fake.run_with(&FakeLedger::default(), &payload),
        Outcome::Injected
    );
    assert_eq!((fake.calls("paste-buffer"), fake.enters()), (1, 1));
}

/// A clipped composer falls back; readable empty composers still submit without predicting wraps.
#[test]
fn a_plan_unread_composers_fall_back_and_known_empty_composers_submit() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for (size, before) in [
        ("0,,12,10", fixture("busy_empty.ansi")),
        ("0,,46,12", "\n› \n\n  GPT-6-Luna xh".into()),
        ("0,,", fixture("busy_empty.ansi")),
    ] {
        let fake = Fake::new();
        fake.answers("attach", &[size]);
        fake.answers("cap", &[&before, "unread post-paste screen"]);
        let outcome = fake.run(&FakeLedger::default());
        if before == "\n› \n\n  GPT-6-Luna xh" {
            assert_eq!(outcome, Outcome::NotSent(Veto::ComposerUnread));
            assert_eq!((fake.calls("paste-buffer"), fake.enters()), (0, 0));
        } else {
            assert_eq!(outcome, Outcome::Injected);
            assert_eq!((fake.calls("paste-buffer"), fake.enters()), (1, 1));
        }
        assert_eq!(fake.calls("capture-pane"), 1);
    }
}

/// An unread next snapshot falls back and releases the lock for a later readable composer.
#[test]
fn a_plan_unread_next_snapshot_falls_back_and_recovers_without_retyping() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let fake = Fake::new();
    fake.flag("fail_post_capture");
    let ledger = FakeLedger::default();
    let first = fake.run(&ledger);
    assert_eq!(fake.enters(), 1, "{first:?}");
    assert!(matches!(first, Outcome::Injected));
    let next = fake.run_with_nonce(&ledger, "next human message", "c0ffee02");
    assert_eq!(next, Outcome::NotSent(Veto::ComposerUnread));
    assert_eq!((fake.calls("paste-buffer"), fake.enters()), (1, 1));
    fs::remove_file(fake.dir.path().join("fail_post_capture")).unwrap();
    fake.caps(&["busy_empty.ansi", "busy_empty.ansi", "busy_empty.ansi"]);
    assert_eq!(
        fake.run_with_nonce(&ledger, "new message after recovery", "c0ffee03"),
        Outcome::Injected
    );
    assert_eq!((fake.calls("paste-buffer"), fake.enters()), (2, 2));
    assert_eq!(fake.calls("capture-pane"), 3);
}

/// Every menu fixture is refused before buffer loading or input registration.
#[test]
fn a_plan_menus_are_not_sent() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for screen in ["overlay.ansi", "review_picker.ansi", "review_custom.txt"] {
        let fake = Fake::new();
        fake.caps(&[screen]);
        let ledger = FakeLedger::default();
        assert_eq!(fake.run(&ledger), Outcome::NotSent(Veto::Modal), "{screen}");
        assert_eq!(
            (
                fake.calls("load-buffer"),
                fake.calls("paste-buffer"),
                fake.enters()
            ),
            (0, 0, 0)
        );
        assert_eq!(ledger.registered.get(), 0);
    }
}

/// Transport failures, including lost acknowledgements, never become local success.
#[test]
fn pane_loss_and_write_failures_are_never_success() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for (fault, expected, pasted, entered) in [
        (
            "fail.display-message",
            Outcome::NotSent(Veto::PaneUnavailable),
            0,
            0,
        ),
        ("fail.load-buffer", Outcome::NotSent(Veto::LoadFailed), 0, 0),
        (
            "gone_after_load",
            Outcome::NotSent(Veto::PaneUnavailable),
            0,
            0,
        ),
        (
            "fail.paste-buffer",
            Outcome::Unconfirmed(Unconfirmed::PasteFailed),
            1,
            0,
        ),
        (
            "fail_after.paste-buffer",
            Outcome::Unconfirmed(Unconfirmed::PasteFailed),
            1,
            0,
        ),
        (
            "gone_after_paste",
            Outcome::Unconfirmed(Unconfirmed::EnterFailed),
            1,
            1,
        ),
        (
            "fail.send-keys",
            Outcome::Unconfirmed(Unconfirmed::EnterFailed),
            1,
            1,
        ),
        (
            "fail_after.send-keys",
            Outcome::Unconfirmed(Unconfirmed::EnterFailed),
            1,
            1,
        ),
    ] {
        let fake = Fake::new();
        fake.flag(fault);
        assert_eq!(fake.run(&FakeLedger::default()), expected, "{fault}");
        assert_eq!(
            (fake.calls("paste-buffer"), fake.enters()),
            (pasted, entered),
            "{fault}"
        );
    }
}

/// Receipt collection can finish after injection, or lose the rollout, without changing success.
#[test]
fn late_or_unreadable_receipts_are_diagnostic_only() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let fake = Fake::new();
    assert_eq!(fake.run(&FakeLedger::default()), Outcome::Injected);
    assert!(
        crate::services::tui_input::human_frame::history::registered(
            NONCE,
            &fake.rollout,
            &frame_text()
        )
    );
    assert_eq!(
        crate::services::tui_input::human_frame::history::receipt_row(NONCE, &fake.rollout),
        None
    );
    fake.accept_into(T);
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&fake.rollout)
        .unwrap();
    file.write_all(
        fs::read(fake.dir.path().join("on_enter"))
            .unwrap()
            .as_slice(),
    )
    .unwrap();
    wait_receipt(&fake);
    assert_eq!((fake.calls("capture-pane"), fake.enters()), (1, 1));
    let fake = Fake::new();
    fake.flag("remove_rollout");
    assert_eq!(fake.run(&FakeLedger::default()), Outcome::Injected);
    assert!(!fake.rollout.exists());
    assert_eq!(fake.enters(), 1);
}
