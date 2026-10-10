use super::*;
use crate::services::tui_o::channel_policy::Candidate;
use crate::services::tui_o::cutover::claims_judged;
use crate::services::tui_o::ownership::GatewayOwnership;
use crate::services::tui_o::writer::host::{FencedFacts, HostParts};
use tokio::time::Instant;
use tracing_subscriber::layer::SubscriberExt;

const SOFT: Duration = Duration::from_secs(40 * 60);
const HARD: Duration = Duration::from_secs(80 * 60);
const TICK: Duration = Duration::from_secs(5);

fn candidate() -> Candidate {
    test_override::with_channels(|boot| boot.unwrap().candidate(CHANNEL).unwrap().clone())
}

async fn drained(condition: impl Fn() -> bool) {
    for _ in 0..2000 {
        if condition() {
            return;
        }
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        condition(),
        "the actual host did not finish its retry cycle"
    );
}

/// A runnable task prevents paused time from advancing while a pin awaits its blocking worker.
struct Clock {
    awake: tokio::task::JoinHandle<()>,
    start: Arc<Mutex<Option<Instant>>>,
    booted: Arc<AtomicBool>,
}

impl Clock {
    fn new() -> Self {
        let start = Arc::new(Mutex::new(None));
        let stamped = Arc::clone(&start);
        test_hook::set(CHANNEL, Step::DeferredStarted, move || {
            *stamped.lock().unwrap() = Some(Instant::now());
            Ok(())
        });
        let booted = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&booted);
        test_hook::set(CHANNEL, Step::DeferredTick, move || {
            seen.store(true, Ordering::SeqCst);
            Ok(())
        });
        let awake = tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        Self {
            awake,
            start,
            booted,
        }
    }

    async fn started(&self) {
        drained(|| self.start.lock().unwrap().is_some() && self.booted.load(Ordering::SeqCst))
            .await;
        assert_eq!(adoption(CHANNEL), Adoption::Deferred);
    }

    async fn to(&self, target: Duration) {
        let start = self.start.lock().unwrap().unwrap();
        while start.elapsed() < target {
            let elapsed = start.elapsed();
            let remainder =
                Duration::from_nanos(u64::try_from(elapsed.as_nanos() % TICK.as_nanos()).unwrap());
            let to_tick = TICK - remainder;
            let step = to_tick.min(target - elapsed);
            let crosses_tick = step == to_tick;
            let waiting = adoption(CHANNEL) == Adoption::Deferred;
            let done = Arc::new(AtomicBool::new(false));
            if waiting && crosses_tick {
                let seen = Arc::clone(&done);
                test_hook::set(CHANNEL, Step::DeferredTick, move || {
                    seen.store(true, Ordering::SeqCst);
                    Ok(())
                });
            }
            tokio::time::advance(step).await;
            if waiting && crosses_tick {
                drained(|| done.load(Ordering::SeqCst) || adoption(CHANNEL) != Adoption::Deferred)
                    .await;
            } else {
                tokio::task::yield_now().await;
            }
        }
        assert_eq!(start.elapsed(), target, "the paused clock must not drift");
    }
}

impl Drop for Clock {
    fn drop(&mut self) {
        self.awake.abort();
        // A commit returns before the next Tick, leaving this clock's completion hook queued.
        // Consume it so the next fixture's bootstrap hook runs first.
        test_hook::run(CHANNEL, Step::DeferredTick).expect("clock completion hook cleanup");
    }
}

async fn finish(tasks: Vec<tokio::task::JoinHandle<()>>) {
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        let _ = task.await;
    }
}

fn fence_calls(io: &TestIo) -> usize {
    io.calls()
        .iter()
        .filter(|call| **call == ("fence", CHANNEL))
        .count()
}

#[derive(Clone, Default)]
struct HandoffLog(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for HandoffLog {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        #[derive(Default)]
        struct Fields {
            kind: Option<String>,
            message: String,
        }
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                match field.name() {
                    "kind" => self.kind = Some(format!("{value:?}")),
                    "message" => self.message = format!("{value:?}"),
                    _ => {}
                }
            }
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields
            .message
            .contains("Legacy delivered nothing through the stall")
            && let Some(kind) = fields.kind
        {
            self.0
                .lock()
                .unwrap()
                .push(kind.trim_matches('"').to_owned());
        }
    }
}

#[tokio::test(start_paused = true)]
async fn zombie_markers_expire_soft_at_exact_boundary() {
    if !isolated(concat!(
        module_path!(),
        "::zombie_markers_expire_soft_at_exact_boundary"
    )) {
        return;
    }
    let stalled = Stalled::dead_tail();
    stalled.legacy.tail.store(true, Ordering::SeqCst);
    *stalled.io.custody.lock().unwrap() = Ok(Custody::Active);
    stalled.io.busy.store(true, Ordering::SeqCst);
    stalled.io.relaying.store(true, Ordering::SeqCst);
    let clock = Clock::new();
    let tasks = stalled.start();
    clock.started().await;
    clock.to(SOFT - Duration::from_nanos(1)).await;
    stalled.assert_waiting("the soft clock has not expired");
    clock.to(SOFT).await;
    assert_eq!(
        adoption(CHANNEL),
        Adoption::Committed,
        "the exact soft boundary expires"
    );
    assert_eq!(candidate().sends(), (0, 0));
    assert_eq!(fence_calls(&stalled.io), 1);
    clock.to(SOFT + Duration::from_nanos(1)).await;
    assert_eq!(fence_calls(&stalled.io), 1, "the handoff commits once");
    drop(clock);
    stalled.assert_adopted_at_end(Some(0)).await;
    finish(tasks).await;
}

#[tokio::test(start_paused = true)]
async fn transcript_cursor_and_placement_do_not_restart_delivery_clocks() {
    if !isolated(concat!(
        module_path!(),
        "::transcript_cursor_and_placement_do_not_restart_delivery_clocks"
    )) {
        return;
    }
    let stalled = Stalled::dead_tail();
    let clock = Clock::new();
    let tasks = stalled.start();
    clock.started().await;
    for minute in [5, 10, 15, 20, 25, 30, 35] {
        clock.to(minute * MINUTE).await;
        append(
            &stalled.path,
            &[row(&format!("m{minute}"), "capture only"), closed()].concat(),
        );
        stalled.legacy.at(stalled.len());
        for _ in 0..5 {
            assert!(!candidate().claim(CHANNEL));
        }
        assert_eq!(candidate().sends(), (0, 0));
    }
    clock.to(SOFT - TICK).await;
    stalled.assert_waiting("capture and placement did not shorten the clock");
    clock.to(SOFT).await;
    assert_eq!(
        adoption(CHANNEL),
        Adoption::Committed,
        "capture is not delivery progress"
    );
    clock.to(SOFT + TICK).await;
    drop(clock);
    stalled.assert_adopted_at_end(Some(0)).await;
    finish(tasks).await;
}

#[tokio::test(start_paused = true)]
async fn frontier_progress_restarts_both_clocks_at_the_observed_tick() {
    if !isolated(concat!(
        module_path!(),
        "::frontier_progress_restarts_both_clocks_at_the_observed_tick"
    )) {
        return;
    }
    let first = row("m0", "delivered late");
    let body = [first.clone(), row("m1", "undelivered"), closed()].concat();
    let stalled = Stalled::new(&body, body.len() as u64, Some(0), Custody::Row);
    let clock = Clock::new();
    let tasks = stalled.start();
    clock.started().await;
    clock.to(30 * MINUTE - TICK).await;
    *stalled.legacy.frontier.lock().unwrap() = Some(first.len() as u64);
    clock.to(30 * MINUTE).await;
    clock.to(70 * MINUTE - TICK).await;
    stalled.assert_waiting("frontier movement restarted the clock");
    clock.to(70 * MINUTE).await;
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    clock.to(70 * MINUTE + TICK).await;
    drop(clock);
    stalled
        .assert_adopted_at_end(Some(first.len() as u64))
        .await;
    finish(tasks).await;
}

#[tokio::test(start_paused = true)]
async fn actual_completed_body_send_restarts_soft_clock_at_the_observed_tick() {
    if !isolated(concat!(
        module_path!(),
        "::actual_completed_body_send_restarts_soft_clock_at_the_observed_tick"
    )) {
        return;
    }
    let stalled = Stalled::dead_tail();
    let clock = Clock::new();
    let tasks = stalled.start();
    clock.started().await;
    clock.to(30 * MINUTE - TICK).await;
    let sent = claim_then_send(Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))), || async {
        "sent"
    })
    .await;
    assert_eq!(sent, Ok(BodySend::Sent("sent")));
    assert_eq!(candidate().sends(), (1, 1));
    assert!(!claims_judged(CHANNEL).is_empty());
    clock.to(30 * MINUTE).await;
    clock.to(70 * MINUTE - TICK).await;
    stalled.assert_waiting("actual body progress restarted soft expiry");
    clock.to(70 * MINUTE).await;
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    clock.to(70 * MINUTE + TICK).await;
    drop(clock);
    stalled.assert_adopted_at_end(Some(0)).await;
    finish(tasks).await;
}

#[tokio::test(start_paused = true)]
async fn epoch_churn_does_not_postpone_exact_hard_boundary() {
    if !isolated(concat!(
        module_path!(),
        "::epoch_churn_does_not_postpone_exact_hard_boundary"
    )) {
        return;
    }
    let stalled = Stalled::dead_tail();
    let clock = Clock::new();
    let tasks = stalled.start();
    clock.started().await;
    for minute in [20, 40, 60] {
        clock.to(minute * MINUTE - TICK).await;
        stalled.legacy.reconnects.fetch_add(1, Ordering::SeqCst);
        clock.to(minute * MINUTE).await;
        stalled.assert_waiting("epoch movement resets only the soft clock");
    }
    clock.to(HARD - Duration::from_nanos(1)).await;
    stalled.assert_waiting("the hard clock has not expired");
    clock.to(HARD).await;
    assert_eq!(
        adoption(CHANNEL),
        Adoption::Committed,
        "the exact hard boundary expires"
    );
    clock.to(HARD + Duration::from_nanos(1)).await;
    drop(clock);
    stalled.assert_adopted_at_end(Some(0)).await;
    finish(tasks).await;
}

#[tokio::test(start_paused = true)]
async fn relay_started_after_fence_blocks_hard_expiry_without_progress() {
    if !isolated(concat!(
        module_path!(),
        "::relay_started_after_fence_blocks_hard_expiry_without_progress"
    )) {
        return;
    }
    let logged = HandoffLog::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(logged.clone()))
        .unwrap();
    for step in [Step::AfterFence, Step::BeforeLock] {
        let stalled = Stalled::dead_tail();
        let clock = Clock::new();
        let tasks = stalled.start();
        clock.started().await;
        for minute in [20, 40, 60] {
            clock.to(minute * MINUTE - TICK).await;
            stalled.legacy.reconnects.fetch_add(1, Ordering::SeqCst);
            clock.to(minute * MINUTE).await;
            stalled.assert_waiting("only the soft clock follows the new epoch");
        }
        let legacy = Arc::clone(&stalled.legacy);
        let io = Arc::clone(&stalled.io);
        let reached = hook_reached(step, move || {
            assert!(!io.relaying.swap(true, Ordering::SeqCst));
            assert_eq!(legacy.reconnects.load(Ordering::SeqCst), 3);
            assert_eq!(candidate().sends(), (0, 0));
        });
        clock.to(HARD - Duration::from_nanos(1)).await;
        stalled.assert_waiting("the hard clock is one nanosecond short");
        assert!(!reached.load(Ordering::SeqCst));
        clock.to(HARD).await;
        assert!(reached.load(Ordering::SeqCst));
        assert!(fence_calls(&stalled.io) > 0);
        assert_eq!(stalled.legacy.reconnects.load(Ordering::SeqCst), 3);
        assert_eq!(candidate().sends(), (0, 0));
        stalled.assert_waiting("a late relay still blocks the exact hard boundary");
        clock.to(HARD + Duration::from_nanos(1)).await;
        stalled.assert_waiting("the rejected attempt published no init");
        stalled.io.relaying.store(false, Ordering::SeqCst);
        clock.to(HARD + TICK).await;
        assert_eq!(adoption(CHANNEL), Adoption::Committed);
        drop(clock);
        stalled.assert_adopted_at_end(Some(0)).await;
        finish(tasks).await;
    }
    assert_eq!(*logged.0.lock().unwrap(), ["hard", "hard"]);
}

#[tokio::test(start_paused = true)]
async fn redrive_schedule_keeps_hard_expiry_blocked_until_relay_slot_clears() {
    if !isolated(concat!(
        module_path!(),
        "::redrive_schedule_keeps_hard_expiry_blocked_until_relay_slot_clears"
    )) {
        return;
    }
    let body = row("m0", "open through the first redrive rearm");
    let stalled = Stalled::new(&body, body.len() as u64, Some(0), Custody::Row);
    let clock = Clock::new();
    let tasks = stalled.start();
    clock.started().await;
    for seconds in [0, 30, 90, 210, 450, 930, 4530] {
        clock.to(Duration::from_secs(seconds)).await;
        stalled.legacy.reconnects.fetch_add(1, Ordering::SeqCst);
    }
    append(&stalled.path, &closed());
    stalled.io.relaying.store(true, Ordering::SeqCst);
    clock.to(HARD - TICK).await;
    stalled.assert_waiting("recent redrive prevents soft expiry");
    clock.to(HARD).await;
    stalled.assert_waiting("hard expiry still protects the relay slot");
    clock.to(HARD + TICK).await;
    stalled.assert_waiting("an occupied relay slot remains protected");
    stalled.io.relaying.store(false, Ordering::SeqCst);
    clock.to(HARD + 2 * TICK).await;
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    drop(clock);
    stalled.assert_adopted_at_end(Some(0)).await;
    finish(tasks).await;
}

#[tokio::test(start_paused = true)]
async fn both_expired_clocks_choose_soft_after_a_long_open_turn() {
    if !isolated(concat!(
        module_path!(),
        "::both_expired_clocks_choose_soft_after_a_long_open_turn"
    )) {
        return;
    }
    let logged = HandoffLog::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(logged.clone()))
        .unwrap();
    let body = row("m0", "still open");
    let stalled = Stalled::new(&body, body.len() as u64, Some(0), Custody::Active);
    stalled.io.relaying.store(true, Ordering::SeqCst);
    let clock = Clock::new();
    let tasks = stalled.start();
    clock.started().await;
    clock.to(90 * MINUTE).await;
    stalled.assert_waiting("open turn blocks both expired clocks");
    append(&stalled.path, &closed());
    clock.to(90 * MINUTE + 4 * TICK).await;
    assert_eq!(
        adoption(CHANNEL),
        Adoption::Committed,
        "soft expiry relaxes the stale relay slot"
    );
    assert_eq!(
        *logged.0.lock().unwrap(),
        ["soft"],
        "the successful handoff records its chosen clock"
    );
    drop(clock);
    stalled.assert_adopted_at_end(Some(0)).await;
    finish(tasks).await;
}

#[tokio::test(start_paused = true)]
async fn expired_rotation_is_released_before_an_unchanged_open_turn_refusal() {
    if !isolated(concat!(
        module_path!(),
        "::expired_rotation_is_released_before_an_unchanged_open_turn_refusal"
    )) {
        return;
    }
    let open = row("m0", "source A stays open");
    let stalled = Stalled::new(&open, open.len() as u64, Some(0), Custody::Active);
    stalled.legacy.tail.store(true, Ordering::SeqCst);
    let original = std::fs::read(&stalled.path).unwrap();
    let version = crate::services::tui_o::writer::adoption::ReadVersion::of(&stalled.path)
        .expect("source A exists");
    let clock = Clock::new();
    let tasks = stalled.start();
    clock.started().await;
    let path = stalled.path.clone();
    let pinned_version = version.clone();
    let body = original.clone();
    let reached = hook_reached(Step::Snapshot, move || {
        assert_eq!(std::fs::read(&path).unwrap(), body);
        assert_eq!(
            crate::services::tui_o::writer::adoption::ReadVersion::of(&path),
            Some(pinned_version)
        );
    });
    clock.to(SOFT - Duration::from_nanos(1)).await;
    assert!(!reached.load(Ordering::SeqCst));
    clock.to(SOFT).await;
    assert!(
        reached.load(Ordering::SeqCst),
        "the actual expired End pin attempted unchanged open source A"
    );
    stalled.assert_waiting("the actual End pin cached source A's OpenTurn refusal");
    assert_eq!(fence_calls(&stalled.io), 0);
    assert_eq!(candidate().sends(), (0, 0));
    assert_eq!(stalled.legacy.reconnects.load(Ordering::SeqCst), 0);

    // Keep A's refusal unchanged while only the binding names another source.
    let runtime = stalled.harness._runtime.path().to_path_buf();
    let other = runtime.join("rotated-b.jsonl");
    std::fs::write(&other, [row("m1", "source B"), closed()].concat()).unwrap();
    let a = source_id_for("s1", &stalled.path).unwrap();
    let b = source_id_for("s2", &other).unwrap();
    p5_log(&runtime, CHANNEL, &binds(&[&a, &b]));
    assert_eq!(std::fs::read(&stalled.path).unwrap(), original);
    assert_eq!(
        crate::services::tui_o::writer::adoption::ReadVersion::of(&stalled.path),
        Some(version)
    );
    assert!(stalled.legacy.tail.load(Ordering::SeqCst));
    assert_eq!(*stalled.io.custody.lock().unwrap(), Ok(Custody::Active));
    assert_eq!(candidate().sends(), (0, 0));
    assert_eq!(*stalled.legacy.frontier.lock().unwrap(), Some(0));
    assert_eq!(stalled.legacy.reconnects.load(Ordering::SeqCst), 0);
    clock.to(SOFT + TICK).await;
    assert_eq!(
        adoption(CHANNEL),
        Adoption::Released,
        "the next expired tick handles rotation before stale OpenTurn cache or B quiet"
    );
    assert!(!stalled.harness.store.has_channel_dir(CHANNEL));
    assert!(stalled.harness.store.read_init(CHANNEL).unwrap().is_none());
    assert!(!stalled.ready.is_ready(CHANNEL));
    assert_eq!(fence_calls(&stalled.io), 0);
    assert_eq!(stalled.harness.port.posts(), Vec::<String>::new());
    let released = stalled.io.alarms.released();
    assert!(
        matches!(released.as_slice(), [(CHANNEL, detail)]
            if detail.contains("bound while the adoption waited")
                && detail.contains(other.to_string_lossy().as_ref())),
        "exactly one rotation release must name B: {released:?}"
    );
    assert_eq!(stalled.alarms().len(), 1, "no unrelated alarm was reported");
    clock.to(SOFT + 3 * TICK).await;
    assert_eq!(adoption(CHANNEL), Adoption::Released);
    assert_eq!(stalled.io.alarms.released(), released);
    assert_eq!(fence_calls(&stalled.io), 0);
    assert_eq!(stalled.harness.port.posts(), Vec::<String>::new());
    drop(clock);
    finish(tasks).await;
}

#[tokio::test(start_paused = true)]
async fn expired_intake_queue_and_fence_errors_block_without_restarting_the_clocks() {
    if !isolated(concat!(
        module_path!(),
        "::expired_intake_queue_and_fence_errors_block_without_restarting_the_clocks"
    )) {
        return;
    }
    for blocker in ["intake", "queue", "fence"] {
        let stalled = Stalled::dead_tail();
        match blocker {
            "intake" => {
                stalled
                    .io
                    .facts
                    .lock()
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .open_intake = 1
            }
            "queue" => stalled.io.queued_bodies.store(1, Ordering::SeqCst),
            "fence" => *stalled.io.fence_error.lock().unwrap() = Some("fence unavailable".into()),
            _ => unreachable!(),
        }
        let clock = Clock::new();
        let tasks = stalled.start();
        clock.started().await;
        clock.to(SOFT + TICK).await;
        stalled.assert_waiting("fresh fence blockers hold an expired clock");
        assert!(fence_calls(&stalled.io) > 0);
        stalled
            .io
            .facts
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .open_intake = 0;
        stalled.io.queued_bodies.store(0, Ordering::SeqCst);
        *stalled.io.fence_error.lock().unwrap() = None;
        clock.to(SOFT + 4 * TICK).await;
        assert_eq!(
            adoption(CHANNEL),
            Adoption::Committed,
            "clearing {blocker} does not add another40m"
        );
        drop(clock);
        stalled.assert_adopted_at_end(Some(0)).await;
        finish(tasks).await;
    }
}

#[tokio::test(start_paused = true)]
async fn source_appended_when_expiry_pin_starts_must_be_quiet_before_handoff() {
    if !isolated(concat!(
        module_path!(),
        "::source_appended_when_expiry_pin_starts_must_be_quiet_before_handoff"
    )) {
        return;
    }
    let stalled = Stalled::dead_tail();
    stalled.legacy.tail.store(true, Ordering::SeqCst);
    *stalled.io.custody.lock().unwrap() = Ok(Custody::Active);
    stalled.io.busy.store(true, Ordering::SeqCst);
    stalled.io.relaying.store(true, Ordering::SeqCst);
    let clock = Clock::new();
    let tasks = stalled.start();
    clock.started().await;
    let appended = [row("m1", "closed as the expiry pin starts"), closed()].concat();
    let new_end = stalled.len() + u64::try_from(appended.len()).unwrap();
    let path = stalled.path.clone();
    let reached = hook_reached(Step::Snapshot, move || append(&path, &appended));
    clock.to(SOFT - Duration::from_nanos(1)).await;
    assert!(!reached.load(Ordering::SeqCst));
    stalled.assert_waiting("zombie markers prevented an earlier normal pin");
    clock.to(SOFT).await;
    assert!(reached.load(Ordering::SeqCst));
    assert_eq!(stalled.len(), new_end);
    assert_eq!(candidate().sends(), (0, 0));
    stalled.assert_waiting("the End pin captured a source that just changed");
    clock.to(SOFT + TICK).await;
    stalled.assert_waiting("five seconds is shorter than the quiet period");
    clock.to(SOFT + 2 * TICK).await;
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    let init = stalled.harness.store.read_init(CHANNEL).unwrap().unwrap();
    assert_eq!(init.sources[0].delivery_start, new_end);
    drop(clock);
    stalled.assert_adopted_at_end(Some(0)).await;
    finish(tasks).await;
}

#[tokio::test(start_paused = true)]
async fn appended_source_at_expiry_lock_refuses_once_without_restarting_the_clock() {
    if !isolated(concat!(
        module_path!(),
        "::appended_source_at_expiry_lock_refuses_once_without_restarting_the_clock"
    )) {
        return;
    }
    let stalled = Stalled::dead_tail();
    let clock = Clock::new();
    let tasks = stalled.start();
    clock.started().await;
    let path = stalled.path.clone();
    let reached = hook_reached(Step::BeforeLock, move || {
        append(&path, &[row("m1", "raced"), closed()].concat())
    });
    clock.to(SOFT).await;
    assert!(reached.load(Ordering::SeqCst));
    stalled.assert_waiting("the pinned source changed before its locked recheck");
    clock.to(SOFT + 4 * TICK).await;
    assert_eq!(
        adoption(CHANNEL),
        Adoption::Committed,
        "source stats do not restart delivery clocks"
    );
    drop(clock);
    stalled.assert_adopted_at_end(Some(0)).await;
    finish(tasks).await;
}

#[tokio::test(start_paused = true)]
async fn frontier_at_expiry_lock_refuses_and_restarts_delivery_clocks() {
    if !isolated(concat!(
        module_path!(),
        "::frontier_at_expiry_lock_refuses_and_restarts_delivery_clocks"
    )) {
        return;
    }
    let first = row("m0", "delivered late");
    let body = [first.clone(), row("m1", "undelivered"), closed()].concat();
    let stalled = Stalled::new(&body, body.len() as u64, Some(0), Custody::Row);
    let clock = Clock::new();
    let tasks = stalled.start();
    clock.started().await;
    let legacy = Arc::clone(&stalled.legacy);
    let delivered = first.len() as u64;
    let reached = hook_reached(Step::BeforeLock, move || {
        *legacy.frontier.lock().unwrap() = Some(delivered)
    });
    clock.to(SOFT).await;
    assert!(reached.load(Ordering::SeqCst));
    stalled.assert_waiting("a fresh frontier change invalidates the handoff token");
    clock.to(HARD - TICK).await;
    stalled.assert_waiting("locked frontier progress restarted both clocks");
    clock.to(HARD).await;
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    drop(clock);
    stalled.assert_adopted_at_end(Some(delivered)).await;
    finish(tasks).await;
}

#[tokio::test(start_paused = true)]
async fn expired_adoption_legacy_attempts_are_judged_owned_and_o_posts_once() {
    if !isolated(concat!(
        module_path!(),
        "::expired_adoption_legacy_attempts_are_judged_owned_and_o_posts_once"
    )) {
        return;
    }
    let stalled = Stalled::dead_tail();
    let clock = Clock::new();
    let tasks = stalled.start();
    clock.started().await;
    clock.to(SOFT).await;
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    let sends = AtomicUsize::default();
    let sent = claim_then_send(Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))), || async {
        sends.fetch_add(1, Ordering::SeqCst)
    })
    .await;
    assert_eq!(sent, Ok(BodySend::OwnedByO));
    let judged = claims_judged(CHANNEL);
    assert!(
        !judged.is_empty(),
        "the zero-send claim evidence is nonempty"
    );
    assert!(judged.iter().all(|owned| *owned));
    assert_eq!(sends.load(Ordering::SeqCst), 0);
    drop(clock);
    stalled.assert_adopted_at_end(Some(0)).await;
    finish(tasks).await;
}

#[tokio::test(start_paused = true)]
async fn a_real_body_transport_survives_soft_expiry_until_it_finishes() {
    if !isolated(concat!(
        module_path!(),
        "::a_real_body_transport_survives_soft_expiry_until_it_finishes"
    )) {
        return;
    }
    let stalled = Stalled::dead_tail();
    let clock = Clock::new();
    let tasks = stalled.start();
    clock.started().await;
    clock.to(39 * MINUTE).await;
    let (started, at_transport) = tokio::sync::oneshot::channel();
    let (resume, resumed) = tokio::sync::oneshot::channel();
    let transport = tokio::spawn(async move {
        claim_then_send(
            Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))),
            || async move {
                started.send(()).unwrap();
                resumed.await.unwrap();
                "legacy"
            },
        )
        .await
    });
    at_transport.await.unwrap();
    assert_eq!(candidate().sends(), (1, 0));
    clock.to(41 * MINUTE).await;
    stalled.assert_waiting("the body reservation survives the soft deadline");
    resume.send(()).unwrap();
    assert_eq!(transport.await.unwrap(), Ok(BodySend::Sent("legacy")));
    assert_eq!(candidate().sends(), (1, 1));
    clock.to(45 * MINUTE).await;
    stalled.assert_waiting("body completion restarts the soft clock");
    clock.to(HARD - TICK).await;
    stalled.assert_waiting("the frontier hard deadline has not expired");
    clock.to(HARD).await;
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    drop(clock);
    stalled.assert_adopted_at_end(Some(0)).await;
    finish(tasks).await;
}

#[tokio::test(start_paused = true)]
async fn binding_and_epoch_changes_under_the_expiry_lock_refuse_the_old_snapshot() {
    if !isolated(concat!(
        module_path!(),
        "::binding_and_epoch_changes_under_the_expiry_lock_refuse_the_old_snapshot"
    )) {
        return;
    }
    for change in ["binding", "epoch"] {
        let stalled = Stalled::dead_tail();
        let clock = Clock::new();
        let tasks = stalled.start();
        clock.started().await;
        let legacy = Arc::clone(&stalled.legacy);
        let runtime = stalled.harness._runtime.path().to_path_buf();
        let source = source_id_for("s1", &stalled.path).unwrap();
        let reached = hook_reached(Step::BeforeLock, move || {
            if change == "binding" {
                p5_log(&runtime, CHANNEL, &binds(&[&source, &source]));
            } else {
                legacy.reconnects.fetch_add(1, Ordering::SeqCst);
            }
        });
        clock.to(SOFT).await;
        assert!(reached.load(Ordering::SeqCst));
        stalled.assert_waiting("the old binding or progress token cannot publish init");
        if change == "binding" {
            clock.to(SOFT + 4 * TICK).await;
        } else {
            clock.to(HARD - TICK).await;
            stalled.assert_waiting("epoch movement restarted the soft clock");
            clock.to(HARD).await;
        }
        assert_eq!(adoption(CHANNEL), Adoption::Committed);
        drop(clock);
        stalled.assert_adopted_at_end(Some(0)).await;
        finish(tasks).await;
    }
}

#[tokio::test(start_paused = true)]
async fn normal_deferred_commit_rejects_a_real_send_started_before_its_fence() {
    if !isolated(concat!(
        module_path!(),
        "::normal_deferred_commit_rejects_a_real_send_started_before_its_fence"
    )) {
        return;
    }
    let open = Open::new();
    let clock = Clock::new();
    let tasks = start_host(&open.harness, &open.io, &open.ready);
    clock.started().await;
    let sends = Arc::new(AtomicUsize::default());
    let sent = Arc::clone(&sends);
    let (entered, at_transport) = tokio::sync::oneshot::channel();
    let (resume, resumed) = tokio::sync::oneshot::channel();
    let transport = tokio::spawn(async move {
        claim_then_send(
            Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))),
            || async move {
                sent.fetch_add(1, Ordering::SeqCst);
                entered.send(()).unwrap();
                resumed.await.unwrap();
                "legacy"
            },
        )
        .await
    });
    at_transport.await.unwrap();
    assert_eq!(candidate().sends(), (1, 0));
    let pinned_candidate = candidate();
    let fenced_candidate = pinned_candidate.clone();
    let pinned = hook_reached(Step::Snapshot, move || {
        assert_eq!(pinned_candidate.sends(), (1, 0));
    });
    let fencing = hook_reached(Step::BeforeFence, move || {
        assert_eq!(fenced_candidate.sends(), (1, 0));
    });
    open.close();
    clock.to(6 * TICK).await;
    assert!(pinned.load(Ordering::SeqCst), "the normal Cursor pin ran");
    assert!(
        fencing.load(Ordering::SeqCst),
        "the normal commit was attempted"
    );
    assert_eq!(candidate().sends(), (1, 0));
    assert_eq!(sends.load(Ordering::SeqCst), 1);
    open.assert_waiting("the Legacy transport started before the fence sample");
    resume.send(()).unwrap();
    assert_eq!(transport.await.unwrap(), Ok(BodySend::Sent("legacy")));
    assert_eq!(candidate().sends(), (1, 1));
    clock.to(Duration::from_secs(60) + 8 * TICK).await;
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    assert_eq!(sends.load(Ordering::SeqCst), 1);
    drop(clock);
    open.assert_adopted().await;
    finish(tasks).await;
}

#[tokio::test(start_paused = true)]
async fn normal_deferred_commit_rejects_a_real_send_started_before_its_lock() {
    if !isolated(concat!(
        module_path!(),
        "::normal_deferred_commit_rejects_a_real_send_started_before_its_lock"
    )) {
        return;
    }
    let open = Open::new();
    let clock = Clock::new();
    let tasks = start_host(&open.harness, &open.io, &open.ready);
    clock.started().await;
    let share = test_override::shared_channels();
    let (resume, completed) = tokio::sync::oneshot::channel::<()>();
    let transport = Arc::new(Mutex::new(None));
    let save = Arc::clone(&transport);
    let reached = hook_reached(Step::BeforeLock, move || {
        let (entered, at_transport) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _channels = share();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(claim_then_send(
                Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))),
                || async move {
                    entered.send(()).unwrap();
                    completed.await.unwrap();
                    "legacy"
                },
            ))
        });
        at_transport.recv_timeout(Duration::from_secs(2)).unwrap();
        *save.lock().unwrap() = Some(worker);
    });
    open.close();
    clock.to(6 * TICK).await;
    assert!(reached.load(Ordering::SeqCst));
    assert_eq!(candidate().sends(), (1, 0));
    open.assert_waiting("common activation rejects an actual transport in flight");
    assert!(
        fence_calls(&open.io) > 0,
        "normal Deferred activation used its fence"
    );
    resume.send(()).unwrap();
    assert_eq!(
        transport.lock().unwrap().take().unwrap().join().unwrap(),
        Ok(BodySend::Sent("legacy"))
    );
    assert_eq!(candidate().sends(), (1, 1));
    clock.to(20 * TICK).await;
    assert_eq!(
        adoption(CHANNEL),
        Adoption::Committed,
        "normal caught-up adoption need not wait another40m"
    );
    drop(clock);
    open.assert_adopted().await;
    finish(tasks).await;
}

async fn ownership_stays_locked_through_init(expired: bool) {
    let stalled = Stalled::dead_tail();
    let clock = Clock::new();
    let tasks = stalled.start();
    clock.started().await;
    if !expired {
        *stalled.legacy.frontier.lock().unwrap() = Some(stalled.len());
        *stalled.io.custody.lock().unwrap() = Ok(Custody::Free);
    }
    let observer = Arc::clone(&stalled.harness.gate);
    let changing = Arc::clone(&stalled.harness.gate);
    let protected = Arc::new(AtomicBool::new(false));
    let checked = Arc::clone(&protected);
    let thread = Arc::new(Mutex::new(None));
    let save = Arc::clone(&thread);
    let reached = hook_reached(Step::BeforeWrite, move || {
        let locked = std::thread::spawn(move || observer.locked_for_test())
            .join()
            .unwrap();
        checked.store(locked, Ordering::SeqCst);
        *save.lock().unwrap() = Some(std::thread::spawn(move || changing.lost()));
    });
    clock.to(if expired { SOFT } else { 6 * TICK }).await;
    assert!(
        reached.load(Ordering::SeqCst),
        "the actual Deferred activation reached BeforeWrite"
    );
    assert!(
        fence_calls(&stalled.io) > 0,
        "the actual Deferred activation used its intake fence"
    );
    assert!(
        protected.load(Ordering::SeqCst),
        "Owned must stay locked through the init write"
    );
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    assert!(stalled.harness.store.read_init(CHANNEL).unwrap().is_some());
    thread.lock().unwrap().take().unwrap().join().unwrap();
    assert_eq!(stalled.harness.gate.current(), GatewayOwnership::Lost);
    drop(clock);
    finish(tasks).await;
}

#[tokio::test(start_paused = true)]
async fn normal_deferred_commit_holds_ownership_until_init_publication() {
    if !isolated(concat!(
        module_path!(),
        "::normal_deferred_commit_holds_ownership_until_init_publication"
    )) {
        return;
    }
    ownership_stays_locked_through_init(false).await;
}

#[tokio::test(start_paused = true)]
async fn expired_handoff_holds_ownership_until_init_publication() {
    if !isolated(concat!(
        module_path!(),
        "::expired_handoff_holds_ownership_until_init_publication"
    )) {
        return;
    }
    ownership_stays_locked_through_init(true).await;
}

#[tokio::test(start_paused = true)]
async fn renewed_owned_epoch_between_fence_and_admit_refuses_both_commit_paths() {
    if !isolated(concat!(
        module_path!(),
        "::renewed_owned_epoch_between_fence_and_admit_refuses_both_commit_paths"
    )) {
        return;
    }
    for expired in [false, true] {
        let stalled = Stalled::dead_tail();
        let clock = Clock::new();
        let tasks = stalled.start();
        clock.started().await;
        if !expired {
            *stalled.legacy.frontier.lock().unwrap() = Some(stalled.len());
            *stalled.io.custody.lock().unwrap() = Ok(Custody::Free);
        }
        let gate = Arc::clone(&stalled.harness.gate);
        let reached = hook_reached(Step::AfterFence, move || {
            gate.lost();
            gate.acquired();
        });
        let boundary = if expired { SOFT } else { 3 * TICK };
        clock.to(boundary).await;
        assert!(reached.load(Ordering::SeqCst));
        stalled.assert_waiting("a newly Owned epoch is different from the pre-fence epoch");
        clock.to(boundary + 15 * TICK).await;
        assert_eq!(adoption(CHANNEL), Adoption::Committed);
        drop(clock);
        finish(tasks).await;
    }
}

struct DefaultFenceIo(Arc<TestIo>);

impl HostIo for DefaultFenceIo {
    type Port = <TestIo as HostIo>::Port;
    type Lease = <TestIo as HostIo>::Lease;
    type Alarms = <TestIo as HostIo>::Alarms;
    type Bindings = <TestIo as HostIo>::Bindings;

    fn port(&self) -> impl Future<Output = Arc<Self::Port>> + Send {
        self.0.port()
    }

    fn lease(&self) -> Self::Lease {
        self.0.lease()
    }

    fn alarms(&self) -> Self::Alarms {
        self.0.alarms()
    }

    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<Self::Bindings> {
        self.0.bindings(channel, provider)
    }

    fn activation_facts(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl Future<Output = Result<ActivationFacts, String>> + Send {
        self.0.activation_facts(channel, provider)
    }

    fn local_custody(&self, channel: u64, provider: ShadowProvider) -> Result<Custody, String> {
        self.0.local_custody(channel, provider)
    }

    fn legacy(&self) -> Arc<dyn LegacyView> {
        self.0.legacy()
    }

    fn legacy_busy(&self, channel: u64) -> impl Future<Output = bool> + Send {
        self.0.legacy_busy(channel)
    }

    fn relaying(&self, channel: u64) -> bool {
        self.0.relaying(channel)
    }

    fn adopted(&self, channel: u64, provider: ShadowProvider) {
        self.0.adopted(channel, provider);
    }
}

#[tokio::test(start_paused = true)]
async fn default_intake_fence_prevents_actual_deferred_activation() {
    if !isolated(concat!(
        module_path!(),
        "::default_intake_fence_prevents_actual_deferred_activation"
    )) {
        return;
    }
    for expired in [false, true] {
        let stalled = Stalled::dead_tail();
        let io = Arc::new(DefaultFenceIo(Arc::clone(&stalled.io)));
        p5::set_test_root(Some(stalled.harness._runtime.path()));
        let clock = Clock::new();
        let tasks = start(ShadowProvider::Claude, true, || HostParts {
            io: Arc::clone(&io),
            runtime_root: Some(root(&stalled.harness)),
            gate: Arc::clone(&stalled.harness.gate),
            readiness: Arc::clone(&stalled.ready),
        });
        clock.started().await;
        if !expired {
            *stalled.legacy.frontier.lock().unwrap() = Some(stalled.len());
            *stalled.io.custody.lock().unwrap() = Ok(Custody::Free);
        }
        let attempted = hook_reached(Step::BeforeFence, || {});
        clock
            .to(if expired { 120 * MINUTE } else { 6 * TICK })
            .await;
        assert!(attempted.load(Ordering::SeqCst), "the host tried its fence");
        assert_eq!(adoption(CHANNEL), Adoption::Deferred);
        assert!(!stalled.harness.store.has_channel_dir(CHANNEL));
        assert!(stalled.harness.port.posts().is_empty());
        let fenced: Result<FencedFacts, String> =
            io.intake_fence(CHANNEL, ShadowProvider::Claude).await;
        assert!(matches!(fenced, Err(detail) if detail == "this host has no intake fence"));
        drop(clock);
        finish(tasks).await;
    }
}

#[tokio::test(start_paused = true)]
async fn unselected_pending_and_recovered_channels_preserve_their_paths() {
    if !isolated(concat!(
        module_path!(),
        "::unselected_pending_and_recovered_channels_preserve_their_paths"
    )) {
        return;
    }
    let (harness, path) = fresh(startup);
    append(&path, &[row("m0", "legacy"), closed()].concat());
    harness.gate.acquired();
    let selected = test_override::force_candidates(&[]);
    let io = TestIo::over(&harness);
    let ready = Arc::new(Readiness::default());
    let tasks = start_host(&harness, &io, &ready);
    assert!(tasks.is_empty());
    tokio::time::advance(90 * MINUTE).await;
    let sent = claim_then_send(Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))), || async {
        "legacy"
    })
    .await;
    assert_eq!(sent, Ok(BodySend::Sent("legacy")));
    assert!(io.calls().is_empty());
    assert!(!harness.store.has_channel_dir(CHANNEL));
    drop(selected);

    let (harness, _) = fresh(startup);
    harness.gate.acquired();
    let selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let io = TestIo::over(&harness);
    let ready = Arc::new(Readiness::default());
    let tasks = start_host(&harness, &io, &ready);
    polls(3).await;
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    assert_eq!(
        fence_calls(&io),
        0,
        "Pending first activation keeps its path"
    );
    finish(tasks).await;
    drop(selected);

    let (harness, path, source) = switched_over(&row("m0", "before switch"));
    p5_log(
        harness._runtime.path(),
        CHANNEL,
        &p5_event(CHANNEL, "claude", p5::BindingTarget::Source(source)),
    );
    harness.gate.acquired();
    let _selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let io = TestIo::over(&harness);
    let ready = Arc::new(Readiness::default());
    let tasks = start_host(&harness, &io, &ready);
    append(&path, &row("m1", "next"));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["next"]);
    assert_eq!(fence_calls(&io), 0, "Committed recovery keeps its path");
    assert!(!io.calls().contains(&("facts", CHANNEL)));
    finish(tasks).await;
}

#[tokio::test(start_paused = true)]
async fn permanent_open_turn_and_held_body_reservation_block_for_120_minutes() {
    if !isolated(concat!(
        module_path!(),
        "::permanent_open_turn_and_held_body_reservation_block_for_120_minutes"
    )) {
        return;
    }
    for held in [false, true] {
        let body = if held {
            [row("m0", "held Legacy delivery"), closed()].concat()
        } else {
            row("m0", "permanently open")
        };
        let cursor = u64::try_from(body.len()).unwrap();
        let stalled = Stalled::new(&body, cursor, Some(0), Custody::Row);
        let clock = Clock::new();
        let tasks = stalled.start();
        clock.started().await;
        let sends = Arc::new(AtomicUsize::default());
        let transfer = if held {
            let sent = Arc::clone(&sends);
            let (entered, at_transport) = tokio::sync::oneshot::channel();
            let (resume, resumed) = tokio::sync::oneshot::channel();
            let transport = tokio::spawn(async move {
                claim_then_send(
                    Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))),
                    || async move {
                        sent.fetch_add(1, Ordering::SeqCst);
                        entered.send(()).unwrap();
                        resumed.await.unwrap();
                        "legacy"
                    },
                )
                .await
            });
            at_transport.await.unwrap();
            assert_eq!(candidate().sends(), (1, 0));
            Some((resume, transport))
        } else {
            None
        };
        clock.to(120 * MINUTE).await;
        stalled.assert_waiting("the open turn or actual reservation persists for 120 minutes");
        assert!(stalled.harness.port.posts().is_empty());
        if let Some((resume, transport)) = transfer {
            assert_eq!(candidate().sends(), (1, 0));
            assert_eq!(sends.load(Ordering::SeqCst), 1);
            resume.send(()).unwrap();
            assert_eq!(transport.await.unwrap(), Ok(BodySend::Sent("legacy")));
            assert_eq!(candidate().sends(), (1, 1));
            clock.to(120 * MINUTE + TICK).await;
            assert_eq!(adoption(CHANNEL), Adoption::Committed);
            assert_eq!(sends.load(Ordering::SeqCst), 1);
            drop(clock);
            stalled.assert_adopted_at_end(Some(0)).await;
        } else {
            assert_eq!(candidate().sends(), (0, 0));
            assert_eq!(sends.load(Ordering::SeqCst), 0);
            drop(clock);
        }
        finish(tasks).await;
    }
}

#[tokio::test(start_paused = true)]
async fn frontier_lost_or_misaligned_after_deferral_never_initializes_for_120_minutes() {
    if !isolated(concat!(
        module_path!(),
        "::frontier_lost_or_misaligned_after_deferral_never_initializes_for_120_minutes"
    )) {
        return;
    }
    for frontier in [None, Some(5)] {
        let stalled = Stalled::dead_tail();
        assert_ne!(std::fs::read(&stalled.path).unwrap()[4], b'\n');
        let clock = Clock::new();
        let tasks = stalled.start();
        clock.started().await;
        *stalled.legacy.frontier.lock().unwrap() = frontier;
        clock.to(120 * MINUTE).await;
        assert!(!stalled.harness.store.has_channel_dir(CHANNEL));
        assert!(stalled.harness.store.read_init(CHANNEL).unwrap().is_none());
        assert!(stalled.harness.port.posts().is_empty());
        assert_eq!(candidate().sends(), (0, 0));
        if frontier.is_none() {
            assert_eq!(adoption(CHANNEL), Adoption::Released);
            assert_eq!(
                stalled.alarms(),
                [(
                    CHANNEL,
                    WriterAlarm::Released {
                        detail:
                            "writer host: adoption held: the delivery record is not authoritative"
                                .into(),
                    },
                )]
            );
        } else {
            stalled.assert_waiting("a frontier inside a record stays invalid past both deadlines");
        }
        drop(clock);
        finish(tasks).await;
    }
}

#[tokio::test(start_paused = true)]
async fn frontier_decrease_restarts_the_hard_clock_despite_epoch_churn() {
    if !isolated(concat!(
        module_path!(),
        "::frontier_decrease_restarts_the_hard_clock_despite_epoch_churn"
    )) {
        return;
    }
    let first = row("m0", "previously delivered");
    let body = [first.clone(), row("m1", "undelivered"), closed()].concat();
    let stalled = Stalled::new(
        &body,
        body.len() as u64,
        Some(first.len() as u64),
        Custody::Row,
    );
    let clock = Clock::new();
    let tasks = stalled.start();
    clock.started().await;
    assert_eq!(
        *stalled.legacy.frontier.lock().unwrap(),
        Some(first.len() as u64)
    );
    clock.to(30 * MINUTE).await;
    *stalled.legacy.frontier.lock().unwrap() = Some(0);
    // The following tick observes the decrease; both clocks restart at that tick.
    clock.to(30 * MINUTE + TICK).await;
    stalled.assert_waiting("frontier decrease is delivery progress");
    for minute in [40, 50, 60, 70] {
        clock.to(minute * MINUTE).await;
        stalled.legacy.reconnects.fetch_add(1, Ordering::SeqCst);
        clock.to(minute * MINUTE + TICK).await;
        stalled.assert_waiting("epoch churn prevents soft expiry only");
    }
    clock.to(HARD - Duration::from_nanos(1)).await;
    stalled.assert_waiting("the original hard deadline is one nanosecond short");
    clock.to(HARD).await;
    stalled.assert_waiting("frontier decrease restarted the original hard deadline");
    clock.to(HARD + Duration::from_nanos(1)).await;
    stalled.assert_waiting("the original hard deadline remains superseded");
    for minute in [80, 90, 100] {
        clock.to(minute * MINUTE + TICK).await;
        stalled.legacy.reconnects.fetch_add(1, Ordering::SeqCst);
        clock.to(minute * MINUTE + 2 * TICK).await;
        stalled.assert_waiting("epoch churn cannot restart the hard clock");
    }
    let restarted_hard = 110 * MINUTE + TICK;
    clock.to(restarted_hard - Duration::from_nanos(1)).await;
    stalled.assert_waiting("the restarted hard deadline is one nanosecond short");
    clock.to(restarted_hard).await;
    assert_eq!(
        adoption(CHANNEL),
        Adoption::Committed,
        "the exact hard deadline follows the observed frontier decrease"
    );
    assert_eq!(*stalled.legacy.frontier.lock().unwrap(), Some(0));
    assert_eq!(stalled.legacy.reconnects.load(Ordering::SeqCst), 7);
    assert_eq!(candidate().sends(), (0, 0));
    assert_eq!(fence_calls(&stalled.io), 1);
    clock.to(restarted_hard + Duration::from_nanos(1)).await;
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    assert_eq!(fence_calls(&stalled.io), 1, "the handoff commits once");
    drop(clock);
    stalled.assert_adopted_at_end(Some(0)).await;
    finish(tasks).await;
}
