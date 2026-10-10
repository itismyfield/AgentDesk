use super::*;
use crate::db::session_transcripts::{
    NativeClearGeneration, NativeClearRecord, NativeClearResolve,
};
use crate::services::discord::input_runtime::clear::{Execution, Identity};
use crate::services::discord::input_runtime::fence::{Mode, test_health};
use crate::services::tui_input::durability_tests::supported::Recording;
use crate::services::tui_input::handover::{Composer, EnqueueOutcome, MoveEvidence, MoveSource};
use crate::services::tui_input::ledger::{OPENS, SlotError};
use crate::services::tui_input::rows::{AbandonReason, Entry, Row, RowState};
use crate::services::tui_input::transition::{DeletePhase, Input};
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::writer::binding::BindingRecord;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;
use crate::services::tui_prompt_dedupe::binding_events::SourceId;
use crate::services::tui_prompt_dedupe::native_clear::{ClearTicket, InputCutoff};
use crate::services::turn_orchestrator::{ChannelMailboxRegistry, QueuePersistenceContext};
use poise::serenity_prelude::ChannelId;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;
use tokio::time::Instant;

const HOST: &str = "node-a";

fn sandbox() -> tempfile::TempDir {
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
    std::fs::create_dir_all(&parent).unwrap();
    tempfile::tempdir_in(parent).unwrap()
}

// A process: the registry a restart would replace.
fn registry() -> &'static Registry {
    Box::leak(Box::new(Registry::new()))
}

fn tmux(channel: u64) -> String {
    format!("adk-supervisor-test-{channel}")
}

fn opens() -> usize {
    OPENS.with(|opens| opens.get())
}

fn seqs(events: &[BindingEvent]) -> Vec<u64> {
    events.iter().map(|event| event.seq).collect()
}

// Work a fake clear worker runs right after its reset.
type Then = Option<Box<dyn FnOnce() + Send>>;

// Everything the fakes observed, shared across the supervisor, its workers and the test.
#[derive(Default)]
struct World {
    log: Mutex<Vec<&'static str>>,
    clears: Mutex<Vec<(Instant, Vec<String>)>>,
    record: Mutex<Option<NativeClearRecord>>,
    resets: Mutex<VecDeque<(bool, Then)>>,
    retired: AtomicBool,
    notices: Mutex<Vec<String>>,
    undelivered: AtomicUsize,
    failing_delivery: AtomicBool,
    enqueued: Mutex<Vec<u64>>,
    // The next supervisor clear worker signals `entered`, then waits on `latch`.
    latch: Mutex<Option<mpsc::Receiver<()>>>,
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl World {
    fn log(&self) -> Vec<&'static str> {
        self.log.lock().unwrap().clone()
    }
    fn note(&self, entry: &'static str) {
        self.log.lock().unwrap().push(entry);
    }
    fn gaps(&self) -> Vec<u64> {
        let clears = self.clears.lock().unwrap();
        (clears.windows(2))
            .map(|pair| (pair[1].0 - pair[0].0).as_secs())
            .collect()
    }
    fn notices(&self) -> Vec<String> {
        self.notices.lock().unwrap().clone()
    }
    // Queues one reset result; `then` runs inside the clear worker after the reset.
    fn reset(&self, confirmed: bool, then: Then) {
        self.resets.lock().unwrap().push_back((confirmed, then));
    }
    // A clear committed before a crash: its cutoff names `keys` and nothing after it ran.
    fn durable_ticket(&self, channel: u64, ledger: &Ledger, keys: &[u64]) {
        let mut ticket = ticket(channel);
        let rows = ledger.rows().unwrap();
        ticket.input = Some(InputCutoff {
            ledger_generation: ledger.snapshot().map_or(0, |s| s.generation),
            ledger_seq: rows.folded_seq(),
            affected_keys: keys.to_vec(),
        });
        *self.record.lock().unwrap() = Some(NativeClearRecord {
            generation: NativeClearGeneration(1),
            ticket: Some(serde_json::to_value(&ticket).unwrap()),
            resolved: false,
            superseded: false,
            after_frontier: false,
        });
    }
}

fn ticket(channel: u64) -> ClearTicket {
    ClearTicket {
        context: BindingContext {
            schema: 1,
            provider: "claude".into(),
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

struct ClearFake {
    channel: u64,
    world: Arc<World>,
    latch: Option<mpsc::Receiver<()>>,
    _alive: Option<mpsc::Sender<()>>,
}

impl ClearHost for ClearFake {
    fn identity(&self) -> Identity {
        Identity {
            provider: "claude".into(),
            channel: self.channel,
            tmux: tmux(self.channel),
            host: Some(HOST.into()),
        }
    }
    fn capture(&mut self) -> Option<ClearTicket> {
        None
    }
    fn record(&mut self) -> Step<'_, anyhow::Result<Option<NativeClearRecord>>> {
        if let Some(latch) = self.latch.take() {
            if let Some(entered) = self.world.entered.lock().unwrap().take() {
                let _ = entered.send(());
            }
            let _ = latch.recv();
        }
        let record = self.world.record.lock().unwrap().clone();
        Box::pin(async move { Ok(record) })
    }
    fn commit<'a>(
        &'a mut self,
        _: &'a serde_json::Value,
    ) -> Step<'a, anyhow::Result<NativeClearGeneration>> {
        Box::pin(async { Err(anyhow::anyhow!("resume never commits")) })
    }
    fn execution(&mut self, _: &ClearTicket) -> Execution {
        match self.world.retired.load(Ordering::SeqCst) {
            true => Execution::Retired,
            false => Execution::Current,
        }
    }
    fn reset<'a>(&'a mut self, _: &'a ClearTicket) -> Step<'a, bool> {
        let (confirmed, then) =
            (self.world.resets.lock().unwrap().pop_front()).unwrap_or((true, None));
        if let Some(then) = then {
            then();
        }
        self.world.retired.store(confirmed, Ordering::SeqCst);
        Box::pin(async move { confirmed })
    }
    fn resolve(
        &mut self,
        generation: NativeClearGeneration,
    ) -> Step<'_, anyhow::Result<NativeClearResolve>> {
        let mut record = self.world.record.lock().unwrap();
        let resolved = record.as_mut().filter(|r| r.generation == generation);
        let result = match resolved {
            Some(record) => {
                record.resolved = true;
                Ok(NativeClearResolve::Resolved)
            }
            None => Ok(NativeClearResolve::NotCurrent),
        };
        Box::pin(async move { result })
    }
    fn notice<'a>(&'a mut self, _: &'a str) -> Step<'a, ()> {
        Box::pin(async {})
    }
}

// Legacy population of plain queued inputs; a deletion or enqueue is only recorded.
struct MoveFake {
    inputs: Vec<u64>,
    world: Arc<World>,
    notices: Vec<(Option<u64>, &'static str)>,
}

impl Host for MoveFake {
    fn collect(&mut self, _: &Ledger) -> io::Result<Vec<Input>> {
        Ok((self.inputs.iter())
            .map(|&key| Input {
                key,
                payload: json!({ "text": key }),
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
    fn delete(&mut self, _: DeletePhase) -> io::Result<()> {
        self.world.note("delete");
        Ok(())
    }
    fn start_actor(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn reconcile(&mut self, _: u64, _: &Row) -> io::Result<(bool, Composer)> {
        Ok((false, Composer::Empty))
    }
    fn enqueue(&mut self, key: u64, _: &Row) -> io::Result<EnqueueOutcome> {
        self.world.enqueued.lock().unwrap().push(key);
        Ok(EnqueueOutcome::Persisted)
    }
    fn notice(&mut self, key: Option<u64>, reason: &'static str) -> io::Result<()> {
        self.notices.push((key, reason));
        Ok(())
    }
}

struct Fake {
    channel: u64,
    world: Arc<World>,
    registry: &'static Registry,
    inputs: Vec<u64>,
    mailbox: ChannelMailboxRegistry,
}

impl Ports for Fake {
    type Clear = ClearFake;
    type Move = MoveFake;
    fn freeze(&mut self, closing: Arc<Closing>) -> Step<'_, Result<(), Failure>> {
        self.world.note("freeze");
        let handle = self.mailbox.handle(ChannelId::new(self.channel));
        Box::pin(async move {
            let context = QueuePersistenceContext::new(&ProviderKind::Claude, "token", None);
            let ack = handle.freeze_input(closing.clone(), context).await?;
            closing.freeze(ack)
        })
    }
    fn clear(&mut self) -> Step<'_, Option<(ClearFake, OwnedMutexGuard<()>)>> {
        self.world.note("clear");
        let health = self.registry.health_reasons();
        self.world
            .clears
            .lock()
            .unwrap()
            .push((Instant::now(), health));
        // Bounds a mutant that would retry without a budget.
        let exhausted = self.world.clears.lock().unwrap().len() > 2 * BUDGET as usize;
        let host = ClearFake {
            channel: self.channel,
            world: self.world.clone(),
            latch: self.world.latch.lock().unwrap().take(),
            _alive: None,
        };
        Box::pin(async move {
            let guard = Arc::new(tokio::sync::Mutex::new(())).lock_owned().await;
            (!exhausted).then_some((host, guard))
        })
    }
    fn transition(&mut self, _: Arc<Closing>) -> io::Result<MoveFake> {
        self.world.note("move");
        Ok(MoveFake {
            inputs: self.inputs.clone(),
            world: self.world.clone(),
            notices: Vec::new(),
        })
    }
    fn take_notices(host: &mut MoveFake) -> Vec<(Option<u64>, &'static str)> {
        std::mem::take(&mut host.notices)
    }
    fn notice(&mut self, text: String) -> Step<'_, bool> {
        let delivered = !self.world.failing_delivery.load(Ordering::SeqCst);
        match delivered {
            true => self.world.notices.lock().unwrap().push(text),
            false => {
                self.world.undelivered.fetch_add(1, Ordering::SeqCst);
            }
        }
        Box::pin(async move { delivered })
    }
}

fn event(channel: u64, seq: u64) -> BindingEvent {
    BindingEvent {
        seq,
        channel_id: channel,
        provider: ShadowProvider::Claude,
        tmux_session: tmux(channel),
        execution_nonce: "nonce-1".into(),
        record: BindingRecord::Rejected {
            detail: "fixture".into(),
        },
        committed_at: chrono::Utc::now(),
    }
}

// A binding log whose watch carries the latest seq, as the P5 log publishes it.
struct Binding {
    channel: u64,
    world: Arc<World>,
    events: Mutex<Vec<BindingEvent>>,
    tx: Mutex<watch::Sender<u64>>,
    append_on_subscribe: AtomicUsize,
    unreadable: AtomicBool,
}

impl Binding {
    fn new(channel: u64, world: &Arc<World>) -> Arc<Self> {
        Arc::new(Self {
            channel,
            world: world.clone(),
            events: Mutex::default(),
            tx: Mutex::new(watch::channel(0).0),
            append_on_subscribe: AtomicUsize::new(0),
            unreadable: AtomicBool::new(false),
        })
    }
    fn seq(&self) -> u64 {
        self.events.lock().unwrap().last().map_or(0, |e| e.seq)
    }
    fn append(&self, count: u64) {
        let seq = self.seq();
        let mut events = self.events.lock().unwrap();
        events.extend((seq + 1..=seq + count).map(|seq| event(self.channel, seq)));
        drop(events);
        self.tx.lock().unwrap().send_replace(seq + count);
    }
    fn append_after_gap(&self) {
        let seq = self.seq() + 2;
        self.events.lock().unwrap().push(event(self.channel, seq));
        self.tx.lock().unwrap().send_replace(seq);
    }
    // A change notice that names no new event.
    fn repeat(&self) {
        self.tx.lock().unwrap().send_replace(self.seq());
    }
    // The publisher went away; earlier receivers observe a closed watch.
    fn close(&self) {
        *self.tx.lock().unwrap() = watch::channel(self.seq()).0;
    }
}

impl BindingEvents for Binding {
    fn binding_events_since(&self, _: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        if self.unreadable.load(Ordering::SeqCst) {
            return Err("binding log unreadable".into());
        }
        let events = self.events.lock().unwrap();
        Ok(events.iter().filter(|e| e.seq > after).cloned().collect())
    }
    fn subscribe(&self, _: u64) -> watch::Receiver<u64> {
        self.world.note("subscribe");
        let rx = self.tx.lock().unwrap().subscribe();
        let appended = self.append_on_subscribe.swap(0, Ordering::SeqCst);
        if appended > 0 {
            self.append(appended as u64);
        }
        rx
    }
}

struct Env(Option<std::ffi::OsString>);
impl Env {
    fn set(path: &Path) -> Self {
        let old = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
        Self(old)
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        match &self.0 {
            Some(old) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", old) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }
}

// One channel under an isolated runtime root, process registry and fence gate.
// Fields drop in order, so the environment is restored before the lock is released.
struct Rig {
    root: PathBuf,
    channel: u64,
    gate: Arc<Gate>,
    world: Arc<World>,
    binding: Arc<Binding>,
    registry: &'static Registry,
    _health: test_health::Clear,
    _dir: tempfile::TempDir,
    _env: Env,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Rig {
    fn new(channel: u64) -> Self {
        let lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let dir = sandbox();
        let env = Env::set(dir.path());
        let root = dir.path().join("runtime");
        std::fs::create_dir_all(&root).unwrap();
        let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
        let world = Arc::new(World::default());
        Self {
            root,
            channel,
            _health: test_health::Clear::new(&gate),
            gate,
            binding: Binding::new(channel, &world),
            world,
            registry: registry(),
            _dir: dir,
            _env: env,
            _lock: lock,
        }
    }
    fn ledger(&self) -> Ledger {
        Ledger::open(&self.root, self.channel).unwrap()
    }
    // Key `unbound` was committed without a Staged record; `bound` was staged and committed.
    fn unbound_commit(&self, unbound: u64, bound: u64) {
        let mut ledger = self.ledger();
        let staged = Entry::Staged {
            key: bound,
            input: json!({ "text": bound }),
            state: RowState::Received,
        };
        ledger.append_entry(&staged, &[]).unwrap();
        let commit = Entry::MoveCommitted {
            first_staged_seq: 1,
            ids: vec![unbound, bound],
        };
        ledger.append_entry(&commit, &[]).unwrap();
    }
    fn received(&self, key: u64) -> Ledger {
        let mut ledger = self.ledger();
        let entry = Entry::Received {
            key,
            input: json!({ "text": key }),
        };
        ledger.append_entry(&entry, &[]).unwrap();
        ledger
    }
    fn supervisor(&self, request: Request, inputs: Vec<u64>) -> Supervisor<Fake> {
        self.supervisor_with(request, inputs, None)
    }
    fn supervisor_with(
        &self,
        request: Request,
        inputs: Vec<u64>,
        refusal: Option<&'static str>,
    ) -> Supervisor<Fake> {
        let config = Config {
            provider: ProviderKind::Claude,
            channel: self.channel,
            root: self.root.clone(),
            request,
            refusal,
            binding: self.binding.clone(),
        };
        let ports = Fake {
            channel: self.channel,
            world: self.world.clone(),
            registry: self.registry,
            inputs,
            mailbox: ChannelMailboxRegistry::default(),
        };
        Supervisor::start(self.registry, config, ports).unwrap()
    }
    fn health(&self) -> Vec<String> {
        super::super::reasons_with(self.registry)
    }
    // Puts a plain file where the ledger directory belongs, or puts the directory back.
    fn break_ledger(&self, broken: bool) {
        let dir = self.root.join(format!("input_ledger/{}", self.channel));
        let aside = dir.with_extension("aside");
        if broken {
            std::fs::rename(&dir, &aside).unwrap();
            std::fs::write(&dir, b"").unwrap();
        } else {
            std::fs::remove_file(&dir).unwrap();
            std::fs::rename(&aside, &dir).unwrap();
        }
    }
    // Every clear reset from here on is unconfirmed, through the whole boot budget.
    fn unconfirmed_resets(&self) {
        for _ in 0..=BUDGET {
            self.world.reset(false, None);
        }
    }
}

// The ledger as a fresh process would read it.
fn rows(root: &Path, channel: u64) -> crate::services::tui_input::rows::Rows {
    Ledger::open(root, channel).unwrap().rows().unwrap()
}

#[tokio::test(start_paused = true)]
async fn crash_left_clear_resumes_before_move_and_retries_at_once_on_a_binding_change() {
    let rig = Rig::new(6_325_701);
    let ledger = rig.received(11);
    rig.world.durable_ticket(rig.channel, &ledger, &[11]);
    drop(ledger);
    let binding = rig.binding.clone();
    rig.world
        .reset(false, Some(Box::new(move || binding.append(1))));
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    assert_eq!(supervisor.boot().await, Landing::Admitted);
    assert_eq!(
        rig.world.log(),
        [
            "subscribe",
            "freeze",
            "clear",
            "clear",
            "move",
            "delete",
            "delete",
            "delete",
            "delete"
        ],
        "the clear resumes before any move or admission"
    );
    assert_eq!(
        rig.world.gaps(),
        [0],
        "an advancing binding wake retries at once"
    );
    assert!(supervisor.admission_open());
    assert_eq!(
        rig.gate.mode(),
        Mode::Frozen,
        "E2a lands with the gate frozen"
    );
    assert_eq!(
        rows(&rig.root, rig.channel).row(11).unwrap().state,
        RowState::Abandoned(AbandonReason::UserClear)
    );
}

#[tokio::test(start_paused = true)]
async fn unconfirmed_reset_backs_off_within_the_boot_budget_and_only_advancing_wakes_skip_it() {
    let rig = Rig::new(6_325_702);
    let ledger = rig.received(11);
    rig.world.durable_ticket(rig.channel, &ledger, &[11]);
    drop(ledger);
    for retry in 0..=BUDGET {
        let binding = rig.binding.clone();
        let then: Then = match retry {
            1 => Some(Box::new(move || binding.repeat())),
            2 => Some(Box::new(move || binding.append(1))),
            _ => None,
        };
        rig.world.reset(false, then);
    }
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    assert_eq!(
        supervisor.boot().await,
        Landing::Held(HoldCause::TransitionHeld("clear_retry_exhausted"))
    );
    assert_eq!(
        rig.world.gaps(),
        [5, 10, 0, 40, 80, 160, 300, 300],
        "one advancing wake replaces one backoff; a repeated notice does not"
    );
    assert!(
        !rig.world.log().contains(&"move"),
        "nothing runs past the clear"
    );
    assert!(!supervisor.admission_open());
    let exhausted: Vec<_> = (rig.world.notices().into_iter())
        .filter(|text| text.contains("재시도 한도"))
        .collect();
    assert_eq!(exhausted.len(), 1);
    let health = rig.health();
    assert!(
        health.iter().any(|r| r.starts_with("clear_unresolved")
            && r.contains(&format!("channel={}", rig.channel)))
    );
    assert!(
        health
            .iter()
            .any(|r| r.contains("reason=clear_retry_exhausted"))
    );
}

#[tokio::test(start_paused = true)]
async fn closed_binding_watch_during_clear_retry_reports_then_resubscribes() {
    let rig = Rig::new(6_325_703);
    let ledger = rig.received(11);
    rig.world.durable_ticket(rig.channel, &ledger, &[11]);
    drop(ledger);
    let (closer, appender) = (rig.binding.clone(), rig.binding.clone());
    rig.world
        .reset(false, Some(Box::new(move || closer.close())));
    rig.world
        .reset(false, Some(Box::new(move || appender.append(1))));
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    assert_eq!(supervisor.boot().await, Landing::Admitted);
    let clears = rig.world.clears.lock().unwrap().clone();
    let unreadable = |health: &Vec<String>| {
        health
            .iter()
            .any(|r| r.contains("reason=binding_unreadable"))
    };
    let seen: Vec<bool> = clears
        .iter()
        .map(|(_, health)| unreadable(health))
        .collect();
    assert_eq!(seen, [false, true, false], "held while the watch is gone");
    assert_eq!(
        rig.world.gaps(),
        [5, 0],
        "a resubscribe that reads new events retries at once"
    );
    assert_eq!(supervisor.cursor().unwrap().seq(), 1);
    assert!(!unreadable(&rig.health()));
}

#[tokio::test]
async fn baseline_and_wakes_apply_each_event_once_in_seq_order() {
    let world = Arc::new(World::default());
    let binding = Binding::new(6_325_704, &world);
    binding.append(1);
    // An append between subscribe and the baseline read.
    binding.append_on_subscribe.store(1, Ordering::SeqCst);
    let (mut cursor, baseline) = Cursor::start(binding.clone(), 6_325_704).unwrap();
    assert_eq!((seqs(&baseline), cursor.seq()), (vec![1, 2], 2));
    assert_eq!(
        cursor.wake().await.unwrap(),
        vec![],
        "its wake applies nothing"
    );
    binding.append(3);
    assert_eq!(seqs(&cursor.wake().await.unwrap()), [3, 4, 5]);
    binding.repeat();
    assert_eq!(cursor.wake().await.unwrap(), vec![]);
    assert_eq!(cursor.seq(), 5);
}

#[tokio::test]
async fn seq_gap_or_closed_watch_holds_until_a_resubscribe_rereads() {
    let world = Arc::new(World::default());
    let binding = Binding::new(6_325_705, &world);
    let (mut cursor, _) = Cursor::start(binding.clone(), 6_325_705).unwrap();
    binding.append(1);
    assert_eq!(seqs(&cursor.wake().await.unwrap()), [1]);
    binding.append_after_gap();
    assert!(cursor.wake().await.is_err(), "a gap is never skipped");
    assert_eq!(cursor.seq(), 1);
    let world = Arc::new(World::default());
    let binding = Binding::new(6_325_706, &world);
    let (mut cursor, _) = Cursor::start(binding.clone(), 6_325_706).unwrap();
    binding.close();
    binding.append(2);
    assert!(cursor.wake().await.is_err());
    assert_eq!(seqs(&cursor.recover().unwrap()), [1, 2]);
    binding.append(1);
    assert_eq!(seqs(&cursor.wake().await.unwrap()), [3]);
}

#[tokio::test]
async fn refused_session_host_reopens_legacy_only_without_ledger_history() {
    for history in [false, true] {
        let rig = Rig::new(6_325_707 + u64::from(history));
        if history {
            std::fs::create_dir_all(rig.root.join(format!("input_ledger/{}", rig.channel)))
                .unwrap();
        }
        let mut supervisor = rig.supervisor_with(Request::Ledger, vec![], Some("herdr"));
        let landing = supervisor.boot().await;
        let health = rig.health();
        assert!(
            health
                .iter()
                .any(|r| r.starts_with("turn_mode_refused") && r.ends_with("reason=herdr"))
        );
        assert_eq!(
            rig.world.log(),
            ["subscribe"],
            "nothing past the close runs"
        );
        if history {
            assert_eq!(landing, Landing::Held(HoldCause::ModeRefused("herdr")));
            assert_eq!(rig.gate.mode(), Mode::Closing);
        } else {
            assert_eq!(landing, Landing::Legacy);
            assert_eq!(rig.gate.mode(), Mode::LegacyOpen);
            assert!(!rig.root.join("input_ledger").exists());
        }
    }
}

#[tokio::test(start_paused = true)]
async fn unbound_key_stays_held_visible_and_noticed_once_across_retries_and_restart() {
    let (unbound, bound) = (31, 32);
    let rig = Rig::new(6_325_709);
    rig.unbound_commit(unbound, bound);
    let wal = |rig: &Rig| rows(&rig.root, rig.channel).folded_seq();
    let before = wal(&rig);
    let mut supervisor = rig.supervisor(Request::Ledger, vec![unbound, bound]);
    assert_eq!(
        supervisor.boot().await,
        Landing::Held(HoldCause::TransitionHeld("move_held"))
    );
    let health = rig.health();
    let at = format!("provider=claude channel={}", rig.channel);
    assert!(health.contains(&format!(
        "input_reconcile_required {at} reason=unbound keys={unbound}"
    )));
    assert!(health.contains(&format!("turn_transition_held {at} reason=move_held")));
    assert_eq!(rig.world.notices().len(), 1, "one Notice per episode");
    assert!(rig.world.notices()[0].contains(&format!("message_id:{unbound}")));
    assert!(!rig.world.log().contains(&"delete"));
    supervisor.slot().get().unwrap().checkpoint_rows().unwrap();
    let reread = supervisor.slot().reopen().unwrap().rows().unwrap();
    assert_eq!(
        reread.unbound().iter().copied().collect::<Vec<_>>(),
        [unbound]
    );
    assert_eq!(wal(&rig), before, "no record resolves or rebinds the key");
    assert!(supervisor.release());
    assert!(!rig.health().iter().any(|r| r.contains(&at)));
    // A restart sends the episode's Notice again; failed delivery leaves the ledger alone.
    rig.world.failing_delivery.store(true, Ordering::SeqCst);
    let mut restarted = rig.supervisor(Request::Ledger, vec![unbound, bound]);
    assert!(matches!(restarted.boot().await, Landing::Held(_)));
    assert_eq!(
        rig.world.undelivered.load(Ordering::SeqCst),
        1 + BUDGET as usize
    );
    rig.world.failing_delivery.store(false, Ordering::SeqCst);
    assert!(restarted.release());
    let mut third = rig.supervisor(Request::Ledger, vec![unbound, bound]);
    assert!(matches!(third.boot().await, Landing::Held(_)));
    assert_eq!(rig.world.notices().len(), 2);
    assert_eq!(wal(&rig), before);
}

#[tokio::test(start_paused = true)]
async fn held_move_never_enters_handback_while_a_finished_one_does() {
    {
        let rig = Rig::new(6_325_710);
        rig.unbound_commit(31, 32);
        let mut supervisor = rig.supervisor(Request::Legacy, vec![31, 32]);
        assert_eq!(
            supervisor.boot().await,
            Landing::Held(HoldCause::TransitionHeld("move_held"))
        );
        assert_eq!(supervisor.handbacks, 0);
        assert_eq!(rig.gate.mode(), Mode::Frozen);
        assert!(rig.world.enqueued.lock().unwrap().is_empty());
    }
    let rig = Rig::new(6_325_711);
    drop(rig.received(41));
    let mut supervisor = rig.supervisor(Request::Legacy, vec![]);
    assert_eq!(supervisor.boot().await, Landing::HandedBack);
    assert_eq!(supervisor.handbacks, 1);
    assert_eq!(rig.gate.mode(), Mode::Handback);
    assert_eq!(*rig.world.enqueued.lock().unwrap(), [41]);
    assert!(
        !supervisor.admission_open(),
        "a Legacy request never admits ledger input"
    );
}

#[tokio::test(start_paused = true)]
async fn every_boot_closes_admission_until_its_own_clear_and_move_pass() {
    let rig = Rig::new(6_325_718);
    drop(rig.received(11));
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    assert_eq!(supervisor.boot().await, Landing::Admitted);
    assert!(supervisor.admission_open());
    let ledger = supervisor.slot().get().unwrap();
    rig.world.durable_ticket(rig.channel, ledger, &[11]);
    rig.unconfirmed_resets();
    assert_eq!(
        supervisor.boot().await,
        Landing::Held(HoldCause::TransitionHeld("clear_retry_exhausted"))
    );
    assert!(
        !supervisor.admission_open(),
        "a held clear closes admission"
    );
    assert_eq!(supervisor.boot().await, Landing::Admitted);
    assert!(supervisor.admission_open());
    assert_eq!(rig.gate.mode(), Mode::Frozen);
    rig.break_ledger(true);
    assert_eq!(
        supervisor.boot().await,
        Landing::Held(HoldCause::LedgerUnreadable)
    );
    assert!(
        !supervisor.admission_open(),
        "an unreadable ledger closes admission"
    );
    rig.break_ledger(false);
    assert!(supervisor.release());
}

#[tokio::test(start_paused = true)]
async fn a_cause_leaves_health_once_rechecked_while_an_unsettled_clear_stays() {
    let rig = Rig::new(6_325_719);
    let ledger = rig.received(11);
    rig.world.durable_ticket(rig.channel, &ledger, &[11]);
    drop(ledger);
    rig.unconfirmed_resets();
    let mut supervisor = rig.supervisor(Request::Ledger, vec![]);
    let at = format!("provider=claude channel={}", rig.channel);
    let lines = |rig: &Rig| -> Vec<String> {
        (rig.health().into_iter())
            .filter(|r| r.contains(&at))
            .collect()
    };
    let unresolved = format!("clear_unresolved {at} reason=ResetUnconfirmed");
    let clear = format!("turn_transition_held {at} reason=clear_retry_exhausted");
    let binding = format!("input_reconcile_required {at} reason=binding_unreadable");
    let ledger = format!("ledger_unreadable {at}");
    assert_eq!(
        supervisor.boot().await,
        Landing::Held(HoldCause::TransitionHeld("clear_retry_exhausted"))
    );
    rig.binding.unreadable.store(true, Ordering::SeqCst);
    assert_eq!(
        supervisor.boot().await,
        Landing::Held(HoldCause::BindingUnreadable)
    );
    assert_eq!(lines(&rig), [&*unresolved, &*binding, &*clear]);
    rig.binding.unreadable.store(false, Ordering::SeqCst);
    rig.break_ledger(true);
    assert_eq!(
        supervisor.boot().await,
        Landing::Held(HoldCause::LedgerUnreadable)
    );
    assert_eq!(
        lines(&rig),
        [&*unresolved, &*ledger, &*clear],
        "a reread binding leaves; the unsettled clear stays"
    );
    rig.break_ledger(false);
    assert_eq!(supervisor.boot().await, Landing::Admitted);
    assert_eq!(lines(&rig), Vec::<String>::new());
    assert!(supervisor.release());
}

#[tokio::test(start_paused = true)]
async fn unbound_keys_are_reported_before_a_crash_left_clear_resumes() {
    let (unbound, bound) = (31, 32);
    let rig = Rig::new(6_325_720);
    rig.unbound_commit(unbound, bound);
    rig.world
        .durable_ticket(rig.channel, &rig.ledger(), &[bound]);
    let (release, latch) = mpsc::channel();
    let (entered, latched) = tokio::sync::oneshot::channel();
    *rig.world.latch.lock().unwrap() = Some(latch);
    *rig.world.entered.lock().unwrap() = Some(entered);
    let mut supervisor = rig.supervisor(Request::Ledger, vec![unbound, bound]);
    let landing = {
        let mut boot = std::pin::pin!(supervisor.boot());
        tokio::select! {
            landing = &mut boot => panic!("the clear never ran: {landing:?}"),
            entered = latched => entered.unwrap(),
        }
        let at = format!("provider=claude channel={}", rig.channel);
        assert!(rig.health().contains(&format!(
            "input_reconcile_required {at} reason=unbound keys={unbound}"
        )));
        assert_eq!(rig.world.notices().len(), 1);
        assert_eq!(
            rig.world.log(),
            ["subscribe", "freeze", "clear"],
            "no move or handback while the clear worker runs"
        );
        release.send(()).unwrap();
        boot.await
    };
    assert_eq!(
        landing,
        Landing::Held(HoldCause::TransitionHeld("move_held"))
    );
    assert_eq!(supervisor.handbacks, 0);
    assert!(supervisor.release());
}

#[tokio::test]
async fn unused_registry_leaves_health_to_the_fence() {
    if std::env::var("ADK_G1A_OFF_CHILD").as_deref() != Ok("1") {
        let name = format!(
            "{}::unused_registry_leaves_health_to_the_fence",
            module_path!()
        );
        let name = name.split_once("::").unwrap().1;
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([name, "--exact", "--nocapture", "--test-threads=1"])
            .env("ADK_G1A_OFF_CHILD", "1")
            .output()
            .unwrap();
        let result = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{result}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(result.contains("1 passed; 0 failed; 0 ignored"), "{result}");
        return;
    }
    let unused = sandbox();
    if mutant("off_register") {
        let registration = REGISTRY
            .register(&ProviderKind::Claude, 6_325_717, unused.path())
            .unwrap();
        drop(registration);
    }
    let before = super::super::health_reasons();
    assert!(before.is_empty());
    assert!(fence::lookup(&ProviderKind::Claude, 6_325_717).is_none());
    assert!(!fence::modes::order_barrier(
        &ProviderKind::Claude,
        6_325_717
    ));
    assert!(!unused.path().join("input_ledger").exists());
    assert!(
        !REGISTRY.used(),
        "no test or production path registers globally"
    );
    // A clear held elsewhere in this process still must not surface through an unused registry.
    let (dir, channel) = (sandbox(), 6_325_717);
    let world = Arc::new(World::default());
    let mut ledger = Ledger::open(dir.path(), channel).unwrap();
    append(&mut ledger, 11);
    world.durable_ticket(channel, &ledger, &[11]);
    world.reset(false, None);
    let mut host = ClearFake {
        channel,
        world,
        latch: None,
        _alive: None,
    };
    let guard = || Arc::new(tokio::sync::Mutex::new(())).lock_owned();
    let held = clear::resume(&mut ledger, &mut host, guard().await).await;
    assert_eq!(held, clear::Outcome::Held(Unresolved::ResetUnconfirmed));
    let tag = format!("channel={channel} ");
    assert!(clear::health_reasons().iter().any(|r| r.contains(&tag)));
    let settled = (0..50).find_map(|_| {
        let (before, combined) = (fence::health_reasons(), super::super::health_reasons());
        (before == fence::health_reasons()).then_some((before, combined))
    });
    let (fence_only, combined) = settled.expect("fence health never settled");
    assert_eq!(combined, fence_only);
    let cleared = clear::resume(&mut ledger, &mut host, guard().await).await;
    assert_eq!(cleared, clear::Outcome::Cleared);
}

#[path = "supervisor/drive_entry_tests.rs"]
mod drive_entry;

#[test]
fn registered_holds_join_fence_health_and_leave_on_release() {
    let registry = registry();
    let channel = 6_325_712;
    let dir = sandbox();
    let mut registration = registry
        .register(&ProviderKind::Claude, channel, dir.path())
        .unwrap();
    let slot = registration.slot().unwrap();
    registration.report(&HoldCause::LedgerUnreadable, true);
    let line = format!("ledger_unreadable provider=claude channel={channel}");
    assert!(super::super::reasons_with(registry).contains(&line));
    registration.report(&HoldCause::LedgerUnreadable, false);
    assert!(!super::super::reasons_with(registry).contains(&line));
    registration.report(&HoldCause::LedgerUnreadable, true);
    assert!(registration.release(slot));
    assert!(!super::super::reasons_with(registry).contains(&line));
}

fn append(ledger: &mut Ledger, key: u64) -> u64 {
    let entry = Entry::Received {
        key,
        input: json!({ "text": key }),
    };
    ledger.append_entry(&entry, &[]).unwrap()
}

// Bytes of the current WAL generation.
fn wal_bytes(root: &Path, channel: u64) -> Vec<u8> {
    let dir = root.join(format!("input_ledger/{channel}"));
    let generation = Ledger::open(root, channel)
        .unwrap()
        .snapshot()
        .map_or(0, |s| s.generation);
    std::fs::read(dir.join(format!("wal.{generation}.jsonl"))).unwrap()
}

// Whether this process still holds the channel's WAL open, judged from its descriptor table.
fn wal_held(root: &Path, channel: u64, generation: u64) -> bool {
    let wal = root.join(format!("input_ledger/{channel}/wal.{generation}.jsonl"));
    let wal = std::fs::canonicalize(wal).unwrap();
    let table = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let fds: Vec<i32> = (std::fs::read_dir(table).unwrap())
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    fds.into_iter()
        .any(|fd| fd_path(fd).as_deref() == Some(wal.as_path()))
}

#[cfg(target_os = "macos")]
fn fd_path(fd: i32) -> Option<PathBuf> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    if unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr()) } != 0 {
        return None;
    }
    let path = std::ffi::CStr::from_bytes_until_nul(&buf).ok()?;
    Some(PathBuf::from(path.to_str().ok()?))
}

#[cfg(target_os = "linux")]
fn fd_path(fd: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{fd}")).ok()
}

#[tokio::test]
async fn one_slot_handle_continues_through_move_append_checkpoint_and_reopen() {
    let (dir, channel) = (sandbox(), 6_325_713);
    let mut registration = registry()
        .register(&ProviderKind::Claude, channel, dir.path())
        .unwrap();
    let mut slot = registration.slot().unwrap();
    let world = Arc::new(World::default());
    let host = MoveFake {
        inputs: vec![8, 2],
        world,
        notices: vec![],
    };
    let outcome = loan(&mut slot, move |lease| {
        let mut host = host;
        Move::prepare(lease, &mut host)
            .unwrap()
            .advance(lease, &mut host)
    })
    .await;
    assert_eq!(outcome, Some(Outcome::Ledger));
    let fresh = |slot: &mut LedgerSlot| {
        let theirs = rows(dir.path(), channel);
        assert_eq!(
            slot.get().unwrap().rows().unwrap(),
            theirs,
            "one ledger, one view"
        );
        theirs.folded_seq()
    };
    let moved = fresh(&mut slot);
    let before = wal_bytes(dir.path(), channel);
    let transition = Entry::Transition {
        key: 8,
        state: RowState::Ready,
        attempt: None,
    };
    let seq = slot.get().unwrap().append_entry(&transition, &[]).unwrap();
    assert_eq!(seq, moved + 1);
    assert_eq!(append(slot.get().unwrap(), 9), moved + 2);
    let after = wal_bytes(dir.path(), channel);
    assert!(
        after.len() > before.len() && after.starts_with(&before),
        "appends only extend"
    );
    assert_eq!(fresh(&mut slot), moved + 2);
    slot.get().unwrap().checkpoint_rows().unwrap();
    assert_eq!(append(slot.get().unwrap(), 10), moved + 3);
    slot.reopen().unwrap();
    assert_eq!(append(slot.get().unwrap(), 12), moved + 4);
    assert_eq!(fresh(&mut slot), moved + 4);
    assert!(wal_held(dir.path(), channel, 1));
    let root = dir.path().to_owned();
    let (seen, observed) = mpsc::channel();
    let registry = registration.registry;
    *registry.on_release.lock().unwrap() = Some(Box::new(move || {
        seen.send(wal_held(&root, channel, 1)).unwrap();
    }));
    assert!(registration.release(slot));
    assert_eq!(
        observed.try_recv(),
        Ok(false),
        "the channel frees only after its handle closed"
    );
    let mut next = registry
        .register(&ProviderKind::Claude, channel, dir.path())
        .unwrap();
    let mut slot = next.slot().unwrap();
    assert_eq!(append(slot.get().unwrap(), 13), moved + 5);
    assert!(next.release(slot));
}

#[test]
fn failed_append_reopens_on_the_next_access_and_keeps_the_chain() {
    let (dir, channel) = (sandbox(), 6_325_714);
    let mut registration = registry()
        .register(&ProviderKind::Claude, channel, dir.path())
        .unwrap();
    let mut slot = registration.slot().unwrap();
    assert_eq!(append(slot.get().unwrap(), 1), 1);
    let recording = Recording::start(dir.path());
    recording.arm(Some(1));
    let entry = Entry::Received {
        key: 2,
        input: json!({ "text": 2 }),
    };
    assert!(slot.get().unwrap().append_entry(&entry, &[]).is_err());
    drop(recording);
    let reopened = opens();
    // The failed write reached the file before its sync failed, so a reopen adopts it.
    assert_eq!(append(slot.get().unwrap(), 3), 3);
    assert_eq!(
        opens() - reopened,
        1,
        "the unusable handle is replaced, not reused"
    );
    let records: Vec<u64> = (Ledger::open(dir.path(), channel).unwrap().records().iter())
        .map(|r| r.seq)
        .collect();
    assert_eq!(records, [1, 2, 3]);
    // A failed reopen leaves no handle at all, never the one it was replacing.
    let recording = Recording::start(dir.path());
    slot.reopen().unwrap();
    let replay = recording
        .events()
        .iter()
        .position(|e| e.starts_with("file_sync:"));
    recording.arm(replay.map(|index| index + 1));
    assert!(wal_held(dir.path(), channel, 0));
    assert!(slot.reopen().is_err());
    drop(recording);
    assert!(!wal_held(dir.path(), channel, 0));
    assert_eq!(append(slot.get().unwrap(), 4), 4);
    assert!(registration.release(slot));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoned_clear_loan_pins_the_slot_poisons_the_channel_and_keeps_the_worker_write() {
    let (dir, channel) = (sandbox(), 6_325_715);
    let first = registry();
    let mut registration = first
        .register(&ProviderKind::Claude, channel, dir.path())
        .unwrap();
    let mut slot = registration.slot().unwrap();
    let world = Arc::new(World::default());
    let seq = append(slot.get().unwrap(), 11);
    world.durable_ticket(channel, slot.get().unwrap(), &[11]);
    let (release_latch, latch) = mpsc::channel();
    let (alive, finished) = mpsc::channel();
    let host = ClearFake {
        channel,
        world: world.clone(),
        latch: Some(latch),
        _alive: Some(alive),
    };
    let guard = Arc::new(tokio::sync::Mutex::new(())).lock_owned().await;
    let abandoned = tokio::time::timeout(
        Duration::from_millis(200),
        clear_loan(&mut slot, host, guard),
    )
    .await;
    assert!(abandoned.is_err(), "the worker is still latched");
    let before = opens();
    assert!(matches!(slot.get(), Err(SlotError::Loaned)));
    assert!(matches!(slot.reopen(), Err(SlotError::Loaned)));
    assert!(matches!(slot.lend(), Err(SlotError::Loaned)));
    assert_eq!(opens(), before, "no access opens a second handle");
    assert!(!registration.release(slot), "a lent slot is never released");
    assert_eq!(
        first
            .register(&ProviderKind::Claude, channel, dir.path())
            .err(),
        Some(Refused::Poisoned)
    );
    assert!(first.health_reasons().contains(&format!(
        "turn_transition_held provider=claude channel={channel} reason=supervisor_lost"
    )));
    release_latch.send(()).unwrap();
    // The host drops only when the worker closure ends, after its last ledger write.
    assert_eq!(
        finished.recv_timeout(Duration::from_secs(30)),
        Err(mpsc::RecvTimeoutError::Disconnected)
    );
    let mut restarted = registry()
        .register(&ProviderKind::Claude, channel, dir.path())
        .unwrap();
    let mut slot = restarted.slot().unwrap();
    let ledger = slot.get().unwrap();
    assert_eq!(
        ledger.rows().unwrap().row(11).unwrap().state,
        RowState::Abandoned(AbandonReason::UserClear)
    );
    assert_eq!(ledger.rows().unwrap().folded_seq(), seq + 1);
    assert_eq!(append(slot.get().unwrap(), 12), seq + 2);
    assert!(restarted.release(slot));
}

#[tokio::test]
async fn finished_clear_loan_returns_the_handle_to_the_slot() {
    let (dir, channel) = (sandbox(), 6_325_716);
    let mut registration = registry()
        .register(&ProviderKind::Claude, channel, dir.path())
        .unwrap();
    let mut slot = registration.slot().unwrap();
    let world = Arc::new(World::default());
    let seq = append(slot.get().unwrap(), 11);
    world.durable_ticket(channel, slot.get().unwrap(), &[11]);
    let host = ClearFake {
        channel,
        world,
        latch: None,
        _alive: None,
    };
    let guard = Arc::new(tokio::sync::Mutex::new(())).lock_owned().await;
    let before = opens();
    assert_eq!(
        clear_loan(&mut slot, host, guard).await,
        Some(clear::Outcome::Cleared)
    );
    assert_eq!(append(slot.get().unwrap(), 12), seq + 2);
    assert_eq!(opens(), before, "the returned handle is reused");
    assert!(registration.release(slot));
}
