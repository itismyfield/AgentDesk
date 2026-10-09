use std::cell::Cell;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;

/// The model turn the captured `model_turn.jsonl` keeps open until its line 19.
const T: &str = "01a11d88-ef31-7860-9d48-88926c4b1809";
/// The request whose frame the captured `busy_frame.ansi` shows in the composer.
const SOURCE: &str = "discord";
const AUTHOR: &str = "fixture";
const NONCE: &str = "c0ffee01";
const BODY: &str = "fixture body line one\nsecond line of the injected text";

const FAST: Timing = Timing {
    lock_retries: &[Duration::ZERO, Duration::from_millis(5)],
    settle: Duration::ZERO,
    rechecks: 3,
    recheck_interval: Duration::from_millis(5),
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
        non_steerable(compact)
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

/// Each condition of a steerable model turn is required on its own: the turn's context, its user
/// message, both attributed to the same turn, a root turn, no review or compaction, full access.
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
    let compacting = model("t1") + &item("t1", "ContextCompaction", "");
    assert_eq!(verdict(&compacting), non_steerable("t1"));
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
    assert_eq!(verdict(&long), TurnVerdict::Unknown);
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
    frame(SOURCE, AUTHOR, NONCE, BODY)
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
    let shown = screen::read(&fixture("busy_frame.ansi"));
    assert_eq!(shown.composer, screen::Composer::Text(frame_text()));
    assert!(screen::owns(&fixture("busy_frame.ansi"), &frame_text()));
    let other = frame(SOURCE, AUTHOR, "0ther000", BODY);
    assert!(!screen::owns(&fixture("busy_frame.ansi"), &other));
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

/// A scripted `tmux`: `if-shell -F` runs its command only while the scripted attach count is 0;
/// an applied Enter appends `on_enter` to the rollout, and `fail_after.<sub>` fails it afterwards.
const FAKE_TMUX: &str = r#"#!/bin/sh
d='@D@'
next() { n=$(cat "$d/$1.n" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$d/$1.n"; f="$d/$1.$n"; [ -f "$f" ] || f="$d/$1"; cat "$f"; }
echo "$*" >> "$d/log"
[ -f "$d/fail.$2" ] && exit 1
case "$2" in
display-message) next attach ;;
capture-pane) next cap ;;
load-buffer) for last do :; done; cp "$last" "$d/buffer" ;;
if-shell)
  k=$(next keyattach)
  if [ "$k" = 0 ]; then
    sub="${7%% *}"
    echo "-u $7" >> "$d/log"
    [ -f "$d/fail.$sub" ] && exit 1
    if [ "$sub" = send-keys ] && [ -f "$d/on_enter" ]; then cat "$d/on_enter" >> '@T@'; fi
    [ -f "$d/fail_after.$sub" ] && exit 1
  elif [ "$k" = gone ]; then
    echo "agentdesk-busy-inject-vetoed"
  else
    echo "agentdesk-busy-inject-vetoed $k"
  fi ;;
esac
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
        fake.answers("keyattach", &["0"]);
        fake.caps(&["busy_empty.ansi", "busy_frame.ansi"]);
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
        let shown: Vec<String> = names.iter().map(|name| fixture(name)).collect();
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
            item(turn, "UserMessage", &frame_text()),
        )
        .unwrap();
    }

    fn run(&self, ledger: &FakeLedger) -> Outcome {
        self.run_with(ledger, BODY)
    }

    fn run_with(&self, ledger: &FakeLedger, text: &str) -> Outcome {
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
            nonce: NONCE,
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

/// The frame is pasted, proven to be the whole composer, and submitted by exactly one Enter; the
/// turn the rollout names decides whether it joined the target turn.
#[test]
fn one_enter_and_the_rollout_names_the_turn_it_joined() {
    let fake = Fake::new();
    fake.accept_into(T);
    let ledger = FakeLedger::default();
    let joined = Outcome::Injected {
        observed_turn: Some(T.into()),
        joined: true,
    };
    assert_eq!(fake.run(&ledger), joined);
    assert_eq!((fake.calls("paste-buffer"), fake.enters()), (1, 1));
    assert_eq!((ledger.registered.get(), ledger.withdrawn.get()), (1, 0));
    // The turn ended between the check and the Enter: the input opened the next turn.
    let fake = Fake::new();
    fake.accept_into("01b-next-turn");
    let moved = Outcome::Injected {
        observed_turn: Some("01b-next-turn".into()),
        joined: false,
    };
    assert_eq!(fake.run(&FakeLedger::default()), moved);
}

/// An emptied composer is not a submission: without the rollout record the input stays
/// unconfirmed. An Enter whose call fails after it applied is never sent again.
#[test]
fn only_the_rollout_confirms_and_enter_is_never_repeated() {
    let fake = Fake::new();
    fake.caps(&["busy_empty.ansi", "busy_frame.ansi", "busy_empty.ansi"]);
    let ledger = FakeLedger::default();
    let outcome = fake.run(&ledger);
    assert_eq!(outcome, Outcome::Unconfirmed(Unconfirmed::NotObserved));
    assert_eq!((fake.enters(), ledger.withdrawn.get()), (1, 0));
    let fake = Fake::new();
    fake.accept_into(T);
    fake.flag("fail_after.send-keys");
    let outcome = fake.run(&FakeLedger::default());
    assert_eq!(outcome, Outcome::Unconfirmed(Unconfirmed::EnterFailed));
    assert_eq!(fake.enters(), 1);
}

/// Screens a paste would land wrong on are refused before any buffer, paste or Enter, and the
/// ledger never hears of them.
#[test]
fn draft_queue_and_hidden_composer_are_refused_before_the_paste() {
    let cases = [
        ("busy_draft.ansi", Veto::Draft),
        ("busy_queued.ansi", Veto::QueueShown),
        ("compact_queued.txt", Veto::QueueShown),
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

/// A paste that may fold or outgrow the screen, or a pane whose size is unknown, is refused.
#[test]
fn unpredictable_renders_are_refused() {
    let render = Outcome::NotSent(Veto::UnpredictableRender);
    let fake = Fake::new();
    let rows: Vec<String> = (0..14).map(|row| format!("row {row}")).collect();
    assert_eq!(
        fake.run_with(&FakeLedger::default(), &rows.join("\n")),
        render
    );
    let fake = Fake::new();
    assert_eq!(
        fake.run_with(&FakeLedger::default(), &"y".repeat(1000)),
        render
    );
    let fake = Fake::new();
    fake.answers("attach", &["0,,"]);
    assert_eq!(fake.run(&FakeLedger::default()), render);
    assert_eq!(fake.calls("capture-pane"), 0);
    // Thirteen short rows fit an 80x24 pane once the header is counted.
    assert_eq!(predicted_rows(&rows[..13].join("\n"), 80), Some(13));
}

/// The ledger hears of a paste before it runs; a refused registration or a paste the guard kept
/// out withdraws, while a paste of unknown effect leaves the record for a late observation.
#[test]
fn the_ledger_brackets_the_paste() {
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
    fake.answers("keyattach", &["1"]);
    let ledger = FakeLedger::default();
    assert_eq!(fake.run(&ledger), Outcome::NotSent(Veto::HumanAttached));
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

/// A person at the pane, or a composer that does not show exactly the frame after the paste,
/// keeps the Enter back.
#[test]
fn attach_and_foreign_composer_text_withhold_the_enter() {
    let fake = Fake::new();
    fake.answers("attach", &["1,,80,24"]);
    assert_eq!(
        fake.run(&FakeLedger::default()),
        Outcome::NotSent(Veto::HumanAttached)
    );
    let fake = Fake::new();
    fake.caps(&["busy_empty.ansi", "busy_draft.ansi"]);
    let outcome = fake.run(&FakeLedger::default());
    assert_eq!(outcome, Outcome::Unconfirmed(Unconfirmed::DraftNotOwned));
    assert_eq!((fake.calls("paste-buffer"), fake.enters()), (1, 0));
    let fake = Fake::new();
    fake.answers("attach", &["0,,80,24", "1,1791496900,80,24"]);
    let outcome = fake.run(&FakeLedger::default());
    assert_eq!(
        outcome,
        Outcome::Unconfirmed(Unconfirmed::AttachedAfterPaste)
    );
    assert_eq!(fake.enters(), 0);
}

/// The capture with a remote image row drawn above its textarea, as Codex shows a restored draft.
fn with_image(capture: &str) -> String {
    let mut rows: Vec<&str> = capture.lines().collect();
    let plain = crate::services::codex_tui::input::strip_ansi_escape_sequences;
    let prompt = rows.iter().rposition(|row| plain(row).starts_with('›'));
    rows.splice(prompt.unwrap()..prompt.unwrap(), ["  [Image #1]", ""]);
    rows.join("\n")
}

/// An image left above an empty textarea is a draft an Enter would submit: it is refused before
/// any write, and an image that appears after the paste keeps the Enter back.
#[test]
fn an_image_draft_is_refused_before_the_paste_and_never_owned() {
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
    assert_eq!(fake.run(&ledger), Outcome::NotSent(Veto::Draft));
    let writes = fake.calls("load-buffer") + fake.calls("paste-buffer") + fake.enters();
    assert_eq!((writes, ledger.registered.get()), (0, 0));
    let fake = Fake::new();
    let after = with_image(&fixture("busy_frame.ansi"));
    fake.answers("cap", &[&fixture("busy_empty.ansi"), &after]);
    let outcome = fake.run(&FakeLedger::default());
    assert_eq!(outcome, Outcome::Unconfirmed(Unconfirmed::DraftNotOwned));
    assert_eq!((fake.calls("paste-buffer"), fake.enters()), (1, 0));
}

/// Bodies the screen reader could not prove after the paste go back to the queue before any
/// write: a blank or blank-looking line, or more rows than the reader scans in a tall pane.
#[test]
fn bodies_the_reader_cannot_prove_are_refused_before_any_write() {
    let tall: Vec<String> = (0..40).map(|row| format!("row {row}")).collect();
    let bodies = [
        "first\n\nsecond".to_string(),
        "first\n   \nsecond".to_string(),
        "first\n".to_string(),
        tall.join("\n"),
    ];
    for body in bodies {
        let fake = Fake::new();
        fake.answers("attach", &["0,,80,100"]);
        let ledger = FakeLedger::default();
        let outcome = fake.run_with(&ledger, &body);
        assert_eq!(
            outcome,
            Outcome::NotSent(Veto::UnpredictableRender),
            "{body:?}"
        );
        let writes = fake.calls("load-buffer") + fake.calls("paste-buffer") + fake.enters();
        assert_eq!((writes, ledger.registered.get()), (0, 0), "{body:?}");
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
