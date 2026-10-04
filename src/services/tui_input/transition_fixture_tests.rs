#![cfg(any(target_os = "macos", target_os = "linux"))]

use super::handover::{Composer, EnqueueOutcome, MoveEvidence, MoveSource};
use super::ledger::Ledger;
use super::rows::{Entry, Owner, Row};
use super::transition::{DeletePhase, Host, Input, Move, Outcome, backoff};
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
    fn notice(&mut self, _: Option<u64>, _: &'static str) -> io::Result<()> {
        self.notices += 1;
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
        let mut movement = Move::prepare(root.path(), 9, &mut host).unwrap();
        assert_eq!(movement.advance(&mut host), Outcome::Held);
        assert_eq!(movement.advance(&mut host), Outcome::Held);
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
        drop(movement);
        host.fail = None;
        let mut resumed = Move::prepare(root.path(), 9, &mut host).unwrap();
        assert_eq!(resumed.advance(&mut host), Outcome::Ledger);
        assert_eq!(
            &host.events[host.events.len() - 4..],
            &phases,
            "source retirement order is fixed"
        );
        assert_eq!(host.actor, 1);
        assert_eq!(
            handback(root.path(), 9, &mut host).unwrap(),
            Outcome::Legacy
        );
        assert_eq!(host.enqueued, vec![99, 8, 2]);
    }
}

#[test]
fn retry_backoff_is_bounded() {
    assert_eq!(backoff(0).as_secs(), 5);
    assert_eq!(backoff(1).as_secs(), 10);
    assert_eq!(backoff(6).as_secs(), 300);
    assert_eq!(backoff(u32::MAX).as_secs(), 300);
}

#[test]
fn uncertain_commit_whole_and_cut_reopen_decide_responsibility() {
    use super::durability_tests::supported::Recording;
    for cut in [false, true] {
        let root = sandbox();
        let mut host = Fixture::new(root.path());
        let mut movement = Move::prepare(root.path(), 9, &mut host).unwrap();
        let recording = Recording::start(root.path());
        Ledger::open(root.path(), 9).unwrap();
        let reopen_events = recording.events().len();
        // Measure directory fsyncs plus replay sync, then two stages write+sync.
        recording.arm(Some(reopen_events + 5));
        assert_eq!(movement.advance(&mut host), Outcome::Held);
        assert!(host.path(DeletePhase::Queue).exists());
        assert!(host.events.is_empty());
        drop(recording);
        if cut {
            let dir = root.path().join("input_ledger/9");
            let wal = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
                .unwrap();
            let file = std::fs::OpenOptions::new().write(true).open(wal).unwrap();
            file.set_len(file.metadata().unwrap().len() - 10).unwrap();
            file.sync_all().unwrap();
        }
        assert_eq!(
            movement.advance(&mut host),
            if cut {
                Outcome::Legacy
            } else {
                Outcome::Ledger
            }
        );
        for key in [8, 2] {
            assert_eq!(
                Ledger::open(root.path(), 9)
                    .unwrap()
                    .rows()
                    .unwrap()
                    .owner(key),
                if cut { Owner::Legacy } else { Owner::Ledger }
            );
        }
        if cut {
            let mut next = Move::prepare(root.path(), 9, &mut host).unwrap();
            assert_eq!(next.advance(&mut host), Outcome::Ledger);
        }
        assert_eq!(host.actor, 1);
    }
}

#[test]
fn second_staging_failure_requires_another_successful_reopen_before_legacy() {
    use super::durability_tests::supported::Recording;
    let root = sandbox();
    let mut host = Fixture::new(root.path());
    let mut movement = Move::prepare(root.path(), 9, &mut host).unwrap();
    let recording = Recording::start(root.path());
    Ledger::open(root.path(), 9).unwrap();
    let reopen_events = recording.events().len();
    recording.arm(Some(reopen_events + 1));
    assert_eq!(movement.advance(&mut host), Outcome::Held);
    // Adopt the first whole staging write; fail at the remaining stage's write.
    recording.arm(Some(reopen_events + 1));
    assert_eq!(movement.advance(&mut host), Outcome::Held);
    recording.arm(Some(1));
    assert_eq!(
        movement.advance(&mut host),
        Outcome::Held,
        "failed reopen must not release Legacy"
    );
    drop(recording);
    assert_eq!(movement.advance(&mut host), Outcome::Legacy);
    assert!(host.path(DeletePhase::Queue).exists());
    assert!(host.events.is_empty());
}

#[tokio::test(start_paused = true)]
async fn bounded_retry_resumes_transient_failure_and_holds_permanent_failure() {
    for permanent in [false, true] {
        let root = sandbox();
        let mut host = Fixture::new(root.path());
        host.fail = Some(DeletePhase::Row);
        let mut movement = Move::prepare(root.path(), 9, &mut host).unwrap();
        assert_eq!(movement.advance(&mut host), Outcome::Held);
        if !permanent {
            host.fail = None;
        }
        assert_eq!(
            movement.retry(&mut host, 2).await,
            if permanent {
                Outcome::Held
            } else {
                Outcome::Ledger
            }
        );
        assert_eq!(host.actor, if permanent { 0 } else { 1 });
    }
}
