//! #5147: forensics the self-watchdog records when it decides the runtime is
//! unresponsive.
//!
//! ## Why this exists
//!
//! Three investigations (#4756, #4770, #5147) tried to explain watchdog kills
//! from the `sample` dump that `discord::health::recovery::spawn_watchdog`
//! captures, and all three stalled. The dump cannot answer the question, for a
//! structural rather than incidental reason: a `sample` of *any*
//! `#[tokio::main]` process shows the main thread parked in
//! `_pthread_cond_wait` (sitting in `Runtime::block_on`) and most
//! `tokio-rt-worker` threads parked the same way (no work), with one worker in
//! `kevent` (it holds the I/O driver). A *healthy* AgentDesk shows exactly that
//! shape, so the shape proves nothing.
//!
//! What the dump also cannot show is what actually matters: the watchdog probes
//! `GET /api/health`, that handler `await`s several Postgres queries, and a task
//! blocked in an `await` is not a running thread — it does not appear in a
//! thread sample at all.
//!
//! So this module records the facts the dump structurally cannot carry. Two
//! of them land here; which stage of the probe failed follows.
//!
//! ### 1. Whether the runtime is scheduling tasks — [`RuntimeLiveness`]
//!
//! This is the discriminator `stage=` cannot be, and why the two are always
//! reported together. [`spawn_runtime_liveness_beacon`] runs one tokio task
//! storing a monotonic timestamp every [`RUNTIME_TICK_PERIOD`]. It depends on
//! the timer driver and a worker thread and nothing else — not the acceptor,
//! not the HTTP stack, not Postgres. A stale tick is positive evidence the
//! runtime stopped polling; a fresh one, that it did not. `verdict` (next commit) combines
//! the two into the single conclusion the watchdog is entitled to draw.
//!
//! Read both labels literally; the asymmetry between them is the whole subtlety.
//! The runtime is multi-threaded (`Runtime::new()`, so `worker_threads =
//! available_parallelism()`, measured at **14** on the release host):
//!
//! * `runtime=stalled` means a task that wakes on a timer and stores two
//!   atomics was not scheduled on **any** of those workers for
//!   [`RUNTIME_TICK_STALE_MS`]. That is a strong statement and it is meant to
//!   be: nothing short of a fully wedged runtime produces it.
//! * `runtime=scheduling` is correspondingly weak. **One** idle worker is
//!   enough to tick the beacon, so it rules out a fully wedged runtime and
//!   nothing weaker. Thirteen of fourteen workers blocked in sync I/O or
//!   `block_in_place` — executor starvation, which *is* the executor's fault —
//!   still reads as `scheduling`, and the `handler_*` verdict that follows would
//!   name the wrong component. `runtime_workers=` is logged beside it so the
//!   next investigator can weigh how much slack that leaves.
//!
//! ### 2. What the database was doing — [`Breadcrumbs`]
//!
//! [`observe_db`] brackets one Postgres await with a [`DbProbeGuard`], so
//! `db_in_flight` says how many are outstanding right now and
//! `db_probes_started/failed` how many have run. Nothing calls it yet; the
//! commit that brackets the public `GET /api/health` path enumerates the
//! sites, their conditions and the deliberate omissions here.
//!
//! ## Cost and deadlock-safety
//!
//! The recording path is [`DbProbeGuard::new`] plus exactly one of `finish` /
//! `Drop`. The three outcomes do not cost the same, so they are enumerated
//! rather than averaged — "three atomics" was quoted for all three and is right
//! for only one:
//!
//! | outcome | relaxed RMW | relaxed store | clock read |
//! |---|---|---|---|
//! | success (`finish(true)`) | 3 (`started+`, `in_flight+`, `in_flight-`) | 1 (`LAST_DB_OK_AT`) | 1 |
//! | failure (`finish(false)`) | 4 (the above + `failed+`) | 1 (`LAST_DB_ERR_AT`) | 1 |
//! | cancellation (`Drop`, unsettled) | 3 (`started+`, `in_flight+`, `in_flight-`) | 0 | 0 |
//!
//! No lock, no allocation and nothing that blocks on any of the three paths, so
//! it cannot participate in — let alone deepen — a deadlock. It brackets a
//! Postgres round trip costing milliseconds: overhead ~1e-7 in every column.
//!
//! The beacon costs one timer wakeup per [`RUNTIME_TICK_PERIOD`] and two
//! relaxed stores; it never allocates after spawn and never touches the
//! database, so it cannot itself be blocked by what it is measuring.
//!
//! The reading path ([`snapshot`]) runs on the watchdog's dedicated OS thread,
//! which is not a tokio worker, at most once per 30s check. It only reads
//! atomics, so it stays readable even when every tokio worker is stuck.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Monotonic epoch for every timestamp in this module. `Instant` cannot live in
/// an atomic, so timestamps are stored as milliseconds since this point.
/// Monotonic (rather than `SystemTime`) so a wall-clock adjustment cannot
/// produce a nonsense age.
///
/// Being a `LazyLock`, this is *first use*, not process start: it is whichever
/// of a probe, a beacon tick or a watchdog snapshot happens first. Nothing here
/// reports an absolute time — every consumer takes a difference between two
/// values measured against this same epoch — so the distinction cannot reach a
/// log line. It is called out only so nobody later builds an uptime field on it.
static PROCESS_START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Stored timestamps are `mono_ms() + 1` so that `0` unambiguously means
/// "never happened" even during the first millisecond of the process.
fn mono_ms() -> u64 {
    PROCESS_START.elapsed().as_millis() as u64
}

static DB_PROBES_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);
static DB_PROBES_STARTED: AtomicU64 = AtomicU64::new(0);
static DB_PROBES_FAILED: AtomicU64 = AtomicU64::new(0);
static LAST_DB_OK_AT: AtomicU64 = AtomicU64::new(0);
static LAST_DB_ERR_AT: AtomicU64 = AtomicU64::new(0);
static RUNTIME_TICKS: AtomicU64 = AtomicU64::new(0);
static LAST_RUNTIME_TICK_AT: AtomicU64 = AtomicU64::new(0);
/// Worker threads the runtime was built with, recorded once by
/// [`spawn_runtime_liveness_beacon`]. `0` means the beacon never started, so
/// nothing has ever asked the runtime.
///
/// Logged because `runtime=scheduling` says only that *one* worker was free.
/// Without this number a reader cannot tell whether that leaves 13 other
/// workers unaccounted for or none at all. See [`RuntimeLiveness`].
static RUNTIME_WORKERS: AtomicU64 = AtomicU64::new(0);

/// How often [`spawn_runtime_liveness_beacon`] proves the runtime is alive.
///
/// Short relative to the watchdog's 5s probe timeout so that "the runtime did
/// not poll a ready timer for the entire probe window" is unambiguous, and long
/// enough that the beacon is a rounding error next to the 30s check interval.
pub(crate) const RUNTIME_TICK_PERIOD: std::time::Duration = std::time::Duration::from_secs(1);

/// A tick older than this means the runtime is not scheduling tasks.
///
/// Five periods. The beacon is a timer, not a deadline, so it drifts under load
/// and under `MissedTickBehavior::Delay`; the threshold has to tolerate that
/// without tolerating a stall. It equals the probe's own read timeout
/// (`recovery::spawn_watchdog`'s `TCP_TIMEOUT`, 5s), so crossing it means the
/// runtime failed to run one trivial task for at least as long as the probe
/// waited for a byte.
///
/// Both numbers, and the two relations that justify them, are pinned as
/// literals by `tests::the_beacon_constants_are_pinned_in_absolute_units` and
/// `tests::the_stale_threshold_matches_the_watchdogs_own_probe_timeout`. They
/// have to be: an oracle written as `RUNTIME_TICK_STALE_MS + 1` moves with the
/// constant it is meant to be checking and cannot fail.
pub(crate) const RUNTIME_TICK_STALE_MS: u64 = 5_000;

/// Brackets one Postgres health probe.
///
/// Created before the query is issued and resolved by [`DbProbeGuard::finish`].
/// If the future is instead *cancelled* mid-query the guard is dropped without
/// `finish`, and `Drop` still decrements the in-flight counter — otherwise a
/// cancelled probe would inflate `db_in_flight` forever and the breadcrumb
/// would lie in exactly the situation it exists to describe.
#[must_use = "the probe stays counted as in-flight until the guard is dropped"]
pub(crate) struct DbProbeGuard {
    settled: bool,
}

impl DbProbeGuard {
    pub(crate) fn new() -> Self {
        DB_PROBES_STARTED.fetch_add(1, Ordering::Relaxed);
        DB_PROBES_IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
        Self { settled: false }
    }

    /// Records the probe's result and releases the in-flight slot.
    pub(crate) fn finish(mut self, ok: bool) {
        let at = mono_ms().saturating_add(1);
        if ok {
            LAST_DB_OK_AT.store(at, Ordering::Relaxed);
        } else {
            DB_PROBES_FAILED.fetch_add(1, Ordering::Relaxed);
            LAST_DB_ERR_AT.store(at, Ordering::Relaxed);
        }
        self.settled = true;
        DB_PROBES_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Drop for DbProbeGuard {
    fn drop(&mut self) {
        if !self.settled {
            DB_PROBES_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Brackets one `/api/health` Postgres await with a [`DbProbeGuard`].
///
/// Takes the future rather than leaving each call site to open-code
/// `new()`/`finish()`: the health handler awaits seven of these in sequence
/// unconditionally (eight with a `health_registry`, nine on a cluster standby)
/// and an unbracketed one is invisible — it would report `db_in_flight=0` at
/// kill time and clear the database of a stall it caused.
///
/// `succeeded` decides what counts as a healthy round trip. For a `sqlx` call
/// that is `Result::is_ok`: a query that legitimately matched no rows still
/// proves Postgres answered, and must not be recorded as a failed probe.
///
/// Cancellation-safe by construction. If the caller's future is dropped while
/// the query is pending, `guard` is dropped without `finish` and its `Drop`
/// releases the in-flight slot.
pub(crate) async fn observe_db<T>(
    query: impl std::future::Future<Output = T>,
    succeeded: impl FnOnce(&T) -> bool,
) -> T {
    let guard = DbProbeGuard::new();
    let outcome = query.await;
    guard.finish(succeeded(&outcome));
    outcome
}

/// Records one proof that the tokio runtime is still scheduling tasks.
///
/// Separate from [`spawn_runtime_liveness_beacon`] so the beacon's body is
/// testable without a timer and without a 1s wait.
pub(crate) fn record_runtime_tick() {
    RUNTIME_TICKS.fetch_add(1, Ordering::Relaxed);
    LAST_RUNTIME_TICK_AT.store(mono_ms().saturating_add(1), Ordering::Relaxed);
}

/// Spawns the runtime-liveness beacon. Call once, from inside the runtime.
///
/// The task awaits a timer and stores two atomics. It touches no lock, no
/// channel, no socket and no database, so the only thing that can stop it is
/// the runtime failing to poll a ready task — which is precisely the condition
/// it exists to report. See [`RuntimeLiveness`].
///
/// The single production caller is
/// [`discord::health::self_watchdog::spawn_watchdog`](crate::services::discord::health::self_watchdog::spawn_watchdog),
/// which arms the beacon before it creates its thread. That is deliberate: the
/// beacon is the watchdog's only evidence, and a separate boot-site call could
/// be deleted while the watchdog kept running and kept concluding nothing.
///
/// Returns [`BeaconArmed`], which is both the outcome and the token
/// `spawn_watchdog` needs to create its thread. Off a runtime it reports the
/// failure through that token rather than panicking: a missing beacon costs
/// `verdict=undetermined_no_beacon`, a panic at boot costs the whole service.
pub(crate) fn spawn_runtime_liveness_beacon() -> BeaconArmed {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return BeaconArmed { workers: None };
    };
    // Recorded here rather than at the read side because the watchdog reads
    // from its own OS thread, where a runtime handle is not available. One
    // relaxed store, once per process.
    let workers = handle.metrics().num_workers() as u64;
    RUNTIME_WORKERS.store(workers, Ordering::Relaxed);
    handle.spawn(async {
        let mut interval = tokio::time::interval(RUNTIME_TICK_PERIOD);
        // Default `Burst` would replay every tick missed during a stall in a
        // tight loop the instant the runtime recovers, which is noise. `Delay`
        // just resumes, so the recorded age reflects the real gap.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            record_runtime_tick();
        }
    });
    BeaconArmed {
        workers: Some(workers),
    }
}

/// Proof that [`spawn_runtime_liveness_beacon`] has already run.
///
/// #5147: this exists so the *order* of the two boot steps is a data
/// dependency rather than a claim.
/// `discord::health::self_watchdog::spawn_watchdog_thread` takes one by value,
/// and the only way to obtain one is to call the arming function — so
/// "the beacon is armed before the watchdog thread exists" is checked by the
/// compiler on every build.
///
/// It replaces a test that read `self_watchdog.rs` with `include_str!` and
/// compared byte offsets after blanking comments. Adversarial review broke that
/// six ways and also made it fail on *correct* code — `self_watchdog`'s module
/// doc enumerates the seven inputs. None of them is expressible against a type.
///
/// There is deliberately no public constructor and no `Default`: a caller
/// outside this module cannot fabricate the proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "the watchdog thread takes this by value; dropping it means the beacon was armed for nothing"]
pub(crate) struct BeaconArmed {
    /// Worker threads the beacon saw, or `None` when there was no runtime
    /// handle and therefore no beacon.
    workers: Option<u64>,
}

impl BeaconArmed {
    /// `Some(n)` when the beacon is running on an `n`-worker runtime.
    pub(crate) fn workers(self) -> Option<u64> {
        self.workers
    }

    /// The line the boot path must emit, and at which level.
    ///
    /// `Err` means the beacon is **not** running, so every later watchdog
    /// failure will report `verdict=undetermined_no_beacon`. Returned rather
    /// than logged here so the text is assertable without a tracing subscriber.
    pub(crate) fn boot_report(self) -> Result<String, String> {
        match self.workers {
            Some(workers) => Ok(format!(
                "hang_forensics: runtime-liveness beacon armed on {workers} worker thread(s)"
            )),
            None => Err(
                "hang_forensics: runtime-liveness beacon NOT armed — no tokio runtime \
                 on the calling thread. Every watchdog failure will report \
                 verdict=undetermined_no_beacon and the next hang investigation is back to \
                 reading a `sample` dump that cannot answer the question."
                    .to_string(),
            ),
        }
    }
}

/// Point-in-time view of the breadcrumbs, taken by the watchdog thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Breadcrumbs {
    /// Health-path Postgres awaits outstanding right now, **process-wide**.
    ///
    /// This is a global gauge, not a per-request one — that is deliberate, it
    /// is what lets the watchdog's own OS thread read it without touching
    /// anything the runtime could be blocked on. The cost is that concurrent
    /// `GET /api/health`, `/api/health/detail` and dashboard-poller requests
    /// all contribute. So a non-zero value proves that *some* health request is
    /// inside a Postgres await; it does **not** prove the request this probe
    /// made is. `handler_blocked_on_db` inherits exactly that weakness and must
    /// be read with it.
    pub(crate) db_in_flight: u64,
    pub(crate) db_probes_started: u64,
    pub(crate) db_probes_failed: u64,
    /// Age of the last *successful* probe, or `None` if none ever succeeded.
    pub(crate) last_db_ok_age_ms: Option<u64>,
    /// Age of the last *failed* probe, or `None` if none ever failed.
    pub(crate) last_db_err_age_ms: Option<u64>,
    /// Total beacon ticks since start. Zero means the beacon never ran.
    pub(crate) runtime_ticks: u64,
    /// Age of the last beacon tick, or `None` if it never ticked.
    pub(crate) runtime_tick_age_ms: Option<u64>,
    /// Worker threads in the runtime, or `0` if the beacon never started. See
    /// [`RUNTIME_WORKERS`] for why a verdict is unreadable without it.
    pub(crate) runtime_workers: u64,
}

/// What the beacon says about the runtime, independently of the acceptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeLiveness {
    /// The beacon ticked within [`RUNTIME_TICK_STALE_MS`]: **at least one**
    /// worker thread ran a ready timer task.
    ///
    /// State what this excludes and what it does not, because an earlier draft
    /// asserted the executor's innocence here — *"a failed probe is the
    /// handler's fault, not the executor's"* — and that is false. The runtime
    /// is multi-threaded with `available_parallelism()` workers (14 on the
    /// release host), and one free worker is enough to tick a beacon that only
    /// stores two atomics.
    ///
    /// * **Excluded:** a fully wedged runtime — no worker polling anything.
    /// * **Not excluded:** partial executor starvation. 13 of 14 workers
    ///   blocked in sync I/O or `block_in_place` still tick the beacon, still
    ///   read as `Scheduling`, and still produce a `handler_*` verdict — while
    ///   the actual fault is the executor. Compare `runtime_workers=` against
    ///   what the process is known to run concurrently before believing a
    ///   `handler_*` verdict.
    Scheduling { age_ms: u64 },
    /// The beacon has not ticked for [`RUNTIME_TICK_STALE_MS`]: a task that
    /// wakes on a timer and stores two atomics was not scheduled on **any** of
    /// the runtime's `runtime_workers` worker threads for at least as long as
    /// the probe waited for a byte. Nothing short of a fully wedged runtime
    /// produces this.
    Stalled { age_ms: u64 },
    /// The beacon never ticked. Either [`spawn_runtime_liveness_beacon`] was
    /// never called, or it was called and the runtime never once polled it.
    /// These are not distinguishable from the counters, so this deliberately
    /// concludes nothing rather than guessing `Stalled`.
    Unknown,
}

impl RuntimeLiveness {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Scheduling { .. } => "scheduling",
            Self::Stalled { .. } => "stalled",
            Self::Unknown => "unknown",
        }
    }
}

fn age_since(stored: u64, now: u64) -> Option<u64> {
    // `0` is the "never happened" sentinel; stored values are `mono_ms() + 1`.
    let at = stored.checked_sub(1)?;
    Some(now.saturating_sub(at))
}

pub(crate) fn snapshot() -> Breadcrumbs {
    let now = mono_ms();
    Breadcrumbs {
        db_in_flight: DB_PROBES_IN_FLIGHT.load(Ordering::Relaxed),
        db_probes_started: DB_PROBES_STARTED.load(Ordering::Relaxed),
        db_probes_failed: DB_PROBES_FAILED.load(Ordering::Relaxed),
        last_db_ok_age_ms: age_since(LAST_DB_OK_AT.load(Ordering::Relaxed), now),
        last_db_err_age_ms: age_since(LAST_DB_ERR_AT.load(Ordering::Relaxed), now),
        runtime_ticks: RUNTIME_TICKS.load(Ordering::Relaxed),
        runtime_tick_age_ms: age_since(LAST_RUNTIME_TICK_AT.load(Ordering::Relaxed), now),
        runtime_workers: RUNTIME_WORKERS.load(Ordering::Relaxed),
    }
}

impl Breadcrumbs {
    /// Classifies the beacon. See [`RuntimeLiveness`].
    pub(crate) fn runtime_liveness(&self) -> RuntimeLiveness {
        match self.runtime_tick_age_ms {
            None => RuntimeLiveness::Unknown,
            Some(age_ms) if age_ms <= RUNTIME_TICK_STALE_MS => {
                RuntimeLiveness::Scheduling { age_ms }
            }
            Some(age_ms) => RuntimeLiveness::Stalled { age_ms },
        }
    }

    /// Renders as `key=value` pairs. `tracing` writes string fields unquoted,
    /// so this stays greppable without post-processing.
    pub(crate) fn render(&self) -> String {
        fn age(value: Option<u64>) -> String {
            value.map_or_else(|| "never".to_string(), |ms| ms.to_string())
        }
        format!(
            "db_in_flight={} db_probes_started={} db_probes_failed={} last_db_ok_age_ms={} last_db_err_age_ms={} runtime={} runtime_workers={} runtime_ticks={} runtime_tick_age_ms={}",
            self.db_in_flight,
            self.db_probes_started,
            self.db_probes_failed,
            age(self.last_db_ok_age_ms),
            age(self.last_db_err_age_ms),
            self.runtime_liveness().label(),
            // Rendered next to `runtime=` on purpose: `scheduling` alone means
            // "one worker was free", and this is the denominator that makes
            // that readable. `0` == the beacon never started.
            self.runtime_workers,
            self.runtime_ticks,
            age(self.runtime_tick_age_ms),
        )
    }
}

/// The breadcrumb counters are process-global — that is the whole point of the
/// design, since the watchdog thread has to read them without holding anything
/// the runtime could be blocked on. `cargo test` runs tests in parallel, so
/// every test that asserts on a *delta* must first take this lock; otherwise a
/// sibling test's guard moves the gauge mid-assertion.
///
/// Lives outside `mod tests` because `services::health_diagnostics` asserts on
/// the same counters and has to share the lock, not a copy of it.
#[cfg(test)]
pub(crate) fn counter_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static COUNTER_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    COUNTER_TESTS.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::MutexGuard;
    use std::time::Duration;

    fn exclusive() -> MutexGuard<'static, ()> {
        counter_test_lock()
    }

    #[test]
    fn guard_records_success_and_releases_in_flight() {
        let _serial = exclusive();
        let before = snapshot();
        let guard = DbProbeGuard::new();
        let during = snapshot();
        assert_eq!(
            during.db_in_flight,
            before.db_in_flight + 1,
            "an unresolved probe must be visible as in-flight"
        );
        assert_eq!(during.db_probes_started, before.db_probes_started + 1);

        guard.finish(true);
        let after = snapshot();
        assert_eq!(
            after.db_in_flight, before.db_in_flight,
            "finishing must release the in-flight slot"
        );
        assert_eq!(
            after.db_probes_failed, before.db_probes_failed,
            "a successful probe must not count as a failure"
        );
        assert!(
            after.last_db_ok_age_ms.is_some(),
            "a successful probe must leave a success timestamp"
        );
    }

    #[test]
    fn guard_records_failure() {
        let _serial = exclusive();
        let before = snapshot();
        DbProbeGuard::new().finish(false);
        let after = snapshot();
        assert_eq!(after.db_in_flight, before.db_in_flight);
        assert_eq!(
            after.db_probes_failed,
            before.db_probes_failed + 1,
            "a failed probe must be counted"
        );
        assert!(after.last_db_err_age_ms.is_some());
    }

    #[test]
    fn dropping_an_unfinished_guard_still_releases_the_slot() {
        // Models a cancelled `/api/health` request: the future is dropped
        // mid-query. Without `Drop` the in-flight gauge would ratchet up
        // forever and the kill-time breadcrumb would be pure fiction.
        let _serial = exclusive();
        let before = snapshot();
        drop(DbProbeGuard::new());
        let after = snapshot();
        assert_eq!(
            after.db_in_flight, before.db_in_flight,
            "a cancelled probe must not leak an in-flight slot"
        );
        assert_eq!(
            after.db_probes_started,
            before.db_probes_started + 1,
            "a cancelled probe must still be counted as started"
        );
    }

    /// Base value for tests that vary one field. Nothing reads the constants
    /// themselves; they exist so an assertion failure names the field it means.
    fn crumbs() -> Breadcrumbs {
        Breadcrumbs {
            db_in_flight: 0,
            db_probes_started: 9,
            db_probes_failed: 3,
            last_db_ok_age_ms: Some(10),
            last_db_err_age_ms: Some(1234),
            runtime_ticks: 500,
            runtime_tick_age_ms: Some(120),
            runtime_workers: 14,
        }
    }

    #[test]
    fn never_observed_ages_render_as_never() {
        let crumbs = Breadcrumbs {
            db_in_flight: 2,
            last_db_ok_age_ms: None,
            runtime_ticks: 0,
            runtime_tick_age_ms: None,
            ..crumbs()
        };
        let rendered = crumbs.render();
        assert!(rendered.contains("db_in_flight=2"), "{rendered}");
        assert!(rendered.contains("db_probes_started=9"), "{rendered}");
        assert!(rendered.contains("db_probes_failed=3"), "{rendered}");
        assert!(rendered.contains("last_db_ok_age_ms=never"), "{rendered}");
        assert!(rendered.contains("last_db_err_age_ms=1234"), "{rendered}");
        assert!(rendered.contains("runtime_ticks=0"), "{rendered}");
        assert!(rendered.contains("runtime_tick_age_ms=never"), "{rendered}");
        assert!(
            rendered.contains("runtime=unknown"),
            "a beacon that never ticked must not render as stalled: {rendered}"
        );
    }

    #[test]
    fn a_live_beacon_renders_its_age_and_label() {
        let rendered = crumbs().render();
        assert!(rendered.contains("runtime=scheduling"), "{rendered}");
        assert!(rendered.contains("runtime_ticks=500"), "{rendered}");
        assert!(rendered.contains("runtime_tick_age_ms=120"), "{rendered}");
        assert!(
            rendered.contains("runtime_workers=14"),
            "`runtime=scheduling` only says ONE worker was free; without the \
             worker count beside it a reader cannot tell whether that leaves 13 \
             workers unaccounted for: {rendered}"
        );
    }

    /// The worker count has to reach the log line, and `0` in production would
    /// silently mean "we never asked". Pin that the beacon records the runtime
    /// it was started on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn the_beacon_records_the_runtime_worker_count() {
        let _serial = exclusive();
        spawn_runtime_liveness_beacon();
        let after = snapshot();
        assert_eq!(
            after.runtime_workers, 3,
            "the beacon must record the worker count of the runtime it was \
             spawned on, or `runtime=scheduling` is unreadable"
        );
        assert!(
            after.render().contains("runtime_workers=3"),
            "{}",
            after.render()
        );
    }

    #[test]
    fn age_sentinel_treats_zero_as_never() {
        assert_eq!(age_since(0, 500), None, "0 is the never-happened sentinel");
        // Stored values are `mono_ms() + 1`, so a probe at t=0 stores 1.
        assert_eq!(age_since(1, 500), Some(500));
    }

    #[test]
    fn recording_a_tick_advances_the_beacon() {
        let _serial = exclusive();
        let before = snapshot();
        record_runtime_tick();
        let after = snapshot();
        assert_eq!(
            after.runtime_ticks,
            before.runtime_ticks + 1,
            "every tick must be counted"
        );
        let age = after
            .runtime_tick_age_ms
            .expect("a tick must leave a timestamp");
        assert!(
            age <= 5_000,
            "a tick recorded just now must read as scheduling, got {age}ms"
        );
        assert!(matches!(
            after.runtime_liveness(),
            RuntimeLiveness::Scheduling { .. }
        ));
    }

    /// The beacon is only a discriminator if something starts it, and nothing
    /// else fails when the call is dropped — `verdict` degrades to
    /// `undetermined_no_beacon`, quietly and forever.
    ///
    /// Pinning that at a *boot site* would mean `include_str!` on
    /// `cli/dcserver.rs` plus `contains("spawn_runtime_liveness_beacon()")`,
    /// and `str::contains` is comment-blind — `// spawn_runtime_liveness_beacon();`
    /// satisfies it. So the coupling is a data dependency instead:
    /// `spawn_watchdog` arms the beacon itself and feeds the resulting
    /// [`BeaconArmed`] into the thread-spawning function, so there is no
    /// separate call left to delete and no way to reorder the two. What remains
    /// testable is the arming function's own contract, which is what these two
    /// pin.
    #[tokio::test]
    async fn arming_the_beacon_inside_a_runtime_reports_the_worker_count() {
        let _serial = exclusive();
        let armed = spawn_runtime_liveness_beacon();
        let workers = armed
            .workers()
            .expect("on a runtime the beacon must arm and see the worker count");
        assert!(
            workers > 0,
            "a runtime has at least one worker, got {workers}"
        );
        assert_eq!(
            snapshot().runtime_workers,
            workers,
            "arming must publish the same count the breadcrumbs render; \
             `runtime_workers=0` is the signal that reads as `the beacon never started`"
        );
        let report = armed
            .boot_report()
            .expect("an armed beacon reports success");
        assert!(report.contains("armed"), "{report}");
    }

    /// Off a runtime it must degrade, not panic. `spawn_watchdog` calls this
    /// unconditionally, and a panic there would turn a missing breadcrumb into
    /// a failed boot.
    #[test]
    fn arming_the_beacon_off_a_runtime_is_reported_not_fatal() {
        let armed = spawn_runtime_liveness_beacon();
        assert_eq!(
            armed.workers(),
            None,
            "without a runtime handle there is nothing to spawn onto"
        );
        let report = armed
            .boot_report()
            .expect_err("a beacon that did not arm must report an error, not a success");
        assert!(
            report.contains("NOT armed") && report.contains("undetermined_no_beacon"),
            "the failure line has to name the consequence, not just the fact: {report}"
        );
    }
}
