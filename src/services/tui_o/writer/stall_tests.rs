//! A deferred channel whose Legacy stays behind, driven through the real host: O starts at the
//! source's end once Legacy stood still for `STALLED`, and any Legacy movement restarts the wait.

use super::*;
use crate::services::tui_o::writer::adoption::LegacyEpoch;

const MINUTE: Duration = Duration::from_secs(60);

/// Legacy's relay with a cursor, frontier and redrive epoch the test moves.
struct Behind {
    path: PathBuf,
    cursor: Mutex<LegacyCursor>,
    frontier: Mutex<Option<u64>>,
    reconnects: AtomicU64,
}

impl Behind {
    fn at(&self, offset: u64) {
        let path = self.path.clone();
        *self.cursor.lock().unwrap() = LegacyCursor::Bound { path, offset };
    }
}

impl LegacyView for Behind {
    fn started(&self) -> bool {
        true
    }

    fn cursor(&self, _: &str) -> LegacyCursor {
        self.cursor.lock().unwrap().clone()
    }

    fn frontier(&self, _: u64, _: &str, _: u64) -> Option<u64> {
        *self.frontier.lock().unwrap()
    }

    fn tail_running(&self, _: &str) -> bool {
        false
    }

    fn epoch(&self, _: u64) -> LegacyEpoch {
        let reconnects = self.reconnects.load(Ordering::SeqCst);
        LegacyEpoch {
            reconnects,
            ..LegacyEpoch::default()
        }
    }
}

/// A selected channel whose transcript holds `body` and whose Legacy reads it from `cursor` with
/// its delivered frontier at `frontier`; the gateway reports `custody`.
struct Stalled {
    harness: Harness,
    path: PathBuf,
    io: Arc<TestIo>,
    legacy: Arc<Behind>,
    ready: Arc<Readiness>,
    selected: Option<ChannelsGuard>,
}

impl Stalled {
    fn new(body: &[u8], cursor: u64, frontier: Option<u64>, custody: Custody) -> Self {
        let (harness, path) = fresh(startup);
        append(&path, body);
        harness.gate.acquired();
        let selected = Some(test_override::force_candidates(&[(CHANNEL, ClaudeTui)]));
        let io = TestIo::over(&harness);
        let legacy = Arc::new(Behind {
            path: path.clone(),
            cursor: Mutex::new(LegacyCursor::Unbound),
            frontier: Mutex::new(frontier),
            reconnects: AtomicU64::default(),
        });
        legacy.at(cursor);
        *io.legacy.lock().unwrap() = Some(Arc::clone(&legacy) as Arc<dyn LegacyView>);
        *io.custody.lock().unwrap() = Ok(custody);
        let ready = Arc::new(Readiness::default());
        Self {
            harness,
            path,
            io,
            legacy,
            ready,
            selected,
        }
    }

    /// A closed turn Legacy never delivered, behind an inflight row; Legacy read all of it.
    fn dead_tail() -> Self {
        let body = [row("m0", "undelivered"), closed()].concat();
        Self::new(&body, body.len() as u64, Some(0), Custody::Row)
    }

    fn start(&self) -> Vec<tokio::task::JoinHandle<()>> {
        start_host(&self.harness, &self.io, &self.ready)
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
    }

    /// Committed at the source's end with `abandoned` reported once, and the next unit posted once.
    async fn assert_adopted_at_end(&self, abandoned: Option<u64>) {
        assert_eq!(
            adoption(CHANNEL),
            Adoption::Committed,
            "{:?}",
            self.alarms()
        );
        let init = self.harness.store.read_init(CHANNEL).unwrap().unwrap();
        let starts: Vec<_> = init.sources.iter().map(|s| s.delivery_start).collect();
        assert_eq!(starts, [self.len()], "O starts at the source's end");
        let source = init.sources[0].source_id.clone();
        let to = self.len();
        let reported = abandoned.map(|from| WriterAlarm::Abandoned { source, from, to });
        let reported: Vec<_> = reported.into_iter().map(|alarm| (CHANNEL, alarm)).collect();
        assert_eq!(self.alarms(), reported);
        append(&self.path, &row("m9", "next"));
        polls(3).await;
        assert_eq!(self.harness.port.posts(), ["next"]);
    }
}

#[tokio::test(start_paused = true)]
async fn a_cursor_behind_a_closed_tail_at_boot_waits_out_the_stall_and_starts_at_the_end() {
    let tail = [row("m0", "undelivered"), closed()].concat();
    let cursor = row("m0", "undelivered").len() as u64;
    let stalled = Stalled::new(&tail, cursor, Some(0), Custody::Row);
    let _tasks = stalled.start();
    tokio::time::sleep(39 * MINUTE).await;
    stalled.assert_waiting("Legacy may still catch up");
    tokio::time::sleep(2 * MINUTE).await;
    stalled.assert_adopted_at_end(Some(0)).await;

    // Legacy delivered everything but never read the closing record: nothing is reported.
    let cursor = row("m0", "delivered").len() as u64;
    let body = [row("m0", "delivered"), closed()].concat();
    let caught_up = Stalled::new(&body, cursor, Some(body.len() as u64), Custody::Free);
    let _tasks = caught_up.start();
    tokio::time::sleep(39 * MINUTE).await;
    caught_up.assert_waiting("the cursor still lags");
    tokio::time::sleep(2 * MINUTE).await;
    caught_up.assert_adopted_at_end(None).await;
}

#[tokio::test(start_paused = true)]
async fn a_cursor_that_cannot_be_waited_out_leaves_the_channel_to_legacy_at_boot() {
    let open = row("m0", "open");
    let closed_tail = [row("m0", "undelivered"), closed()].concat();
    let other = PathBuf::from("/elsewhere.jsonl");
    type Case = (&'static str, Vec<u8>, Option<u64>, Option<LegacyCursor>);
    let cases: [Case; 5] = [
        (
            "unbound",
            closed_tail.clone(),
            Some(0),
            Some(LegacyCursor::Unbound),
        ),
        (
            "another path",
            closed_tail.clone(),
            Some(0),
            Some(LegacyCursor::Bound {
                path: other,
                offset: 0,
            }),
        ),
        ("not authoritative", closed_tail.clone(), None, None),
        (
            "open, frontier inside a record",
            open.clone(),
            Some(5),
            None,
        ),
        (
            "open, frontier past the end",
            open.clone(),
            Some(1 << 20),
            None,
        ),
    ];
    async fn released(stalled: Stalled, why: &str) {
        let _tasks = stalled.start();
        polls(3).await;
        assert_eq!(adoption(CHANNEL), Adoption::Released, "{why}");
        assert_eq!(stalled.io.alarms.released().len(), 1, "{why}");
        assert!(!stalled.harness.store.has_channel_dir(CHANNEL), "{why}");
    }
    for (why, body, frontier, cursor) in cases {
        let stalled = Stalled::new(&body, 0, frontier, Custody::Row);
        if let Some(cursor) = cursor {
            *stalled.legacy.cursor.lock().unwrap() = cursor;
        }
        released(stalled, why).await;
    }
    let past = Stalled::new(&closed_tail, 1 << 20, Some(0), Custody::Row);
    released(past, "a cursor past the end").await;
    // An open turn with a frontier on a record waits for the turn instead.
    let stalled = Stalled::new(&open, 0, Some(0), Custody::Row);
    let _tasks = stalled.start();
    polls(3).await;
    stalled.assert_waiting("an open turn with a sound frontier");
}

#[tokio::test(start_paused = true)]
async fn an_open_turn_at_the_cursor_waits_only_on_a_sound_frontier() {
    let open = row("m0", "open");
    let end = open.len() as u64;
    for (why, frontier) in [("inside a record", 5), ("past the end", end + 1)] {
        let stalled = Stalled::new(&open, end, Some(frontier), Custody::Row);
        let _tasks = stalled.start();
        polls(3).await;
        assert_eq!(adoption(CHANNEL), Adoption::Released, "{why}");
        assert!(!stalled.harness.store.has_channel_dir(CHANNEL), "{why}");
    }
    let stalled = Stalled::new(&open, end, Some(0), Custody::Row);
    let _tasks = stalled.start();
    polls(3).await;
    stalled.assert_waiting("an open turn with a sound frontier");
}

#[tokio::test(start_paused = true)]
async fn an_inflight_row_alone_does_not_keep_legacy_busy_but_an_active_custody_does() {
    let open = Open::new();
    let _tasks = open.start().await;
    *open.io.custody.lock().unwrap() = Ok(Custody::Active);
    open.close();
    retried().await;
    retried().await;
    open.assert_waiting("a pending start or terminal custody");
    *open.io.custody.lock().unwrap() = Ok(Custody::Row);
    retried().await;
    open.assert_adopted().await;
}

#[tokio::test(start_paused = true)]
async fn a_turn_opened_and_closed_during_the_stall_restarts_it() {
    let stalled = Stalled::dead_tail();
    let _tasks = stalled.start();
    tokio::time::sleep(30 * MINUTE).await;
    append(&stalled.path, &row("m1", "again"));
    tokio::time::sleep(15 * MINUTE).await;
    append(&stalled.path, &closed());
    tokio::time::sleep(39 * MINUTE).await;
    stalled.assert_waiting("the stall restarted when the turn closed");
    tokio::time::sleep(3 * MINUTE).await;
    stalled.assert_adopted_at_end(Some(0)).await;
}

/// Legacy moves for one retry at 30 minutes while the source, cursor and frontier stand still:
/// the stall starts over from then and still ends in an adoption.
async fn stall_restarted_by(nudge: impl FnOnce(&Stalled), settle: impl FnOnce(&Stalled)) {
    let stalled = Stalled::dead_tail();
    let _tasks = stalled.start();
    tokio::time::sleep(30 * MINUTE).await;
    nudge(&stalled);
    tokio::time::sleep(Duration::from_secs(6)).await;
    settle(&stalled);
    tokio::time::sleep(39 * MINUTE).await;
    stalled.assert_waiting("the stall restarted");
    tokio::time::sleep(3 * MINUTE).await;
    stalled.assert_adopted_at_end(Some(0)).await;
}

#[tokio::test(start_paused = true)]
async fn an_active_custody_seen_once_restarts_the_stall() {
    stall_restarted_by(
        |s| *s.io.custody.lock().unwrap() = Ok(Custody::Active),
        |s| *s.io.custody.lock().unwrap() = Ok(Custody::Row),
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn an_emission_between_rereads_restarts_the_stall() {
    stall_restarted_by(
        |s| s.io.relaying.store(true, Ordering::SeqCst),
        |s| s.io.relaying.store(false, Ordering::SeqCst),
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_new_legacy_redrive_episode_restarts_the_stall() {
    stall_restarted_by(
        |s| {
            s.legacy.reconnects.fetch_add(1, Ordering::SeqCst);
        },
        |_| {},
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_cursor_that_moves_restarts_the_stall() {
    stall_restarted_by(|s| s.legacy.at(0), |s| s.legacy.at(s.len())).await;
}

#[tokio::test(start_paused = true)]
async fn legacy_intake_open_when_the_stall_ends_restarts_it() {
    let stalled = Stalled::dead_tail();
    let _tasks = stalled.start();
    let intake = |open| {
        stalled
            .io
            .facts
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .open_intake = open
    };
    tokio::time::sleep(39 * MINUTE).await;
    intake(1);
    tokio::time::sleep(2 * MINUTE).await;
    stalled.assert_waiting("an open intake holds the adoption");
    intake(0);
    tokio::time::sleep(39 * MINUTE).await;
    stalled.assert_waiting("the stall restarted");
    tokio::time::sleep(3 * MINUTE).await;
    stalled.assert_adopted_at_end(Some(0)).await;
}

#[tokio::test(start_paused = true)]
async fn a_restart_starts_the_stall_over() {
    let mut stalled = Stalled::dead_tail();
    let tasks = stalled.start();
    tokio::time::sleep(30 * MINUTE).await;
    abort(tasks);
    // A new process starts every adoption pending again.
    stalled.selected = None;
    stalled.selected = Some(test_override::force_candidates(&[(CHANNEL, ClaudeTui)]));
    stalled.ready = Arc::new(Readiness::default());
    let _tasks = stalled.start();
    tokio::time::sleep(39 * MINUTE).await;
    stalled.assert_waiting("the clock is this process's own");
    tokio::time::sleep(3 * MINUTE).await;
    stalled.assert_adopted_at_end(Some(0)).await;
}

#[tokio::test(start_paused = true)]
async fn a_custody_that_appears_after_its_first_read_releases_the_channel() {
    if !isolated(concat!(
        module_path!(),
        "::a_custody_that_appears_after_its_first_read_releases_the_channel"
    )) {
        return;
    }
    let body = [row("m0", "delivered"), closed()].concat();
    let len = body.len() as u64;
    let stalled = Stalled::new(&body, len, Some(len), Custody::Free);
    let io = Arc::clone(&stalled.io);
    let reached = hook_reached(Step::BeforeLock, move || {
        *io.custody.lock().unwrap() = Ok(Custody::Row);
    });
    let _tasks = stalled.start();
    polls(3).await;
    assert!(reached.load(Ordering::SeqCst));
    assert_eq!(adoption(CHANNEL), Adoption::Released);
    let released = stalled.io.alarms.released();
    assert!(
        matches!(released.as_slice(), [(CHANNEL, detail)] if detail.contains("custody")),
        "{released:?}"
    );
    assert!(!stalled.harness.store.has_channel_dir(CHANNEL));
}

#[tokio::test(start_paused = true)]
async fn a_stalled_adoption_refused_under_the_lock_reports_nothing_and_waits_again() {
    if !isolated(concat!(
        module_path!(),
        "::a_stalled_adoption_refused_under_the_lock_reports_nothing_and_waits_again"
    )) {
        return;
    }
    let stalled = Stalled::dead_tail();
    let io = Arc::clone(&stalled.io);
    let reached = hook_reached(Step::BeforeLock, move || {
        io.relaying.store(true, Ordering::SeqCst);
    });
    let _tasks = stalled.start();
    tokio::time::sleep(41 * MINUTE).await;
    assert!(
        reached.load(Ordering::SeqCst),
        "the stalled adoption reached the lock"
    );
    stalled.assert_waiting("an emission under the lock refuses it");
    stalled.io.relaying.store(false, Ordering::SeqCst);
    tokio::time::sleep(39 * MINUTE).await;
    stalled.assert_waiting("the stall started over");
    tokio::time::sleep(3 * MINUTE).await;
    stalled.assert_adopted_at_end(Some(0)).await;
}

/// Legacy moves right before the lock of the adoption its stall ended in: nothing commits, and a
/// new stall from the move ends in an adoption reporting from `abandoned`.
async fn moved_before_the_lock(stalled: Stalled, nudge: fn(&Behind), abandoned: u64) {
    let legacy = Arc::clone(&stalled.legacy);
    let reached = hook_reached(Step::BeforeLock, move || nudge(&legacy));
    let _tasks = stalled.start();
    tokio::time::sleep(41 * MINUTE).await;
    assert!(
        reached.load(Ordering::SeqCst),
        "the stall ended in an adoption"
    );
    stalled.assert_waiting("Legacy moved before the lock");
    tokio::time::sleep(38 * MINUTE).await;
    stalled.assert_waiting("a new stall started");
    tokio::time::sleep(3 * MINUTE).await;
    stalled.assert_adopted_at_end(Some(abandoned)).await;
}

#[tokio::test(start_paused = true)]
async fn a_redrive_episode_starting_right_before_the_lock_restarts_the_stall() {
    if !isolated(concat!(
        module_path!(),
        "::a_redrive_episode_starting_right_before_the_lock_restarts_the_stall"
    )) {
        return;
    }
    let nudge: fn(&Behind) = |legacy| {
        legacy.reconnects.fetch_add(1, Ordering::SeqCst);
    };
    moved_before_the_lock(Stalled::dead_tail(), nudge, 0).await;
}

#[tokio::test(start_paused = true)]
async fn a_cursor_moving_right_before_the_lock_restarts_the_stall() {
    if !isolated(concat!(
        module_path!(),
        "::a_cursor_moving_right_before_the_lock_restarts_the_stall"
    )) {
        return;
    }
    moved_before_the_lock(Stalled::dead_tail(), |legacy| legacy.at(0), 0).await;
}

#[tokio::test(start_paused = true)]
async fn a_delivery_right_before_the_lock_restarts_the_stall() {
    if !isolated(concat!(
        module_path!(),
        "::a_delivery_right_before_the_lock_restarts_the_stall"
    )) {
        return;
    }
    let first = row("m0", "delivered late");
    let body = [first.clone(), row("m1", "undelivered"), closed()].concat();
    let stalled = Stalled::new(&body, body.len() as u64, Some(0), Custody::Row);
    let nudge: fn(&Behind) = |legacy| {
        let delivered = row("m0", "delivered late").len() as u64;
        *legacy.frontier.lock().unwrap() = Some(delivered);
    };
    moved_before_the_lock(stalled, nudge, first.len() as u64).await;
}
