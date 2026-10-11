//! A real Move: every candidate proves its identity before the first pin or stage,
//! at prepare and again on the fresh fold, and a collected tracked tombstone keeps the key Legacy.
use super::super::rows_tests::g1b::{collect, input, tracked};
use super::*;
use crate::services::tui_input::rows::{AbandonReason, RowState};
use serde_json::Value;

const BACK: RowState = RowState::Abandoned(AbandonReason::Handback);

#[derive(Default)]
struct Probe {
    inputs: Vec<(u64, Value)>,
    pinned: Vec<u64>,
    deleted: usize,
    actors: usize,
}

impl Host for Probe {
    fn collect(&mut self, _: &Ledger) -> io::Result<Vec<Input>> {
        Ok((self.inputs.iter())
            .map(|(key, payload)| Input {
                key: *key,
                payload: payload.clone(),
                source: MoveSource::Queue,
                pins: vec![],
            })
            .collect())
    }
    fn evidence(&mut self, _: &Input) -> io::Result<MoveEvidence> {
        Ok(MoveEvidence {
            user_record: false,
            turn_open: false,
            never_started: true,
            composer: Composer::Empty,
        })
    }
    fn pin_input(&mut self, _: &Ledger, input: &mut Input) -> io::Result<()> {
        self.pinned.push(input.key);
        Ok(())
    }
    fn delete(&mut self, _: DeletePhase) -> io::Result<()> {
        self.deleted += 1;
        Ok(())
    }
    fn start_actor(&mut self) -> io::Result<()> {
        self.actors += 1;
        Ok(())
    }
    fn enqueue(&mut self, _: u64, _: &Row) -> io::Result<EnqueueOutcome> {
        Ok(EnqueueOutcome::Persisted)
    }
    fn notice(&mut self, _: Option<u64>, _: &'static str) -> io::Result<()> {
        Ok(())
    }
}

// A key handed back to Legacy untracked, under the identity {key, alias}.
fn handed_back(lease: &mut LedgerLease, key: u64, alias: u64) {
    let ledger = lease.get().unwrap();
    let received = Entry::Received {
        key,
        input: input(key, &[key, alias]),
    };
    ledger.append_entry(&received, &[]).unwrap();
    let back = Entry::Transition {
        key,
        state: BACK,
        attempt: None,
    };
    ledger.append_entry(&back, &[]).unwrap();
}

fn effects(lease: &mut LedgerLease, probe: &Probe) -> (usize, Vec<u64>, usize, usize) {
    let records = lease.reopen().unwrap().records().len();
    (records, probe.pinned.clone(), probe.deleted, probe.actors)
}

#[test]
fn g1b_move_preflights_all_candidates_before_effects() {
    // Only the last candidate returns under another identity: 20 was {20,21}, now {20,22}.
    let candidates = vec![(10, input(10, &[10])), (20, input(20, &[20, 22]))];
    let root = sandbox();
    let mut lease = LedgerLease::new(root.path(), 9);
    handed_back(&mut lease, 20, 21);
    let mut probe = Probe {
        inputs: candidates.clone(),
        ..Probe::default()
    };
    assert!(Move::prepare(&mut lease, &mut probe).is_err());
    assert_eq!(effects(&mut lease, &probe), (2, vec![], 0, 0));
    // The conflict appears only after prepare, so the fresh fold of the retry must catch it.
    let root = sandbox();
    let mut lease = LedgerLease::new(root.path(), 9);
    let mut probe = Probe {
        inputs: candidates,
        ..Probe::default()
    };
    let mut movement = Move::prepare(&mut lease, &mut probe).unwrap();
    handed_back(&mut lease, 20, 21);
    assert_eq!(movement.advance(&mut lease, &mut probe), Outcome::Held);
    assert_eq!(effects(&mut lease, &probe), (2, vec![], 0, 0));
    let rows = lease.reopen().unwrap().rows().unwrap();
    assert!(rows.row(10).is_none());
    assert_eq!(rows.row(20).unwrap().state, BACK);
}

#[test]
fn g1b_tracked_handback_after_real_gc_never_reactivates_through_move() {
    let root = sandbox();
    let mut lease = LedgerLease::new(root.path(), 9);
    let ledger = lease.get().unwrap();
    let received = Entry::Received {
        key: 1,
        input: input(1, &[1]),
    };
    ledger.append_entry(&received, &[]).unwrap();
    tracked(ledger, 1);
    for state in [RowState::Ready, BACK] {
        let entry = Entry::Transition {
            key: 1,
            state,
            attempt: None,
        };
        ledger.append_entry(&entry, &[]).unwrap();
    }
    collect(ledger);
    let kept = ledger.rows().unwrap().row(1).unwrap().clone();
    assert_eq!(kept.attempts.len(), 0, "attempt GC actually occurred");
    assert!(kept.retention.is_some());
    let records = ledger.records().len();
    let mut probe = Probe {
        inputs: vec![(1, input(1, &[1]))],
        ..Probe::default()
    };
    let mut movement = Move::prepare(&mut lease, &mut probe).unwrap();
    assert_eq!(movement.advance(&mut lease, &mut probe), Outcome::Legacy);
    assert_eq!(effects(&mut lease, &probe), (records, vec![], 0, 0));
    assert_eq!(lease.reopen().unwrap().rows().unwrap().row(1), Some(&kept));
}
