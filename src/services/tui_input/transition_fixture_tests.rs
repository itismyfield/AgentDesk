#![cfg(any(target_os = "macos", target_os = "linux"))]

use super::handover::{Composer, EnqueueOutcome, MoveEvidence, MoveSource};
use super::ledger::{Ledger, LedgerLease, Presence};
use super::rows::{Entry, Owner, Row};
use super::transition::{DeletePhase, Host, Input, Move, Outcome, backoff, handback};
use serde_json::json;
use std::io;
use std::path::{Path, PathBuf};

struct Fixture {
    root: PathBuf,
    fail: Option<DeletePhase>,
    events: Vec<DeletePhase>,
    enqueued: Vec<u64>,
    actor: usize,
    notices: usize,
    noticed: Vec<(Option<u64>, &'static str)>,
    reject: bool,
}
impl Fixture {
    fn new(root: &Path) -> Self {
        for name in ["dispatch", "queue", "accessories", "row"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        Self {
            root: root.to_owned(),
            fail: None,
            events: Vec::new(),
            enqueued: vec![99],
            actor: 0,
            notices: 0,
            noticed: Vec::new(),
            reject: false,
        }
    }
    fn path(&self, phase: DeletePhase) -> PathBuf {
        self.root.join(match phase {
            DeletePhase::Dispatch => "dispatch",
            DeletePhase::Queue => "queue",
            DeletePhase::Accessories => "accessories",
            DeletePhase::Row => "row",
        })
    }
}
impl Host for Fixture {
    fn collect(&mut self, _: &Ledger) -> io::Result<Vec<Input>> {
        Ok(if self.path(DeletePhase::Queue).exists() {
            vec![8, 2]
                .into_iter()
                .map(|key| Input {
                    key,
                    payload: json!({"text":key}),
                    source: MoveSource::Queue,
                    pins: vec![],
                })
                .collect()
        } else {
            vec![]
        })
    }
    fn evidence(&mut self, _: &Input) -> io::Result<MoveEvidence> {
        Ok(MoveEvidence {
            user_record: false,
            turn_open: false,
            never_started: true,
            composer: Composer::Empty,
        })
    }
    fn delete(&mut self, phase: DeletePhase) -> io::Result<()> {
        let rows = Ledger::open(&self.root, 9)?.rows()?;
        assert_eq!(
            rows.owner(8),
            Owner::Ledger,
            "commit must precede source deletion"
        );
        if self.fail == Some(phase) {
            return Err(io::Error::other("injected deletion failure"));
        }
        self.events.push(phase);
        let path = self.path(phase);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
    fn start_actor(&mut self) -> io::Result<()> {
        self.actor += 1;
        Ok(())
    }
    fn reconcile(&mut self, _: u64, _: &Row) -> io::Result<(bool, Composer)> {
        Ok((false, Composer::Empty))
    }
    fn enqueue(&mut self, key: u64, _: &Row) -> io::Result<EnqueueOutcome> {
        if self.reject {
            return Ok(EnqueueOutcome::Rejected);
        }
        if self.enqueued.contains(&key) {
            Ok(EnqueueOutcome::AlreadyPreserved)
        } else {
            self.enqueued.push(key);
            Ok(EnqueueOutcome::Persisted)
        }
    }
    fn notice(&mut self, key: Option<u64>, reason: &'static str) -> io::Result<()> {
        self.notices += 1;
        self.noticed.push((key, reason));
        Ok(())
    }
}
fn sandbox() -> tempfile::TempDir {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
    std::fs::create_dir_all(&path).unwrap();
    tempfile::tempdir_in(path).unwrap()
}

#[test]
fn deletion_boundaries_restart_keep_one_owner_and_order() {
    let phases = [
        DeletePhase::Dispatch,
        DeletePhase::Queue,
        DeletePhase::Accessories,
        DeletePhase::Row,
    ];
    for fail in phases {
        let root = sandbox();
        let mut host = Fixture::new(root.path());
        host.fail = Some(fail);
        let mut lease = LedgerLease::new(root.path(), 9);
        let mut movement = Move::prepare(&mut lease, &mut host).unwrap();
        assert_eq!(movement.advance(&mut lease, &mut host), Outcome::Held);
        assert_eq!(movement.advance(&mut lease, &mut host), Outcome::Held);
        assert_eq!(host.notices, 1);
        assert_eq!(host.actor, 0);
        for key in [8, 2] {
            assert_eq!(
                Ledger::open(root.path(), 9)
                    .unwrap()
                    .rows()
                    .unwrap()
                    .owner(key),
                Owner::Ledger
            );
        }
        drop((movement, lease));
        host.fail = None;
        let mut lease = LedgerLease::new(root.path(), 9);
        let mut resumed = Move::prepare(&mut lease, &mut host).unwrap();
        assert_eq!(resumed.advance(&mut lease, &mut host), Outcome::Ledger);
        assert_eq!(
            &host.events[host.events.len() - 4..],
            &phases,
            "source retirement order is fixed"
        );
        assert_eq!(host.actor, 1);
        assert_eq!(handback(&mut lease, &mut host).unwrap(), Outcome::Legacy);
        assert_eq!(host.enqueued, vec![99, 8, 2]);
    }
}

#[test]
fn handback_rejection_preserves_order_and_enqueue_crash_is_idempotent() {
    let root = sandbox();
    let mut host = Fixture::new(root.path());
    let mut lease = LedgerLease::new(root.path(), 9);
    let mut movement = Move::prepare(&mut lease, &mut host).unwrap();
    assert_eq!(movement.advance(&mut lease, &mut host), Outcome::Ledger);
    host.reject = true;
    assert_eq!(handback(&mut lease, &mut host).unwrap(), Outcome::Held);
    assert_eq!(host.enqueued, vec![99]);
    host.reject = false;
    host.enqueued.push(8);
    assert_eq!(handback(&mut lease, &mut host).unwrap(), Outcome::Legacy);
    assert_eq!(host.enqueued, vec![99, 8, 2]);
    assert_eq!(handback(&mut lease, &mut host).unwrap(), Outcome::Legacy);
    assert_eq!(host.enqueued, vec![99, 8, 2]);
}

#[test]
fn old_snapshot_without_received_order_holds_without_enqueue() {
    let root = sandbox();
    let mut ledger = Ledger::open(root.path(), 9).unwrap();
    ledger
        .append_entry(
            &Entry::Received {
                key: 8,
                input: json!({"text":"input"}),
            },
            &[],
        )
        .unwrap();
    let mut snapshot = ledger.rows().unwrap().compact().unwrap();
    snapshot["rows"]["8"]
        .as_object_mut()
        .unwrap()
        .remove("received_seq");
    ledger.checkpoint(snapshot).unwrap();
    drop(ledger);
    let mut host = Fixture::new(root.path());
    let mut lease = LedgerLease::new(root.path(), 9);
    assert_eq!(handback(&mut lease, &mut host).unwrap(), Outcome::Held);
    assert_eq!(host.enqueued, vec![99]);
}

// A pasted row's queued copy may still reach the model, so an empty composer is not "never sent".
#[test]
fn handback_holds_an_attempted_row_even_on_an_empty_composer() {
    use super::rows::{HeldReason, RowState};
    for state in [
        RowState::Injecting,
        RowState::AwaitTurn,
        RowState::Unaccepted,
        RowState::Held(HeldReason::Ambiguous),
    ] {
        let root = sandbox();
        let mut host = Fixture::new(root.path());
        let mut lease = LedgerLease::new(root.path(), 9);
        let mut movement = Move::prepare(&mut lease, &mut host).unwrap();
        assert_eq!(movement.advance(&mut lease, &mut host), Outcome::Ledger);
        let transition = Entry::Transition {
            key: 8,
            state,
            attempt: None,
        };
        lease.get().unwrap().append_entry(&transition, &[]).unwrap();
        assert_eq!(handback(&mut lease, &mut host).unwrap(), Outcome::Held);
        assert_eq!(host.enqueued, vec![99], "{state:?} went back to Legacy");
        let rows = lease.get().unwrap().rows().unwrap();
        assert_eq!(rows.owner(8), Owner::Ledger, "{state:?}");
        assert!(host.noticed.contains(&(Some(8), "handback_ambiguous")));
    }
}

#[test]
fn retry_backoff_is_bounded() {
    assert_eq!(backoff(0).as_secs(), 5);
    assert_eq!(backoff(1).as_secs(), 10);
    assert_eq!(backoff(6).as_secs(), 300);
    assert_eq!(backoff(u32::MAX).as_secs(), 300);
}

// A scenario after its move's advances: one outcome, durable event list and reopen count per step.
struct Run {
    root: tempfile::TempDir,
    host: Fixture,
    lease: LedgerLease,
    movement: Move,
    steps: Vec<(Outcome, Vec<String>, usize)>,
}

// Each step arms `fault` (a 1-based event position) and lets `fail` refuse that source deletion.
fn run(plan: &[(Option<usize>, Option<DeletePhase>)]) -> Run {
    use super::durability_tests::supported::Recording;
    let root = sandbox();
    let mut host = Fixture::new(root.path());
    let mut lease = LedgerLease::new(root.path(), 9);
    let mut movement = Move::prepare(&mut lease, &mut host).unwrap();
    let recording = Recording::start(root.path());
    let mut steps = Vec::new();
    for (fault, fail) in plan {
        host.fail = *fail;
        recording.arm(*fault);
        let outcome = movement.advance(&mut lease, &mut host);
        steps.push((outcome, recording.events(), lease.reopens));
    }
    host.fail = None;
    Run {
        root,
        host,
        lease,
        movement,
        steps,
    }
}

// Finds a durability boundary in a fault-free dry run: walks the WAL events matching `kinds` in
// order and returns the last one's position, so the fault never depends on a counted offset.
fn label(events: &[String], kinds: &[&str]) -> usize {
    let mut kinds = kinds.iter().peekable();
    for (index, event) in events.iter().enumerate() {
        let next = kinds.peek().map(|kind| format!("{kind}:"));
        if event.ends_with(".jsonl") && next.is_some_and(|kind| event.starts_with(&kind)) {
            kinds.next();
            if kinds.peek().is_none() {
                return index + 1;
            }
        }
    }
    panic!("dry run never reached the boundary in {events:?}");
}

fn owners(root: &Path) -> Vec<Owner> {
    let rows = Ledger::open(root, 9).unwrap().rows().unwrap();
    vec![rows.owner(8), rows.owner(2)]
}

#[test]
fn uncertain_commit_whole_and_cut_reopen_decide_responsibility() {
    let dry = run(&[(None, None)]);
    let commit_sync = label(&dry.steps[0].1, &["write", "write", "write", "file_sync"]);
    for cut in [false, true] {
        let mut run = run(&[(Some(commit_sync), None)]);
        assert_eq!(run.steps[0].0, Outcome::Held);
        assert!(
            run.lease.needs_reopen,
            "an uncertain commit forces a fresh read"
        );
        assert!(run.host.path(DeletePhase::Queue).exists());
        assert!(run.host.events.is_empty());
        if cut {
            let dir = run.root.path().join("input_ledger/9");
            let wal = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
                .unwrap();
            let file = std::fs::OpenOptions::new().write(true).open(wal).unwrap();
            file.set_len(file.metadata().unwrap().len() - 10).unwrap();
            file.sync_all().unwrap();
        }
        let expected = if cut {
            Outcome::Legacy
        } else {
            Outcome::Ledger
        };
        assert_eq!(
            run.movement.advance(&mut run.lease, &mut run.host),
            expected
        );
        let owner = if cut { Owner::Legacy } else { Owner::Ledger };
        assert_eq!(owners(run.root.path()), vec![owner, owner]);
        if cut {
            assert!(run.host.path(DeletePhase::Queue).exists());
            let mut next = Move::prepare(&mut run.lease, &mut run.host).unwrap();
            assert_eq!(next.advance(&mut run.lease, &mut run.host), Outcome::Ledger);
        }
        assert_eq!(run.host.actor, 1);
    }
}

#[test]
fn second_staging_failure_requires_another_successful_reopen_before_legacy() {
    let first_stage = label(&run(&[(None, None)]).steps[0].1, &["write"]);
    let plan = [(Some(first_stage), None), (None, None)];
    // The first stage's bytes are adopted on reopen, so the next write is the remaining stage's.
    let second_stage = label(&run(&plan).steps[1].1, &["write"]);
    let plan = [
        (Some(first_stage), None),
        (Some(second_stage), None),
        (None, None),
    ];
    let reopen = label(&run(&plan).steps[2].1, &["file_sync"]);
    let mut run = run(&[
        (Some(first_stage), None),
        (Some(second_stage), None),
        (Some(reopen), None),
    ]);
    let outcomes: Vec<_> = run.steps.iter().map(|step| step.0).collect();
    assert_eq!(outcomes[..2], [Outcome::Held, Outcome::Held]);
    assert_eq!(
        outcomes[2],
        Outcome::Held,
        "failed reopen must not release Legacy"
    );
    assert!(
        run.lease.needs_reopen,
        "a failed reopen keeps the next step reopening"
    );
    assert_eq!(
        run.movement.advance(&mut run.lease, &mut run.host),
        Outcome::Legacy
    );
    assert!(run.host.path(DeletePhase::Queue).exists());
    assert!(run.host.events.is_empty());
    assert_eq!(owners(run.root.path()), vec![Owner::Legacy, Owner::Legacy]);
}

#[test]
fn host_error_on_a_usable_handle_still_reopens_before_the_next_step() {
    let failed_queue = (None, Some(DeletePhase::Queue));
    let reopen = label(
        &run(&[failed_queue, (None, None)]).steps[1].1,
        &["file_sync"],
    );
    let mut run = run(&[failed_queue, (Some(reopen), None)]);
    assert_eq!(run.steps[0].0, Outcome::Held);
    assert_eq!(run.host.events, vec![DeletePhase::Dispatch]);
    let (outcome, _, reopens) = &run.steps[1];
    assert_eq!(
        (*outcome, &run.host.events, reopens - run.steps[0].2),
        (Outcome::Held, &vec![DeletePhase::Dispatch], 1),
        "the retry must reopen and stop at the failed read before any deletion"
    );
    assert_eq!(
        run.movement.advance(&mut run.lease, &mut run.host),
        Outcome::Ledger
    );
    assert_eq!(
        run.host.events,
        vec![
            DeletePhase::Dispatch,
            DeletePhase::Queue,
            DeletePhase::Accessories,
            DeletePhase::Row
        ]
    );
    assert_eq!(run.host.actor, 1);
}

#[tokio::test(start_paused = true)]
async fn bounded_retry_resumes_transient_failure_and_holds_permanent_failure() {
    for permanent in [false, true] {
        let root = sandbox();
        let mut host = Fixture::new(root.path());
        host.fail = Some(DeletePhase::Row);
        let mut lease = LedgerLease::new(root.path(), 9);
        let mut movement = Move::prepare(&mut lease, &mut host).unwrap();
        assert_eq!(movement.advance(&mut lease, &mut host), Outcome::Held);
        if !permanent {
            host.fail = None;
        }
        assert_eq!(
            movement.retry(&mut lease, &mut host, 2).await,
            if permanent {
                Outcome::Held
            } else {
                Outcome::Ledger
            }
        );
        assert_eq!(host.actor, if permanent { 0 } else { 1 });
    }
}

#[test]
fn e1_unbound_handback_holds_and_names_every_unbound_key() {
    let root = sandbox();
    let mut ledger = Ledger::open(root.path(), 9).unwrap();
    ledger
        .append_entry(
            &Entry::Received {
                key: 5,
                input: json!({"text":"bound"}),
            },
            &[],
        )
        .unwrap();
    // A commit claiming keys without a Staged record in its window leaves them unbound.
    ledger
        .append_entry(
            &Entry::MoveCommitted {
                first_staged_seq: 2,
                ids: vec![11, 12],
            },
            &[],
        )
        .unwrap();
    assert_eq!(
        ledger
            .rows()
            .unwrap()
            .unbound()
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![11, 12]
    );
    drop(ledger);
    let mut host = Fixture::new(root.path());
    let mut lease = LedgerLease::new(root.path(), 9);
    assert_eq!(handback(&mut lease, &mut host).unwrap(), Outcome::Held);
    assert_eq!(
        host.noticed,
        vec![
            (Some(11), "handback_unbound"),
            (Some(12), "handback_unbound")
        ]
    );
    assert_eq!(
        host.enqueued,
        vec![99],
        "a later row never jumps an unbound key"
    );
}

#[test]
fn e1_probe_reports_history_without_creating_a_ledger() {
    let root = sandbox();
    let opens = super::ledger::OPENS.with(|opens| opens.get());
    assert_eq!(Ledger::probe(root.path(), 9), Presence::Absent);
    assert!(!root.path().join("input_ledger").exists());
    assert_eq!(super::ledger::OPENS.with(|opens| opens.get()), opens);
    std::fs::create_dir_all(root.path().join("input_ledger/9")).unwrap();
    assert_eq!(Ledger::probe(root.path(), 9), Presence::Present);
    std::fs::write(root.path().join("input_ledger/10"), b"not a directory").unwrap();
    assert_eq!(Ledger::probe(root.path(), 10), Presence::Unreadable);
    assert!(
        !root.path().join("input_ledger/9/wal.0.jsonl").exists(),
        "probing must not open the ledger"
    );
}

// A turn row the transcript accepted before the move, with or without the accepted record's end.
struct Accepted(Option<u64>);
impl Host for Accepted {
    fn collect(&mut self, _: &Ledger) -> io::Result<Vec<Input>> {
        use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding, SourceId};
        let attempt = super::rows::AttemptEvidence {
            binding: SourceBinding {
                channel_id: 9,
                provider: ShadowProvider::Claude,
                source: SourceId {
                    session_id: "session".into(),
                    path: "/nonexistent/session.jsonl".into(),
                    dev: 1,
                    ino: 1,
                },
            },
            execution_nonce: "nonce".into(),
            eof: 0,
            rendered_prompt: "input".into(),
            source_ids: vec![8],
            record_end: self.0,
            native_turn_id: None,
        };
        let payload = json!({"text": "input", "move_attempt": serde_json::to_value(attempt)?});
        Ok(vec![Input {
            key: 8,
            payload,
            source: MoveSource::TurnRow,
            pins: vec![],
        }])
    }
    fn evidence(&mut self, _: &Input) -> io::Result<MoveEvidence> {
        Ok(MoveEvidence {
            user_record: true,
            turn_open: true,
            never_started: false,
            composer: Composer::Empty,
        })
    }
    fn delete(&mut self, _: DeletePhase) -> io::Result<()> {
        Ok(())
    }
    fn start_actor(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn reconcile(&mut self, _: u64, _: &Row) -> io::Result<(bool, Composer)> {
        Ok((false, Composer::Empty))
    }
    fn enqueue(&mut self, _: u64, _: &Row) -> io::Result<EnqueueOutcome> {
        Ok(EnqueueOutcome::Persisted)
    }
    fn notice(&mut self, _: Option<u64>, _: &'static str) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn moved_running_row_needs_the_accepted_record_end() {
    use super::rows::{HeldReason, RowState};
    let cases = [
        (Some(40), RowState::Running),
        (None, RowState::Held(HeldReason::Ambiguous)),
    ];
    for (end, state) in cases {
        let root = sandbox();
        let mut lease = LedgerLease::new(root.path(), 9);
        let mut host = Accepted(end);
        let mut movement = Move::prepare(&mut lease, &mut host).unwrap();
        assert_eq!(movement.advance(&mut lease, &mut host), Outcome::Ledger);
        let rows = lease.get().unwrap().rows().unwrap();
        assert_eq!(rows.row(8).unwrap().state, state, "record end {end:?}");
    }
}
