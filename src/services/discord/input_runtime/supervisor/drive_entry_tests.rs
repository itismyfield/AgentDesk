//! The drive behind the supervisor's real entry: boot to admission, S8, then passes over a fake
//! binding log, pane and reaction port, with real transcripts and a real ledger.

use super::*;
use crate::services::discord::input_runtime::supervisor::drive::{
    ChannelDrive, DrivePorts, Eligibility, HOLD_GRACE, SupervisorCmd, TICK, ViewKey,
};
use crate::services::tui_input::actor::pane::{Pane, SendOutcome};
use crate::services::tui_input::rows::{AbandonReason, DoneReason, HeldReason};
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::writer::binding::{BindingCause, BindingEvidence, BindingTarget};
use crate::services::tui_o::writer::input_facts::reactions::ReactionPort;
use std::future::Future;
use std::io::Write;
use std::pin::Pin;

#[path = "receipt_entry_tests.rs"]
mod receipt_entry;

/// A named binding state a test plants before the drive's first pass.
type Fixture = (&'static str, fn(&Rig));

const READY: &str = "\
⏺ Done.

────────────────────────────────────────────────────────────
❯\u{00a0}
────────────────────────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle)";
// A Discord message ID; smaller keys are synthetic inputs with nothing to react on.
const REAL: u64 = 1_300_000_000_000_000_000;

// What the fake pane and reaction port observed; `record` is where the TUI writes a prompt.
// The next submit signals `entered` and waits on `latch`; a dropped pane signals `dropped`.
#[derive(Default)]
struct Screen {
    busy: AtomicBool,
    draft: AtomicBool,
    refuse: AtomicBool,
    panic: AtomicBool,
    unreachable: AtomicUsize,
    sent: Mutex<Vec<String>>,
    off_worker: AtomicUsize,
    record: Mutex<Option<PathBuf>>,
    reactions: Mutex<Vec<u64>>,
    reactions_fail: AtomicBool,
    latch: Mutex<Option<mpsc::Receiver<()>>>,
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    dropped: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

struct TestPane(Arc<Screen>);

impl Drop for TestPane {
    fn drop(&mut self) {
        if let Some(dropped) = self.0.dropped.lock().unwrap().take() {
            let _ = dropped.send(());
        }
    }
}

impl Pane for TestPane {
    fn capture(&mut self) -> Result<String, String> {
        assert!(!self.0.panic.load(Ordering::SeqCst), "the pane worker died");
        if fence::require_worker().is_err() {
            self.0.off_worker.fetch_add(1, Ordering::SeqCst);
        }
        Ok(if self.0.busy.load(Ordering::SeqCst) {
            "loading".into()
        } else if self.0.draft.load(Ordering::SeqCst) {
            READY.replace("❯\u{00a0}", "❯\u{00a0}human draft")
        } else {
            READY.into()
        })
    }
    fn submit(&mut self, text: &str) -> SendOutcome {
        if self.0.refuse.load(Ordering::SeqCst) {
            return SendOutcome::Refused("oversized prompt".into());
        }
        if let Some(latch) = self.0.latch.lock().unwrap().take() {
            if let Some(entered) = self.0.entered.lock().unwrap().take() {
                let _ = entered.send(());
            }
            let _ = latch.recv();
        }
        self.0.sent.lock().unwrap().push(text.to_owned());
        if let Some(path) = self.0.record.lock().unwrap().as_ref() {
            user(path, text);
        }
        SendOutcome::Sent
    }
    fn execution_nonce(&self) -> Option<String> {
        Some("n1".into())
    }
}

struct DriveFake(Arc<Screen>);

impl DrivePorts for DriveFake {
    type Pane = TestPane;
    type Reactions = Self;
    fn pane(&mut self, _: &ViewKey) -> Option<TestPane> {
        let left = self.0.unreachable.load(Ordering::SeqCst);
        if left > 0 {
            self.0.unreachable.store(left - 1, Ordering::SeqCst);
            return None;
        }
        Some(TestPane(self.0.clone()))
    }
    fn reactions(&self) -> &Self {
        self
    }
}

impl ReactionPort for DriveFake {
    fn set(
        &self,
        _: u64,
        message: u64,
        _: char,
        _: bool,
    ) -> impl Future<Output = Result<(), String>> + Send {
        self.0.reactions.lock().unwrap().push(message);
        let fail = self.0.reactions_fail.load(Ordering::SeqCst);
        async move { if fail { Err("http 503".into()) } else { Ok(()) } }
    }
}

fn append_line(path: &Path, value: serde_json::Value) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{value}").unwrap();
}

fn user(path: &Path, text: &str) {
    let id = uuid::Uuid::new_v4().to_string();
    let record = json!({"type":"user","uuid":id,"message":{"role":"user","content":text}});
    append_line(path, record);
}

fn turn_end(source: &SourceId) {
    append_line(
        &source.path,
        json!({"type":"system","subtype":"turn_duration"}),
    );
}

/// A parent transcript with no turn yet; a padded one takes a fresh reader two polls to finish.
fn transcript(rig: &Rig, session: &str, padded: bool) -> SourceId {
    let path = rig.root.join(format!("{session}.jsonl"));
    let mut text = String::from("{\"type\":\"summary\"}\n");
    if padded {
        let pad = format!(
            "{{\"type\":\"summary\",\"pad\":\"{}\"}}\n",
            "x".repeat(1000)
        );
        text.push_str(&pad.repeat(1100));
    }
    std::fs::write(&path, text).unwrap();
    source_id_for(session, &path).unwrap()
}

fn idle_transcript(rig: &Rig, session: &str, padded: bool) -> SourceId {
    let source = transcript(rig, session, padded);
    turn_end(&source);
    source
}

fn push(rig: &Rig, nonce: &str, record: BindingRecord) -> u64 {
    let seq = rig.binding.seq() + 1;
    rig.binding.events.lock().unwrap().push(BindingEvent {
        seq,
        channel_id: rig.channel,
        provider: ShadowProvider::Claude,
        tmux_session: tmux(rig.channel),
        execution_nonce: nonce.into(),
        record,
        committed_at: chrono::Utc::now(),
    });
    rig.binding.tx.lock().unwrap().send_replace(seq);
    seq
}

fn bound(old: Option<&SourceId>, new: BindingTarget) -> BindingRecord {
    BindingRecord::Bound {
        old: old.cloned(),
        new,
        cause: BindingCause::Startup,
        parent_hint: None,
        evidence: BindingEvidence {
            hook_event: "SessionStart".into(),
            received_at: chrono::Utc::now(),
            reclaims: false,
        },
    }
}

fn source(old: Option<&SourceId>, new: &SourceId) -> BindingRecord {
    bound(old, BindingTarget::Source(new.clone()))
}

fn pending(session: &str, path: &Path) -> BindingRecord {
    let new = BindingTarget::Pending {
        payload_session_id: session.into(),
        payload_transcript_path: path.to_path_buf(),
    };
    bound(None, new)
}

fn rejected() -> BindingRecord {
    BindingRecord::Rejected {
        detail: "late bind".into(),
    }
}

fn state(rig: &Rig, key: u64) -> RowState {
    rows(&rig.root, rig.channel).row(key).unwrap().state
}

fn held_line(rig: &Rig, reason: &str, head: u64, held: usize) -> String {
    let at = format!("provider=claude channel={}", rig.channel);
    format!("input_reconcile_required {at} reason={reason} head={head} held={held}")
}

fn input_lines(rig: &Rig) -> Vec<String> {
    let tag = format!("channel={} reason=", rig.channel);
    (rig.health().into_iter())
        .filter(|line| line.starts_with("input_reconcile_required") && line.contains(&tag))
        .collect()
}

/// Boots to admission, then receives `keys` through the supervisor's own slot.
async fn admitted(rig: &Rig, keys: &[u64]) -> Supervisor<Fake> {
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    assert_eq!(supervisor.boot().await, Landing::Admitted);
    let ledger = supervisor.slot().get().unwrap();
    for &key in keys {
        let input = json!({ "text": format!("input {key}") });
        ledger
            .append_entry(&Entry::Received { key, input }, &[])
            .unwrap();
    }
    supervisor
}

struct Driven {
    supervisor: Supervisor<Fake>,
    drive: ChannelDrive<DriveFake>,
    screen: Arc<Screen>,
    at: std::time::Instant,
}

impl Driven {
    /// S8 and its first pass over the log replayed from the start.
    async fn start(mut supervisor: Supervisor<Fake>, screen: &Arc<Screen>) -> Self {
        let ports = DriveFake(screen.clone());
        let (drive, replay) = supervisor.start_drive(ports).unwrap();
        let at = std::time::Instant::now();
        let mut driven = Self {
            supervisor,
            drive,
            screen: screen.clone(),
            at,
        };
        driven.pass(replay).await;
        driven
    }
    async fn pass(&mut self, woke: Result<Vec<BindingEvent>, String>) {
        (self.supervisor).tick(&mut self.drive, woke, self.at).await;
    }
    /// One pass over everything the log gained since the last read, as a single wake.
    async fn wake(&mut self) {
        let woke = self.supervisor.cursor().unwrap().read();
        self.pass(woke).await;
    }
    /// Passes at the same instant, so no backoff or window elapses between them.
    async fn idle(&mut self, passes: usize) {
        for _ in 0..passes {
            self.pass(Ok(Vec::new())).await;
        }
    }
    /// Passes ten seconds apart until `span` of drive time has gone by.
    async fn wait(&mut self, span: Duration) {
        let end = self.at + span;
        while self.at < end {
            self.at += Duration::from_secs(9);
            self.pass(Ok(Vec::new())).await;
        }
    }
    fn sent(&self) -> usize {
        self.screen.sent.lock().unwrap().len()
    }
    fn release(self) -> bool {
        let Self {
            mut supervisor,
            drive,
            ..
        } = self;
        supervisor.stop_drive(drive);
        supervisor.release()
    }
}

#[tokio::test]
async fn pending_binding_opens_the_gate_once_holds_input_and_its_resolution_submits_once() {
    let rig = Rig::new(6_325_801);
    let path = rig.root.join("next.jsonl");
    let waiting = push(&rig, "n1", pending("next", &path));
    let supervisor = admitted(&rig, &[41]).await;
    assert_eq!(
        rig.gate.mode(),
        Mode::Frozen,
        "admission alone opens nothing"
    );
    let screen = Arc::new(Screen::default());
    let mut driven = Driven::start(supervisor, &screen).await;
    assert_eq!(rig.gate.mode(), Mode::LedgerOpen);
    driven.wait(2 * HOLD_GRACE).await;
    assert_eq!(
        driven.drive.counts.opens, 1,
        "one open per readiness episode"
    );
    assert_eq!((driven.sent(), driven.drive.counts.builds), (0, 0));
    assert_eq!(
        input_lines(&rig),
        [held_line(&rig, "binding_pending", 41, 1)]
    );
    // The transcript appears without a finished turn: facts stay Unknown until one closes.
    let next = transcript(&rig, "next", false);
    *screen.record.lock().unwrap() = Some(next.path.clone());
    let resolved = BindingRecord::Resolved {
        resolves_seq: waiting,
        source: next.clone(),
    };
    push(&rig, "n1", resolved);
    driven.wake().await;
    driven.idle(3).await;
    assert_eq!(driven.sent(), 0);
    turn_end(&next);
    driven.idle(3).await;
    assert_eq!(driven.sent(), 1, "exactly one submission after Idle");
    assert_eq!(state(&rig, 41), RowState::Running);
    assert!(input_lines(&rig).is_empty(), "the hold clears");
    assert_eq!(screen.off_worker.load(Ordering::SeqCst), 0);
    assert_eq!(driven.drive.counts.opens, 1);
    assert!(driven.release());
}

#[tokio::test]
async fn unready_bindings_hold_input_without_writes_and_report_after_grace_per_boot() {
    let fixtures: [Fixture; 5] = [
        ("binding_absent", |_| {}),
        ("binding_pending", |rig| {
            push(rig, "n1", pending("p", &rig.root.join("p.jsonl")));
        }),
        ("binding_rejected", |rig| {
            let a = idle_transcript(rig, "a", false);
            push(rig, "n1", source(None, &a));
            push(rig, "n1", rejected());
        }),
        ("binding_empty_nonce", |rig| {
            let a = idle_transcript(rig, "a", false);
            push(rig, "", source(None, &a));
        }),
        ("facts_unknown", |rig| {
            let a = transcript(rig, "a", false);
            push(rig, "n1", source(None, &a));
        }),
    ];
    for (index, (reason, bind)) in fixtures.into_iter().enumerate() {
        let rig = Rig::new(6_325_810 + index as u64);
        bind(&rig);
        let screen = Arc::new(Screen::default());
        let mut supervisor = Some(admitted(&rig, &[41]).await);
        for boot in 1..=2 {
            let first = match supervisor.take() {
                Some(first) => first,
                None => admitted(&rig, &[]).await,
            };
            let mut driven = Driven::start(first, &screen).await;
            let wal = wal_bytes(&rig.root, rig.channel);
            driven.wait(2 * HOLD_GRACE).await;
            assert_eq!(driven.sent(), 0, "{reason}");
            assert_eq!(wal_bytes(&rig.root, rig.channel), wal, "{reason}");
            assert_eq!(state(&rig, 41), RowState::Received, "{reason}");
            assert_eq!(input_lines(&rig), [held_line(&rig, reason, 41, 1)]);
            assert_eq!(rig.world.notices().len(), boot, "{reason}: one per boot");
            assert!(rig.world.notices()[boot - 1].contains("message_id:41"));
            let concrete = reason == "facts_unknown";
            assert_eq!(
                driven.drive.counts.builds,
                usize::from(concrete),
                "{reason}"
            );
            assert_eq!(driven.drive.counts.held_steps, 0, "{reason}");
            assert!(driven.release());
        }
    }
}

/// Source A saw Idle on a busy pane, then Rejected and a rebind, in one wake or two. Returns
/// submissions after the rebind pass and the next one, creation seqs and actor builds.
async fn rebound(channel: u64, merged: bool, to_b: bool) -> (usize, usize, Vec<u64>, usize) {
    let rig = Rig::new(channel);
    let a = idle_transcript(&rig, "a", true);
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    screen.busy.store(true, Ordering::SeqCst);
    let mut driven = Driven::start(admitted(&rig, &[41]).await, &screen).await;
    driven.idle(2).await;
    assert!(matches!(
        driven.drive.view().identity(),
        Some((ref s, _)) if *s == a
    ));
    let target = match to_b {
        true => idle_transcript(&rig, "b", true),
        false => a.clone(),
    };
    push(&rig, "n1", rejected());
    if !merged {
        driven.wake().await;
        assert_eq!(driven.drive.counts.builds, 1, "Rejected builds nothing");
        assert_eq!(driven.drive.view().eligibility(), Eligibility::Rejected);
    }
    push(&rig, "n1", source(Some(&a), &target));
    screen.busy.store(false, Ordering::SeqCst);
    driven.wake().await;
    let at_rebind = driven.sent();
    driven.idle(3).await;
    let counts = &driven.drive.counts;
    let (creations, builds) = (counts.creations.clone(), counts.builds);
    assert!(driven.release());
    (
        at_rebind,
        screen.sent.lock().unwrap().len(),
        creations,
        builds,
    )
}

#[tokio::test]
async fn a_rejected_binding_ends_its_facts_and_only_the_rebound_instances_own_idle_submits() {
    for (index, (merged, to_b)) in [(false, false), (true, false), (true, true)]
        .into_iter()
        .enumerate()
    {
        let case = format!("merged={merged} to_b={to_b}");
        let (at_rebind, after, creations, builds) =
            rebound(6_325_820 + index as u64, merged, to_b).await;
        assert_eq!(
            at_rebind, 0,
            "{case}: no submission before the fresh facts' Idle"
        );
        assert_eq!(after, 1, "{case}");
        assert_eq!((creations, builds), (vec![1, 3], 2), "{case}");
    }
}

#[tokio::test]
async fn a_pending_bind_drops_the_instance_whose_facts_already_saw_idle() {
    let rig = Rig::new(6_325_830);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    screen.busy.store(true, Ordering::SeqCst);
    let mut driven = Driven::start(admitted(&rig, &[41]).await, &screen).await;
    driven.idle(1).await;
    push(&rig, "n1", pending("next", &rig.root.join("next.jsonl")));
    screen.busy.store(false, Ordering::SeqCst);
    driven.wake().await;
    driven.idle(3).await;
    assert_eq!(driven.drive.counts.held_steps, 0);
    assert_eq!(driven.sent(), 0);
    assert!(driven.release());
}

#[tokio::test]
async fn each_binding_change_rebuilds_facts_and_one_actor_and_old_idle_never_submits() {
    let rig = Rig::new(6_325_831);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    screen.busy.store(true, Ordering::SeqCst);
    screen.unreachable.store(2, Ordering::SeqCst);
    let mut driven = Driven::start(admitted(&rig, &[41]).await, &screen).await;
    driven.idle(1).await;
    assert_eq!(
        driven.drive.counts.builds, 0,
        "an unreachable pane builds no actor"
    );
    driven.idle(4).await;
    assert_eq!(
        driven.drive.counts.builds, 1,
        "one actor per concrete binding"
    );
    // Rotated to B, whose transcript has no finished turn: A's Idle must not submit.
    let b = transcript(&rig, "b", false);
    push(&rig, "n1", source(Some(&a), &b));
    screen.busy.store(false, Ordering::SeqCst);
    driven.wake().await;
    driven.idle(2).await;
    assert_eq!(driven.sent(), 0);
    assert_eq!(driven.drive.counts.builds, 2);
    let path = rig.root.join("c.jsonl");
    let waiting = push(&rig, "n1", pending("c", &path));
    driven.wake().await;
    let c = idle_transcript(&rig, "c", false);
    let resolved = BindingRecord::Resolved {
        resolves_seq: waiting,
        source: c,
    };
    push(&rig, "n1", resolved);
    driven.wake().await;
    driven.idle(2).await;
    assert_eq!(driven.sent(), 1);
    assert_eq!(driven.drive.counts.builds, 3);
    assert_eq!(driven.drive.counts.creations, [1, 2, 4]);
    assert!(driven.release());
}

#[tokio::test]
async fn held_readiness_closes_the_gate_once_and_reopens_it_once_on_recovery() {
    // A clear held on an unconfirmed reset retries only when the binding log advances.
    let rig = Rig::new(6_325_840);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    screen.busy.store(true, Ordering::SeqCst);
    let mut driven = Driven::start(admitted(&rig, &[41]).await, &screen).await;
    assert_eq!(rig.gate.mode(), Mode::LedgerOpen);
    let ledger = driven.supervisor.slot().get().unwrap();
    rig.world.durable_ticket(rig.channel, ledger, &[]);
    rig.world.reset(false, None);
    let at = driven.at;
    (driven.supervisor).clear(&mut driven.drive, at).await;
    screen.busy.store(false, Ordering::SeqCst);
    driven.idle(10).await;
    let counts = &driven.drive.counts;
    assert_eq!((counts.opens, counts.holds), (1, 1));
    assert_eq!(rig.gate.mode(), Mode::Held);
    assert_eq!(
        driven.sent(),
        0,
        "nothing submits while the clear is unsettled"
    );
    push(&rig, "n1", source(Some(&a), &a));
    driven.wake().await;
    assert_eq!(rig.gate.mode(), Mode::LedgerOpen);
    driven.idle(3).await;
    let counts = &driven.drive.counts;
    assert_eq!((counts.opens, counts.holds), (2, 1));
    assert_eq!(driven.sent(), 1);
    assert!(driven.release());
    drop(rig);

    // A handle a failed append left unusable cannot reopen: held until a reopen succeeds.
    let rig = Rig::new(6_325_841);
    push(&rig, "n1", pending("p", &rig.root.join("p.jsonl")));
    let screen = Arc::new(Screen::default());
    let mut driven = Driven::start(admitted(&rig, &[41]).await, &screen).await;
    let recording = Recording::start(&rig.root);
    recording.arm(Some(1));
    let entry = Entry::Received {
        key: 42,
        input: json!({ "text": "42" }),
    };
    let ledger = driven.supervisor.slot().get().unwrap();
    assert!(ledger.append_entry(&entry, &[]).is_err());
    drop(recording);
    let dir = crate::services::tui_input::ledger::dir(&rig.root, rig.channel);
    let aside = dir.with_extension("aside");
    std::fs::rename(&dir, &aside).unwrap();
    std::fs::write(&dir, b"").unwrap();
    driven.idle(10).await;
    let counts = &driven.drive.counts;
    assert_eq!((counts.opens, counts.holds), (1, 1));
    assert_eq!(rig.gate.mode(), Mode::Held);
    let line = format!("ledger_unreadable provider=claude channel={}", rig.channel);
    assert!(rig.health().contains(&line));
    std::fs::remove_file(&dir).unwrap();
    std::fs::rename(&aside, &dir).unwrap();
    driven.idle(3).await;
    let counts = &driven.drive.counts;
    assert_eq!((counts.opens, counts.holds), (2, 1));
    assert_eq!(rig.gate.mode(), Mode::LedgerOpen);
    assert!(!rig.health().contains(&line));
    assert!(driven.release());
}

#[tokio::test]
async fn reactions_skip_synthetic_inputs_and_a_failed_one_changes_no_ledger_record() {
    let rig = Rig::new(6_325_850);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    *screen.record.lock().unwrap() = Some(a.path.clone());
    screen.reactions_fail.store(true, Ordering::SeqCst);
    let mut driven = Driven::start(admitted(&rig, &[41, REAL]).await, &screen).await;
    driven.idle(2).await;
    assert_eq!(state(&rig, 41), RowState::Running);
    turn_end(&a);
    driven.idle(1).await;
    assert_eq!(state(&rig, 41), RowState::Done(DoneReason::Completed));
    driven.idle(2).await;
    assert_eq!(state(&rig, REAL), RowState::Running);
    // Someone typed before our turn closed: the row is held and the person is told once.
    user(&a.path, "typed in the pane");
    turn_end(&a);
    driven.idle(1).await;
    let held = RowState::Held(HeldReason::Ambiguous);
    assert_eq!(state(&rig, REAL), held);
    let wal = wal_bytes(&rig.root, rig.channel);
    driven.idle(3).await;
    assert_eq!(wal_bytes(&rig.root, rig.channel), wal);
    assert_eq!(
        *screen.reactions.lock().unwrap(),
        [REAL],
        "only the real input"
    );
    let notices = rig.world.notices();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains(&format!("입력 `{REAL}`의 처리 결과를 확인할 수 없어")));
    assert!(driven.release());
}

#[tokio::test]
async fn an_open_turn_is_no_hold_and_an_in_flight_row_still_steps_under_it() {
    let rig = Rig::new(6_325_860);
    let a = transcript(&rig, "a", false);
    user(&a.path, "an earlier prompt");
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    *screen.record.lock().unwrap() = Some(a.path.clone());
    let mut driven = Driven::start(admitted(&rig, &[41]).await, &screen).await;
    driven.wait(2 * HOLD_GRACE).await;
    assert_eq!(driven.sent(), 0);
    assert!(input_lines(&rig).is_empty());
    assert!(rig.world.notices().is_empty());
    turn_end(&a);
    driven.idle(1).await;
    assert_eq!(state(&rig, 41), RowState::AwaitTurn);
    // Our own prompt opened the turn; the AwaitTurn head confirms under that Open fact.
    driven.idle(1).await;
    assert_eq!(state(&rig, 41), RowState::Running);
    assert!(driven.release());
}

#[tokio::test(start_paused = true)]
async fn the_drive_loop_submits_pauses_for_a_clear_and_holds_the_gate_when_it_ends() {
    let rig = Rig::new(6_325_870);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    let mut supervisor = admitted(&rig, &[41]).await;
    let (commands, received) = tokio::sync::mpsc::channel(1);
    let ports = DriveFake(screen.clone());
    let script = async {
        let sent = || screen.sent.lock().unwrap().len();
        while sent() == 0 {
            tokio::time::sleep(TICK).await;
        }
        assert_eq!(rig.gate.mode(), Mode::LedgerOpen);
        commands.send(SupervisorCmd::Clear).await.unwrap();
        tokio::time::sleep(3 * TICK).await;
        assert_eq!(
            rig.gate.mode(),
            Mode::LedgerOpen,
            "a clear with nothing to cut resumes"
        );
        drop(commands);
    };
    tokio::join!(supervisor.run(ports, received), script);
    assert_eq!(rig.gate.mode(), Mode::Held);
    assert_eq!(screen.sent.lock().unwrap().len(), 1);
    assert!(supervisor.release());
}

/// Polls the live loop until `done` holds; the loop must not end first.
async fn until(run: &mut Pin<Box<impl Future<Output = ()>>>, done: impl Fn() -> bool) {
    let wait = async {
        while !done() {
            tokio::task::yield_now().await;
        }
    };
    let raced = async {
        tokio::select! {
            () = run.as_mut() => panic!("the live loop ended"),
            () = wait => {}
        }
    };
    (tokio::time::timeout(Duration::from_secs(10), raced).await).expect("the loop got there");
}

#[tokio::test]
async fn failed_row_notice_keeps_health_and_retries_until_the_row_is_settled() {
    let rig = Rig::new(6_325_880);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    screen.refuse.store(true, Ordering::SeqCst);
    screen.reactions_fail.store(true, Ordering::SeqCst);
    rig.world.failing_delivery.store(true, Ordering::SeqCst);
    let mut driven = Driven::start(admitted(&rig, &[REAL, 42]).await, &screen).await;
    assert_eq!(state(&rig, REAL), RowState::Held(HeldReason::NotReady));
    assert_eq!(driven.sent(), 0);
    let line = held_line(&rig, "row_held", REAL, 2);
    assert_eq!(input_lines(&rig), [line.as_str()], "reported at once");
    driven.wait(2 * HOLD_GRACE).await;
    assert!(
        rig.world.undelivered.load(Ordering::SeqCst) > 1,
        "each pass retries"
    );
    assert_eq!(input_lines(&rig), [line.as_str()]);
    rig.world.failing_delivery.store(false, Ordering::SeqCst);
    driven.idle(3).await;
    let notices = rig.world.notices();
    assert_eq!(notices.len(), 1, "delivered once, then never again");
    assert!(notices[0].contains(&format!("입력 `{REAL}`의 처리 결과를 확인할 수 없어")));
    assert_eq!(
        input_lines(&rig),
        [line],
        "a delivered Notice settles nothing"
    );
    assert_eq!(
        *screen.reactions.lock().unwrap(),
        [REAL],
        "reacted on the move only"
    );
    // Someone settles the held input: the row behind it goes and the line leaves.
    screen.refuse.store(false, Ordering::SeqCst);
    *screen.record.lock().unwrap() = Some(a.path.clone());
    let settled = Entry::Transition {
        key: REAL,
        state: RowState::Abandoned(AbandonReason::UserClear),
        attempt: None,
    };
    let ledger = driven.supervisor.slot().get().unwrap();
    ledger.append_entry(&settled, &[]).unwrap();
    driven.idle(3).await;
    assert_eq!(driven.sent(), 1);
    assert_eq!(state(&rig, 42), RowState::Running);
    assert!(input_lines(&rig).is_empty());
    assert!(driven.release());
}

#[tokio::test]
async fn lost_worker_closes_gate_before_tick_returns() {
    let rig = Rig::new(6_325_881);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    screen.busy.store(true, Ordering::SeqCst);
    let mut driven = Driven::start(admitted(&rig, &[41]).await, &screen).await;
    assert_eq!(rig.gate.mode(), Mode::LedgerOpen);
    screen.panic.store(true, Ordering::SeqCst);
    driven.idle(1).await;
    assert_eq!(rig.gate.mode(), Mode::Held, "held before the pass returns");
    let at = format!("provider=claude channel={}", rig.channel);
    let lost = format!("turn_transition_held {at} reason=supervisor_lost");
    assert!(rig.health().contains(&lost));
    screen.panic.store(false, Ordering::SeqCst);
    screen.busy.store(false, Ordering::SeqCst);
    driven.idle(3).await;
    assert_eq!(rig.gate.mode(), Mode::Held, "a lost drive never reopens");
    assert_eq!(driven.sent(), 0);
    assert!(driven.release());
}

#[tokio::test]
async fn run_cancellation_holds_gate_even_when_supervisor_remains() {
    let rig = Rig::new(6_325_882);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    screen.busy.store(true, Ordering::SeqCst);
    let mut supervisor = admitted(&rig, &[41]).await;
    let (_commands, received) = tokio::sync::mpsc::channel(1);
    let mut run = Box::pin(supervisor.run(DriveFake(screen), received));
    until(&mut run, || rig.gate.mode() == Mode::LedgerOpen).await;
    drop(run);
    assert_eq!(
        rig.gate.mode(),
        Mode::Held,
        "dropping the loop holds its gate"
    );
    drop(supervisor);
    let lost = format!("channel={} reason=supervisor_lost", rig.channel);
    assert!(rig.health().iter().any(|line| line.contains(&lost)));
}

#[tokio::test]
async fn run_cancellation_mid_step_keeps_the_worker_write() {
    let rig = Rig::new(6_325_883);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    let (release, latch) = mpsc::channel();
    let (entered, inside) = tokio::sync::oneshot::channel();
    let (dropped, finished) = tokio::sync::oneshot::channel();
    *screen.latch.lock().unwrap() = Some(latch);
    *screen.entered.lock().unwrap() = Some(entered);
    let mut supervisor = admitted(&rig, &[41]).await;
    let (_commands, received) = tokio::sync::mpsc::channel(1);
    let mut run = Box::pin(supervisor.run(DriveFake(screen.clone()), received));
    let submitting = async {
        tokio::select! {
            () = &mut run => panic!("the live loop ended"),
            inside = inside => inside.unwrap(),
        }
    };
    let submitting = tokio::time::timeout(Duration::from_secs(10), submitting).await;
    submitting.expect("the worker reached its submit");
    *screen.dropped.lock().unwrap() = Some(dropped);
    drop(run);
    assert_eq!(
        rig.gate.mode(),
        Mode::Held,
        "the gate holds while the worker still runs"
    );
    assert_eq!(state(&rig, 41), RowState::Injecting);
    release.send(()).unwrap();
    // The worker's output, pane included, drops only after its last ledger write.
    let finished = tokio::time::timeout(Duration::from_secs(10), finished).await;
    finished.expect("the worker finished").unwrap();
    assert_eq!(state(&rig, 41), RowState::AwaitTurn, "its write landed");
    assert_eq!(screen.sent.lock().unwrap().len(), 1);
    assert!(
        !supervisor.release(),
        "the abandoned loan keeps the slot lent"
    );
}

#[tokio::test]
async fn unreadable_binding_does_not_ignore_clear_or_command_channel_close() {
    let rig = Rig::new(6_325_884);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    screen.busy.store(true, Ordering::SeqCst);
    let mut supervisor = admitted(&rig, &[41]).await;
    let (commands, received) = tokio::sync::mpsc::channel(1);
    let mut run = Box::pin(supervisor.run(DriveFake(screen), received));
    until(&mut run, || rig.gate.mode() == Mode::LedgerOpen).await;
    rig.binding.unreadable.store(true, Ordering::SeqCst);
    push(&rig, "n1", source(Some(&a), &a));
    let at = format!("provider=claude channel={}", rig.channel);
    let unreadable = format!("input_reconcile_required {at} reason=binding_unreadable");
    until(&mut run, || rig.health().contains(&unreadable)).await;
    let clears = || rig.world.clears.lock().unwrap().len();
    let before = clears();
    commands.send(SupervisorCmd::Clear).await.unwrap();
    until(&mut run, || clears() > before).await;
    // The clear held the gate; the pass after it reopens the gate over a reread log.
    until(&mut run, || rig.gate.mode() == Mode::LedgerOpen).await;
    assert!(
        rig.health().contains(&unreadable),
        "only a working reread clears it"
    );
    drop(commands);
    let ended = tokio::time::timeout(Duration::from_secs(5), &mut run).await;
    assert!(
        ended.is_ok(),
        "a closed sender ends the loop while the log is unreadable"
    );
    drop(run);
    assert_eq!(rig.gate.mode(), Mode::Held);
    assert!(supervisor.release());
}

/// Passes past any backoff that each advance the binding log, so every one may retry a clear.
async fn advance(rig: &Rig, a: &SourceId, driven: &mut Driven, passes: usize) {
    for _ in 0..passes {
        driven.at += Duration::from_secs(301);
        push(rig, "n1", source(Some(a), a));
        driven.wake().await;
    }
}

#[tokio::test]
async fn clear_retry_budget_is_eight_and_exhaustion_stays_held() {
    let rig = Rig::new(6_325_885);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    screen.busy.store(true, Ordering::SeqCst);
    let mut driven = Driven::start(admitted(&rig, &[41]).await, &screen).await;
    let ledger = driven.supervisor.slot().get().unwrap();
    rig.world.durable_ticket(rig.channel, ledger, &[]);
    for _ in 0..3 * BUDGET {
        rig.world.reset(false, None);
    }
    let clears = || rig.world.clears.lock().unwrap().len();
    let before = clears();
    let at = driven.at;
    (driven.supervisor).clear(&mut driven.drive, at).await;
    screen.busy.store(false, Ordering::SeqCst);
    advance(&rig, &a, &mut driven, 12).await;
    assert_eq!(
        clears() - before,
        1 + BUDGET as usize,
        "one clear, eight retries"
    );
    assert_eq!(rig.gate.mode(), Mode::Held);
    let exhausted = |rig: &Rig| {
        let notices = rig.world.notices();
        notices.iter().filter(|n| n.contains("재시도 한도")).count()
    };
    assert_eq!(exhausted(&rig), 1);
    let at = format!("provider=claude channel={}", rig.channel);
    let line = format!("turn_transition_held {at} reason=clear_retry_exhausted");
    assert!(rig.health().contains(&line));
    advance(&rig, &a, &mut driven, 5).await;
    driven.idle(3).await;
    assert_eq!(
        clears() - before,
        1 + BUDGET as usize,
        "nothing retries after"
    );
    assert_eq!(
        (rig.gate.mode(), driven.sent(), exhausted(&rig)),
        (Mode::Held, 0, 1)
    );
    assert!(rig.health().contains(&line));
    assert!(driven.release());
}

#[tokio::test]
async fn g1a_close_request_holds_gate_before_flush_ack() {
    let rig = Rig::new(6_325_930);
    let screen = Arc::new(Screen::default());
    let mut supervisor = admitted(&rig, &[]).await;
    let before = supervisor
        .slot()
        .get()
        .unwrap()
        .rows()
        .unwrap()
        .folded_seq();
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let script = async {
        super::super::command::request(&sender, |ack| SupervisorCmd::Close { ack })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rig.gate.mode(), Mode::Held);
        drop(sender);
    };
    tokio::join!(supervisor.run(DriveFake(screen), receiver), script);
    assert_eq!(
        supervisor
            .slot()
            .reopen()
            .unwrap()
            .rows()
            .unwrap()
            .folded_seq(),
        before
    );
    assert!(supervisor.release());
    drop(rig);

    let rig = Rig::new(6_325_935);
    let mut supervisor = admitted(&rig, &[]).await;
    let generation = supervisor.admission_gen;
    let (ack, response) = tokio::sync::oneshot::channel();
    supervisor
        .input_command(SupervisorCmd::Close { ack }, true)
        .await;
    response.await.unwrap().unwrap();
    assert!(
        !supervisor.admission_open(),
        "close is enforced independently of drive readiness"
    );
    assert!(supervisor.admission_gen > generation);
    assert!(supervisor.release());
}
