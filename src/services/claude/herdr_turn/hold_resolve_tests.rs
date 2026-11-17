use super::*;
use crate::services::claude::herdr_turn::hold_record::{read, write};
use crate::services::claude::herdr_turn::{InputRun, hold_path};
use std::cell::{Cell, RefCell};

const FRAME: &str = "[User: a] hold me";

fn pane(status: &str, input: &str, footer: &str) -> String {
    let border = "─".repeat(52);
    format!(
        "✻ Baked\n{status}\n{border}\n❯ {input}\n{border}\n  🤖 Opus(H) │ 7% │ MCP: 2\n  ⏵⏵ bypass permissions on (shift+tab to cycle){footer}"
    )
}
fn shown(input: &str) -> String {
    pane("", input, "")
}

#[derive(Default)]
struct Fake {
    unconfirmed: bool,
    screen: RefCell<Option<String>>,
    keys: Cell<usize>,
    notices: RefCell<Vec<String>>,
    failing_notices: Cell<u32>,
    /// The screen an enqueue that succeeds leaves behind, as if it recovered meanwhile.
    after_notice: RefCell<Option<String>>,
}
impl Fake {
    fn on(screen: &str) -> Self {
        let fake = Fake::default();
        fake.set(screen);
        fake
    }
    fn set(&self, screen: &str) {
        *self.screen.borrow_mut() = Some(screen.to_owned());
    }
}
impl Ports for Fake {
    fn confirmed(&self) -> bool {
        !self.unconfirmed
    }
    fn capture(&self) -> Option<String> {
        self.screen.borrow().clone()
    }
    fn clear(&self, count: usize) -> bool {
        self.keys.set(self.keys.get() + count);
        self.set(&shown(""));
        true
    }
    fn notice(&self, hold: &str) -> Result<i64, String> {
        if self.failing_notices.get() > 0 {
            self.failing_notices.set(self.failing_notices.get() - 1);
            return Err("outbox down".into());
        }
        self.notices.borrow_mut().push(hold.to_owned());
        if let Some(screen) = self.after_notice.borrow_mut().take() {
            self.set(&screen);
        }
        Ok(1)
    }
}

/// A hold whose write recorded `outcome` an hour ago, so every window has passed.
fn hold(nonce: &str, outcome: SendOutcome, transcript: &std::path::Path) -> HoldRecord {
    let ago = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let proof = match outcome {
        SendOutcome::InFlight | SendOutcome::EnterSendFailed => UnsentProof::None,
        _ => UnsentProof::SendOutcome,
    };
    HoldRecord {
        schema: 2,
        recorded_at: ago.clone(),
        provider: "claude".into(),
        logical_key: "hold-session".into(),
        execution_nonce: nonce.into(),
        hold_id: format!("{nonce}-hold"),
        frame_sha256: hold_record::sha256_hex(FRAME.as_bytes()),
        screen_exact: true,
        transcript: transcript.display().to_string(),
        file: None,
        offset_before: 0,
        started_at: ago.clone(),
        send_outcome: outcome,
        outcome_at: (outcome != SendOutcome::InFlight).then_some(ago),
        unsent_proof: proof,
        cleanup_phase: CleanupPhase::None,
        cleanup_at: None,
        reservation: None,
        unclear_passes: 0,
        notice_intent_at: None,
        notice_enqueued_at: None,
    }
}

fn user_record(text: &str) -> String {
    serde_json::json!({"type":"user","message":{"role":"user","content":text}}).to_string() + "\n"
}

fn backdate_clear_receipt(record: &HoldRecord) {
    let at = Instant::now() - RECEIPT - Duration::from_secs(1);
    memory().clear_seen.insert(record.hold_id.clone(), at);
}

/// Each rule over one hold: what the pass decides, which keys and notice rows it makes.
#[test]
fn hold_resolve_rules_delete_only_the_exact_frame_and_deliver_only_on_its_digest() {
    use Disposition::*;
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let fresh = |mut record: HoldRecord| {
        record.recorded_at = chrono::Utc::now().to_rfc3339();
        record.outcome_at = record.outcome_at.map(|_| record.recorded_at.clone());
        record
    };
    let unseen = |mut record: HoldRecord| {
        record.screen_exact = false;
        record
    };
    let other = |mut record: HoldRecord| {
        record.logical_key = "other-session".into();
        record
    };
    let same = |record: HoldRecord| record;
    type Case<'a> = (
        &'a str,
        SendOutcome,
        &'a dyn Fn(HoldRecord) -> HoldRecord,
        String,
        &'a str,
    );
    let cases: [(Case, Verdict, usize, usize); 13] = [
        // A4: a person's suffix, a whitespace edit, or an unprovable frame is never cleared.
        (
            (
                "suffix",
                SendOutcome::EnterRefused,
                &same,
                shown(&format!("{FRAME} and more")),
                "",
            ),
            Verdict::Kept,
            0,
            0,
        ),
        (
            (
                "spaces",
                SendOutcome::EnterRefused,
                &same,
                shown("[User: a]  hold me"),
                "",
            ),
            Verdict::Kept,
            0,
            0,
        ),
        (
            (
                "unexact",
                SendOutcome::EnterRefused,
                &unseen,
                shown(FRAME),
                "",
            ),
            Verdict::Kept,
            0,
            0,
        ),
        // A3: no empty read decides before the paste settles or, if Enter may have gone, the receipt.
        (
            ("settle", SendOutcome::EnterRefused, &fresh, shown(""), ""),
            Verdict::Kept,
            0,
            0,
        ),
        (
            (
                "receipt",
                SendOutcome::EnterSendFailed,
                &fresh,
                shown(""),
                "",
            ),
            Verdict::Kept,
            0,
            0,
        ),
        // A9: in flight over an empty composer releases unconfirmed after its one notice row.
        (
            ("inflight", SendOutcome::InFlight, &same, shown(""), ""),
            Verdict::Clear(Some(Unconfirmed)),
            0,
            1,
        ),
        (
            ("refused", SendOutcome::EnterRefused, &same, shown(""), ""),
            Verdict::Clear(Some(Undelivered)),
            0,
            0,
        ),
        // A5, A9: only the frame's digest after the hold's offset is delivery.
        (
            ("digest", SendOutcome::InFlight, &same, shown(""), FRAME),
            Verdict::Clear(Some(Delivered)),
            0,
            0,
        ),
        (
            (
                "other",
                SendOutcome::InFlight,
                &same,
                shown(FRAME),
                "[User: b] other",
            ),
            Verdict::Kept,
            FRAME.chars().count(),
            0,
        ),
        // A10 and rule 1: another session's hold, an unconfirmed execution or a running turn.
        (
            ("session", SendOutcome::EnterRefused, &other, shown(""), ""),
            Verdict::Kept,
            0,
            0,
        ),
        (
            (
                "busy",
                SendOutcome::EnterRefused,
                &same,
                pane("", "", " · esc to interrupt"),
                "",
            ),
            Verdict::Kept,
            0,
            0,
        ),
        // A5: a stash is unreadable, never read as our text or an empty composer.
        (
            (
                "stash",
                SendOutcome::EnterRefused,
                &same,
                pane("› stashed", "", ""),
                "",
            ),
            Verdict::Kept,
            0,
            0,
        ),
        (
            ("clear", SendOutcome::EnterRefused, &same, shown(FRAME), ""),
            Verdict::Kept,
            FRAME.chars().count(),
            0,
        ),
    ];
    for ((nonce, outcome, shape, screen, transcript), verdict, keys, notices) in cases {
        let path = root.path().join(format!("{nonce}.jsonl"));
        std::fs::write(
            &path,
            if transcript.is_empty() {
                String::new()
            } else {
                user_record(transcript)
            },
        )
        .unwrap();
        write(&shape(hold(nonce, outcome, &path))).unwrap();
        let fake = Fake::on(&screen);
        assert_eq!(
            resolve("hold-session", nonce, &fake),
            Ok(verdict),
            "{nonce}"
        );
        assert_eq!(
            (fake.keys.get(), fake.notices.borrow().len()),
            (keys, notices),
            "{nonce}"
        );
        assert_eq!(
            read(nonce).unwrap().is_none(),
            verdict != Verdict::Kept,
            "{nonce}"
        );
    }
    let unconfirmed = Fake {
        unconfirmed: true,
        ..Fake::on(&shown(""))
    };
    write(&hold("gate", SendOutcome::EnterRefused, root.path())).unwrap();
    assert_eq!(
        resolve("hold-session", "gate", &unconfirmed),
        Ok(Verdict::Kept)
    );
    // A4: a v1 hold over text is never cleared; over an empty composer it ends with no claim.
    let v1 = hold_path("legacy").unwrap();
    std::fs::write(&v1, "2026-11-16T09:00:00+00:00").unwrap();
    let text = Fake::on(&shown(FRAME));
    assert_eq!(resolve("hold-session", "legacy", &text), Ok(Verdict::Kept));
    assert_eq!(text.keys.get(), 0);
    let empty = Fake::on(&shown(""));
    assert_eq!(
        resolve("hold-session", "legacy", &empty),
        Ok(Verdict::Clear(Some(Unknown)))
    );
}

/// A9, A15: after our exact read the composer empties without our confirmed keys (a restart
/// between keys and confirmation, or a person's Enter); its digest is delivery, else unconfirmed.
#[test]
fn hold_resolve_clearing_phase_empty_is_delivered_on_digest_else_unconfirmed() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    for (nonce, transcript, disposition, rows) in [
        ("late", String::new(), Disposition::Unconfirmed, 1),
        ("person", user_record(FRAME), Disposition::Delivered, 0),
    ] {
        let path = root.path().join(format!("{nonce}.jsonl"));
        std::fs::write(&path, "").unwrap();
        let mut record = hold(nonce, SendOutcome::EnterRefused, &path);
        record.unsent_proof = UnsentProof::ExactTextSeen;
        record.cleanup_phase = CleanupPhase::Clearing;
        write(&record).unwrap();
        let fake = Fake::on(&shown(""));
        assert_eq!(
            resolve("hold-session", nonce, &fake),
            Ok(Verdict::Kept),
            "receipt window"
        );
        std::fs::write(&path, transcript).unwrap();
        backdate_clear_receipt(&record);
        assert_eq!(
            resolve("hold-session", nonce, &fake),
            Ok(Verdict::Clear(Some(disposition)))
        );
        assert_eq!((fake.keys.get(), fake.notices.borrow().len()), (0, rows));
    }
}

/// A1, A2: the reservation a refused Enter left blocks the next input until the hold's release
/// settles exactly it; the next input then reserves and submits once.
#[test]
fn hold_resolve_enter_refused_from_real_reserve_clears_paste_settles_reservation_next_input_submits_once()
 {
    use crate::services::claude::herdr_turn::composer::{
        READY_WAIT_FOR_TESTS, take_touched, when_idle,
    };
    use crate::services::claude_tui::host_input::{InputRefusal, StopCause};
    use crate::services::claude_tui::readiness;
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    READY_WAIT_FOR_TESTS.with(|wait| wait.set(Some(Duration::from_millis(300))));
    let (path, other) = (
        root.path().join("turn.jsonl"),
        root.path().join("other.jsonl"),
    );
    readiness::tests::pre_source("hold-session", &path);
    readiness::tests::pre_source("other-session", &other);
    let others = readiness::reserve(readiness::observe("other-session", Some(&other)).unwrap());
    let empty = || Some(shown(""));
    let refused = InputRun::Indeterminate {
        confirmed: 1,
        cause: StopCause::Refused(InputRefusal::Unknown),
    };
    assert_eq!(
        when_idle("hold-session", &path, empty, None, || refused.clone()),
        refused
    );
    let record = hold("refused", SendOutcome::EnterRefused, &path);
    write(&record).unwrap();
    entrust(
        &record.hold_id,
        take_touched().expect("a touched run keeps its reservation"),
    );
    let sends = Cell::new(0);
    let send = || {
        sends.set(sends.get() + 1);
        InputRun::Applied
    };
    assert_eq!(
        when_idle("hold-session", &path, empty, None, send),
        InputRun::Refused(InputRefusal::Unknown)
    );
    let fake = Fake::on(&shown(FRAME));
    assert_eq!(resolve("hold-session", "refused", &fake), Ok(Verdict::Kept));
    assert_eq!(
        read("refused")
            .unwrap()
            .map(|held| matches!(held, Held::V2(r) if r.cleanup_phase == CleanupPhase::Cleared)),
        Some(true)
    );
    backdate_clear_receipt(&record);
    let released = resolve("hold-session", "refused", &fake);
    assert_eq!(released, Ok(Verdict::Clear(Some(Disposition::Undelivered))));
    assert_eq!(
        when_idle("hold-session", &path, empty, None, send),
        InputRun::Applied
    );
    assert_eq!((sends.get(), fake.keys.get()), (1, FRAME.chars().count()));
    assert!(
        others.is_some_and(|other| other.current()),
        "another source's reservation stays"
    );
    READY_WAIT_FOR_TESTS.with(|wait| wait.set(None));
}

/// A6, A7, A7a: an unreadable composer notices once per hold, retries a failed enqueue, and the
/// later unconfirmed release reuses that row; the watch wakes when the read changes.
#[test]
fn hold_resolve_pre_noticed_inflight_then_empty_unconfirmed_once() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    write(&hold("noticed", SendOutcome::InFlight, root.path())).unwrap();
    let fake = Fake::on("no composer");
    fake.failing_notices.set(1);
    for _ in 0..2 {
        assert_eq!(resolve("hold-session", "noticed", &fake), Ok(Verdict::Kept));
    }
    let mut seen = None;
    assert_eq!(
        judge("hold-session", "noticed", &fake, &mut seen),
        Ok(Verdict::Kept)
    );
    let screen = std::sync::Arc::new(std::sync::Mutex::new(Some("no composer".to_owned())));
    let shared = screen.clone();
    let mut probe = watch("noticed".into(), seen, move || {
        shared.lock().unwrap().clone()
    });
    let Some(Held::V2(record)) = read("noticed").unwrap() else {
        panic!("v2 hold")
    };
    assert!(record.notice_intent_at.is_some() && record.notice_enqueued_at.is_some());
    assert_eq!(*fake.notices.borrow(), ["noticed-hold"]);
    assert_eq!(probe(), Screen::Unknown);
    *screen.lock().unwrap() = Some(shown(""));
    assert_eq!(
        probe(),
        Screen::Ready,
        "a readable composer wakes the held front"
    );
    fake.set(&shown(""));
    assert_eq!(
        resolve("hold-session", "noticed", &fake),
        Ok(Verdict::Clear(Some(Disposition::Unconfirmed)))
    );
    assert_eq!(
        fake.notices.borrow().len(),
        1,
        "the release posts no second row"
    );
}

/// A11: after a busy input's receipt the session's hold is judged once and released on digest.
#[test]
fn busy_inject_applied_then_resolve_releases_on_digest_receipt() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let path = root.path().join("busy.jsonl");
    std::fs::write(&path, user_record(FRAME)).unwrap();
    write(&hold("busy", SendOutcome::EnterSendFailed, &path)).unwrap();
    after_busy("hold-session", "busy", &Fake::on(&shown("")));
    assert_eq!(read("busy"), Ok(None));
}

/// A busy input judges only the hold of the execution it wrote to, never another of its session.
#[test]
fn busy_inject_judges_only_its_own_execution_hold() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    write(&hold("orphan", SendOutcome::EnterRefused, root.path())).unwrap();
    let before = read("orphan").unwrap();
    let fake = Fake::on(&shown(FRAME));
    after_busy("hold-session", "successor", &fake);
    assert_eq!((read("orphan").unwrap(), fake.keys.get()), (before, 0));
}

/// A composer that recovers while the pass secures its notice row still wakes the watch the
/// pass leaves, and the wake survives until taken; the next pass releases and the input goes.
#[test]
fn hold_resolve_recovery_during_notice_enqueue_wakes_after_registration() {
    use crate::services::claude::herdr_turn::composer::when_idle;
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let path = root.path().join("turn.jsonl");
    crate::services::claude_tui::readiness::tests::pre_source("hold-session", &path);
    write(&hold("recovered", SendOutcome::InFlight, &path)).unwrap();
    let fake = Fake::on("no composer");
    assert_eq!(
        resolve("hold-session", "recovered", &fake),
        Ok(Verdict::Kept)
    );
    *fake.after_notice.borrow_mut() = Some(shown(""));
    let mut seen = None;
    assert_eq!(
        judge("hold-session", "recovered", &fake, &mut seen),
        Ok(Verdict::Kept)
    );
    let Some(Held::V2(record)) = read("recovered").unwrap() else {
        panic!("v2 hold")
    };
    assert!(record.notice_enqueued_at.is_some());
    let screen = std::sync::Arc::new(std::sync::Mutex::new(Some(shown(""))));
    let shared = screen.clone();
    let mut probe = watch("recovered".into(), seen, move || {
        shared.lock().unwrap().clone()
    });
    assert_eq!(
        probe(),
        Screen::Ready,
        "the recovery before registration wakes"
    );
    *screen.lock().unwrap() = Some("no composer".into());
    assert_eq!(probe(), Screen::Ready, "a wake not yet taken is kept");
    let released = resolve("hold-session", "recovered", &fake);
    assert_eq!(released, Ok(Verdict::Clear(Some(Disposition::Unconfirmed))));
    let sends = Cell::new(0);
    let send = || {
        sends.set(sends.get() + 1);
        InputRun::Applied
    };
    let next = when_idle("hold-session", &path, || Some(shown("")), None, send);
    assert_eq!(next, InputRun::Applied);
    assert_eq!(
        (fake.notices.borrow().len(), fake.keys.get(), sends.get()),
        (1, 0, 1)
    );
}
