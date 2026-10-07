use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::screen::{self, Composer, Stash};
use super::*;

const NONCE: &str = "abcd1234";
const TEXT: &str = "are you there?";

/// Windows only bound the waits for evidence that never comes; found evidence returns at once.
/// Lock retries ride out a map shard another test holds for an instant.
const FAST: Timing = Timing {
    lock_retries: &[
        Duration::ZERO,
        Duration::from_millis(5),
        Duration::from_millis(10),
    ],
    settle: Duration::ZERO,
    rechecks: 3,
    recheck_interval: Duration::from_millis(5),
    confirm_window: Duration::from_secs(3),
    confirm_poll: Duration::from_millis(20),
    restore_window: Duration::from_millis(600),
};

/// A scripted Claude TUI behind `tmux`: composer and stash files, the single-slot C-s, the submit
/// that hands a stash back, attach generation, a person after capture N, a `barrier` after Enter.
const FAKE_TUI: &str = r#"#!/bin/sh
d='@D@'
echo "$*" >> "$d/log"
cs() {
  if [ -s "$d/composer" ]; then mv "$d/composer" "$d/stash"; : > "$d/composer"
  elif [ -f "$d/stash" ]; then mv "$d/stash" "$d/composer"; fi
}
enter() {
  [ -s "$d/composer" ] || return 0
  cat "$d/accept" >> '@T@'
  : > "$d/composer"
  [ -f "$d/stash" ] || return 0
  if [ -f "$d/restore_after" ]; then cp "$d/restore_after" "$d/restore_in"; else mv "$d/stash" "$d/composer"; fi
}
paste() {
  if [ -f "$d/fold" ]; then printf '[Pasted text #1 +%s lines]' "$(wc -l < "$d/buffer" | tr -d ' ')" >> "$d/composer"
  else cat "$d/buffer" >> "$d/composer"; fi
}
attach() { echo 1 > "$d/attached"; echo "$1" > "$d/last"; }
detach() { echo 0 > "$d/attached"; }
type_text() { printf '%s' "$1" >> "$d/composer"; }
render() {
  cat "$d/head"
  if [ -f "$d/status" ]; then cat "$d/status"; elif [ -f "$d/stash" ]; then cat "$d/stashrow"; else echo; fi
  cat "$d/border"
  if [ -s "$d/composer" ]; then awk -v p="$(cat "$d/prompt")" 'NR==1{print p $0; next} {print ($0 == "" ? "" : "  " $0)}' "$d/composer"
  else cat "$d/prompt"; echo; fi
  cat "$d/border" "$d/footer"
}
case "$2" in
display-message) echo "$(cat "$d/attached"),$(cat "$d/last"),$(cat "$d/width"),$(cat "$d/height")" ;;
capture-pane)
  n=$(($(cat "$d/cap.n" 2>/dev/null || echo 0) + 1)); echo $n > "$d/cap.n"
  [ -f "$d/fail.cap.$n" ] && exit 1
  # With a barrier, the first capture after Enter waits for go, 3s at most.
  if [ -f "$d/barrier" ] && grep -qx Enter "$d/applied" 2>/dev/null; then
    rm "$d/barrier"; : > "$d/reached"; i=0
    while [ ! -f "$d/go" ] && [ $i -lt 300 ]; do sleep 0.01; i=$((i+1)); done
  fi
  if [ -f "$d/restore_in" ]; then
    k=$(cat "$d/restore_in")
    if [ "$k" = 0 ]; then mv "$d/stash" "$d/composer"; rm "$d/restore_in"
    elif [ "$k" != never ]; then echo $((k-1)) > "$d/restore_in"; fi
  fi
  render
  [ -f "$d/human.$n" ] && . "$d/human.$n"
  ;;
load-buffer) for last do :; done; cp "$last" "$d/buffer" ;;
if-shell)
  a=$(cat "$d/attached"); l=$(cat "$d/last"); ok=0
  [ "$a" = 0 ] && ok=1
  case "$6" in *session_last_attached*)
    g0=$(printf '%s' "$6" | sed 's/.*session_last_attached},\([0-9]*\)}}$/\1/')
    [ "$l" = "$g0" ] || ok=0 ;;
  esac
  [ "$ok" = 1 ] || { echo "agentdesk-busy-inject-vetoed $a $l"; exit 0; }
  set -- $7; eval "key=\${$#}"; [ "$1" = paste-buffer ] && key=paste
  echo "$key" >> "$d/applied"
  case "$key" in C-s) cs ;; Enter) enter ;; paste) paste ;; esac
  # The key took effect but the reply never came back.
  [ -f "$d/lost.$key" ] && exit 1
  ;;
esac
exit 0
"#;

struct Tui {
    dir: tempfile::TempDir,
    transcript: PathBuf,
}

impl Tui {
    /// A busy 60x50 pane whose composer holds `draft` (empty for none) and no stash.
    fn new(draft: &str) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/busy-inject-tmp");
        fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        let transcript = dir.path().join("transcript.jsonl");
        let busy_turn = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"go\"}}\n\
            {\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"working\"}]}}\n";
        fs::write(&transcript, busy_turn).unwrap();
        let script = FAKE_TUI
            .replace("@D@", &dir.path().display().to_string())
            .replace("@T@", &transcript.display().to_string());
        let program = dir.path().join("tmux");
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let tui = Self { dir, transcript };
        let accept = serde_json::json!({"type": "queue-operation", "operation": "enqueue", "content": frame("iMessage", "ann", NONCE, TEXT)});
        for (name, value) in [
            ("attached", "0".to_string()),
            ("last", "100".to_string()),
            ("width", "60".to_string()),
            ("height", "50".to_string()),
            (
                "head",
                "⏺ Working on it.\n\n✻ Thinking… (12s · esc to interrupt)\n".to_string(),
            ),
            ("border", format!("{}\n", "─".repeat(60))),
            ("prompt", "❯\u{00a0}".to_string()),
            (
                "footer",
                "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n".to_string(),
            ),
            ("stashrow", format!("{:>58}\n", "› stashed")),
            ("composer", draft.to_string()),
            ("accept", format!("{accept}\n")),
        ] {
            tui.put(name, &value);
        }
        tui
    }

    fn put(&self, name: &str, value: &str) {
        fs::write(self.dir.path().join(name), value).unwrap();
    }

    fn get(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.dir.path().join(name)).ok()
    }

    /// Shell run by the fake right after it prints capture `n`.
    fn person_after_capture(&self, n: usize, script: &str) {
        self.put(&format!("human.{n}"), script);
    }

    fn session(&self) -> String {
        let name = self.dir.path().file_name().unwrap().to_string_lossy();
        let name: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        format!("stash-{name}")
    }

    fn run_text(&self, text: &str, timing: &Timing) -> Report {
        let session = self.session();
        let pane = Pane::with_program(&session, self.dir.path().join("tmux"));
        let request = Request {
            session: &session,
            transcript: &self.transcript,
            source: "iMessage",
            author: "ann",
            nonce: NONCE,
            text,
        };
        inject_report(&pane, &request, timing)
    }

    fn run(&self) -> Report {
        self.run_text(TEXT, &FAST)
    }

    /// The pane as the fake shows it now.
    fn capture(&self) -> String {
        let pane = Pane::with_program(&self.session(), self.dir.path().join("tmux"));
        pane.capture().unwrap()
    }

    /// Keys the server applied for AgentDesk, in order.
    fn applied(&self) -> Vec<String> {
        let log = self.get("applied").unwrap_or_default();
        log.lines().map(str::to_string).collect()
    }

    /// (composer, stash) as the person would find them.
    fn drafts(&self) -> (String, Option<String>) {
        (self.get("composer").unwrap_or_default(), self.get("stash"))
    }

    fn taken(&self) -> bool {
        transcript_carries(&self.transcript, 0, NONCE)
    }

    fn alerts(&self) -> Vec<serde_json::Value> {
        let session = self.session();
        crate::services::observability::events::recent(usize::MAX)
            .into_iter()
            .filter(|event| event.event_type == "busy_inject_draft_unrecovered")
            .map(|event| event.payload)
            .filter(|payload| payload["tmux_session"] == session.as_str())
            .collect()
    }
}

fn report(outcome: Outcome, draft: DraftState) -> Report {
    Report { outcome, draft }
}

fn framed() -> String {
    frame("iMessage", "ann", NONCE, TEXT)
}

#[test]
fn a_draft_is_stashed_then_the_input_entered_once_and_the_draft_handed_back() {
    for draft in ["human draft A", "first line\n\n  indented third"] {
        let tui = Tui::new(draft);
        let got = tui.run();
        assert_eq!(
            (got, got.delivery()),
            (
                report(Outcome::Injected, DraftState::RestoredObserved),
                Delivery::Observed
            ),
            "{draft:?}"
        );
        assert_eq!(tui.applied(), ["C-s", "paste", "Enter"], "{draft:?}");
        assert_eq!(tui.drafts(), (draft.to_string(), None), "{draft:?}");
        assert!(tui.taken() && tui.alerts().is_empty(), "{draft:?}");
    }
}

/// A person who attaches after the pre-capture, stashes A, types B and leaves keeps both.
#[test]
fn a_person_attaching_after_the_pre_capture_keeps_both_drafts() {
    let tui = Tui::new("human draft A");
    tui.person_after_capture(1, "attach 200; cs; type_text 'human draft B'; detach");
    assert_eq!(
        tui.run(),
        report(Outcome::NotSent(Veto::HumanAttached), DraftState::Unchanged)
    );
    assert!(tui.applied().is_empty());
    let kept = (
        "human draft B".to_string(),
        Some("human draft A".to_string()),
    );
    assert_eq!(tui.drafts(), kept);

    // An attach in the current second could repeat unseen, so nothing is read or sent. One second
    // ahead, so a clock tick during the test cannot age it out.
    let tui = Tui::new("human draft A");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    tui.put("last", &(now + 1).to_string());
    assert_eq!(
        tui.run(),
        report(Outcome::NotSent(Veto::HumanAttached), DraftState::Unchanged)
    );
    assert!(tui.applied().is_empty() && tui.get("cap.n").is_none());

    // Attached and gone again between the last check and Enter: the Enter is withheld.
    let tui = Tui::new("human draft A");
    tui.person_after_capture(3, "attach 300; detach");
    let got = tui.run();
    let withheld = Outcome::Unconfirmed(Unconfirmed::AttachedAfterPaste);
    assert_eq!(got, report(withheld, DraftState::Unknown));
    assert_eq!(tui.applied(), ["C-s", "paste"]);
    assert_eq!(tui.drafts(), (framed(), Some("human draft A".to_string())));
}

/// Delivery and draft are judged apart; nothing is resent and a pane in doubt is held.
#[test]
fn delivery_and_draft_outcomes_stay_apart_and_an_unrecovered_pane_is_held() {
    // Enter applied but its reply lost: no retry, and the restore is still watched.
    let tui = Tui::new("human draft A");
    tui.put("lost.Enter", "");
    let entered = Outcome::Unconfirmed(Unconfirmed::EnterFailed);
    assert_eq!(tui.run(), report(entered, DraftState::RestoredObserved));
    assert_eq!(tui.applied(), ["C-s", "paste", "Enter"]);
    assert!(tui.taken());
    assert_eq!(tui.drafts(), ("human draft A".to_string(), None));

    // The transcript first, the draft two captures later.
    let tui = Tui::new("human draft A");
    tui.put("restore_after", "2");
    assert_eq!(
        tui.run(),
        report(Outcome::Injected, DraftState::RestoredObserved)
    );
    assert_eq!(tui.drafts(), ("human draft A".to_string(), None));

    // Never handed back: still delivered, with an alert and the pane held.
    let tui = Tui::new("human draft A");
    tui.put("restore_after", "never");
    let got = tui.run();
    assert_eq!(
        (got, got.delivery()),
        (
            report(Outcome::Injected, DraftState::Unknown),
            Delivery::Observed
        )
    );
    let alerts = tui.alerts();
    assert_eq!(alerts.len(), 1);
    let fields = ["delivery", "composer", "stash", "automatic_writes_held"]
        .map(|field| alerts[0][field].to_string());
    assert_eq!(fields, ["\"observed\"", "\"empty\"", "\"present\"", "true"]);
    assert!(
        alerts[0]["guidance"]
            .as_str()
            .unwrap()
            .contains("press Ctrl+S once")
    );
    assert_eq!(
        tui.run(),
        report(Outcome::NotSent(Veto::Draft), DraftState::Unchanged)
    );
    assert_eq!(tui.applied().len(), 3);
    // The person restores it; the next input releases the hold and runs again.
    tui.put("composer", "human draft A");
    fs::remove_file(tui.dir.path().join("stash")).unwrap();
    fs::remove_file(tui.dir.path().join("restore_after")).unwrap();
    assert_eq!(
        tui.run(),
        report(Outcome::Injected, DraftState::RestoredObserved)
    );
    assert_eq!(tui.applied().len(), 6);

    // The capture after C-s fails: the input was not sent, the draft is unaccounted for.
    let tui = Tui::new("human draft A");
    tui.put("fail.cap.2", "");
    let got = tui.run();
    assert_eq!(
        (got, got.delivery()),
        (
            report(Outcome::NotSent(Veto::PaneUnavailable), DraftState::Unknown),
            Delivery::NotAttempted
        )
    );
    assert_eq!(tui.applied(), ["C-s"]);
    assert_eq!(
        tui.drafts(),
        (String::new(), Some("human draft A".to_string()))
    );
    let guidance = tui.alerts()[0]["guidance"].as_str().unwrap().to_string();
    assert!(guidance.contains("could not be read"), "{guidance}");
    assert_eq!(
        tui.run(),
        report(Outcome::NotSent(Veto::Draft), DraftState::Unchanged)
    );
    assert_eq!(tui.applied(), ["C-s"]);
}

/// The lock spans C-s to the recovery decision, so a second input never starts a second stash.
#[test]
fn a_second_input_waits_out_the_whole_stash_transaction() {
    let tui = Tui::new("human draft A");
    tui.put("restore_after", "never");
    tui.put("barrier", "");
    std::thread::scope(|scope| {
        let first = scope.spawn(|| tui.run());
        // The first input is parked inside its restore watch, the lock still held.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while tui.get("reached").is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "no restore watch began"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let second = tui.run();
        tui.put("go", "");
        assert_eq!(
            second,
            report(Outcome::NotSent(Veto::LockContended), DraftState::Unchanged)
        );
        let first = first.join().unwrap();
        assert_eq!(first, report(Outcome::Injected, DraftState::Unknown));
    });
    assert_eq!(
        tui.run(),
        report(Outcome::NotSent(Veto::Draft), DraftState::Unchanged)
    );
    assert_eq!(tui.applied(), ["C-s", "paste", "Enter"]);
}

/// The hold covers every automatic writer: the follow-up submit and a native `/clear` send no key
/// until a capture shows the stash gone and the composer readable.
#[test]
fn a_held_pane_keeps_the_follow_up_and_native_clear_out_until_recovered() {
    use crate::services::claude_tui::composer_lock::{DraftRecoveryHold, admit_composer_write};
    use crate::services::claude_tui::host_input::{
        LegacyTmuxGate, NativeClearSubmission, SpyGuard, SpyState, native_clear_composer_empty,
        native_clear_once,
    };
    use crate::services::claude_tui::input::{
        is_prompt_ready_timeout_error, send_followup_prompt_or_idle_transcript as follow_up,
        with_composer_cleanup_lock,
    };
    const BUSY: &str = "\u{2733} Architecting\u{2026}";
    let tui = Tui::new("human draft A");
    tui.put("restore_after", "never");
    let got = tui.run();
    assert_eq!(
        (got, got.delivery()),
        (
            report(Outcome::Injected, DraftState::Unknown),
            Delivery::Observed
        )
    );
    // The turn ends; the draft stays stashed under an empty composer that /clear would accept.
    tui.put("head", "\u{23fa} Done.\n\n");
    let held = tui.capture();
    assert!(native_clear_composer_empty(&held));
    let (session, idle) = (tui.session(), tui.dir.path().join("idle.jsonl"));
    fs::write(
        &idle,
        r#"{"type":"system","subtype":"turn_duration","sessionId":"s"}"#,
    )
    .unwrap();
    let spy = |pane: &str| {
        let captures = [pane, pane, pane, BUSY].map(|c| Some(c.to_string()));
        SpyGuard::install(SpyState {
            captures: captures.into(),
            ..SpyState::default()
        })
    };
    let keys = |calls: Vec<String>| -> Vec<String> {
        let key = |c: &String| {
            ["keys:", "literal:", "load:", "paste:"]
                .iter()
                .any(|k| c.starts_with(k))
        };
        calls.into_iter().filter(key).collect()
    };
    let deadline = || tokio::time::Instant::now() + Duration::from_secs(20);

    let guard = spy(&held);
    let error = follow_up(&session, "follow-up", None, &idle).unwrap_err();
    let requeued = is_prompt_ready_timeout_error(&error)
        && error.contains("follow-up prompt input readiness")
        && error.contains("prompt_marker_detected=true");
    assert!(requeued, "{error}");
    let cleanup = with_composer_cleanup_lock(&session, || -> bool { panic!("held pane cleaned") });
    assert_eq!(cleanup, None);
    assert_eq!(keys(guard.calls()), Vec::<String>::new());
    drop(guard);
    let refuse = |_: &[&str], _: Duration| -> bool { panic!("a held pane took /clear") };
    let clear = native_clear_once(
        &session,
        &LegacyTmuxGate,
        deadline(),
        |_| Some(held.clone()),
        refuse,
    );
    assert_eq!(clear, NativeClearSubmission::NotSent);

    // No stash, but an attachment chip or a footer this reader has not measured: still held.
    fs::remove_file(tui.dir.path().join("stash")).unwrap();
    let footer = "  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle)\n";
    let unreadable = [
        ("[Image #1]", footer),
        ("look at [Image #2]", footer),
        ("[...Truncated text #1 +40 lines...]", footer),
        ("human draft A", "  ? for shortcuts\n"),
        (
            "human draft A",
            "  \u{23f5}\u{23f5} bypass permissions on (shift+tab \n",
        ),
    ];
    for (composer, row) in unreadable {
        tui.put("composer", composer);
        tui.put("footer", row);
        let admitted = admit_composer_write(&session, || Some(tui.capture()));
        assert_eq!(admitted, Err(DraftRecoveryHold), "{composer:?} {row:?}");
    }

    // The person took the draft back and sent it: that capture releases the hold.
    tui.put("composer", "");
    tui.put("footer", footer);
    let recovered = tui.capture();
    let guard = spy(&recovered);
    assert_eq!(follow_up(&session, "follow-up", None, &idle), Ok(()));
    assert!(keys(guard.calls()).contains(&"keys:Enter".to_string()));
    drop(guard);
    let mut sent = 0;
    let clear = native_clear_once(
        &session,
        &LegacyTmuxGate,
        deadline(),
        |_| Some(recovered.clone()),
        |_, _| {
            sent += 1;
            true
        },
    );
    assert_eq!((clear, sent), (NativeClearSubmission::Confirmed, 1));
    for prompt in ["follow-up", "/clear"] {
        crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
            "claude", &session, prompt,
        );
    }
}

/// Only a frame Claude shows unfolded and unwrapped may displace a draft.
#[test]
fn a_frame_that_would_fold_or_wrap_leaves_the_draft_alone() {
    type Setup = fn(&Tui);
    let cases: [(&str, &str, Setup); 3] = [
        ("three more lines", "one\ntwo\nthree", |_| {}),
        ("wraps in a narrow pane", TEXT, |tui| tui.put("width", "30")),
        ("folds in a short pane", "one\ntwo", |tui| {
            tui.put("height", "11")
        }),
    ];
    for (name, text, setup) in cases {
        let tui = Tui::new("human draft A");
        setup(&tui);
        let got = tui.run_text(text, &FAST);
        assert_eq!(
            got,
            report(Outcome::NotSent(Veto::Draft), DraftState::Unchanged),
            "{name}"
        );
        assert!(tui.applied().is_empty(), "{name}");
        assert_eq!(tui.drafts(), ("human draft A".to_string(), None), "{name}");
    }
    // Claude folds after all: a placeholder of the right shape is not proof, so no Enter.
    let tui = Tui::new("human draft A");
    tui.put("fold", "");
    let unowned = Outcome::Unconfirmed(Unconfirmed::DraftNotOwned);
    assert_eq!(tui.run(), report(unowned, DraftState::Unknown));
    assert_eq!(tui.applied(), ["C-s", "paste"]);
    let folded = "[Pasted text #1 +1 lines]".to_string();
    assert_eq!(tui.drafts(), (folded, Some("human draft A".to_string())));
}

#[test]
fn a_draft_beside_an_occupied_or_unreadable_stash_is_left_alone() {
    type Setup = fn(&Tui);
    let cases: [(&str, &str, Setup); 4] = [
        ("stash occupied", "draft B", |tui| {
            tui.put("stash", "draft A")
        }),
        ("status row is output", "draft B", |tui| {
            tui.put("status", "  12. streamed line\n")
        }),
        ("image chip", "[Image #1] look", |_| {}),
        ("unknown pane size", "draft B", |tui| tui.put("width", "")),
    ];
    for (name, draft, setup) in cases {
        let tui = Tui::new(draft);
        setup(&tui);
        let before = tui.drafts();
        let got = tui.run();
        assert_eq!(
            got,
            report(Outcome::NotSent(Veto::Draft), DraftState::Unchanged),
            "{name}"
        );
        assert!(tui.applied().is_empty(), "{name}");
        assert_eq!(tui.drafts(), before, "{name}");
    }
}

/// A box of `width` columns with `status` above it and `rows` inside, as `capture-pane -e` prints.
fn pane(width: usize, status: &str, rows: &[&str], footer: bool) -> String {
    let border = "─".repeat(width);
    let mut lines = vec![
        "⏺ Earlier answer".to_string(),
        String::new(),
        status.to_string(),
        border.clone(),
    ];
    lines.extend(rows.iter().map(|row| row.to_string()));
    lines.push(border);
    if footer {
        lines.push("  ⏵⏵ bypass permissions on".to_string());
    }
    lines.join("\n") + "\n"
}

#[test]
fn the_stash_is_read_from_the_status_row_above_the_active_box_only() {
    let draft = ["❯\u{00a0}half typed"];
    let empty = ["❯\u{00a0}"];
    let on = "  ⏵⏵ bypass permissions on";
    let text = |rows: &[&str]| Composer::Text(rows.iter().map(|row| row.to_string()).collect());
    let old_box = format!("{}\n❯\u{00a0}old\n{}\n", "─".repeat(60), "─".repeat(60));
    let scrollback = format!("{:>58}\n{old_box}", "› stashed") + &pane(60, "", &draft, true);
    let cases = [
        (
            "normal footer",
            pane(60, "", &draft, true),
            text(&["half typed"]),
            Stash::AbsentInRecognizedLayout,
        ),
        (
            "stashed",
            pane(120, &format!("{:>118}", "› stashed"), &empty, true),
            Composer::Empty,
            Stash::Present,
        ),
        (
            "hint then stashed",
            pane(
                100,
                &format!("{:>98}", "ctrl+g to edit in Micro · › stashed"),
                &draft,
                true,
            ),
            text(&["half typed"]),
            Stash::Present,
        ),
        (
            "toast drawn whole",
            pane(120, &format!("{:>118}", "Draft restored"), &draft, true),
            text(&["half typed"]),
            Stash::AbsentInRecognizedLayout,
        ),
        (
            "narrow pane",
            pane(28, &format!("{:>26}", "› stashed"), &empty, true),
            Composer::Empty,
            Stash::Present,
        ),
        (
            "narrow pane, toast cut",
            pane(28, "   Ctrl+Y to paste del", &draft, true),
            text(&["half typed"]),
            Stash::Unknown,
        ),
        (
            "output above the border",
            pane(60, "  12. streamed line", &draft, true),
            text(&["half typed"]),
            Stash::Unknown,
        ),
        (
            "truncated footer",
            pane(60, "", &draft, false),
            Composer::Unknown,
            Stash::Unknown,
        ),
        (
            "box cut before its bottom",
            pane(60, "", &draft, true).replace(&format!("{}\n  ⏵⏵", "─".repeat(60)), "  ⏵⏵"),
            Composer::Unknown,
            Stash::Unknown,
        ),
        (
            "stashed only in scrollback",
            scrollback,
            text(&["half typed"]),
            Stash::AbsentInRecognizedLayout,
        ),
        (
            "multi-line composer",
            pane(60, "", &["❯\u{00a0}first", "", "    indented"], true),
            text(&["first", "", "  indented"]),
            Stash::AbsentInRecognizedLayout,
        ),
        (
            "unequal borders",
            pane(60, "", &draft, true).replacen(&"─".repeat(60), &"─".repeat(59), 1),
            Composer::Unknown,
            Stash::Unknown,
        ),
        (
            "a bash-mode box below an older one",
            pane(60, "", &draft, true) + &pane(60, "", &["! ls"], true),
            Composer::Unknown,
            Stash::Unknown,
        ),
        (
            "image alone",
            pane(60, "", &["❯\u{00a0}[Image #1]"], true),
            Composer::Unknown,
            Stash::AbsentInRecognizedLayout,
        ),
        (
            "text and an image",
            pane(60, "", &["❯\u{00a0}look at", "  this [Image #2]"], true),
            Composer::Unknown,
            Stash::AbsentInRecognizedLayout,
        ),
        (
            "truncated text chip",
            pane(
                60,
                "",
                &["❯\u{00a0}[...Truncated text #1 +40 lines...]"],
                true,
            ),
            Composer::Unknown,
            Stash::AbsentInRecognizedLayout,
        ),
        (
            "footer with both hints",
            pane(60, "", &draft, true)
                .replace(on, &format!("{on} (shift+tab to cycle) · esc to interrupt")),
            text(&["half typed"]),
            Stash::AbsentInRecognizedLayout,
        ),
        (
            "footer cut at the edge",
            pane(60, "", &draft, true).replace(on, &format!("{on} (shift+tab ")),
            Composer::Unknown,
            Stash::Unknown,
        ),
        (
            "unmeasured footer",
            pane(60, "", &draft, true).replace(on, "  ? for shortcuts"),
            Composer::Unknown,
            Stash::Unknown,
        ),
        (
            "a row under the footer",
            pane(60, "", &draft, true) + "  Context left until auto-compact: 9%\n",
            Composer::Unknown,
            Stash::Unknown,
        ),
    ];
    for (name, capture, composer, stash) in cases {
        let got = screen::read(&capture);
        assert_eq!((got.composer, got.stash), (composer, stash), "{name}");
    }
    // Real `capture-pane -e` rows from Claude Code 2.1.292: colours, and a faint placeholder.
    let ansi = format!(
        "\n{}\x1b[38;5;246m › stashed\x1b[39m\n\x1b[38;5;244m{b}\n\x1b[38;5;246m❯\u{00a0}\x1b[39m\n\x1b[38;5;244m{b}\n\x1b[39m  \x1b[38;5;211m⏵⏵ bypass permissions on\x1b[38;5;246m (shift+tab to cycle) · esc to interrupt\x1b[39m\n",
        " ".repeat(108),
        b = "─".repeat(120)
    );
    let got = screen::read(&ansi);
    assert_eq!((got.composer, got.stash), (Composer::Empty, Stash::Present));
    let placeholder = pane(
        60,
        "",
        &["\x1b[39m❯\u{00a0}\x1b[2mTry \"edit <filepath> to...\"\x1b[0m"],
        true,
    );
    assert_eq!(screen::read(&placeholder).composer, Composer::Empty);
    let ghost = pane(60, "", &["❯\u{00a0}/he\x1b[2mlp\x1b[0m"], true);
    assert_eq!(screen::read(&ghost).composer, Composer::Unknown);
}
