//! A writer halted by a store write, driven through the real host: it waits, recovers its store as
//! a restart would and starts again in process, unless the stop was permanent.

use std::io::ErrorKind::{
    self, Interrupted, InvalidData, NotFound, Other, PermissionDenied, QuotaExceeded, StorageFull,
    WouldBlock,
};
use std::time::Duration;

use tokio::time::Instant;

use super::*;
use crate::services::tui_o::cutover::test_override::ChannelsGuard;
use crate::services::tui_o::store::fault::{self, Keep, Step as At};
use crate::services::tui_o::writer::deliver::StopCause;
use crate::services::tui_o::writer::resume;

const SECOND: Duration = Duration::from_secs(1);

/// One selected, switched-over channel hosted with its gate owned; the host's store recoveries are
/// recorded from the start.
struct Scene {
    harness: Harness,
    path: PathBuf,
    io: Arc<TestIo>,
    ready: Arc<Readiness>,
    hosts: Vec<tokio::task::JoinHandle<()>>,
    store: fault::Watch,
    _selected: ChannelsGuard,
}

impl Drop for Scene {
    fn drop(&mut self) {
        abort(std::mem::take(&mut self.hosts));
    }
}

impl Scene {
    async fn hosted() -> Self {
        let (harness, path, source) = switched_over(&row("m0", "before the switch"));
        p5_log(harness._runtime.path(), CHANNEL, &startup(source));
        let selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
        let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
        let store = fault::watch(&Self::dir_of(&harness));
        harness.gate.acquired();
        let hosts = hosted(&harness, &io, true, &ready);
        polls(3).await;
        assert!(ready.accepts(CHANNEL), "the boot actor is up");
        Self {
            harness,
            path,
            io,
            ready,
            hosts,
            store,
            _selected: selected,
        }
    }

    fn dir_of(harness: &Harness) -> PathBuf {
        let store = harness._runtime.path().join("o_store");
        store.join(CHANNEL.to_string())
    }

    fn spool(&self) -> PathBuf {
        Self::dir_of(&self.harness).join("spool")
    }

    fn ledger(&self) -> PathBuf {
        Self::dir_of(&self.harness).join("ledger.jsonl")
    }

    fn say(&self, id: &str, text: &str) {
        append(&self.path, &row(id, text));
    }

    fn posts(&self) -> Vec<String> {
        self.harness.port.posts()
    }

    fn halts(&self) -> usize {
        self.io.alarms.halted().len()
    }

    /// Alarms that stop a writer: a halt, a refused POST or a ledger violation.
    fn stops(&self) -> usize {
        let raised = self.io.alarms.0.lock().unwrap();
        let stops = raised.iter().filter(|(_, alarm)| {
            let stop = matches!(alarm, WriterAlarm::Blocked { .. });
            stop || matches!(
                alarm,
                WriterAlarm::Halted { .. } | WriterAlarm::LedgerViolation { .. }
            )
        });
        stops.count()
    }

    fn resumes(&self) -> Vec<&'static str> {
        let reports = self.io.alarms.1.lock().unwrap();
        reports.iter().map(|(_, report)| *report).collect()
    }

    /// Seconds after `since` at which the host recovered the store.
    fn opens_after(&self, since: Instant) -> Vec<u64> {
        after(since, self.store.opens())
    }

    /// Seconds after `since` at which a writer was built, each with no other writer alive.
    fn writers_after(&self, since: Instant) -> Vec<u64> {
        let leases = self.io.leases.lock().unwrap().clone();
        let alive: Vec<_> = leases.iter().map(|(_, writers, _)| *writers).collect();
        assert!(alive.iter().all(|writers| *writers == 0), "{alive:?}");
        after(since, leases.iter().map(|(at, ..)| *at).collect())
    }

    /// Waits for the next stop and returns when it was seen.
    async fn halt(&self) -> Instant {
        let before = self.stops();
        for _ in 0..20_000 {
            if self.stops() > before {
                return Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no halt after {before}: {:?}", self.posts());
    }

    fn ledger_has(&self, entry: &str) -> bool {
        let text = std::fs::read_to_string(self.ledger()).unwrap();
        text.contains(&format!("\"type\":\"{entry}\""))
    }

    /// Ends the host, waits until its last writer is gone, then checks a restart recovers the
    /// store undamaged and posts nothing more.
    async fn restart_posts_nothing(mut self) {
        abort(std::mem::take(&mut self.hosts));
        for _ in 0..1000 {
            if Arc::strong_count(&self.harness.lease) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            Arc::strong_count(&self.harness.lease),
            2,
            "the last writer is gone"
        );
        let posted = self.posts();
        let reopened = self.harness.channel();
        assert_eq!(reopened.ledger().violation(), None);
        drop(reopened);
        let (io, ready) = (TestIo::over(&self.harness), Arc::new(Readiness::default()));
        let hosts = hosted(&self.harness, &io, true, &ready);
        polls(3).await;
        assert!(ready.accepts(CHANNEL), "a restart takes the channel over");
        assert_eq!(self.posts(), posted, "a restart reposts nothing");
        abort(hosts);
    }
}

fn after(since: Instant, times: Vec<Instant>) -> Vec<u64> {
    let later = times.into_iter().filter(|at| *at > since);
    later
        .map(|at| (at - since).as_secs_f64().round() as u64)
        .collect()
}

/// Each time is within two seconds of the one expected.
fn near(actual: &[u64], expected: &[u64]) -> bool {
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected)
            .all(|(a, e)| a.abs_diff(*e) <= 2)
}

async fn sleep_until(at: Instant) {
    tokio::time::sleep_until(at).await;
}

#[test]
fn only_a_transient_store_io_halt_is_resumable() {
    let cause = |alarm: &WriterAlarm, io| StopCause {
        alarm: alarm.clone(),
        io,
        unsent: None,
    };
    let halted = WriterAlarm::Halted {
        detail: "spool append".into(),
    };
    let violation = WriterAlarm::LedgerViolation {
        detail: "serial".into(),
    };
    for kind in [StorageFull, QuotaExceeded, Interrupted, WouldBlock] {
        assert!(resume::resumable(&cause(&halted, Some(kind))), "{kind:?}");
        for other in [&violation, &WriterAlarm::Blocked { status: 403 }] {
            assert!(!resume::resumable(&cause(other, Some(kind))), "{other:?}");
        }
    }
    let lasting: [Option<ErrorKind>; 5] = [
        None,
        Some(PermissionDenied),
        Some(InvalidData),
        Some(NotFound),
        Some(Other),
    ];
    for io in lasting {
        assert!(!resume::resumable(&cause(&halted, io)), "{io:?}");
    }
}

/// The spool cannot take `three`: the writer halts, waits, and once space is back posts what was
/// owed, what it failed on and what arrived while it was stopped, each once.
async fn out_of_space(keep: Keep) {
    let scene = Scene::hosted().await;
    let booted = Instant::now();
    scene.say("m1", "one");
    polls(3).await;
    scene.harness.gate.lost();
    scene.say("m2", "two");
    polls(3).await;
    assert_eq!(scene.posts(), ["one"], "{keep:?}");
    let full = fault::plant(&scene.spool(), At::Append(keep), StorageFull, None);
    scene.say("m3", "three");
    let halted = scene.halt().await;
    scene.say("m4", "four");
    sleep_until(halted + 25 * SECOND).await;
    assert!(
        scene.opens_after(booted).is_empty(),
        "{keep:?}: no recovery before the first wait"
    );
    assert!(scene.writers_after(booted).is_empty());
    assert_eq!(scene.posts(), ["one"]);
    assert!(!scene.ready.is_ready(CHANNEL));
    assert_eq!(scene.resumes(), ["pending"]);
    drop(full);
    scene.harness.gate.acquired();
    sleep_until(halted + 35 * SECOND).await;
    polls(3).await;
    assert_eq!(scene.posts(), ["one", "two", "three", "four"], "{keep:?}");
    assert!(
        near(&scene.opens_after(halted), &[30]),
        "{:?}",
        scene.opens_after(halted)
    );
    assert!(near(&scene.writers_after(halted), &[30]));
    assert_eq!(scene.resumes(), ["pending", "cleared", "settled"]);
    assert_eq!(scene.halts(), 1);
    assert!(scene.ready.accepts(CHANNEL));
    scene.restart_posts_nothing().await;
}

#[tokio::test(start_paused = true)]
async fn an_out_of_space_spool_append_halts_then_resumes_in_process_and_posts_each_unit_once() {
    for keep in [Keep::Nothing, Keep::Half, Keep::All] {
        out_of_space(keep).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_resume_that_halts_again_waits_longer_up_to_the_cap_until_a_steady_run_starts_over() {
    let scene = Scene::hosted().await;
    scene.say("m1", "one");
    polls(3).await;
    let full = fault::plant(&scene.spool(), At::Append(Keep::Nothing), StorageFull, None);
    scene.say("m2", "two");
    let halted = scene.halt().await;
    sleep_until(halted + 1060 * SECOND).await;
    let waits = [30, 90, 210, 450, 750, 1050];
    let opens = scene.opens_after(halted);
    assert!(
        near(&opens, &waits),
        "each attempt halts again and waits longer: {opens:?}"
    );
    let writers = scene.writers_after(halted);
    assert!(near(&writers, &waits), "{writers:?}");
    assert_eq!(scene.posts(), ["one"]);
    drop(full);
    sleep_until(halted + 1360 * SECOND).await;
    assert_eq!(scene.posts(), ["one", "two"]);
    assert!(near(&scene.opens_after(halted)[6..], &[1350]));

    // A writer that ran for six minutes before its next halt waits only the first wait again.
    sleep_until(halted + 1720 * SECOND).await;
    let full = fault::plant(&scene.spool(), At::Append(Keep::Nothing), StorageFull, None);
    scene.say("m3", "three");
    let again = scene.halt().await;
    sleep_until(again + 40 * SECOND).await;
    assert!(
        near(&scene.opens_after(again), &[30]),
        "{:?}",
        scene.opens_after(again)
    );
    drop(full);
}

#[tokio::test(start_paused = true)]
async fn a_recovery_that_fails_again_keeps_the_wait_growing() {
    let scene = Scene::hosted().await;
    let full = fault::plant(&scene.spool(), At::Append(Keep::Nothing), StorageFull, None);
    scene.say("m1", "one");
    let halted = scene.halt().await;
    drop(full);
    let held = std::fs::File::open(scene.ledger()).unwrap();
    held.try_lock().unwrap();
    sleep_until(halted + 300 * SECOND).await;
    drop(held);
    sleep_until(halted + 460 * SECOND).await;
    let opens = scene.opens_after(halted);
    assert!(
        near(&opens, &[30, 90, 210, 450]),
        "a locked ledger is retried: {opens:?}"
    );
    assert!(
        near(&scene.writers_after(halted), &[450]),
        "no writer while it fails"
    );
    assert_eq!(scene.posts(), ["one"]);
    assert_eq!(
        scene.halts(),
        1,
        "a failed recovery does not hold the channel"
    );
    assert_eq!(scene.resumes(), ["pending", "cleared", "settled"]);
}

#[tokio::test(start_paused = true)]
async fn a_permanent_stop_is_never_resumed() {
    type Stop = Box<dyn Fn(&Scene) -> Option<fault::Planted>>;
    let replied = |reply: Reply| -> Stop {
        Box::new(move |scene| {
            scene.harness.port.replies.lock().unwrap().push_back(reply);
            None
        })
    };
    let shrunk: Stop = Box::new(|scene| {
        std::fs::write(&scene.path, b"").unwrap();
        None
    });
    let denied: Stop = Box::new(|scene| {
        let spool = scene.spool();
        Some(fault::plant(
            &spool,
            At::Append(Keep::Nothing),
            PermissionDenied,
            None,
        ))
    });
    let stops = [
        ("violation", replied(Reply::CreatedBehind)),
        ("blocked", replied(Reply::Refused(403))),
        ("shrunk", shrunk),
        ("denied", denied),
    ];
    for (name, stop) in stops {
        let scene = Scene::hosted().await;
        let booted = Instant::now();
        let _planted = stop(&scene);
        scene.say("m1", "one");
        let halted = scene.halt().await;
        sleep_until(halted + 1800 * SECOND).await;
        assert!(scene.opens_after(booted).is_empty(), "{name}");
        assert!(scene.writers_after(booted).is_empty(), "{name}");
        assert_eq!(scene.resumes(), Vec::<&str>::new(), "{name}");
        assert!(!scene.ready.is_ready(CHANNEL), "{name}");
    }
}

#[tokio::test(start_paused = true)]
async fn store_damage_found_while_resuming_holds_the_channel_without_more_attempts() {
    let scene = Scene::hosted().await;
    scene.say("m1", "one");
    polls(3).await;
    let full = fault::plant(&scene.spool(), At::Append(Keep::Nothing), StorageFull, None);
    scene.say("m2", "two");
    let halted = scene.halt().await;
    drop(full);
    let mut ledger = std::fs::read(scene.ledger()).unwrap();
    ledger[0] = b'X';
    std::fs::write(scene.ledger(), ledger).unwrap();
    sleep_until(halted + 1800 * SECOND).await;
    assert!(
        near(&scene.opens_after(halted), &[30]),
        "{:?}",
        scene.opens_after(halted)
    );
    assert!(scene.writers_after(halted).is_empty());
    let held = scene.io.alarms.halted();
    assert!(
        matches!(held.as_slice(), [_, (CHANNEL, detail)] if detail.starts_with("writer host: recovery")),
        "{held:?}"
    );
    assert_eq!(scene.resumes(), ["pending", "settled"]);
    assert_eq!(scene.posts(), ["one"]);
}

#[tokio::test(start_paused = true)]
async fn repeated_whole_ledger_failures_never_spend_an_unposted_piece() {
    let scene = Scene::hosted().await;
    scene.harness.gate.lost();
    for piece in ["p1", "p2", "p3", "p4"] {
        scene.say(piece, piece);
    }
    polls(3).await;
    scene.harness.port.unreadable.store(true, Ordering::SeqCst);
    let full = fault::plant(&scene.ledger(), At::Append(Keep::All), StorageFull, None);
    scene.harness.gate.acquired();
    let halted = scene.halt().await;
    sleep_until(halted + 760 * SECOND).await;
    let settled = scene.ledger_has("unresolved") || scene.ledger_has("not_found");
    assert!(
        !settled,
        "no attempt settles the unposted piece from history"
    );
    assert_eq!(
        scene.store.withdrawals(),
        5,
        "each attempt takes back the same unposted entry"
    );
    assert!(scene.posts().is_empty());
    let opens = scene.opens_after(halted);
    assert!(near(&opens, &[30, 90, 210, 450, 750]), "{opens:?}");
    drop(full);
    sleep_until(halted + 1060 * SECOND).await;
    polls(3).await;
    assert_eq!(scene.store.withdrawals(), 6);
    assert_eq!(scene.posts(), ["p1", "p2", "p3", "p4"]);
    assert!(!scene.ledger_has("unresolved") && !scene.ledger_has("not_found"));
    let raised = scene.io.alarms.0.lock().unwrap().clone();
    let settled = raised.iter().filter(|(_, alarm)| {
        matches!(
            alarm,
            WriterAlarm::Unresolved { .. } | WriterAlarm::NotFound { .. }
        )
    });
    assert_eq!(settled.count(), 0, "{raised:?}");
    scene.restart_posts_nothing().await;
}

/// The piece's unposted entry is taken back, its next POST goes out, and then its `Posted` fails
/// to append with `keep`; the halt that follows names nothing.
async fn posted_then_unrecorded(keep: Keep) {
    let scene = Scene::hosted().await;
    let first = fault::plant(&scene.ledger(), At::Append(Keep::All), StorageFull, Some(1));
    let (ledger, armed) = (scene.ledger(), Arc::new(Mutex::new(None)));
    let arming = Arc::clone(&armed);
    *scene.harness.port.on_post.lock().unwrap() = Some(Box::new(move || {
        let planted = fault::plant(&ledger, At::Append(keep), StorageFull, Some(1));
        arming.lock().unwrap().get_or_insert(planted);
    }));
    scene.say("p1", "p1");
    let unsent = scene.halt().await;
    let unrecorded = scene.halt().await;
    assert!(near(&after(unsent, vec![unrecorded]), &[30]), "{keep:?}");
    assert_eq!(scene.posts(), ["p1"]);
    sleep_until(unrecorded + 70 * SECOND).await;
    polls(3).await;
    assert_eq!(
        scene.posts(),
        ["p1"],
        "{keep:?}: a POST that went out is settled from history"
    );
    assert_eq!(scene.store.withdrawals(), 1, "{keep:?}");
    assert!(scene.ledger_has("posted") && !scene.ledger_has("not_found"));
    drop((first, armed));
    scene.restart_posts_nothing().await;
}

#[tokio::test(start_paused = true)]
async fn a_withdrawn_piece_whose_post_went_out_is_settled_from_history_not_withdrawn_again() {
    for keep in [Keep::Nothing, Keep::Half] {
        posted_then_unrecorded(keep).await;
    }
}

/// The unposted entry's withdrawal or the spool recovery after it fails once at `at`.
async fn withdrawal_fails_once(at: At, under: fn(&Scene) -> PathBuf, withdrawn: usize) {
    let scene = Scene::hosted().await;
    let first = fault::plant(&scene.ledger(), At::Append(Keep::All), StorageFull, Some(1));
    let failing = fault::plant(&under(&scene), at, Interrupted, Some(1));
    scene.say("p1", "p1");
    let halted = scene.halt().await;
    sleep_until(halted + 40 * SECOND).await;
    assert!(near(&scene.opens_after(halted), &[30]), "{at:?}");
    assert!(
        scene.writers_after(halted).is_empty(),
        "{at:?}: no writer over a failed recovery"
    );
    assert_eq!(scene.resumes(), ["pending"]);
    sleep_until(halted + 100 * SECOND).await;
    polls(3).await;
    assert!(near(&scene.opens_after(halted), &[30, 90]), "{at:?}");
    assert!(near(&scene.writers_after(halted), &[90]), "{at:?}");
    assert_eq!(
        scene.posts(),
        ["p1"],
        "{at:?}: the evidence outlives the failed attempt"
    );
    assert_eq!(scene.store.withdrawals(), withdrawn, "{at:?}");
    drop((first, failing));
    scene.restart_posts_nothing().await;
}

#[tokio::test(start_paused = true)]
async fn a_withdrawal_that_fails_keeps_its_evidence_and_starts_no_writer_until_a_retry_recovers() {
    let ledger: fn(&Scene) -> PathBuf = Scene::ledger;
    let channel: fn(&Scene) -> PathBuf = |scene| Scene::dir_of(&scene.harness);
    withdrawal_fails_once(At::Cut, ledger, 1).await;
    withdrawal_fails_once(At::CutSync, ledger, 0).await;
    withdrawal_fails_once(At::SpoolRecovery, channel, 1).await;
}

#[tokio::test(start_paused = true)]
async fn a_slow_post_releases_its_lease_before_the_next_writer() {
    let scene = Scene::hosted().await;
    let port = &scene.harness.port;
    port.lazy.store(true, Ordering::SeqCst);
    let (_never, hold) = tokio::sync::oneshot::channel();
    *port.hold.lock().unwrap() = Some(hold);
    let acquired = Arc::new(AtomicUsize::new(0));
    let counting = Arc::clone(&acquired);
    *scene.harness.lease.on_acquire.lock().unwrap() = Some(Box::new(move || {
        counting.fetch_add(1, Ordering::SeqCst);
    }));
    let (ledger, armed) = (scene.ledger(), Arc::new(Mutex::new(None)));
    let arming = Arc::clone(&armed);
    *port.on_post.lock().unwrap() = Some(Box::new(move || {
        let planted = fault::plant(&ledger, At::Append(Keep::Nothing), StorageFull, Some(1));
        arming.lock().unwrap().get_or_insert(planted);
    }));
    scene.say("p1", "p1");
    let halted = scene.halt().await;
    let taken = acquired.load(Ordering::SeqCst);
    sleep_until(halted + 35 * SECOND).await;
    polls(3).await;
    let leases = scene.io.leases.lock().unwrap().clone();
    let (_, writers, back) = *leases.last().unwrap();
    assert_eq!(
        (leases.len(), writers, back),
        (2, 0, taken),
        "the slow POST's lease came back first"
    );
    assert_eq!(scene.posts(), ["p1"]);
    drop(armed);
}
