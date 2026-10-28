//! The receipt check after a sent paste: when one more Enter may finish it, and when it must not.
use std::sync::mpsc;

use super::*;

/// Windows short enough for a test; found evidence returns at once.
const CONFIRM: Timing = Timing {
    lock_retries: FAST.lock_retries,
    settle: Duration::ZERO,
    confirm_window: Duration::from_millis(300),
    confirm_poll: Duration::from_millis(20),
};

const LONG: &str = "첫 줄\n둘째 줄\n셋째 줄";

/// Submits `text`, runs `between`, then confirms; `park` lets a visible draft be parked.
fn submit(tui: &Tui, park: bool, between: impl FnOnce()) -> (Outcome, Option<Receipt>) {
    let session = tui.session();
    let pane = Pane::with_program(&session, tui.dir.path().join("tmux"));
    let nonce = tui.next_nonce();
    let request = Request {
        session: &session,
        transcript: &tui.transcript,
        source: "iMessage",
        author: "200",
        name: "ann",
        nonce: &nonce,
        text: LONG,
    };
    let report = inject_gated(&pane, &request, &CONFIRM, park);
    between();
    (report.outcome, confirm(&pane, &request, &report, &CONFIRM))
}

fn folded(draft: &str) -> Tui {
    let tui = Tui::new(draft);
    tui.put("fold", "");
    tui
}

const MISSING: (Outcome, Option<Receipt>) =
    (Outcome::Injected, Some(Receipt::Missing { resent: false }));

#[test]
fn a_received_paste_takes_no_further_read_or_key() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let tui = folded("");
    let observed = (Outcome::Injected, Some(Receipt::Observed));
    assert_eq!(submit(&tui, false, || {}), observed);
    assert_eq!(tui.applied(), ["paste", "Enter"]);
    assert_eq!(tui.get("cap.n").as_deref(), Some("1\n"));
}

/// Folded or flat, the paste still shown after an ignored Enter takes exactly one more.
#[test]
fn an_ignored_enter_is_finished_once_on_its_own_paste() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for fold in [true, false] {
        let tui = if fold { folded("") } else { Tui::new("") };
        tui.put("drop.Enter", "1");
        let observed = (Outcome::Injected, Some(Receipt::Observed));
        assert_eq!(submit(&tui, false, || {}), observed, "fold={fold}");
        assert_eq!(tui.applied(), ["paste", "Enter", "Enter"], "fold={fold}");
        assert_eq!(tui.records(), [tui.get("buffer").unwrap()], "fold={fold}");
    }
}

/// A parked draft that a late first Enter would hand back, a draft already handed back, and a
/// draft the paste went behind each keep the second Enter off.
#[test]
fn a_person_draft_never_takes_the_finishing_enter() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let draft = "사람 초안";
    for (case, park, fault) in [
        ("parked", true, "drop.Enter"),
        ("handed back", true, "mute"),
        ("behind", false, "drop.Enter"),
    ] {
        let tui = if park { folded(draft) } else { Tui::new(draft) };
        tui.put("attached", "4");
        tui.put(fault, "1");
        assert_eq!(submit(&tui, park, || {}), MISSING, "{case}");
        let keys = if park {
            vec!["C-s", "paste", "Enter"]
        } else {
            vec!["paste", "Enter"]
        };
        assert_eq!(tui.applied(), keys, "{case}");
        assert!(tui.records().is_empty(), "{case}");
    }
}

/// A record late past the first window, one never written, and one landing just as the still
/// shown paste is read: none of them takes a second Enter.
#[test]
fn a_late_or_lost_record_takes_no_second_enter() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let observed = (Outcome::Injected, Some(Receipt::Observed));
    let record = serde_json::json!({"type": "queue-operation", "operation": "enqueue",
        "content": frame("iMessage", "200", "ann", "", LONG)});
    for (case, fault, expected) in [
        ("late", "late", observed),
        ("lost", "mute", MISSING),
        ("shown", "drop.Enter", observed),
    ] {
        let tui = folded("");
        tui.put(fault, "1");
        // The fake lands `late.rec` at the next capture, the one read before a second Enter.
        let land = || {
            if case == "shown" {
                tui.put("late.rec", &format!("{record}\n"));
            }
        };
        assert_eq!(submit(&tui, false, land), expected, "{case}");
        assert_eq!(tui.applied(), ["paste", "Enter"], "{case}");
    }
}

/// Another writer holding the composer lock keeps the finishing Enter off.
#[test]
fn a_held_composer_lock_keeps_the_finishing_enter_off() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let tui = folded("");
    tui.put("drop.Enter", "1");
    let session = tui.session();
    let (release, released) = mpsc::channel::<()>();
    let (held, holding) = mpsc::channel();
    let mut holder = None;
    // The holder takes the lock only after the paste and Enter, before the check.
    let result = submit(&tui, false, || {
        holder = Some(std::thread::spawn(move || {
            super::super::super::composer_lock::with_composer_mutation_lock(&session, || {
                held.send(()).unwrap();
                let _ = released.recv();
            })
        }));
        holding.recv().unwrap();
    });
    drop(release);
    holder.unwrap().join().unwrap();
    assert_eq!(result, MISSING);
    assert_eq!(tui.applied(), ["paste", "Enter"]);
}
