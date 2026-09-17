//! #5147: forensics the self-watchdog records when it decides the runtime is
//! unresponsive.
//!
//! A thread `sample` cannot answer this: a task blocked in `await` (e.g. on
//! Postgres) is invisible to it, and a healthy and a wedged runtime produce
//! the same thread shapes. This module records three facts a sample cannot:
//!
//! 1. Which stage the probe reached — [`HealthProbeOutcome`]. `ConnectFailed`
//!    does *not* mean the runtime is innocent: the kernel completes a TCP
//!    handshake from the listen backlog (128 slots) with no accept-loop
//!    participation, so a wedged acceptor reads as `NoResponse` until the
//!    backlog fills — [`verdict`] always consults the beacon for this stage.
//! 2. Whether the runtime is scheduling tasks — [`RuntimeLiveness`], driven
//!    by [`spawn_runtime_liveness_beacon`]. One idle worker is enough to tick
//!    it, so `Scheduling` rules out a wedged runtime but not partial executor
//!    starvation; `runtime_workers=` is logged alongside it.
//! 3. What Postgres was doing — [`Breadcrumbs::db_in_flight`], set by every
//!    [`DbProbeGuard`] around a `GET /api/health` query (sites enumerated in
//!    `services::health_diagnostics`). [`verdict`] combines all three; the
//!    log-evidence analysis behind this design lives in #5147.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Monotonic epoch for every timestamp in this module. `Instant` cannot live
/// in an atomic, so timestamps are stored as milliseconds since this point.
/// Monotonic rather than `SystemTime` so a wall-clock adjustment cannot
/// produce a nonsense age. This is first-use, not process start, but every
/// consumer only ever takes a difference against this same epoch, so that
/// distinction never reaches a log line — do not build an uptime field on it.
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
/// [`spawn_runtime_liveness_beacon`]. `0` means the beacon never started.
/// Logged because `runtime=scheduling` only says *one* worker was free; this
/// is the denominator that makes that readable. See [`RuntimeLiveness`].
static RUNTIME_WORKERS: AtomicU64 = AtomicU64::new(0);

/// How often [`spawn_runtime_liveness_beacon`] proves the runtime is alive.
/// Short enough that a stalled tick unambiguously spans the watchdog's 5s
/// probe timeout, long enough to be a rounding error next to the 30s check.
pub(crate) const RUNTIME_TICK_PERIOD: std::time::Duration = std::time::Duration::from_secs(1);

/// A tick older than this means the runtime is not scheduling tasks. Five
/// periods, chosen to equal the probe's own read timeout
/// (`recovery::spawn_watchdog`'s `TCP_TIMEOUT`, 5s) so crossing it means the
/// runtime failed to run one trivial task for as long as the probe waited for
/// a byte. Both this and `RUNTIME_TICK_PERIOD` are pinned as absolute
/// literals in `tests` — an oracle relative to either constant would move
/// with it and never fail.
pub(crate) const RUNTIME_TICK_STALE_MS: u64 = 5_000;

/// Brackets one Postgres health probe.
///
/// Created before the query is issued and resolved by [`DbProbeGuard::finish`].
/// If instead the future is *cancelled* mid-query, `Drop` still decrements the
/// in-flight counter — otherwise a cancelled probe would inflate
/// `db_in_flight` forever.
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
/// Takes the future itself, rather than leaving each call site to open-code
/// `new()`/`finish()`, so an unbracketed await cannot go unrecorded and clear
/// the database of a stall it caused.
///
/// `succeeded` decides what counts as a healthy round trip — for `sqlx`,
/// `Result::is_ok`, since a query matching no rows still proves Postgres
/// answered. Cancellation-safe: a dropped future still releases the slot via
/// `guard`'s `Drop`.
pub(crate) async fn observe_db<T>(
    query: impl std::future::Future<Output = T>,
    succeeded: impl FnOnce(&T) -> bool,
) -> T {
    let guard = DbProbeGuard::new();
    let outcome = query.await;
    guard.finish(succeeded(&outcome));
    outcome
}

/// A `PgPool` whose only *ordinary* use is an await under a [`DbProbeGuard`].
///
/// #5147: `services::health_diagnostics` wraps its `Option<&PgPool>` in one of
/// these at the top of every `GET /api/health` function, shadowing the raw
/// pool: with no `&PgPool` binding left, an unbracketed
/// `.fetch_one`/`.fetch_optional`/`.fetch_all` stops compiling instead of
/// going unrecorded.
///
/// **Not a capability boundary.** [`probe`](ProbedPool::probe) hands the raw
/// `&'p PgPool` to its closure with no bound on the return type, so
/// `|p| ready(p.clone())` extracts an owned pool that can be awaited with no
/// guard — only a future *built* by the closure is bracketed.
///
/// **Does not cover:** a new sibling function taking `Option<&PgPool>`
/// instead of wrapping it (nothing forces the parameter type —
/// `assert_bracketed!` only pins the already-converted ones); a closure that
/// awaits twice (records one probe for two round trips); anything outside
/// this module (`server::routes::health_api` is the obvious gap); or the
/// extracted-handle path above, which the foreign `fn(&PgPool)` signature in
/// `auto_queue::cleanup_tasks` rules out closing.
pub(crate) struct ProbedPool<'p> {
    pool: &'p sqlx::PgPool,
}

impl<'p> ProbedPool<'p> {
    /// Wraps a health-path pool. Returns `None` for `None` so callers keep
    /// their existing "no pool, no work" early return.
    pub(crate) fn wrap(pool: Option<&'p sqlx::PgPool>) -> Option<Self> {
        pool.map(|pool| Self { pool })
    }

    /// Runs one query under a [`DbProbeGuard`].
    ///
    /// `build` receives the raw handle so `sqlx`'s builders can bind to it, and
    /// the future it returns is awaited *inside* the bracket — `build` is not
    /// `async`, so it cannot await out from under the guard. It **can** return
    /// the handle, though: `T` is unbounded. See [`ProbedPool`] for why that is
    /// declared debt rather than a hole this closes.
    pub(crate) async fn probe<T, Fut>(
        &self,
        build: impl FnOnce(&'p sqlx::PgPool) -> Fut,
        succeeded: impl FnOnce(&T) -> bool,
    ) -> T
    where
        Fut: std::future::Future<Output = T>,
    {
        observe_db(build(self.pool), succeeded).await
    }
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
/// The task awaits a timer and stores two atomics — touching no lock,
/// channel, socket, or database — so only the runtime itself failing to poll
/// a ready task can stop it. See [`RuntimeLiveness`].
///
/// Single production caller:
/// [`spawn_watchdog`](crate::services::discord::health::self_watchdog::spawn_watchdog),
/// which arms the beacon before creating its thread so the two steps cannot
/// be reordered or split apart.
///
/// Returns [`BeaconArmed`], the token `spawn_watchdog` needs to create its
/// thread. Off a runtime this reports failure through that token instead of
/// panicking: a missing beacon costs `verdict=undetermined_no_beacon`; a
/// panic at boot costs the whole service.
pub(crate) fn spawn_runtime_liveness_beacon() -> BeaconArmed {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return BeaconArmed { workers: None };
    };
    // Recorded here, not at the read side: the watchdog reads from its own OS
    // thread, where no runtime handle is available.
    let workers = handle.metrics().num_workers() as u64;
    RUNTIME_WORKERS.store(workers, Ordering::Relaxed);
    handle.spawn(async {
        let mut interval = tokio::time::interval(RUNTIME_TICK_PERIOD);
        // `Delay` (not the default `Burst`) so a stall doesn't replay every
        // missed tick in a burst once the runtime recovers.
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
/// #5147: makes the *order* of the two boot steps a data dependency rather
/// than a convention — `spawn_watchdog_thread` takes one by value, and the
/// only way to obtain one is to call the arming function, so deleting the
/// arming (or moving it into the spawned closure) stops compiling.
///
/// Scoped narrowly: `Copy` (one token can start two threads), says nothing
/// about other ways to start a thread (`std::thread::spawn` needs no token),
/// and is issued on the failure path too (`workers: None`) — it proves arming
/// was *attempted*, not that it succeeded. [`BeaconArmed::boot_report`]
/// distinguishes those, logged at ERROR by `spawn_watchdog_thread`.
///
/// Deliberately no public constructor and no `Default`.
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

    /// The line the boot path must emit, and at which level. Returned rather
    /// than logged here so the text is assertable without a tracing
    /// subscriber; `Err` means every later watchdog failure will report
    /// `verdict=undetermined_no_beacon`.
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
    /// Health-path Postgres awaits outstanding right now, **process-wide**
    /// rather than per-request — deliberately, so the watchdog's OS thread can
    /// read it without touching anything the runtime could be blocked on. A
    /// non-zero value proves *some* health request is inside a Postgres await,
    /// not that the request this probe made is; `handler_blocked_on_db`
    /// inherits that weakness.
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
    /// worker thread ran a ready timer task. One free worker is enough to tick
    /// a beacon that only stores two atomics, so this:
    ///
    /// * **Excludes** a fully wedged runtime — no worker polling anything.
    /// * **Does not exclude** partial executor starvation: N-1 of N workers
    ///   blocked in sync I/O or `block_in_place` still tick the beacon and
    ///   still produce a `handler_*` verdict, while the actual fault is the
    ///   executor. Compare `runtime_workers=` against known concurrent load
    ///   before believing a `handler_*` verdict.
    Scheduling { age_ms: u64 },
    /// The beacon has not ticked for [`RUNTIME_TICK_STALE_MS`]: not **any** of
    /// the runtime's worker threads ran the beacon task for at least as long
    /// as the probe waited for a byte. Nothing short of a fully wedged runtime
    /// produces this.
    Stalled { age_ms: u64 },
    /// The beacon never ticked — either [`spawn_runtime_liveness_beacon`] was
    /// never called, or the runtime never once polled it. Not distinguishable
    /// from the counters, so this deliberately concludes nothing rather than
    /// guessing `Stalled`.
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

/// The one conclusion the watchdog is entitled to draw, from the probe stage
/// and the beacon together — neither alone is enough: `stage=` cannot see a
/// wedged runtime (the backlog answers `connect()` without the acceptor), and
/// the beacon cannot see a dead listener or a stuck handler. `db_in_flight`
/// splits the remainder but is process-wide (see
/// [`Breadcrumbs::db_in_flight`]), so `handler_blocked_on_db` names *a* health
/// request stuck in a query, not necessarily this one.
///
/// | verdict                           | means                                                         |
/// |-----------------------------------|---------------------------------------------------------------|
/// | `responsive`                      | the probe got bytes back; the watchdog prints no failure line  |
/// | `runtime_stalled`                 | no worker ran a timer task for 5s — a wedged runtime           |
/// | `listener_gone`                   | runtime scheduling, yet the handshake did not complete         |
/// | `handler_blocked_on_db`           | runtime scheduling, *a* health request is inside a query       |
/// | `handler_slow_db_idle`            | runtime scheduling, no health query outstanding                |
/// | `connection_reset_before_request` | connected, then the write failed — handler never reached; read `err=`, this arm also absorbs a write timeout |
/// | `undetermined_no_beacon`          | the beacon never ran; nothing may be concluded                 |
///
/// Pinned exhaustively, stage-first with no `_` arm, by
/// `tests::every_stage_and_liveness_combination_has_a_pinned_verdict` — adding
/// a stage is a compile error here, not a silent DB verdict.
pub(crate) fn verdict(outcome: &HealthProbeOutcome, crumbs: &Breadcrumbs) -> &'static str {
    use HealthProbeOutcome as Stage;
    match outcome {
        Stage::Responded { .. } => "responsive",
        // Beacon-independent: the connection was established, so the write
        // was refused by the peer, not by a wedged runtime (an accepted
        // socket stays writable in the kernel regardless of the executor).
        // Exception: this variant also absorbs a write *timeout*, which is
        // not a peer reset — read `err=` before trusting this verdict.
        Stage::RequestFailed { .. } => "connection_reset_before_request",
        // `ConnectFailed` does NOT settle this alone: a wedged acceptor
        // eventually fills the 128-slot backlog, after which `connect()`
        // fails exactly like a closed socket. The beacon still decides.
        Stage::ConnectFailed { .. } => match crumbs.runtime_liveness() {
            RuntimeLiveness::Unknown => "undetermined_no_beacon",
            RuntimeLiveness::Stalled { .. } => "runtime_stalled",
            RuntimeLiveness::Scheduling { .. } => "listener_gone",
        },
        Stage::NoResponse { .. } => match crumbs.runtime_liveness() {
            RuntimeLiveness::Unknown => "undetermined_no_beacon",
            RuntimeLiveness::Stalled { .. } => "runtime_stalled",
            RuntimeLiveness::Scheduling { .. } if crumbs.db_in_flight > 0 => {
                "handler_blocked_on_db"
            }
            RuntimeLiveness::Scheduling { .. } => "handler_slow_db_idle",
        },
    }
}

/// Which stage of the watchdog's loopback probe the check reached.
///
/// This records *how far the probe got* and nothing more. It is a necessary
/// input to a conclusion, not a conclusion — pair it with [`RuntimeLiveness`]
/// via [`verdict`] before deciding anything about the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HealthProbeOutcome {
    /// The server answered. Any HTTP status counts — a 503 still proves the
    /// runtime is scheduling tasks, and killing on 503 would crash-loop the
    /// process whenever a provider is briefly disconnected.
    Responded { elapsed_ms: u64 },
    /// The TCP handshake never completed: the listening socket is gone, the
    /// backlog is full, or the address is unroutable. Not the first place a
    /// wedged runtime shows up — the kernel completes the handshake from the
    /// backlog with no accept loop running, so this is only reached once the
    /// backlog is also exhausted. [`verdict`] still reads the beacon here.
    ConnectFailed { elapsed_ms: u64, error: String },
    /// The connection was established but the request could not be written.
    /// The handler was never reached, so this must never be classified as a
    /// database or handler problem. Every `write_all` error folds into this
    /// variant, including a write *timeout* — which is not a peer reset, so
    /// `err=` is what tells them apart if it matters.
    RequestFailed { elapsed_ms: u64, error: String },
    /// The connection was established and the request written, but no bytes
    /// came back before the read timeout. Holds **two different failures**
    /// and cannot separate them on its own: a wedged runtime (accepted by the
    /// backlog, never polled) and a live runtime waiting on Postgres.
    /// [`RuntimeLiveness`] tells them apart.
    NoResponse { elapsed_ms: u64, error: String },
}

impl HealthProbeOutcome {
    pub(crate) fn is_ok(&self) -> bool {
        matches!(self, Self::Responded { .. })
    }

    pub(crate) fn stage(&self) -> &'static str {
        match self {
            Self::Responded { .. } => "responded",
            Self::ConnectFailed { .. } => "connect_failed",
            Self::RequestFailed { .. } => "request_failed",
            Self::NoResponse { .. } => "no_response",
        }
    }

    pub(crate) fn elapsed_ms(&self) -> u64 {
        match self {
            Self::Responded { elapsed_ms }
            | Self::ConnectFailed { elapsed_ms, .. }
            | Self::RequestFailed { elapsed_ms, .. }
            | Self::NoResponse { elapsed_ms, .. } => *elapsed_ms,
        }
    }

    fn error(&self) -> &str {
        match self {
            Self::Responded { .. } => "-",
            Self::ConnectFailed { error, .. }
            | Self::RequestFailed { error, .. }
            | Self::NoResponse { error, .. } => error.as_str(),
        }
    }

    pub(crate) fn render(&self) -> String {
        format!(
            "stage={} elapsed_ms={} err={}",
            self.stage(),
            self.elapsed_ms(),
            self.error()
        )
    }
}

/// Runs one loopback `GET /api/health` and classifies where it got to.
///
/// Deliberately synchronous and dependency-free: it runs on the watchdog's own
/// OS thread so that it keeps working when every tokio worker is blocked.
pub(crate) fn probe_health_once(
    addr: &str,
    host: &str,
    timeout: std::time::Duration,
) -> HealthProbeOutcome {
    use std::io::{Read, Write};

    let started = Instant::now();
    let elapsed_ms = |started: &Instant| started.elapsed().as_millis() as u64;

    let socket_addr = match addr.parse() {
        Ok(parsed) => parsed,
        Err(e) => {
            return HealthProbeOutcome::ConnectFailed {
                elapsed_ms: elapsed_ms(&started),
                error: format!("bad addr {addr}: {e}"),
            };
        }
    };
    let mut stream = match std::net::TcpStream::connect_timeout(&socket_addr, timeout) {
        Ok(stream) => stream,
        Err(e) => {
            return HealthProbeOutcome::ConnectFailed {
                elapsed_ms: elapsed_ms(&started),
                error: e.to_string(),
            };
        }
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    let request = format!("GET /api/health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    if let Err(e) = stream.write_all(request.as_bytes()) {
        return HealthProbeOutcome::RequestFailed {
            elapsed_ms: elapsed_ms(&started),
            error: e.to_string(),
        };
    }

    let mut buf = [0u8; 512];
    match stream.read(&mut buf) {
        Ok(n) if n > 0 => HealthProbeOutcome::Responded {
            elapsed_ms: elapsed_ms(&started),
        },
        Ok(_) => HealthProbeOutcome::NoResponse {
            elapsed_ms: elapsed_ms(&started),
            error: "peer closed without responding".to_string(),
        },
        Err(e) => HealthProbeOutcome::NoResponse {
            elapsed_ms: elapsed_ms(&started),
            error: e.to_string(),
        },
    }
}

/// The breadcrumb counters are process-global, so the watchdog thread can
/// read them without holding anything the runtime could be blocked on. Tests
/// run in parallel, so any test asserting on a *delta* must take this lock
/// first, or a sibling test's guard moves the gauge mid-assertion.
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

    /// A failed probe that reached the read timeout. Shared so the verdict
    /// tests below vary only the field under test.
    fn no_response() -> HealthProbeOutcome {
        HealthProbeOutcome::NoResponse {
            elapsed_ms: 5_000,
            error: "timed out".to_string(),
        }
    }

    /// Upper bound on beacon staleness at kill time, measured **from the last
    /// successful check** — three `CHECK_INTERVAL`s (30s), not the length of
    /// the failure streak (three failures 30s apart span two intervals, not
    /// three). A literal, deliberately — see
    /// [`the_beacon_constants_are_pinned_in_absolute_units`].
    const AGE_AT_KILL_MS: u64 = 90_000;

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

    /// `0` in production would silently mean "we never asked"; pin that the
    /// beacon records the worker count of the runtime it was started on.
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

    // ── The discriminating test ────────────────────────────────────────────
    // These reproduce the two failure modes the production watchdog cannot
    // currently tell apart.

    #[test]
    fn accepted_but_silent_server_is_classified_as_no_response() {
        // Production shape: connection accepted, handler blocked on Postgres,
        // nothing written back before the read timeout.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let accepted = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            std::thread::sleep(Duration::from_millis(600));
            drop(stream);
        });

        let outcome = probe_health_once(&addr, "127.0.0.1", Duration::from_millis(150));
        accepted.join().expect("listener thread");

        assert_eq!(outcome.stage(), "no_response", "got {outcome:?}");
        assert!(!outcome.is_ok());
        assert!(
            !outcome.render().contains("err=-"),
            "a failure must carry the underlying error: {}",
            outcome.render()
        );
    }

    #[test]
    fn dead_port_is_classified_as_connect_failed() {
        // Bind then drop, so the port is almost certainly unused.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        drop(listener);

        let outcome = probe_health_once(&addr, "127.0.0.1", Duration::from_millis(150));

        assert_eq!(outcome.stage(), "connect_failed", "got {outcome:?}");
        assert!(!outcome.is_ok());
    }

    #[test]
    fn responding_server_is_classified_as_responded() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let responder = std::thread::spawn(move || {
            use std::io::Write;
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\n\r\n");
            let _ = stream.flush();
        });

        let outcome = probe_health_once(&addr, "127.0.0.1", Duration::from_secs(2));
        responder.join().expect("responder thread");

        assert_eq!(outcome.stage(), "responded", "got {outcome:?}");
        assert!(
            outcome.is_ok(),
            "a 503 still proves the runtime is scheduling tasks"
        );
    }

    #[test]
    fn unparseable_address_is_connect_failed_not_a_panic() {
        let outcome = probe_health_once("not-an-addr", "h", Duration::from_millis(50));
        assert_eq!(outcome.stage(), "connect_failed", "got {outcome:?}");
    }

    /// A live `RequestFailed` needs the peer to vanish between `connect` and
    /// `write`, which is a race, so pin the whole table by construction
    /// instead of reproducing it live.
    #[test]
    fn every_stage_reports_its_own_label_elapsed_error_and_verdict_input() {
        let cases = [
            (
                HealthProbeOutcome::Responded { elapsed_ms: 7 },
                "responded",
                7u64,
                "-",
                true,
            ),
            (
                HealthProbeOutcome::ConnectFailed {
                    elapsed_ms: 11,
                    error: "connection refused".to_string(),
                },
                "connect_failed",
                11,
                "connection refused",
                false,
            ),
            (
                HealthProbeOutcome::RequestFailed {
                    elapsed_ms: 13,
                    error: "broken pipe".to_string(),
                },
                "request_failed",
                13,
                "broken pipe",
                false,
            ),
            (
                HealthProbeOutcome::NoResponse {
                    elapsed_ms: 17,
                    error: "timed out".to_string(),
                },
                "no_response",
                17,
                "timed out",
                false,
            ),
        ];

        for (outcome, stage, elapsed_ms, error, is_ok) in cases {
            assert_eq!(outcome.stage(), stage, "{outcome:?}");
            assert_eq!(outcome.elapsed_ms(), elapsed_ms, "{outcome:?}");
            assert_eq!(outcome.error(), error, "{outcome:?}");
            assert_eq!(
                outcome.is_ok(),
                is_ok,
                "only a response may count as healthy: {outcome:?}"
            );
            assert_eq!(
                outcome.render(),
                format!("stage={stage} elapsed_ms={elapsed_ms} err={error}"),
                "{outcome:?}"
            );
        }
    }

    /// `read` returning `Ok(0)` means the peer completed the handshake, took
    /// the request, and closed without answering — a failure, not to be
    /// mistaken for the healthy `Ok(n > 0)` path. The server drains the
    /// request before shutting down: closing with unread bytes buffered would
    /// make the kernel send RST, taking the `Err` arm instead of `Ok(0)`.
    #[test]
    fn a_peer_that_takes_the_request_and_closes_without_answering_is_a_failure() {
        use std::io::Read;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let closer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = stream.shutdown(std::net::Shutdown::Write);
            std::thread::sleep(Duration::from_millis(200));
        });

        let outcome = probe_health_once(&addr, "127.0.0.1", Duration::from_secs(2));
        closer.join().expect("closer thread");

        assert!(
            !outcome.is_ok(),
            "a peer that answered nothing is not healthy: {outcome:?}"
        );
        assert_eq!(outcome.stage(), "no_response", "got {outcome:?}");
        assert!(
            outcome.render().contains("peer closed without responding"),
            "the clean-EOF case must be distinguishable from a read timeout: {}",
            outcome.render()
        );
    }

    // ── The discriminating test ────────────────────────────────────────────
    // `stage=` alone cannot tell a wedged runtime from a slow handler. These
    // reproduce why, and pin the field that can.

    /// A listener whose `accept` is never called still completes the TCP
    /// handshake, because the kernel does it from the listen backlog — the
    /// shape of a runtime that stopped polling its acceptor, classifying as
    /// `no_response`, not `connect_failed`. If this ever returns
    /// `connect_failed`, the module docs and [`verdict`] are both wrong.
    #[test]
    fn a_never_accepted_connection_is_no_response_not_connect_failed() {
        // Held for the whole probe so the socket stays open and the backlog
        // stays available.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();

        let outcome = probe_health_once(&addr, "127.0.0.1", Duration::from_millis(200));
        drop(listener);

        assert_eq!(
            outcome.stage(),
            "no_response",
            "the kernel completes the handshake from the backlog with no accept, \
             so a wedged acceptor cannot surface as connect_failed: {outcome:?}"
        );
        assert!(!outcome.is_ok());
    }

    /// The pair above is exactly why `stage=` is not read on its own. Same
    /// stage, same elapsed, opposite root cause — split only by the beacon.
    #[test]
    fn the_beacon_splits_a_wedged_runtime_from_a_blocked_handler() {
        let no_response = no_response();

        let wedged = Breadcrumbs {
            // Absolute, not `RUNTIME_TICK_STALE_MS + 1`: an expectation
            // phrased relative to the constant under test moves with it.
            runtime_tick_age_ms: Some(AGE_AT_KILL_MS),
            db_in_flight: 0,
            ..crumbs()
        };
        let blocked_on_db = Breadcrumbs {
            runtime_tick_age_ms: Some(200),
            db_in_flight: 1,
            ..crumbs()
        };

        assert_eq!(verdict(&no_response, &wedged), "runtime_stalled");
        assert_eq!(
            verdict(&no_response, &blocked_on_db),
            "handler_blocked_on_db"
        );
        assert_ne!(
            verdict(&no_response, &wedged),
            verdict(&no_response, &blocked_on_db),
            "the two failures share a stage, so the verdict is the only thing \
             that can tell an investigator which one happened"
        );
    }

    // ── The beacon's two numbers, pinned absolutely ───────────────────────
    // #5177 defect class: an oracle computed from the constant it checks
    // (`RUNTIME_TICK_STALE_MS + 1`, `RUNTIME_TICK_PERIOD * 3`) moves with a
    // mutation and never fails. Nothing below may name a constant on the
    // expectation side — literals only.

    /// The literals themselves, plus the "Five periods" relation between
    /// them — pinned separately since the relation alone is satisfiable by
    /// any pair in a 1:5 ratio.
    #[test]
    fn the_beacon_constants_are_pinned_in_absolute_units() {
        assert_eq!(
            RUNTIME_TICK_PERIOD,
            Duration::from_millis(1_000),
            "the beacon must tick once a second. Longer and a single tick \
             cannot resolve the runtime's state inside the probe's 5s window; \
             this is not free to drift with whatever the constant happens to say"
        );
        assert_eq!(
            RUNTIME_TICK_STALE_MS, 5_000,
            "the stale threshold must be 5000ms. Raising it silently makes \
             `runtime_stalled` unreachable: the watchdog exits three 30s \
             intervals after the last successful check, so a threshold above \
             90000ms can never be crossed while the process is still alive"
        );
        assert_eq!(
            RUNTIME_TICK_STALE_MS,
            5 * RUNTIME_TICK_PERIOD.as_millis() as u64,
            "the docs call the threshold `Five periods`; if that stops being \
             true, one of the two literals above moved and the comment lies"
        );
    }

    /// The threshold's *other* documented relation: it equals the watchdog's
    /// own read timeout, so crossing it means the runtime failed to run one
    /// trivial task for at least as long as the probe waited for a byte.
    /// Every expectation here is a literal on both sides — `assert_eq!(A, B)`
    /// between two constants that move together is the #5177 defect class.
    #[test]
    fn the_stale_threshold_matches_the_watchdogs_own_probe_timeout() {
        use crate::services::discord::health::self_watchdog;

        assert_eq!(
            self_watchdog::TCP_TIMEOUT,
            Duration::from_secs(5),
            "the watchdog waits 5s for a byte. `RUNTIME_TICK_STALE_MS` is that \
             same 5s in milliseconds, so the two must be changed together"
        );
        assert_eq!(
            self_watchdog::CHECK_INTERVAL,
            Duration::from_secs(30),
            "the watchdog probes every 30s; `AGE_AT_KILL_MS` is derived from it"
        );
        assert_eq!(
            self_watchdog::MAX_FAILURES,
            3,
            "three consecutive failures is what force-exits the process"
        );
        assert_eq!(
            RUNTIME_TICK_STALE_MS, 5_000,
            "the threshold is the probe's 5s read timeout expressed in ms"
        );
        assert_eq!(
            AGE_AT_KILL_MS, 90_000,
            "three CHECK_INTERVALs from the last successful check to the exit. \
             NOT the failure-streak length: three failures 30s apart span two \
             intervals, measured at 60.3s / 70.3s in production"
        );
    }

    /// The behavioural consequence, stated in absolute milliseconds: by the
    /// time the watchdog gives up, a runtime that stopped ticking must already
    /// read as stalled. A threshold that production cannot reach before the
    /// process dies is a threshold that classifies nothing.
    #[test]
    fn a_runtime_that_stopped_ticking_reads_as_stalled_before_the_watchdog_kills() {
        for age_ms in [5_001_u64, 10_000, 35_000, 60_000, AGE_AT_KILL_MS] {
            let crumbs = Breadcrumbs {
                runtime_tick_age_ms: Some(age_ms),
                ..crumbs()
            };
            assert_eq!(
                crumbs.runtime_liveness(),
                RuntimeLiveness::Stalled { age_ms },
                "a beacon {age_ms}ms stale is a stalled runtime; the watchdog \
                 kills at {AGE_AT_KILL_MS}ms, so anything the threshold cannot \
                 catch by then it never catches"
            );
            assert_eq!(
                verdict(&no_response(), &crumbs),
                "runtime_stalled",
                "at {age_ms}ms stale"
            );
        }
        // The other side: a threshold mutated *down* must also be caught.
        for age_ms in [0_u64, 1, 999, 1_000, 4_999, 5_000] {
            let crumbs = Breadcrumbs {
                runtime_tick_age_ms: Some(age_ms),
                ..crumbs()
            };
            assert_eq!(
                crumbs.runtime_liveness(),
                RuntimeLiveness::Scheduling { age_ms },
                "a beacon {age_ms}ms old is inside the 5000ms window and the \
                 runtime is demonstrably scheduling"
            );
            assert_ne!(
                verdict(&no_response(), &crumbs),
                "runtime_stalled",
                "at {age_ms}ms stale"
            );
        }
    }

    #[test]
    fn verdict_reports_a_live_runtime_with_a_dead_listener_as_listener_gone() {
        let connect_failed = HealthProbeOutcome::ConnectFailed {
            elapsed_ms: 1,
            error: "connection refused".to_string(),
        };
        assert_eq!(verdict(&connect_failed, &crumbs()), "listener_gone");
    }

    #[test]
    fn verdict_without_a_beacon_concludes_nothing() {
        let failed = HealthProbeOutcome::NoResponse {
            elapsed_ms: 5_000,
            error: "timed out".to_string(),
        };
        let no_beacon = Breadcrumbs {
            runtime_ticks: 0,
            runtime_tick_age_ms: None,
            ..crumbs()
        };
        assert_eq!(
            verdict(&failed, &no_beacon),
            "undetermined_no_beacon",
            "a missing beacon is missing evidence, not evidence of a stall"
        );
    }

    /// #5147: the whole verdict table, every cell — including
    /// `request_failed` (never blamed on the handler or database) and
    /// `connect_failed` with no beacon (stays `undetermined_no_beacon`, since
    /// a wedged acceptor also fails `connect()` once the backlog fills).
    #[test]
    fn every_stage_and_liveness_combination_has_a_pinned_verdict() {
        // (label, breadcrumbs)
        let liveness = [
            (
                "no beacon",
                Breadcrumbs {
                    runtime_ticks: 0,
                    runtime_tick_age_ms: None,
                    db_in_flight: 0,
                    ..crumbs()
                },
            ),
            (
                "beacon stale",
                Breadcrumbs {
                    runtime_tick_age_ms: Some(AGE_AT_KILL_MS),
                    db_in_flight: 0,
                    ..crumbs()
                },
            ),
            (
                "beacon live, db idle",
                Breadcrumbs {
                    runtime_tick_age_ms: Some(120),
                    db_in_flight: 0,
                    ..crumbs()
                },
            ),
            (
                "beacon live, db busy",
                Breadcrumbs {
                    runtime_tick_age_ms: Some(120),
                    db_in_flight: 1,
                    ..crumbs()
                },
            ),
        ];
        let stages = [
            HealthProbeOutcome::Responded { elapsed_ms: 3 },
            HealthProbeOutcome::ConnectFailed {
                elapsed_ms: 1,
                error: "connection refused".to_string(),
            },
            HealthProbeOutcome::RequestFailed {
                elapsed_ms: 2,
                error: "broken pipe".to_string(),
            },
            no_response(),
        ];
        // Rows are stages in the order above; columns are liveness states.
        let expected = [
            ["responsive", "responsive", "responsive", "responsive"],
            [
                "undetermined_no_beacon",
                "runtime_stalled",
                "listener_gone",
                "listener_gone",
            ],
            [
                "connection_reset_before_request",
                "connection_reset_before_request",
                "connection_reset_before_request",
                "connection_reset_before_request",
            ],
            [
                "undetermined_no_beacon",
                "runtime_stalled",
                "handler_slow_db_idle",
                "handler_blocked_on_db",
            ],
        ];

        for (stage, row) in stages.iter().zip(expected) {
            for ((label, crumbs), want) in liveness.iter().zip(row) {
                let got = verdict(stage, crumbs);
                assert_eq!(
                    got,
                    want,
                    "stage={} with {label} must render verdict={want}, got {got}",
                    stage.stage()
                );
                if stage.stage() == "request_failed" {
                    assert!(
                        !got.starts_with("handler_"),
                        "request_failed means the write was refused, so the \
                         handler was never reached — it must never be blamed \
                         on the handler or the database ({label} -> {got})"
                    );
                }
            }
        }

        // Every verdict the watchdog can print, in one place.
        let mut rendered: Vec<&str> = expected.iter().flatten().copied().collect();
        rendered.sort_unstable();
        rendered.dedup();
        assert_eq!(
            rendered,
            [
                "connection_reset_before_request",
                "handler_blocked_on_db",
                "handler_slow_db_idle",
                "listener_gone",
                "responsive",
                "runtime_stalled",
                "undetermined_no_beacon",
            ],
            "the verdict vocabulary is 7 values; anything added must be \
             documented in the module docs and in `spawn_watchdog`"
        );
    }

    #[test]
    fn a_healthy_probe_is_responsive_whatever_the_breadcrumbs_say() {
        let responded = HealthProbeOutcome::Responded { elapsed_ms: 3 };
        let ugly = Breadcrumbs {
            db_in_flight: 8,
            runtime_tick_age_ms: Some(AGE_AT_KILL_MS),
            ..crumbs()
        };
        assert_eq!(verdict(&responded, &ugly), "responsive");
    }

    /// Absolute on both sides. Writing this as `RUNTIME_TICK_STALE_MS` and
    /// `RUNTIME_TICK_STALE_MS + 1` is what let the constant be mutated to
    /// 86_400_000 with the whole file still green.
    #[test]
    fn liveness_threshold_is_inclusive_and_only_then_stalls() {
        let at = Breadcrumbs {
            runtime_tick_age_ms: Some(5_000),
            ..crumbs()
        };
        let past = Breadcrumbs {
            runtime_tick_age_ms: Some(5_001),
            ..crumbs()
        };
        assert_eq!(
            at.runtime_liveness(),
            RuntimeLiveness::Scheduling { age_ms: 5_000 },
            "5000ms is the threshold and the threshold is inclusive"
        );
        assert_eq!(
            past.runtime_liveness(),
            RuntimeLiveness::Stalled { age_ms: 5_001 },
            "one millisecond past 5000ms must stall"
        );
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

    /// The beacon has to survive being spawned on a real runtime, not just be
    /// callable; time is paused so this costs no wall-clock. The window is
    /// **5 000 ms as a literal**, not `RUNTIME_TICK_PERIOD * n` — the property
    /// worth pinning is "several ticks fit inside 5s", not one relative to
    /// whatever the period happens to be.
    #[tokio::test(start_paused = true)]
    async fn the_spawned_beacon_keeps_ticking() {
        let _serial = exclusive();
        let before = snapshot();
        spawn_runtime_liveness_beacon();
        tokio::time::sleep(Duration::from_millis(5_100)).await;
        let after = snapshot();
        assert!(
            after.runtime_ticks >= before.runtime_ticks + 5,
            "the beacon must tick at least 5 times in the 5s the watchdog's \
             probe is willing to wait, otherwise a single missed tick is \
             indistinguishable from a stall: {} -> {}",
            before.runtime_ticks,
            after.runtime_ticks
        );
        // No age assertion: under `start_paused`, every tick lands within a
        // millisecond of real "now", so any age bound would pass vacuously.
        after
            .runtime_tick_age_ms
            .expect("a running beacon must leave a timestamp");
    }

    /// The beacon is only a discriminator if something starts it, and nothing
    /// else fails when the call is dropped — `verdict` degrades to
    /// `undetermined_no_beacon`, quietly and forever. Boot-site coupling is
    /// enforced elsewhere as a data dependency; what remains testable here is
    /// the arming function's own contract.
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

    // ── Build-setting guard ───────────────────────────────────────────────
    // Every field this module records is useless if the dump it accompanies
    // still renders our own frames as bare offsets. That depends entirely on
    // one Cargo setting, so pin it here rather than trusting a comment.

    /// Returns the body of `[profile.release]` from the workspace manifest.
    fn release_profile_section() -> String {
        let manifest = include_str!("../../Cargo.toml");
        let start = manifest
            .find("\n[profile.release]\n")
            .expect("Cargo.toml must define [profile.release]");
        let body = &manifest[start + "\n[profile.release]\n".len()..];
        match body.find("\n[") {
            Some(end) => body[..end].to_string(),
            None => body.to_string(),
        }
    }

    /// Parses rather than searches, truncating the value at a trailing `#`:
    /// without that, `strip = true # keep symbols` yields `true # keep
    /// symbols`, which is `!= "true"`, and
    /// [`release_profile_keeps_the_mach_o_symbol_table`] goes green on a fully
    /// stripped binary. TOML has no `#` inside a bare value, so truncating is
    /// exact for the bool/int and quoted-string keys read here.
    fn release_profile_key(key: &str) -> Option<String> {
        release_profile_section()
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with('#'))
            .find_map(|line| {
                let (name, value) = line.split_once('=')?;
                let value = value.split('#').next().unwrap_or(value);
                (name.trim() == key).then(|| value.trim().to_string())
            })
    }

    #[test]
    fn an_inline_comment_cannot_forge_a_release_profile_value() {
        let section = "strip = true # keep symbols\ndebug = 1\n";
        let value = section
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with('#'))
            .find_map(|line| {
                let (name, value) = line.split_once('=')?;
                let value = value.split('#').next().unwrap_or(value);
                (name.trim() == "strip").then(|| value.trim().to_string())
            });
        assert_eq!(value.as_deref(), Some("true"));
    }

    #[test]
    fn release_profile_keeps_the_mach_o_symbol_table() {
        // #5147: `sample`/`atos` have two independent name sources — the
        // binary's own Mach-O symbol table (LC_SYMTAB) and a UUID-matched
        // .dSYM, which deploy ships fail-open and is ignored on a UUID
        // mismatch. `strip = true` deletes LC_SYMTAB, leaving the .dSYM as a
        // single point of failure; keeping the symbol table is the layer that
        // cannot get separated from the binary.
        let strip = release_profile_key("strip")
            .expect("[profile.release] must state `strip` explicitly, not inherit it");
        assert!(
            strip != "true" && strip != "\"symbols\"",
            "[profile.release] strip = {strip} removes the Mach-O symbol table, leaving \
             a UUID-matched .dSYM as the only way to symbolicate a hang dump. When that \
             dSYM is missing or stale the dump again shows only raw offsets for our own \
             frames. Use `strip = \"debuginfo\"` (drops DWARF from the executable, keeps \
             LC_SYMTAB)."
        );
    }

    #[test]
    fn release_profile_emits_a_dsym_for_line_numbers() {
        // File:line and inlined frames need debug info collected into a
        // .dSYM by `split-debuginfo = "packed"`; without `debug` there is
        // nothing for dsymutil to collect.
        let debug = release_profile_key("debug")
            .expect("[profile.release] must set `debug` so a .dSYM has content");
        assert!(
            debug != "0" && debug != "false" && debug != "\"none\"",
            "[profile.release] debug = {debug} emits no debug info, so the .dSYM \
             would be empty and hang dumps would carry no file:line"
        );
        assert_eq!(
            release_profile_key("split-debuginfo").as_deref(),
            Some("\"packed\""),
            "[profile.release] must set split-debuginfo = \"packed\" so the build \
             produces target/release/agentdesk.dSYM that deploy-release.sh can ship"
        );
    }
}
