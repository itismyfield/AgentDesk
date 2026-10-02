//! A channel whose first adoption met an open Legacy turn, driven through the real host: Legacy
//! keeps it while the turn runs, and O adopts it without a restart only once Legacy is done.

use std::sync::atomic::AtomicU64;
use std::time::Duration;

use super::*;
use crate::services::tui_o::cutover::test_override::ChannelsGuard;
use crate::services::tui_o::cutover::{BodyClaim, BodySend, claim_then_send};
use crate::services::tui_o::writer::activation::test_hook::{self, Step};

/// Legacy's relay as it moves: cursor and delivered frontier on `path`, and how often a pin read them.
struct Moving {
    path: Mutex<PathBuf>,
    cursor: AtomicU64,
    frontier: AtomicU64,
    reads: AtomicUsize,
}

impl Moving {
    /// Legacy has read and delivered all of `path`.
    fn caught_up(&self, path: &Path) {
        let end = std::fs::metadata(path).unwrap().len();
        *self.path.lock().unwrap() = path.to_path_buf();
        self.cursor.store(end, Ordering::SeqCst);
        self.frontier.store(end, Ordering::SeqCst);
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl LegacyView for Moving {
    fn started(&self) -> bool {
        true
    }

    fn cursor(&self, _: &str) -> LegacyCursor {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let path = self.path.lock().unwrap().clone();
        let offset = self.cursor.load(Ordering::SeqCst);
        LegacyCursor::Bound { path, offset }
    }

    fn frontier(&self, _: u64, _: &str, _: u64) -> Option<u64> {
        Some(self.frontier.load(Ordering::SeqCst))
    }

    fn tail_running(&self, _: &str) -> bool {
        false
    }
}

fn closed() -> Vec<u8> {
    let record = serde_json::json!({"type":"system", "subtype":"turn_duration", "durationMs":5});
    format!("{record}\n").into_bytes()
}

/// Long enough for the host to see a change, wait out its quiet period and retry.
async fn retried() {
    tokio::time::sleep(Duration::from_secs(30)).await;
}

/// A selected channel whose transcript ends in an open turn Legacy has read and delivered.
struct Open {
    harness: Harness,
    path: PathBuf,
    io: Arc<TestIo>,
    legacy: Arc<Moving>,
    ready: Arc<Readiness>,
    _selected: ChannelsGuard,
}

impl Open {
    fn new() -> Self {
        let (harness, path) = fresh(startup);
        append(&path, &row("m0", "asked"));
        harness.gate.acquired();
        let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
        let io = TestIo::over(&harness);
        let legacy = Arc::new(Moving {
            path: Mutex::new(path.clone()),
            cursor: AtomicU64::default(),
            frontier: AtomicU64::default(),
            reads: AtomicUsize::default(),
        });
        legacy.caught_up(&path);
        *io.legacy.lock().unwrap() = Some(Arc::clone(&legacy) as Arc<dyn LegacyView>);
        let ready = Arc::new(Readiness::default());
        Self {
            harness,
            path,
            io,
            legacy,
            ready,
            _selected,
        }
    }

    /// Hosts the channel and lets its first attempt run.
    async fn start(&self) -> Vec<tokio::task::JoinHandle<()>> {
        let tasks = start_host(&self.harness, &self.io, &self.ready);
        polls(3).await;
        tasks
    }

    /// The TUI closes the turn and Legacy delivers it.
    fn close(&self) {
        append(&self.path, &closed());
        self.legacy.caught_up(&self.path);
    }

    fn facts(&self) -> usize {
        let calls = self.io.calls();
        calls
            .iter()
            .filter(|call| **call == ("facts", CHANNEL))
            .count()
    }

    fn start_of_o(&self) -> Option<u64> {
        let init = self.harness.store.read_init(CHANNEL).unwrap()?;
        init.sources.first().map(|source| source.delivery_start)
    }

    fn len(&self) -> u64 {
        std::fs::metadata(&self.path).unwrap().len()
    }

    fn alarms(&self) -> Vec<(u64, WriterAlarm)> {
        self.io.alarms.0.lock().unwrap().clone()
    }

    fn assert_waiting(&self, why: &str) {
        assert_eq!(adoption(CHANNEL), Adoption::Deferred, "{why}");
        assert!(!self.harness.store.has_channel_dir(CHANNEL), "{why}");
        assert_eq!(self.alarms(), [], "{why}");
        assert!(!self.ready.is_ready(CHANNEL), "{why}");
    }

    /// Committed at Legacy's cursor, with a ready actor that posts the next unit once.
    async fn assert_adopted(&self) {
        assert_eq!(adoption(CHANNEL), Adoption::Committed);
        assert_eq!(
            self.start_of_o(),
            Some(self.len()),
            "O starts at Legacy's cursor"
        );
        assert_eq!(self.alarms(), [], "a deferred adoption abandons nothing");
        append(&self.path, &row("m9", "next"));
        polls(3).await;
        assert!(self.ready.is_ready(CHANNEL));
        assert_eq!(self.harness.port.posts(), ["next"]);
    }
}

/// Hooks are process-wide and keyed by channel, so a test that sets one runs in its own process.
fn isolated(name: &str) -> bool {
    test_override::isolated_binding_case(name)
}

fn hook_reached(step: Step, then: impl FnOnce() + Send + 'static) -> Arc<AtomicBool> {
    let reached = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&reached);
    test_hook::set(CHANNEL, step, move || {
        then();
        seen.store(true, Ordering::SeqCst);
        Ok(())
    });
    reached
}

#[tokio::test(start_paused = true)]
async fn an_open_turn_defers_the_adoption_and_its_close_adopts_it_without_a_restart() {
    let open = Open::new();
    let _tasks = open.start().await;
    open.assert_waiting("an open turn defers the adoption");
    let body = Some(BodyClaim::new(CHANNEL, Some(ClaudeTui)));
    let sent = claim_then_send(body, || async { "legacy" }).await;
    assert_eq!(
        sent,
        Ok(BodySend::Sent("legacy")),
        "Legacy sends while it waits"
    );
    retried().await;
    open.assert_waiting("the turn is still open");

    open.close();
    retried().await;
    open.assert_adopted().await;
    assert_eq!(open.facts(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_unit_written_before_the_retry_takes_the_lock_waits_for_legacy_to_read_it() {
    if !isolated(concat!(
        module_path!(),
        "::a_unit_written_before_the_retry_takes_the_lock_waits_for_legacy_to_read_it"
    )) {
        return;
    }
    let open = Open::new();
    let _tasks = open.start().await;
    open.close();
    let path = open.path.clone();
    let raced = [row("m1", "raced"), closed()].concat();
    let reached = hook_reached(Step::BeforeLock, move || append(&path, &raced));
    retried().await;
    assert!(reached.load(Ordering::SeqCst), "the retry reached the lock");
    open.assert_waiting("the recheck saw the unit");
    retried().await;
    open.assert_waiting("Legacy's cursor is behind the unit");

    open.legacy.caught_up(&open.path);
    retried().await;
    open.assert_adopted().await;
}

#[tokio::test(start_paused = true)]
async fn a_unit_written_after_the_recheck_goes_to_o_while_a_legacy_body_waits_on_the_lock() {
    if !isolated(concat!(
        module_path!(),
        "::a_unit_written_after_the_recheck_goes_to_o_while_a_legacy_body_waits_on_the_lock"
    )) {
        return;
    }
    let open = Open::new();
    let _tasks = open.start().await;
    open.close();
    let boundary = open.len();
    let (path, share) = (open.path.clone(), test_override::shared_channels());
    let (sends, blocked) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicBool::new(false)),
    );
    let claimer = Arc::new(Mutex::new(None));
    let (counted, waited, slot) = (
        Arc::clone(&sends),
        Arc::clone(&blocked),
        Arc::clone(&claimer),
    );
    let reached = hook_reached(Step::BeforeWrite, move || {
        append(&path, &row("m1", "second"));
        let (at_claim, reached_claim) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _channels = share();
            let runtime = tokio::runtime::Builder::new_current_thread().build();
            at_claim.send(()).unwrap();
            let body = Some(BodyClaim::new(CHANNEL, Some(ClaudeTui)));
            let send = || async move { counted.fetch_add(1, Ordering::SeqCst) };
            runtime.unwrap().block_on(claim_then_send(body, send))
        });
        reached_claim.recv().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        waited.store(!thread.is_finished(), Ordering::SeqCst);
        *slot.lock().unwrap() = Some(thread);
    });
    retried().await;
    assert!(
        reached.load(Ordering::SeqCst),
        "the retry reached the write"
    );
    let claimed = claimer.lock().unwrap().take().unwrap().join().unwrap();
    assert_eq!(claimed, Ok(BodySend::OwnedByO), "the body went to O");
    assert!(
        blocked.load(Ordering::SeqCst),
        "the body waited on the adoption lock"
    );
    assert_eq!(sends.load(Ordering::SeqCst), 0, "Legacy sent nothing");
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    assert_eq!(
        open.start_of_o(),
        Some(boundary),
        "O starts at the rechecked end"
    );
    polls(3).await;
    assert_eq!(open.harness.port.posts(), ["second"]);
}

#[tokio::test(start_paused = true)]
async fn a_busy_legacy_keeps_the_adoption_deferred_and_one_read_serves_the_open_turn() {
    let open = Open::new();
    let _tasks = open.start().await;
    retried().await;
    assert_eq!(
        (open.legacy.reads(), open.facts()),
        (1, 1),
        "an unchanged open turn is not read again"
    );
    open.close();
    *open.io.custody.lock().unwrap() = Ok(Custody::Active);
    retried().await;
    open.assert_waiting("Legacy custody");
    *open.io.custody.lock().unwrap() = Ok(Custody::Free);
    open.io.busy.store(true, Ordering::SeqCst);
    retried().await;
    open.assert_waiting("Legacy's mailbox");
    open.io.busy.store(false, Ordering::SeqCst);
    open.io.relaying.store(true, Ordering::SeqCst);
    retried().await;
    open.assert_waiting("Legacy's emission");
    assert_eq!(
        (open.legacy.reads(), open.facts()),
        (1, 1),
        "nothing is read while Legacy is busy"
    );

    open.io.relaying.store(false, Ordering::SeqCst);
    retried().await;
    open.assert_adopted().await;
}

#[tokio::test(start_paused = true)]
async fn an_emission_that_starts_before_the_lock_is_seen_under_it() {
    if !isolated(concat!(
        module_path!(),
        "::an_emission_that_starts_before_the_lock_is_seen_under_it"
    )) {
        return;
    }
    let open = Open::new();
    let _tasks = open.start().await;
    open.close();
    let io = Arc::clone(&open.io);
    let reached = hook_reached(Step::BeforeLock, move || {
        io.relaying.store(true, Ordering::SeqCst);
    });
    retried().await;
    assert!(
        reached.load(Ordering::SeqCst),
        "the retry passed the idle check"
    );
    open.assert_waiting("the emission is seen under the lock");

    open.io.relaying.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(70)).await;
    open.assert_adopted().await;
}

#[tokio::test(start_paused = true)]
async fn a_final_reason_ends_the_adoption_even_behind_an_open_turn() {
    let cases: [(&str, fn(&mut ActivationFacts)); 2] = [
        ("node override to runner", |f| {
            f.node_override = Some("runner".into())
        }),
        ("sessions on another node", |f| f.runner_sessions = 1),
    ];
    for (why, edit) in cases {
        let open = Open::new();
        edit(open.io.facts.lock().unwrap().as_mut().unwrap());
        let _tasks = open.start().await;
        assert_eq!(adoption(CHANNEL), Adoption::Released, "{why}");
        let released = open.io.alarms.released();
        assert!(
            matches!(released.as_slice(), [(CHANNEL, d)] if d.contains(why)),
            "{why}: {released:?}"
        );
        assert_eq!(open.legacy.reads(), 0, "{why}: nothing was pinned");
        open.close();
        retried().await;
        assert_eq!((open.facts(), open.legacy.reads()), (1, 0), "{why}");
        assert_eq!(open.io.alarms.released().len(), 1, "{why}");
    }

    let open = Open::new();
    open.io.facts.lock().unwrap().as_mut().unwrap().open_intake = 1;
    let _tasks = open.start().await;
    open.assert_waiting("open intake may clear");
    open.io
        .facts
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .node_override = Some("runner".into());
    open.close();
    retried().await;
    assert_eq!(adoption(CHANNEL), Adoption::Released);
    let released = open.io.alarms.released();
    assert!(
        matches!(released.as_slice(), [(CHANNEL, d)] if d.contains("node override")),
        "{released:?}"
    );
    let facts = open.facts();
    retried().await;
    assert_eq!(open.facts(), facts, "a released channel is not retried");
    assert!(!open.harness.store.has_channel_dir(CHANNEL));
}

#[tokio::test(start_paused = true)]
async fn a_cursor_that_catches_up_without_a_write_lets_the_adoption_through() {
    let open = Open::new();
    let _tasks = open.start().await;
    append(&open.path, &closed());
    retried().await;
    open.assert_waiting("Legacy's cursor is behind the turn end");
    assert!(open.legacy.reads() > 1, "the closed turn was read");

    open.legacy.caught_up(&open.path);
    retried().await;
    open.assert_adopted().await;
}

#[tokio::test(start_paused = true)]
async fn a_unit_written_after_the_cursor_caught_up_is_read_again_before_adoption() {
    if !isolated(concat!(
        module_path!(),
        "::a_unit_written_after_the_cursor_caught_up_is_read_again_before_adoption"
    )) {
        return;
    }
    let open = Open::new();
    let _tasks = open.start().await;
    append(&open.path, &closed());
    retried().await;
    open.assert_waiting("Legacy's cursor is behind the turn end");
    let path = open.path.clone();
    let late = [row("m1", "late"), closed()].concat();
    let reached = hook_reached(Step::Snapshot, move || append(&path, &late));
    open.legacy.caught_up(&open.path);
    retried().await;
    assert!(
        reached.load(Ordering::SeqCst),
        "the caught-up cursor led to a read"
    );
    open.assert_waiting("the unit is past Legacy's cursor");

    open.legacy.caught_up(&open.path);
    retried().await;
    open.assert_adopted().await;
}

#[tokio::test(start_paused = true)]
async fn a_record_past_legacys_frontier_waits_until_the_frontier_covers_it() {
    let open = Open::new();
    let _tasks = open.start().await;
    let frontier = open.len();
    append(&open.path, &[row("m1", "owed"), closed()].concat());
    open.legacy.caught_up(&open.path);
    open.legacy.frontier.store(frontier, Ordering::SeqCst);
    retried().await;
    open.assert_waiting("Legacy may still send the record");
    retried().await;
    open.assert_waiting("the frontier has not moved");

    open.legacy.caught_up(&open.path);
    retried().await;
    open.assert_adopted().await;
}

#[tokio::test(start_paused = true)]
async fn legacy_taking_the_channel_during_the_first_read_leaves_nothing_to_retry() {
    if !isolated(concat!(
        module_path!(),
        "::legacy_taking_the_channel_during_the_first_read_leaves_nothing_to_retry"
    )) {
        return;
    }
    let open = Open::new();
    let candidate = test_override::with_channels(|boot| boot.unwrap().candidate(CHANNEL).cloned());
    let candidate = candidate.unwrap();
    let reached = hook_reached(Step::Snapshot, move || {
        assert!(!candidate.claim(CHANNEL));
    });
    let _tasks = open.start().await;
    assert!(reached.load(Ordering::SeqCst));
    assert_eq!(adoption(CHANNEL), Adoption::Released);
    let released = open.io.alarms.released();
    assert!(
        matches!(released.as_slice(), [(CHANNEL, d)] if d.contains("still open")),
        "{released:?}"
    );
    open.close();
    retried().await;
    assert_eq!((open.facts(), open.legacy.reads()), (1, 1), "no retry runs");
    assert_eq!(open.io.alarms.released().len(), 1);
    assert!(!open.harness.store.has_channel_dir(CHANNEL));
}

/// A binding log that binds each of `sources` in turn, from seq 1.
fn binds(sources: &[&SourceId]) -> Vec<u8> {
    let event = |(at, source): (usize, &&SourceId)| {
        let target = p5::BindingTarget::Source((*source).clone());
        let mut event: serde_json::Value =
            serde_json::from_slice(&p5_event(CHANNEL, "claude", target)).unwrap();
        event["seq"] = (at as u64 + 1).into();
        [serde_json::to_vec(&event).unwrap(), b"\n".to_vec()].concat()
    };
    sources.iter().enumerate().flat_map(event).collect()
}

#[tokio::test(start_paused = true)]
async fn a_source_bound_while_waiting_leaves_the_channel_to_legacy() {
    for resumed in [false, true] {
        let open = Open::new();
        let runtime = open.harness._runtime.path().to_path_buf();
        let other = runtime.join("b.jsonl");
        std::fs::write(&other, [row("m1", "another session"), closed()].concat()).unwrap();
        let (a, b) = (
            source_id_for("s1", &open.path).unwrap(),
            source_id_for("s2", &other).unwrap(),
        );
        // Resumed: b was bound before a when the adoption deferred, then bound again and left.
        if resumed {
            p5_log(&runtime, CHANNEL, &binds(&[&b, &a]));
        }
        let _tasks = open.start().await;
        open.assert_waiting("an open turn defers the adoption");
        append(&open.path, &closed());
        let (log, current) = match resumed {
            false => (binds(&[&a, &b]), &other),
            true => (binds(&[&b, &a, &b, &a]), &open.path),
        };
        p5_log(&runtime, CHANNEL, &log);
        open.legacy.caught_up(current);
        retried().await;
        assert_eq!(adoption(CHANNEL), Adoption::Released, "resumed {resumed}");
        let released = open.io.alarms.released();
        assert!(
            matches!(released.as_slice(), [(CHANNEL, d)] if d.contains("bound while the adoption waited")),
            "resumed {resumed}: {released:?}"
        );
        assert!(!open.harness.store.has_channel_dir(CHANNEL));
        let facts = open.facts();
        retried().await;
        assert_eq!(open.facts(), facts, "a released channel is not retried");
    }
}

#[tokio::test(start_paused = true)]
async fn a_binding_log_that_moves_under_the_lock_is_retried_once_it_moved() {
    if !isolated(concat!(
        module_path!(),
        "::a_binding_log_that_moves_under_the_lock_is_retried_once_it_moved"
    )) {
        return;
    }
    let open = Open::new();
    let _tasks = open.start().await;
    open.close();
    let (root, source) = (
        open.harness._runtime.path().to_path_buf(),
        source_id_for("s1", &open.path).unwrap(),
    );
    let refused_at = Arc::new(Mutex::new(None));
    let at = Arc::clone(&refused_at);
    let reached = hook_reached(Step::BeforeLock, move || {
        p5_log(&root, CHANNEL, &binds(&[&source, &source]));
        *at.lock().unwrap() = Some(tokio::time::Instant::now());
    });
    for _ in 0..120 {
        if adoption(CHANNEL) == Adoption::Committed {
            break;
        }
        polls(1).await;
    }
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    assert!(reached.load(Ordering::SeqCst));
    let waited = refused_at.lock().unwrap().unwrap().elapsed();
    assert!(
        waited <= Duration::from_secs(10),
        "retried once the log moved: {waited:?}"
    );
    open.assert_adopted().await;
}

#[path = "stall_tests.rs"]
mod stall;
