//! The diagnostic receipt thread records a late row into the attempt that sent it, after the
//! caller's history scope ended, and holds nothing another attempt waits on.
use super::{Outcome, Report, Request, Sent, Timing, collect_receipt, frame};
use crate::services::tui_input::human_frame::history::{
    self, ObservedAttempt,
    v2::{Registry, with_attempt},
};
use serde_json::{Value, json};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};

const NS: &str = "receipt-scope";

fn input(id: u64, nonce: &str, session: &str, path: &Path, frame: &str) -> Value {
    let meta = std::fs::metadata(path).unwrap();
    json!({"identity":{"primary":id,"queued_generation":1,"sources":[{"message_id":id,"queued_generation":1}]},
        "attempt":{"nonce":nonce,"provider":"claude","target":{"host":"tmux","owner":session,"root":"/tmp","execution_nonce":"spawn"},
            "source":{"kind":"existing","session_id":"native","transcript":path,"file":[meta.dev(),meta.ino()],"offset":0}},
        "original":{"author_id":720205},"payload":{"frame":frame}})
}

fn key(input: &Value) -> Value {
    json!({"identity":input["identity"],"attempt":input["attempt"]})
}

#[test]
fn a_late_row_reaches_the_sending_attempt_from_the_receipt_thread() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native.jsonl");
    std::fs::write(&path, "").unwrap();
    let session = format!("receipt-scope-{}", uuid::Uuid::new_v4());
    let (na, nb) = ("ca7272a1", "ca7272b1");
    let fa = frame("discord", "720205", "ann", na, "alpha body");
    let fb = frame("discord", "720205", "ann", nb, "beta body");
    let history = Registry::runtime().unwrap();
    let (ia, ib) = (
        input(1, na, &session, &path, &fa),
        input(2, nb, &session, &path, &fb),
    );
    history.prepare(NS, &ia).unwrap();
    history.prepare(NS, &ib).unwrap();
    let (a, b) = (key(&ia), key(&ib));
    let observed = |key: &Value, prompt: &str, row: &str| {
        with_attempt(&history, NS, key, || {
            match history::observed_attempt("claude", &session, &[prompt.to_string()], Some(row)) {
                ObservedAttempt::Known(id) => format!("known:{id}"),
                ObservedAttempt::Pending => "pending".to_string(),
                ObservedAttempt::None => "none".to_string(),
            }
        })
        .unwrap()
    };
    let request = Request {
        session: &session,
        transcript: &path,
        source: "discord",
        author: "720205",
        name: "ann",
        nonce: na,
        text: "alpha body",
    };
    let report = Report {
        outcome: Outcome::Injected,
        sent: Some(Sent {
            offset: 0,
            text: fa.clone(),
            parked: false,
        }),
    };
    let timing = Timing {
        lock_retries: &[],
        settle: Duration::ZERO,
        confirm_window: Duration::from_secs(5),
        confirm_poll: Duration::from_millis(20),
    };
    with_attempt(&history, NS, &a, || {
        collect_receipt(&request, &report, &timing)
    })
    .unwrap();
    assert_eq!(observed(&a, &fa, "row-a"), "pending");

    // While the thread polls, another attempt registers on this thread without waiting on it.
    let started = Instant::now();
    with_attempt(&history, NS, &b, || {
        history::register(("claude", &session, nb, "720205"), &path, 0, &fb)
    })
    .unwrap()
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(
        file,
        "{}",
        json!({"type":"user","uuid":"row-a","message":{"content":fa}})
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while observed(&a, &fa, "row-a") == "pending" && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(observed(&a, &fa, "row-a"), format!("known:{na}"));
    assert_eq!(
        with_attempt(&history, NS, &a, || history::receipt_row(na, &path)).unwrap(),
        Some("row-a".into())
    );
    assert_eq!(
        with_attempt(&history, NS, &b, || history::receipt_row(nb, &path)).unwrap(),
        None,
        "A's row is not attributed to B"
    );
    assert_eq!(observed(&b, &fb, "row-a"), "pending");
}

/// Starts A's receipt thread for `frame` over `path` and returns once its window has passed.
fn receive(
    history: &Registry,
    a: &Value,
    (session, path, nonce): (&str, &Path, &str),
    frame: &str,
) {
    let request = Request {
        session,
        transcript: path,
        source: "discord",
        author: "720205",
        name: "ann",
        nonce,
        text: "same body",
    };
    let report = Report {
        outcome: Outcome::Injected,
        sent: Some(Sent {
            offset: 0,
            text: frame.to_string(),
            parked: false,
        }),
    };
    let timing = Timing {
        lock_retries: &[],
        settle: Duration::ZERO,
        confirm_window: Duration::from_millis(300),
        confirm_poll: Duration::from_millis(20),
    };
    with_attempt(history, NS, a, || {
        collect_receipt(&request, &report, &timing)
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(600));
}

fn row(path: &Path, record: Value) -> u64 {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{record}").unwrap();
    std::fs::metadata(path).unwrap().len()
}

/// Same-frame A and B: the row goes to A only when B's paste came after it, otherwise to neither;
/// the receipt scope registers nothing.
#[test]
fn a_same_frame_row_reaches_the_receipt_attempt_only_with_evidence() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for ambiguous in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("native.jsonl");
        std::fs::write(&path, "").unwrap();
        let session = format!("receipt-same-{}", uuid::Uuid::new_v4());
        let (na, nb) = ("ca7272a2", "ca7272b2");
        let f = frame("discord", "720205", "ann", na, "same body");
        assert_eq!(f, frame("discord", "720205", "ann", nb, "same body"));
        let history = Registry::runtime().unwrap();
        let ia = input(3, na, &session, &path, &f);
        history.prepare(NS, &ia).unwrap();
        let end = row(
            &path,
            json!({"type":"user","uuid":"row-a","message":{"content":f}}),
        );
        let mut ib = input(4, nb, &session, &path, &f);
        if !ambiguous {
            ib["attempt"]["source"]["offset"] = json!(end);
        }
        history.prepare(NS, &ib).unwrap();
        let (a, b) = (key(&ia), key(&ib));
        let scope = with_attempt(&history, NS, &a, history::ReceiptScope::capture).unwrap();
        let (plain, append) = scope.enter(|| {
            (
                history::register(("claude", &session, na, "720205"), &path, 0, &f),
                history::append::register(
                    (&session, na, "720205"),
                    &path,
                    0,
                    &f,
                    "t0",
                    None,
                    false,
                ),
            )
        });
        assert!(plain.is_err() && append.is_err(), "{plain:?} {append:?}");
        receive(&history, &a, (&session, &path, na), &f);
        let rows = [(na, &a), (nb, &b)].map(|(nonce, key)| {
            with_attempt(&history, NS, key, || history::receipt_row(nonce, &path)).unwrap()
        });
        let expected = if ambiguous {
            None
        } else {
            Some("row-a".into())
        };
        assert_eq!(rows, [expected, None], "ambiguous={ambiguous}");
    }
}

/// A user row the next user does not chain to is still recorded as observed; the receipt settles
/// nothing, so it claims no completed run.
#[test]
fn an_orphaned_user_row_is_recorded_as_observed_without_settling_the_attempt() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native.jsonl");
    std::fs::write(&path, "").unwrap();
    let session = format!("receipt-orphan-{}", uuid::Uuid::new_v4());
    let na = "ca7272a3";
    let f = frame("discord", "720205", "ann", na, "same body");
    let history = Registry::runtime().unwrap();
    let ia = input(5, na, &session, &path, &f);
    history.prepare(NS, &ia).unwrap();
    let a = key(&ia);
    row(
        &path,
        json!({"type":"user","uuid":"u1","parentUuid":"p0","message":{"content":f}}),
    );
    row(
        &path,
        json!({"type":"user","uuid":"u2","parentUuid":"p0","message":{"content":"next"}}),
    );
    receive(&history, &a, (&session, &path, na), &f);
    assert_eq!(
        with_attempt(&history, NS, &a, || history::receipt_row(na, &path)).unwrap(),
        Some("u1".into())
    );
    let state = history.json().unwrap();
    assert!(
        state["live"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["registration"]["key"] == a),
        "{state}"
    );
    assert!(state["passive"].as_array().unwrap().is_empty(), "{state}");
}
