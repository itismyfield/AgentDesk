use super::*;
use crate::services::tui_input::durability_tests::supported::Recording;
use crate::services::tui_input::handover::{Composer, EnqueueOutcome, MoveEvidence};
use crate::services::tui_input::rows::{DoneReason, Row};
use crate::services::tui_input::transition::{self, DeletePhase, Host, Input};
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;
use crate::services::tui_prompt_dedupe::binding_events::SourceId;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const HOST: &str = "node-a";
const CUT: RowState = RowState::Abandoned(AbandonReason::UserClear);

#[derive(Default)]
struct Db {
    record: Option<NativeClearRecord>,
    down: bool,
    lose_ack: bool,
    fail_commit: bool,
    commits: usize,
    resolves: usize,
}

#[derive(Clone)]
struct Fake {
    provider: &'static str,
    channel: u64,
    capture: bool,
    db: Arc<Mutex<Db>>,
    execution: Arc<Mutex<Execution>>,
    after_reset: Execution,
    resets: Arc<AtomicUsize>,
    notices: Arc<Mutex<Vec<String>>>,
    transition: Option<Arc<tokio::sync::Mutex<()>>>,
    interleaved: Arc<AtomicUsize>,
}

impl Fake {
    fn new(provider: &'static str, channel: u64) -> Self {
        Self {
            provider,
            channel,
            capture: true,
            db: Arc::default(),
            execution: Arc::new(Mutex::new(Execution::Current)),
            after_reset: Execution::Retired,
            resets: Arc::default(),
            notices: Arc::default(),
            transition: None,
            interleaved: Arc::default(),
        }
    }
    fn db(&self) -> std::sync::MutexGuard<'_, Db> {
        self.db.lock().unwrap()
    }
    fn resets(&self) -> usize {
        self.resets.load(Ordering::SeqCst)
    }
    fn notices(&self) -> Vec<String> {
        self.notices.lock().unwrap().clone()
    }
    fn set_execution(&self, execution: Execution) {
        *self.execution.lock().unwrap() = execution;
    }
    fn stored(&self) -> ClearTicket {
        let ticket = self.db().record.clone().unwrap().ticket.unwrap();
        serde_json::from_value(ticket).unwrap()
    }
    // A ticket committed by a clear that stopped before any later step.
    fn durable_ticket(&self, ledger: &Ledger, keys: &[u64]) -> ClearTicket {
        let mut ticket = ticket(self.provider, self.channel);
        let rows = ledger.rows().unwrap();
        ticket.input = Some(InputCutoff {
            ledger_generation: ledger.snapshot().map_or(0, |s| s.generation),
            ledger_seq: rows.folded_seq(),
            affected_keys: keys.to_vec(),
        });
        self.db().record = Some(record(1, &ticket));
        ticket
    }
}

fn record(generation: i64, ticket: &ClearTicket) -> NativeClearRecord {
    NativeClearRecord {
        generation: NativeClearGeneration(generation),
        ticket: Some(serde_json::to_value(ticket).unwrap()),
        resolved: false,
        superseded: false,
        after_frontier: false,
    }
}

fn tmux(channel: u64) -> String {
    format!("adk-input-clear-test-{channel}")
}

fn ticket(provider: &str, channel: u64) -> ClearTicket {
    ClearTicket {
        context: BindingContext {
            schema: 1,
            provider: provider.into(),
            created_at: chrono::Utc::now(),
            execution_nonce: "nonce-1".into(),
            tmux_session: tmux(channel),
            channel_id: Some(channel),
            owner_runtime_root: "/nonexistent".into(),
            host: Some(HOST.into()),
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: None,
            first_prompt_digest: None,
            source_policy: None,
        },
        old: SourceId {
            session_id: "old".into(),
            path: "/nonexistent/old.jsonl".into(),
            dev: 1,
            ino: 1,
        },
        baseline: 0,
        input: None,
    }
}

impl ClearHost for Fake {
    fn identity(&self) -> Identity {
        Identity {
            provider: self.provider.into(),
            channel: self.channel,
            tmux: tmux(self.channel),
            host: Some(HOST.into()),
        }
    }
    fn capture(&mut self) -> Option<ClearTicket> {
        self.capture.then(|| ticket(self.provider, self.channel))
    }
    fn record(&mut self) -> Step<'_, anyhow::Result<Option<NativeClearRecord>>> {
        let db = self.db();
        let result = match db.down {
            true => Err(anyhow::anyhow!("postgres unavailable")),
            false => Ok(db.record.clone()),
        };
        Box::pin(async move { result })
    }
    fn commit<'a>(
        &'a mut self,
        ticket: &'a serde_json::Value,
    ) -> Step<'a, anyhow::Result<NativeClearGeneration>> {
        let mut db = self.db();
        db.commits += 1;
        let result = if db.down || db.fail_commit {
            Err(anyhow::anyhow!("commit failed"))
        } else {
            let generation = db.record.as_ref().map_or(1, |r| r.generation.0 + 1);
            db.record = Some(NativeClearRecord {
                generation: NativeClearGeneration(generation),
                ticket: Some(ticket.clone()),
                resolved: false,
                superseded: false,
                after_frontier: false,
            });
            match db.lose_ack {
                true => Err(anyhow::anyhow!("commit acknowledgement lost")),
                false => Ok(NativeClearGeneration(generation)),
            }
        };
        Box::pin(async move { result })
    }
    fn execution(&mut self, _: &ClearTicket) -> Execution {
        *self.execution.lock().unwrap()
    }
    fn reset<'a>(&'a mut self, _: &'a ClearTicket) -> Step<'a, bool> {
        self.resets.fetch_add(1, Ordering::SeqCst);
        // Any session transition taken here would interleave with the clear.
        if let Some(lock) = &self.transition
            && lock.clone().try_lock_owned().is_ok()
        {
            self.interleaved.fetch_add(1, Ordering::SeqCst);
        }
        self.set_execution(self.after_reset);
        Box::pin(async { true })
    }
    fn resolve(
        &mut self,
        generation: NativeClearGeneration,
    ) -> Step<'_, anyhow::Result<NativeClearResolve>> {
        let mut db = self.db();
        db.resolves += 1;
        let result = match (db.down, db.record.as_mut()) {
            (true, _) => Err(anyhow::anyhow!("postgres unavailable")),
            (false, Some(r)) if r.generation == generation && !r.resolved => {
                r.resolved = true;
                Ok(NativeClearResolve::Resolved)
            }
            (false, _) => Ok(NativeClearResolve::NotCurrent),
        };
        Box::pin(async move { result })
    }
    fn notice<'a>(&'a mut self, text: &'a str) -> Step<'a, ()> {
        self.notices.lock().unwrap().push(text.to_owned());
        Box::pin(async {})
    }
}

fn sandbox() -> tempfile::TempDir {
    let parent = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
    std::fs::create_dir_all(&parent).unwrap();
    tempfile::tempdir_in(parent).unwrap()
}

fn guard() -> OwnedMutexGuard<()> {
    Arc::new(tokio::sync::Mutex::new(()))
        .try_lock_owned()
        .unwrap()
}

fn receive(ledger: &mut Ledger, key: u64) {
    let input = json!({"text": format!("input {key}")});
    ledger
        .append_entry(&Entry::Received { key, input }, &[])
        .unwrap();
}

fn set(ledger: &mut Ledger, key: u64, state: RowState) {
    let entry = Entry::Transition {
        key,
        state,
        attempt: None,
    };
    ledger.append_entry(&entry, &[]).unwrap();
}

fn open(root: &std::path::Path, channel: u64, keys: &[u64]) -> Ledger {
    let mut ledger = Ledger::open(root, channel).unwrap();
    for &key in keys {
        receive(&mut ledger, key);
    }
    ledger
}

fn states(root: &std::path::Path, channel: u64) -> BTreeMap<u64, RowState> {
    let rows = Ledger::open(root, channel).unwrap().rows().unwrap();
    let keys: Vec<u64> = (1..=400).filter(|k| rows.row(*k).is_some()).collect();
    keys.into_iter()
        .map(|k| (k, rows.row(k).unwrap().state))
        .collect()
}

fn health(channel: u64) -> Option<String> {
    let needle = format!(" channel={channel} ");
    health_reasons().into_iter().find(|r| r.contains(&needle))
}

#[tokio::test]
async fn clear_cuts_the_cutoff_inputs_then_resets_and_resolves_for_both_providers() {
    for (provider, channel) in [("claude", 6_325_401), ("codex", 6_325_402)] {
        let root = sandbox();
        let mut ledger = open(root.path(), channel, &[11, 12, 13]);
        set(&mut ledger, 13, RowState::Done(DoneReason::Completed));
        let mut host = Fake::new(provider, channel);
        assert_eq!(run(&mut ledger, &mut host, guard()).await, Outcome::Cleared);
        let cut = host.stored().input.unwrap();
        assert_eq!(cut.affected_keys, vec![11, 12]);
        assert_eq!(cut.ledger_seq, 4);
        assert_eq!((host.resets(), host.db().resolves), (1, 1));
        assert!(host.db().record.as_ref().unwrap().resolved);
        let done = RowState::Done(DoneReason::Completed);
        assert_eq!(
            states(root.path(), channel),
            BTreeMap::from([(11, CUT), (12, CUT), (13, done)])
        );
        assert!(host.notices().is_empty() && health(channel).is_none());
    }
}

#[tokio::test]
async fn an_input_received_after_the_cutoff_keeps_its_row() {
    let (root, channel) = (sandbox(), 6_325_403);
    let mut ledger = open(root.path(), channel, &[11, 12]);
    let mut host = Fake::new("claude", channel);
    host.durable_ticket(&ledger, &[11, 12]);
    receive(&mut ledger, 13);
    assert_eq!(
        resume(&mut ledger, &mut host, guard()).await,
        Outcome::Cleared
    );
    assert_eq!(
        states(root.path(), channel),
        BTreeMap::from([(11, CUT), (12, CUT), (13, RowState::Received)])
    );
}

struct Legacy(Vec<u64>);
impl Host for Legacy {
    fn collect(&mut self, _: &Ledger) -> std::io::Result<Vec<Input>> {
        unreachable!()
    }
    fn evidence(&mut self, _: &Input) -> std::io::Result<MoveEvidence> {
        unreachable!()
    }
    fn delete(&mut self, _: DeletePhase) -> std::io::Result<()> {
        unreachable!()
    }
    fn start_actor(&mut self) -> std::io::Result<()> {
        unreachable!()
    }
    fn reconcile(&mut self, _: u64, _: &Row) -> std::io::Result<(bool, Composer)> {
        Ok((false, Composer::Empty))
    }
    fn enqueue(&mut self, key: u64, _: &Row) -> std::io::Result<EnqueueOutcome> {
        self.0.push(key);
        Ok(EnqueueOutcome::Persisted)
    }
    fn notice(&mut self, _: Option<u64>, _: &'static str) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_ticket_left_by_a_crash_is_replayed_before_handback_submits_anything() {
    let (root, channel) = (sandbox(), 6_325_404);
    let mut ledger = open(root.path(), channel, &[11, 12]);
    let mut host = Fake::new("claude", channel);
    host.durable_ticket(&ledger, &[11, 12]);
    let replayed = resume(&mut ledger, &mut host, guard()).await;
    drop(ledger);
    let mut legacy = Legacy(Vec::new());
    let outcome = transition::handback(root.path(), channel, &mut legacy).unwrap();
    assert!(
        legacy.0.is_empty(),
        "cut inputs resubmitted: {:?}",
        legacy.0
    );
    assert_eq!(outcome, transition::Outcome::Legacy);
    assert_eq!(replayed, Outcome::Cleared);
}

#[test]
fn a_wal_failure_holds_unresolved_and_resume_cuts_only_the_rest() {
    let (root, channel) = (sandbox(), 6_325_405);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut ledger = open(root.path(), channel, &[11, 12, 13]);
    let mut host = Fake::new("claude", channel);
    let recording = Recording::start(root.path());
    // Fail the second transition's write, after its bytes reached the file.
    recording.arm(Some(3));
    let outcome = runtime.block_on(run(&mut ledger, &mut host, guard()));
    drop(recording);
    assert_eq!(outcome, Outcome::Held(Unresolved::WalUncertain));
    assert_eq!((host.resets(), host.db().resolves), (0, 0));
    assert!(!host.db().record.as_ref().unwrap().resolved);
    assert!(health(channel).is_some_and(|r| r.contains("WalUncertain")));
    let mut ledger = Ledger::open(root.path(), channel).unwrap();
    let before = ledger.records().len();
    let outcome = runtime.block_on(resume(&mut ledger, &mut host, guard()));
    assert_eq!(outcome, Outcome::Cleared);
    assert_eq!(ledger.records().len(), before + 1);
    assert_eq!(
        states(root.path(), channel),
        BTreeMap::from([(11, CUT), (12, CUT), (13, CUT)])
    );
    assert!(host.db().record.as_ref().unwrap().resolved);
    assert!(health(channel).is_none());
}

#[tokio::test]
async fn the_clear_owns_the_transition_guard_until_it_resolves() {
    let (root, channel) = (sandbox(), 6_325_406);
    let mut ledger = open(root.path(), channel, &[11]);
    let mut host = Fake::new("claude", channel);
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    host.transition = Some(lock.clone());
    let guard = lock.clone().try_lock_owned().unwrap();
    assert_eq!(run(&mut ledger, &mut host, guard).await, Outcome::Cleared);
    assert_eq!(host.resets(), 1);
    assert_eq!(host.interleaved.load(Ordering::SeqCst), 0);
    assert!(lock.try_lock().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_receiver_neither_cancels_the_clear_nor_frees_its_guard_early() {
    let (root, channel) = (sandbox(), 6_325_407);
    let ledger = open(root.path(), channel, &[11, 12]);
    let host = Fake::new("claude", channel);
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    drop(start(ledger, host.clone(), lock.clone().lock_owned().await));
    let _released = lock.lock().await;
    assert!(host.db().record.as_ref().unwrap().resolved);
    assert_eq!(
        states(root.path(), channel),
        BTreeMap::from([(11, CUT), (12, CUT)])
    );
}

#[tokio::test]
async fn postgres_outage_holds_without_reset_or_cut_and_reports_it() {
    let (root, channel) = (sandbox(), 6_325_408);
    let mut ledger = open(root.path(), channel, &[11]);
    let mut host = Fake::new("claude", channel);
    host.db().down = true;
    let outcome = run(&mut ledger, &mut host, guard()).await;
    assert_eq!(outcome, Outcome::Held(Unresolved::PgUnavailable));
    assert_eq!((host.resets(), host.db().commits), (0, 0));
    assert_eq!(host.notices(), vec![PG_RETRY_NOTICE.to_owned()]);
    let reason = health(channel).unwrap();
    assert!(
        reason.contains("clear_unresolved provider=claude") && reason.contains("PgUnavailable")
    );
    assert_eq!(
        states(root.path(), channel),
        BTreeMap::from([(11, RowState::Received)])
    );
    host.db().down = false;
    assert_eq!(resume(&mut ledger, &mut host, guard()).await, Outcome::Idle);
    assert!(health(channel).is_none());
}

#[tokio::test]
async fn an_uncertain_commit_resets_nothing_until_the_ticket_is_read_back() {
    let (root, channel) = (sandbox(), 6_325_409);
    let mut ledger = open(root.path(), channel, &[11]);
    let mut host = Fake::new("claude", channel);
    host.db().lose_ack = true;
    // The read-back fails too, so the stored ticket is unknown.
    let outcome = {
        let mut uncertain = Uncertain(host.clone());
        run(&mut ledger, &mut uncertain, guard()).await
    };
    assert_eq!(host.resets(), 0);
    assert_eq!(outcome, Outcome::Held(Unresolved::CommitUncertain));
    assert_eq!(host.notices(), vec![PG_RETRY_NOTICE.to_owned()]);
    assert_eq!(
        states(root.path(), channel),
        BTreeMap::from([(11, RowState::Received)])
    );
    assert!(health(channel).is_some_and(|r| r.contains("CommitUncertain")));
    host.db().lose_ack = false;
    assert_eq!(
        resume(&mut ledger, &mut host, guard()).await,
        Outcome::Cleared
    );
    assert_eq!((host.resets(), host.db().commits), (1, 1));
    assert_eq!(states(root.path(), channel), BTreeMap::from([(11, CUT)]));
    assert!(health(channel).is_none());
}

// Reads succeed until the first commit attempt, as when Postgres drops mid-transaction.
struct Uncertain(Fake);
impl ClearHost for Uncertain {
    fn identity(&self) -> Identity {
        self.0.identity()
    }
    fn capture(&mut self) -> Option<ClearTicket> {
        self.0.capture()
    }
    fn record(&mut self) -> Step<'_, anyhow::Result<Option<NativeClearRecord>>> {
        let commits = self.0.db().commits;
        match commits {
            0 => self.0.record(),
            _ => Box::pin(async { Err(anyhow::anyhow!("postgres dropped")) }),
        }
    }
    fn commit<'a>(
        &'a mut self,
        ticket: &'a serde_json::Value,
    ) -> Step<'a, anyhow::Result<NativeClearGeneration>> {
        self.0.commit(ticket)
    }
    fn execution(&mut self, ticket: &ClearTicket) -> Execution {
        self.0.execution(ticket)
    }
    fn reset<'a>(&'a mut self, ticket: &'a ClearTicket) -> Step<'a, bool> {
        self.0.reset(ticket)
    }
    fn resolve(
        &mut self,
        generation: NativeClearGeneration,
    ) -> Step<'_, anyhow::Result<NativeClearResolve>> {
        self.0.resolve(generation)
    }
    fn notice<'a>(&'a mut self, text: &'a str) -> Step<'a, ()> {
        self.0.notice(text)
    }
}

#[tokio::test]
async fn a_lost_commit_acknowledgement_continues_from_the_stored_ticket() {
    let (root, channel) = (sandbox(), 6_325_410);
    let mut ledger = open(root.path(), channel, &[11]);
    let mut host = Fake::new("codex", channel);
    host.db().lose_ack = true;
    assert_eq!(run(&mut ledger, &mut host, guard()).await, Outcome::Cleared);
    assert_eq!((host.resets(), host.db().commits), (1, 1));
    assert_eq!(states(root.path(), channel), BTreeMap::from([(11, CUT)]));
}

#[tokio::test]
async fn a_confirmed_absent_commit_refuses_with_every_input_kept() {
    let (root, channel) = (sandbox(), 6_325_411);
    let mut ledger = open(root.path(), channel, &[11]);
    let mut host = Fake::new("claude", channel);
    host.db().fail_commit = true;
    let outcome = run(&mut ledger, &mut host, guard()).await;
    assert_eq!(outcome, Outcome::Refused(Refusal::CommitFailed));
    assert_eq!(host.resets(), 0);
    assert_eq!(
        states(root.path(), channel),
        BTreeMap::from([(11, RowState::Received)])
    );
}

#[tokio::test]
async fn a_reset_applied_before_a_crash_is_not_repeated() {
    let (root, channel) = (sandbox(), 6_325_412);
    let mut ledger = open(root.path(), channel, &[11]);
    let mut host = Fake::new("claude", channel);
    host.durable_ticket(&ledger, &[11]);
    set(&mut ledger, 11, CUT);
    host.set_execution(Execution::Retired);
    assert_eq!(
        resume(&mut ledger, &mut host, guard()).await,
        Outcome::Cleared
    );
    assert_eq!((host.resets(), host.db().resolves), (0, 1));
}

#[tokio::test]
async fn an_unconfirmed_reset_holds_and_keeps_the_cut_without_a_second_reset() {
    let (root, channel) = (sandbox(), 6_325_413);
    let mut ledger = open(root.path(), channel, &[11]);
    let mut host = Fake::new("claude", channel);
    host.after_reset = Execution::Unknown;
    let outcome = run(&mut ledger, &mut host, guard()).await;
    assert_eq!(outcome, Outcome::Held(Unresolved::ResetUnconfirmed));
    assert_eq!((host.resets(), host.db().resolves), (1, 0));
    assert_eq!(states(root.path(), channel), BTreeMap::from([(11, CUT)]));
    host.set_execution(Execution::Replaced);
    assert_eq!(
        resume(&mut ledger, &mut host, guard()).await,
        Outcome::Cleared
    );
    assert_eq!((host.resets(), host.db().resolves), (1, 1));
}

#[tokio::test]
async fn superseded_foreign_or_mismatched_tickets_hold_without_effects() {
    let (root, channel) = (sandbox(), 6_325_414);
    let mut ledger = open(root.path(), channel, &[11]);
    let mut host = Fake::new("claude", channel);
    let ticket = host.durable_ticket(&ledger, &[11]);
    let cases = [
        (true, ticket.clone(), Unresolved::Superseded),
        (
            false,
            ClearTicket {
                input: None,
                ..ticket.clone()
            },
            Unresolved::Foreign,
        ),
        (
            false,
            {
                let mut other = ticket.clone();
                other.context.host = Some("node-b".into());
                other
            },
            Unresolved::Mismatch,
        ),
    ];
    for (superseded, stored, reason) in cases {
        let mut stored = record(1, &stored);
        stored.superseded = superseded;
        host.db().record = Some(stored);
        let outcome = resume(&mut ledger, &mut host, guard()).await;
        assert_eq!(outcome, Outcome::Held(reason));
        assert_eq!((host.resets(), host.db().resolves), (0, 0));
        assert_eq!(
            states(root.path(), channel),
            BTreeMap::from([(11, RowState::Received)])
        );
    }
    // A new clear supersedes the stale cutoff and cuts whatever it left open.
    let mut stale = record(1, &ticket);
    stale.superseded = true;
    host.db().record = Some(stale);
    assert_eq!(run(&mut ledger, &mut host, guard()).await, Outcome::Cleared);
    assert_eq!(states(root.path(), channel), BTreeMap::from([(11, CUT)]));
}

#[tokio::test]
async fn a_reused_key_or_regressed_ledger_holds_without_cutting() {
    let (root, channel) = (sandbox(), 6_325_415);
    let mut ledger = open(root.path(), channel, &[11]);
    let mut host = Fake::new("claude", channel);
    host.durable_ticket(&ledger, &[11]);
    let back = RowState::Abandoned(AbandonReason::Handback);
    set(&mut ledger, 11, back);
    receive(&mut ledger, 11);
    let outcome = resume(&mut ledger, &mut host, guard()).await;
    assert_eq!(outcome, Outcome::Held(Unresolved::Mismatch));
    assert_eq!(
        states(root.path(), channel),
        BTreeMap::from([(11, RowState::Received)])
    );

    let (root, channel) = (sandbox(), 6_325_416);
    let mut ledger = open(root.path(), channel, &[11]);
    let mut host = Fake::new("claude", channel);
    let mut ticket = host.durable_ticket(&ledger, &[11]);
    ticket.input.as_mut().unwrap().ledger_seq = 99;
    host.db().record = Some(record(1, &ticket));
    let outcome = resume(&mut ledger, &mut host, guard()).await;
    assert_eq!(outcome, Outcome::Held(Unresolved::Mismatch));
    assert_eq!(
        states(root.path(), channel),
        BTreeMap::from([(11, RowState::Received)])
    );
}

#[tokio::test]
async fn unbound_oversized_or_unidentified_cutoffs_are_refused_before_commit() {
    let (root, channel) = (sandbox(), 6_325_417);
    let mut ledger = open(root.path(), channel, &[]);
    let commit = Entry::MoveCommitted {
        first_staged_seq: 1,
        ids: vec![77],
    };
    ledger.append_entry(&commit, &[]).unwrap();
    let mut host = Fake::new("claude", channel);
    let outcome = run(&mut ledger, &mut host, guard()).await;
    assert_eq!(outcome, Outcome::Refused(Refusal::Unbound));

    let (root, channel) = (sandbox(), 6_325_418);
    let keys: Vec<u64> = (1..=MAX_AFFECTED_KEYS as u64 + 1).collect();
    let mut ledger = open(root.path(), channel, &keys);
    let mut host = Fake::new("claude", channel);
    let outcome = run(&mut ledger, &mut host, guard()).await;
    assert_eq!(outcome, Outcome::Refused(Refusal::TooManyInputs));
    assert_eq!(host.db().commits, 0);

    let (root, channel) = (sandbox(), 6_325_419);
    let mut ledger = open(root.path(), channel, &[11]);
    let mut host = Fake::new("claude", channel);
    host.capture = false;
    let outcome = run(&mut ledger, &mut host, guard()).await;
    assert_eq!(outcome, Outcome::Refused(Refusal::ExecutionUnknown));
    assert_eq!((host.db().commits, host.resets()), (0, 0));
    assert_eq!(
        states(root.path(), channel),
        BTreeMap::from([(11, RowState::Received)])
    );
    drop(root);
}
