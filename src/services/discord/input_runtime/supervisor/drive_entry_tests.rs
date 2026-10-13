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

#[path = "g2b_entry_tests.rs"]
mod g2b_entry;

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
    captures: AtomicUsize,
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
    // Runs once when the drive next asks for a pane, after its gate decision.
    on_pane: Mutex<Option<Box<dyn FnOnce() + Send>>>,
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
        self.0.captures.fetch_add(1, Ordering::SeqCst);
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
        if let Some(hook) = self.0.on_pane.lock().unwrap().take() {
            hook();
        }
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
        let identity = crate::services::tui_input::rows::receipt_identity::ReceiptIdentity::new(
            key,
            vec![key],
            7,
            rig.channel,
            rig.channel,
        )
        .unwrap();
        let input = json!({ "text": format!("input {key}"), "receipt_identity": identity });
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
        input: json!({ "text": "42", "receipt_identity":
            crate::services::tui_input::rows::receipt_identity::ReceiptIdentity::new(
                42, vec![42], 7, rig.channel, rig.channel,
            ).unwrap() }),
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
async fn c2b_policy_approval_stays_bound_when_tracked_close_changes_the_offer_head() {
    use crate::services::discord::input_runtime::supervisor::receipt::Receipt;
    use crate::services::tui_input::actor::{token, witness};
    use crate::services::tui_input::attempt::{
        AttemptMeta, Effect, Tracking, WitnessKind, fresh_token,
    };
    use crate::services::tui_input::rows::AttemptEvidence;
    use crate::services::tui_o::shadow::SourceBinding;
    let rig = Rig::new(6_328_401);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let binding = SourceBinding {
        channel_id: rig.channel,
        provider: ShadowProvider::Claude,
        source: a.clone(),
    };
    let mut supervisor = admitted(&rig, &[41, 42]).await;
    let ledger = supervisor.slot().get().unwrap();
    let anchor = std::fs::metadata(&a.path).unwrap().len();
    let token = fresh_token();
    let frame = token::render(&token, "input 41");
    let profile = token::profile(ShadowProvider::Claude);
    let meta = AttemptMeta {
        generation: 1,
        token: token.clone(),
        frame_digest: token::digest(profile, &frame).unwrap(),
        frame_profile: Some(profile.into()),
        execution_nonce: "n1".into(),
        source: a.clone(),
        anchor,
        effect: Effect::Intent,
        incarnation: None,
        queue_end: None,
    };
    let evidence = AttemptEvidence {
        binding: binding.clone(),
        execution_nonce: "n1".into(),
        eof: anchor,
        rendered_prompt: frame.clone(),
        source_ids: vec![41],
        record_end: None,
        native_turn_id: None,
    };
    ledger
        .append_tracked(
            41,
            RowState::Injecting,
            Some(evidence),
            &Tracking {
                attempt: Some(meta),
                ..Tracking::default()
            },
        )
        .unwrap();
    ledger
        .append_entry(
            &Entry::Transition {
                key: 41,
                state: RowState::AwaitTurn,
                attempt: None,
            },
            &[],
        )
        .unwrap();
    user(&a.path, &frame);
    turn_end(&a);
    let seen = witness::scan_tracked(&binding, &ledger.rows().unwrap()).unwrap();
    assert!(seen.complete && seen.foreign == 0 && seen.altered.is_empty());
    assert_eq!(
        seen.closed,
        [(41, false)],
        "the registered turn has a matched close before the pass"
    );
    assert_eq!(seen.witnesses.len(), 1);
    let (key, observed) = seen.witnesses.into_iter().next().unwrap();
    assert_eq!(
        (key, observed.kind, observed.token.as_str()),
        (41, WitnessKind::User, token.as_str())
    );
    ledger.append_witness(key, observed).unwrap();
    assert_eq!(
        ledger.rows().unwrap().row(41).unwrap().state,
        RowState::Running
    );
    let before_b = ledger.rows().unwrap().row(42).unwrap().clone();
    let identity_b = ledger.rows().unwrap().receipt_identity(42).unwrap();
    rig.world
        .source_policy_by_key
        .lock()
        .unwrap()
        .extend([(41, true), (42, false)]);
    let screen = Arc::new(Screen::default());
    *screen.record.lock().unwrap() = Some(a.path.clone());
    let mut driven = Driven::start(supervisor, &screen).await;
    let after = driven.supervisor.slot().get().unwrap().rows().unwrap();
    assert_eq!(
        after.row(41).unwrap().state,
        RowState::Done(DoneReason::Completed)
    );
    assert_eq!(
        driven.sent(),
        0,
        "A's policy approval cannot offer the newly selected B"
    );
    assert_eq!(after.row(42), Some(&before_b));
    assert_eq!(after.receipt_identity(42), Some(identity_b));
    assert_eq!(*rig.world.source_policy_keys.lock().unwrap(), [41]);
    driven.idle(1).await;
    assert_eq!(
        *rig.world.source_policy_keys.lock().unwrap(),
        [41, 42],
        "the next tick checks B's own policy"
    );
    assert_eq!(driven.sent(), 0);
    assert_eq!(state(&rig, 42), RowState::Received);
    rig.world
        .source_policy_by_key
        .lock()
        .unwrap()
        .insert(42, true);
    let (reply, response) = tokio::sync::oneshot::channel();
    driven
        .supervisor
        .input_command(SupervisorCmd::FetchTicket { reply }, true)
        .await;
    let ticket = response.await.unwrap().unwrap();
    let (reply, response) = tokio::sync::oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::BeginScan {
                ticket,
                sources: vec![41, 42],
                horizon: 42,
                complete_fetch: true,
                reply,
            },
            true,
        )
        .await;
    let mut capability = response.await.unwrap().unwrap();
    for key in [41, 42] {
        let rows = driven.supervisor.slot().get().unwrap().rows().unwrap();
        let source = super::super::source::Source::new(
            key,
            rows.receipt_identity(key).unwrap(),
            rows.row(key).unwrap().input.clone(),
            vec![],
        )
        .unwrap();
        let (reply, response) = tokio::sync::oneshot::channel();
        driven
            .supervisor
            .input_command(
                SupervisorCmd::CommitFromScan {
                    source: Box::new(source),
                    capability,
                    reply,
                },
                true,
            )
            .await;
        let committed = response.await.unwrap();
        assert!(matches!(committed.receipt, Receipt::DuplicateQueued(_)));
        assert_eq!(committed.settlement, Ok(()));
        capability = committed.capability;
    }
    let (reply, response) = tokio::sync::oneshot::channel();
    driven
        .supervisor
        .input_command(SupervisorCmd::CompleteScan { capability, reply }, true)
        .await;
    assert!(response.await.unwrap().is_ok());
    driven.idle(2).await;
    assert_eq!(
        driven.sent(),
        1,
        "B's own approval and settled scan allow its offer"
    );
    assert_eq!(driven.drive.counts.builds, 1);
    assert!(driven.release());
}

#[tokio::test]
async fn c2b_cancelled_pending_during_policy_wait_blocks_offer_until_actual_rescan() {
    use crate::services::discord::input_runtime::supervisor::command::FinalDisposition;
    let rig = Rig::new(6_328_402);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    screen.busy.store(true, Ordering::SeqCst);
    *screen.record.lock().unwrap() = Some(a.path.clone());
    let mut driven = Driven::start(admitted(&rig, &[41]).await, &screen).await;
    let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
    let client = driven.supervisor.bind_intake(sender).unwrap();
    driven.supervisor.adopt_task(rig.channel).unwrap();
    driven.supervisor.publish_intake();
    assert!(driven.supervisor.admission_open());
    assert!(driven.supervisor.order.offer_ready());
    let overflow = driven.supervisor.pending_overflow();
    let generation = overflow.generation();
    let before = driven.supervisor.slot().get().unwrap().rows().unwrap();
    let captures = screen.captures.load(Ordering::SeqCst);
    let builds = driven.drive.counts.builds;
    assert_eq!(builds, 1);
    assert!(before.row(41).unwrap().attempt.is_none());
    let (entered, inside) = tokio::sync::oneshot::channel();
    let (resume, continuation) = tokio::sync::oneshot::channel();
    *rig.world.source_policy_wait.lock().unwrap() = Some((entered, continuation));
    screen.busy.store(false, Ordering::SeqCst);
    let cancel = async {
        inside.await.unwrap();
        let mut pending = Box::pin(client.pending(vec![42]));
        let command = tokio::select! {
            command = receiver.recv() => command.unwrap(),
            result = &mut pending => panic!("the actual PendingSource reply is not yet produced: {result:?}"),
        };
        assert!(
            matches!(&command, SupervisorCmd::PendingSource { sources, .. } if sources == &[42])
        );
        drop(pending);
        assert_eq!(
            overflow.generation(),
            generation + 1,
            "cancellation dirties the same supervisor overflow inside policy IO"
        );
        resume.send(()).unwrap();
        command
    };
    let ((), pending) = tokio::join!(driven.idle(1), cancel);
    assert_eq!(
        driven.sent(),
        0,
        "policy approval cannot reuse readiness from before the await"
    );
    assert_eq!(
        driven.supervisor.slot().get().unwrap().rows().unwrap(),
        before
    );
    assert_eq!(screen.captures.load(Ordering::SeqCst), captures);
    assert_eq!(driven.drive.counts.builds, builds);
    assert!(!driven.supervisor.slot.loaned());
    assert!(!driven.supervisor.order.offer_ready());
    driven.supervisor.input_command(pending, true).await;
    assert_eq!(driven.supervisor.pending_snapshot().sources, [42]);
    let (reply, response) = tokio::sync::oneshot::channel();
    driven
        .supervisor
        .input_command(SupervisorCmd::FetchTicket { reply }, true)
        .await;
    let ticket = response.await.unwrap().unwrap();
    let (reply, response) = tokio::sync::oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::BeginScan {
                ticket,
                sources: vec![42],
                horizon: 42,
                complete_fetch: true,
                reply,
            },
            true,
        )
        .await;
    let capability = response.await.unwrap().unwrap();
    let (reply, response) = tokio::sync::oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::SettleFromScan {
                source: 42,
                disposition: FinalDisposition::NonTarget,
                capability,
                reply,
            },
            true,
        )
        .await;
    let capability = response.await.unwrap().unwrap();
    let (reply, response) = tokio::sync::oneshot::channel();
    driven
        .supervisor
        .input_command(SupervisorCmd::CompleteScan { capability, reply }, true)
        .await;
    assert!(response.await.unwrap().is_ok());
    assert!(driven.supervisor.order.offer_ready());
    assert!(driven.supervisor.pending_snapshot().sources.is_empty());
    driven.idle(2).await;
    assert_eq!(
        driven.sent(),
        1,
        "the existing actor proceeds after the actual current scan"
    );
    assert_eq!(driven.drive.counts.builds, builds);
    assert!(driven.release());
}

#[tokio::test]
async fn c2_policy_before_receipt_defers_unknown_scan_without_ack_or_source_loss() {
    use crate::services::discord::input_runtime::intake::{Classification, Retry};
    use crate::services::discord::input_runtime::supervisor::receipt::{Deferred, Receipt};
    use crate::services::tui_input::rows::receipt_identity::{ReceiptIdentity, Responsibility};
    let rig = Rig::new(6_326_976);
    let screen = Arc::new(Screen::default());
    let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
    let identity = ReceiptIdentity::new(41, vec![41], 7, rig.channel, rig.channel).unwrap();
    let input = json!({"text":"request", "original_text":"request", "author_is_bot":false});
    let before = driven
        .supervisor
        .slot()
        .get()
        .unwrap()
        .rows()
        .unwrap()
        .folded_seq();
    let (reply, response) = tokio::sync::oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::PendingSource {
                sources: vec![41],
                reply,
            },
            true,
        )
        .await;
    assert_eq!(response.await.unwrap().unwrap().sources, [41]);
    for accepted in [false, true] {
        *rig.world.source_policy.lock().unwrap() = Some(if accepted {
            Classification::Target
        } else {
            Classification::Retry(Retry::Unknown)
        });
        let (reply, response) = tokio::sync::oneshot::channel();
        driven
            .supervisor
            .input_command(SupervisorCmd::FetchTicket { reply }, true)
            .await;
        let ticket = response.await.unwrap().unwrap();
        let (reply, response) = tokio::sync::oneshot::channel();
        driven
            .supervisor
            .input_command(
                SupervisorCmd::BeginScan {
                    ticket,
                    sources: vec![41],
                    horizon: 41,
                    complete_fetch: true,
                    reply,
                },
                true,
            )
            .await;
        let capability = response.await.unwrap().unwrap();
        let (reply, response) = tokio::sync::oneshot::channel();
        let source =
            super::super::source::Source::new(41, identity.clone(), input.clone(), vec![]).unwrap();
        driven
            .supervisor
            .input_command(
                SupervisorCmd::CommitFromScan {
                    source: Box::new(source),
                    capability,
                    reply,
                },
                true,
            )
            .await;
        let result = response.await.unwrap();
        let rows = driven.supervisor.slot().get().unwrap().rows().unwrap();
        if accepted {
            assert!(matches!(result.receipt, Receipt::Accepted(_)));
            assert_eq!(rows.open_rows().count(), 1);
            assert_eq!(rows.receipt_identity(41), Some(identity.clone()));
            assert_eq!(rows.row(41).unwrap().input["original_text"], "request");
            assert!(driven.supervisor.pending_snapshot().sources.is_empty());
        } else {
            assert_eq!(result.receipt, Receipt::Deferred(Deferred::Unknown));
            assert_eq!(
                rows.folded_seq(),
                before,
                "Unknown cannot append or acknowledge a receipt"
            );
            assert_eq!(rows.open_rows().count(), 0);
            assert_eq!(rows.responsibility(&identity), Responsibility::Absent);
            assert_eq!(driven.supervisor.pending_snapshot().sources, [41]);
        }
        assert_eq!(result.settlement, Ok(()));
        assert_eq!(driven.sent(), 0);
    }
    assert!(driven.release());
}

#[tokio::test]
async fn c2_policy_before_receipt_defers_unknown_external_and_retries_same_origin() {
    use crate::services::discord::input_runtime::intake::{Classification, Retry};
    use crate::services::discord::input_runtime::supervisor::{external, receipt::Deferred};
    use crate::services::tui_input::rows::receipt_identity::Responsibility;
    let rig = Rig::new(6_326_977);
    let screen = Arc::new(Screen::default());
    let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
    let source =
        external::source(rig.channel, 7, "request", Some("policy"), "actual-origin").unwrap();
    let identity = source.identity().clone();
    let original = source.input().clone();
    let before = driven
        .supervisor
        .slot()
        .get()
        .unwrap()
        .rows()
        .unwrap()
        .folded_seq();
    *rig.world.source_policy.lock().unwrap() = Some(Classification::Retry(Retry::Unknown));
    let (reply, response) = tokio::sync::oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::SubmitExternal {
                source: Box::new(source),
                reply,
            },
            true,
        )
        .await;
    assert_eq!(
        response.await.unwrap(),
        external::ExternalReceipt::Deferred(Deferred::Unknown)
    );
    let rows = driven.supervisor.slot().get().unwrap().rows().unwrap();
    assert_eq!(rows.folded_seq(), before);
    assert_eq!(rows.open_rows().count(), 0);
    assert_eq!(rows.responsibility(&identity), Responsibility::Absent);
    *rig.world.source_policy.lock().unwrap() = Some(Classification::Target);
    let source =
        external::source(rig.channel, 7, "request", Some("policy"), "actual-origin").unwrap();
    let (reply, response) = tokio::sync::oneshot::channel();
    driven
        .supervisor
        .input_command(
            SupervisorCmd::SubmitExternal {
                source: Box::new(source),
                reply,
            },
            true,
        )
        .await;
    assert!(matches!(
        response.await.unwrap(),
        external::ExternalReceipt::Received {
            duplicate: false,
            ..
        }
    ));
    let rows = driven.supervisor.slot().get().unwrap().rows().unwrap();
    assert_eq!(rows.open_rows().count(), 1);
    assert_eq!(rows.row(identity.source_ids[0]).unwrap().input, original);
    assert_eq!(
        rows.receipt_identity(identity.source_ids[0]),
        Some(identity)
    );
    assert_eq!(driven.sent(), 0);
    assert!(driven.release());
}

#[tokio::test]
async fn c2_policy_change_after_receipt_keeps_identity_and_blocks_offer_until_rechecked() {
    use crate::services::discord::input_runtime::intake::{Classification, Refusal, Retry};
    for (index, (policy, reason)) in [
        (
            Classification::Rejected(Refusal::Unauthorized),
            "source_policy_not_target",
        ),
        (
            Classification::Rejected(Refusal::ProviderMismatch),
            "source_policy_not_target",
        ),
        (
            Classification::Retry(Retry::Unknown),
            "source_policy_not_target",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let rig = Rig::new(6_326_970 + index as u64);
        let a = idle_transcript(&rig, "a", false);
        push(&rig, "n1", source(None, &a));
        let screen = Arc::new(Screen::default());
        screen.busy.store(true, Ordering::SeqCst);
        *screen.record.lock().unwrap() = Some(a.path.clone());
        let mut driven = Driven::start(admitted(&rig, &[41]).await, &screen).await;
        let before = rows(&rig.root, rig.channel);
        let input = before.row(41).unwrap().input.clone();
        let identity = before.receipt_identity(41).unwrap();
        let responsibility = before.responsibility(&identity);
        rig.world.source_policy_inputs.lock().unwrap().clear();
        *rig.world.source_policy.lock().unwrap() = Some(policy);
        screen.busy.store(false, Ordering::SeqCst);
        driven.wait(2 * HOLD_GRACE).await;
        let after = rows(&rig.root, rig.channel);
        assert_eq!(
            driven.sent(),
            0,
            "receipt-time permission cannot survive a policy change"
        );
        assert_eq!(after.row(41).unwrap().input, input);
        assert_eq!(after.receipt_identity(41), Some(identity));
        assert_eq!(
            after.responsibility(&before.receipt_identity(41).unwrap()),
            responsibility
        );
        assert!(!after.row(41).unwrap().state.is_terminal());
        assert_eq!(input_lines(&rig), [held_line(&rig, reason, 41, 1)]);
        let inputs = rig.world.source_policy_inputs.lock().unwrap().clone();
        assert!(!inputs.is_empty(), "every held pass rereads the policy");
        assert!(inputs.iter().all(|seen| seen == &input));
        *rig.world.source_policy.lock().unwrap() = Some(Classification::Target);
        driven.wait(2 * HOLD_GRACE).await;
        assert_eq!(
            driven.sent(),
            1,
            "fresh normal policy and actual Idle facts permit one offer"
        );
        assert!(input_lines(&rig).is_empty());
        assert!(driven.release());
    }
}

#[tokio::test]
async fn c2_closed_ack_certifies_flush_before_later_clear_not_ledger_immutability() {
    let rig = Rig::new(6_326_975);
    push(&rig, "n1", pending("next", &rig.root.join("next.jsonl")));
    let screen = Arc::new(Screen::default());
    let mut driven = Driven::start(admitted(&rig, &[41]).await, &screen).await;
    let (ack, response) = tokio::sync::oneshot::channel();
    driven
        .supervisor
        .input_command(SupervisorCmd::Close { ack }, true)
        .await;
    assert!(
        response.await.unwrap().is_ok(),
        "the typed close ACK confirms the current flush"
    );
    let before = driven.supervisor.slot().get().unwrap().rows().unwrap();
    rig.world
        .durable_ticket(rig.channel, driven.supervisor.slot().get().unwrap(), &[41]);
    driven.supervisor.clear(&mut driven.drive, driven.at).await;
    let after = driven.supervisor.slot().get().unwrap().rows().unwrap();
    assert!(
        after.folded_seq() > before.folded_seq(),
        "a later Clear may write after a Close ACK"
    );
    assert_eq!(
        after.row(41).unwrap().state,
        RowState::Abandoned(AbandonReason::UserClear)
    );
    assert!(!driven.supervisor.admission_open());
    assert_eq!(driven.sent(), 0);
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

fn mapped_at(rig: &Rig, thread: u64, at: &'static str) -> HoldCause {
    let edge = crate::services::discord::input_runtime::supervisor::mapping::Edge {
        writer: "claude".into(),
        parent: rig.channel,
        thread,
    };
    HoldCause::MappingPresent(edge, at)
}

#[tokio::test]
async fn x13_mapping_late_violation_stops_ledger_open_and_offer_in_the_drive() {
    // An edge after admission: the first pass latches before its open, and no later pass opens.
    let rig = Rig::new(6_325_160);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let supervisor = admitted(&rig, &[41]).await;
    rig.world
        .parents()
        .insert(ChannelId::new(rig.channel), ChannelId::new(6));
    let screen = Arc::new(Screen::default());
    let mut driven = Driven::start(supervisor, &screen).await;
    driven.idle(3).await;
    let cause = mapped_at(&rig, 6, "ledger_open");
    assert_eq!(
        driven.supervisor.registration.latched(),
        Some(cause.clone())
    );
    assert_eq!((driven.drive.counts.opens, driven.sent()), (0, 0));
    assert_eq!(rig.gate.mode(), Mode::Held);
    assert!(!driven.supervisor.admission_open());
    assert_eq!(state(&rig, 41), RowState::Received);
    let Driven {
        mut supervisor,
        drive,
        ..
    } = driven;
    supervisor.stop_drive(drive);
    rig.world.parents().clear();
    assert_eq!(supervisor.boot().await, Landing::Held(cause.clone()));
    let ports = DriveFake(screen.clone());
    assert!(
        supervisor.start_drive(ports).is_none(),
        "no drive after the latch"
    );
    drop((supervisor, rig));
    // An edge after the open but before the actor's step: nothing is submitted.
    let rig = Rig::new(6_325_161);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    *screen.record.lock().unwrap() = Some(a.path.clone());
    let world = rig.world.clone();
    let channel = rig.channel;
    *screen.on_pane.lock().unwrap() = Some(Box::new(move || {
        world
            .parents()
            .insert(ChannelId::new(channel), ChannelId::new(6));
    }));
    let mut driven = Driven::start(admitted(&rig, &[41]).await, &screen).await;
    driven.idle(3).await;
    let cause = mapped_at(&rig, 6, "offer");
    assert_eq!(driven.supervisor.registration.latched(), Some(cause));
    assert_eq!((driven.drive.counts.opens, driven.sent()), (1, 0));
    assert_eq!(rig.gate.mode(), Mode::Held);
    assert_eq!(
        state(&rig, 41),
        RowState::Received,
        "the row keeps its responsibility"
    );
    assert!(!driven.supervisor.admission_open());
}

/// A live loop over a busy pane with an armed presence registration on its channel.
async fn presence_rig(channel: u64, probe: &PresenceProbe) -> (Rig, Supervisor<Fake>, Arc<Screen>) {
    let rig = Rig::new(channel);
    let a = idle_transcript(&rig, "a", false);
    push(&rig, "n1", source(None, &a));
    let screen = Arc::new(Screen::default());
    screen.busy.store(true, Ordering::SeqCst);
    probe.arm(channel);
    let supervisor = admitted(&rig, &[41]).await;
    (rig, supervisor, screen)
}

type PresenceProbe = crate::services::discord::turn_presence::entrypoints::tests::Probe;

// Close, the loop's end and its cancellation withdraw the presence registration the drive
// started under, an approval adopted since included; one registered after a reset stays.
#[cfg(unix)]
#[tokio::test]
async fn b2b2_close_and_drive_end_withdraw_only_the_presence_the_drive_started_under() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let probe = PresenceProbe::install(&shared);
    let gate = crate::services::tui_o::ownership::gate("b2b2-drive");
    let current = PresenceProbe::current;
    {
        let (rig, mut supervisor, screen) = presence_rig(6_325_891, &probe).await;
        let (commands, received) = tokio::sync::mpsc::channel(1);
        let mut run = Box::pin(supervisor.run(DriveFake(screen), received));
        until(&mut run, || rig.gate.mode() == Mode::LedgerOpen).await;
        let adopted = probe.arm_gateway(rig.channel, gate.clone());
        let (ack, _closed) = tokio::sync::oneshot::channel();
        commands.send(SupervisorCmd::Close { ack }).await.unwrap();
        until(&mut run, || !current(&adopted)).await;
        let later = probe.arm(rig.channel);
        drop(commands);
        run.await;
        assert!(
            current(&later),
            "a registration made after the drive's own stays"
        );
    }
    for cancelled in [false, true] {
        let channel = 6_325_892 + u64::from(cancelled);
        let (rig, mut supervisor, screen) = presence_rig(channel, &probe).await;
        let (commands, received) = tokio::sync::mpsc::channel(1);
        let mut run = Box::pin(supervisor.run(DriveFake(screen), received));
        until(&mut run, || rig.gate.mode() == Mode::LedgerOpen).await;
        let adopted = probe.arm_gateway(rig.channel, gate.clone());
        if cancelled {
            drop(run);
        } else {
            drop(commands);
            run.await;
        }
        assert!(
            !current(&adopted),
            "cancelled={cancelled}: the drive's end withdraws"
        );
    }
}
