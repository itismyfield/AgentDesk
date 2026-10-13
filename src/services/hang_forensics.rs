//! Forensics the self-watchdog logs when the runtime stops answering: facts a
//! thread `sample` cannot show, since a task parked in an `await` is not a
//! running thread. [`verdict`] combines how far the probe got
//! ([`HealthProbeOutcome`]), whether the runtime still schedules tasks
//! ([`RuntimeLiveness`]), and the health-path Postgres awaits outstanding
//! ([`Breadcrumbs::db_in_flight`], sites listed in `services::health_diagnostics`).

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Epoch for the stored ms timestamps (`Instant` cannot live in an atomic). Set on
/// first use, not at process start, so only differences against it mean anything.
static PROCESS_START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Stored timestamps are `mono_ms() + 1` so that `0` means "never happened",
/// even in the first millisecond.
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
/// Worker count of the beacon's runtime; `0` means the beacon never started.
/// Logged as the denominator for `runtime=scheduling`, which means one free worker.
static RUNTIME_WORKERS: AtomicU64 = AtomicU64::new(0);

/// Beacon tick period. Several ticks fit in the watchdog's 5s probe timeout, so
/// one late tick is not mistaken for a stall.
pub(crate) const RUNTIME_TICK_PERIOD: std::time::Duration = std::time::Duration::from_secs(1);

/// Stale-tick threshold: five periods, equal to the normal probe limit. A probe
/// near the blackout deadline may wait less without changing beacon classification.
pub(crate) const RUNTIME_TICK_STALE_MS: u64 = 5_000;

/// Brackets one Postgres health probe. `Drop` also releases the in-flight slot,
/// so a probe cancelled mid-query cannot leave `db_in_flight` inflated.
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

/// Awaits one `/api/health` Postgres query under a [`DbProbeGuard`].
/// `succeeded` decides what counts as a healthy round trip; for `sqlx` use
/// `Result::is_ok`, since a query matching no rows still proves Postgres answered.
pub(crate) async fn observe_db<T>(
    query: impl std::future::Future<Output = T>,
    succeeded: impl FnOnce(&T) -> bool,
) -> T {
    let guard = DbProbeGuard::new();
    let outcome = query.await;
    guard.finish(succeeded(&outcome));
    outcome
}

/// A `PgPool` whose only ordinary use is an await under a [`DbProbeGuard`].
///
/// `services::health_diagnostics` shadows its raw pool with one, so an
/// unbracketed `.fetch_*` there stops compiling. This stops accidents, not
/// misuse: [`probe`](ProbedPool::probe)'s closure gets the raw handle and may
/// return it, a closure that awaits twice records one probe, and code that
/// never wraps its pool is not covered.
pub(crate) struct ProbedPool<'p> {
    pool: &'p sqlx::PgPool,
}

impl<'p> ProbedPool<'p> {
    /// Wraps a health-path pool; `None` stays `None` for "no pool, no work".
    pub(crate) fn wrap(pool: Option<&'p sqlx::PgPool>) -> Option<Self> {
        pool.map(|pool| Self { pool })
    }

    /// Runs one query under a [`DbProbeGuard`]. `build` is not `async`, so the
    /// future it returns is awaited inside the bracket.
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

/// Records one beacon tick. Separate from [`spawn_runtime_liveness_beacon`] so
/// tests can tick without a timer.
pub(crate) fn record_runtime_tick() {
    RUNTIME_TICKS.fetch_add(1, Ordering::Relaxed);
    LAST_RUNTIME_TICK_AT.store(mono_ms().saturating_add(1), Ordering::Relaxed);
}

/// Spawns the runtime-liveness beacon. Call once, from inside the runtime.
///
/// The task only awaits a timer and stores atomics (no lock, channel, socket or
/// database), so only the runtime failing to poll a ready task can stop it.
/// Off a runtime it returns a failed [`BeaconArmed`] instead of panicking: a
/// missing beacon costs `verdict=undetermined_no_beacon`, a panic the whole service.
pub(crate) fn spawn_runtime_liveness_beacon() -> BeaconArmed {
    spawn_runtime_liveness_beacon_with_period(RUNTIME_TICK_PERIOD)
}

pub(crate) fn spawn_runtime_liveness_beacon_with_period(
    period: std::time::Duration,
) -> BeaconArmed {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return BeaconArmed { workers: None };
    };
    // Recorded here: the watchdog reads from its own OS thread, with no runtime handle.
    let workers = handle.metrics().num_workers() as u64;
    RUNTIME_WORKERS.store(workers, Ordering::Relaxed);
    handle.spawn(async move {
        let mut interval = tokio::time::interval(period);
        // Not the default `Burst`: a recovering runtime must not replay missed ticks.
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

/// Proof that [`spawn_runtime_liveness_beacon`] ran; nothing else constructs one.
/// `spawn_watchdog_thread` takes it by value, so the beacon is armed before the
/// watchdog thread exists. Also issued when arming failed (`workers: None`);
/// [`BeaconArmed::boot_report`] says which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "the watchdog thread takes this by value; dropping it means the beacon was armed for nothing"]
pub(crate) struct BeaconArmed {
    /// Worker threads the beacon saw; `None` when there was no runtime to spawn on.
    workers: Option<u64>,
}

impl BeaconArmed {
    /// `Some(n)` when the beacon is running on an `n`-worker runtime.
    #[cfg(test)]
    pub(crate) fn workers(self) -> Option<u64> {
        self.workers
    }

    /// The boot line to log, at ERROR when `Err`. Returned rather than logged so
    /// tests can assert on it without a tracing subscriber.
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
    /// Health-path Postgres awaits outstanding, process-wide so the watchdog
    /// thread can read it without touching anything the runtime could block on.
    /// Non-zero means *some* health request is in a query, not necessarily the probe's.
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
    /// Worker threads in the runtime, or `0` if the beacon never started.
    pub(crate) runtime_workers: u64,
}

/// What the beacon says about the runtime, independently of the acceptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeLiveness {
    /// Ticked within [`RUNTIME_TICK_STALE_MS`]: at least one worker is polling.
    /// Rules out a wedged runtime, not partial starvation (N-1 blocked workers
    /// still tick it), so weigh any `handler_*` verdict against `runtime_workers=`.
    Scheduling { age_ms: u64 },
    /// No tick within [`RUNTIME_TICK_STALE_MS`]. Consistent with a wedged runtime,
    /// not proof of one; the probe may use a shorter remaining blackout budget.
    Stalled { age_ms: u64 },
    /// Never ticked: the beacon was never started or never polled. The counters
    /// cannot tell which, so this concludes nothing rather than guess `Stalled`.
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
        self.runtime_liveness_with_threshold(RUNTIME_TICK_STALE_MS)
    }

    pub(crate) fn runtime_liveness_with_threshold(&self, stale_limit_ms: u64) -> RuntimeLiveness {
        match self.runtime_tick_age_ms {
            None => RuntimeLiveness::Unknown,
            Some(age_ms) if age_ms <= stale_limit_ms => RuntimeLiveness::Scheduling { age_ms },
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
            self.runtime_workers,
            self.runtime_ticks,
            age(self.runtime_tick_age_ms),
        )
    }
}

/// The probe stage and runtime beacon together distinguish a failed listener,
/// stalled runtime, and handler delay without tying diagnosis to exit policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HealthVerdict {
    Responsive,
    RuntimeStalled,
    ListenerGone,
    HandlerBlockedOnDb,
    HandlerSlowDbIdle,
    ConnectionResetBeforeRequest,
    UndeterminedNoBeacon,
}

impl HealthVerdict {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Responsive => "responsive",
            Self::RuntimeStalled => "runtime_stalled",
            Self::ListenerGone => "listener_gone",
            Self::HandlerBlockedOnDb => "handler_blocked_on_db",
            Self::HandlerSlowDbIdle => "handler_slow_db_idle",
            Self::ConnectionResetBeforeRequest => "connection_reset_before_request",
            Self::UndeterminedNoBeacon => "undetermined_no_beacon",
        }
    }
}

/// Pure classification over one captured observation; it never samples atomics.
pub(crate) fn classify_health(
    outcome: &HealthProbeOutcome,
    liveness: RuntimeLiveness,
    crumbs: &Breadcrumbs,
) -> HealthVerdict {
    use HealthProbeOutcome as Stage;
    use HealthVerdict as Verdict;
    match outcome {
        Stage::Responded { .. } => Verdict::Responsive,
        // A failed request write never reached the handler, regardless of beacon state.
        Stage::RequestFailed { .. } => Verdict::ConnectionResetBeforeRequest,
        Stage::ConnectFailed { .. } => match liveness {
            RuntimeLiveness::Unknown => Verdict::UndeterminedNoBeacon,
            RuntimeLiveness::Stalled { .. } => Verdict::RuntimeStalled,
            RuntimeLiveness::Scheduling { .. } => Verdict::ListenerGone,
        },
        Stage::NoResponse { .. } => match liveness {
            RuntimeLiveness::Unknown => Verdict::UndeterminedNoBeacon,
            RuntimeLiveness::Stalled { .. } => Verdict::RuntimeStalled,
            RuntimeLiveness::Scheduling { .. } if crumbs.db_in_flight > 0 => {
                Verdict::HandlerBlockedOnDb
            }
            RuntimeLiveness::Scheduling { .. } => Verdict::HandlerSlowDbIdle,
        },
    }
}

pub(crate) fn verdict(outcome: &HealthProbeOutcome, crumbs: &Breadcrumbs) -> &'static str {
    classify_health(outcome, crumbs.runtime_liveness(), crumbs).label()
}

/// How far the watchdog's loopback probe got. Not a conclusion on its own: pair
/// it with [`RuntimeLiveness`] via [`verdict`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HealthProbeOutcome {
    /// The server answered. Any HTTP status counts: a 503 still proves the
    /// runtime schedules tasks, and killing on it would crash-loop the process.
    Responded { elapsed_ms: u64 },
    /// The handshake never completed (or `addr` did not parse). A wedged
    /// runtime lands here only once the listen backlog is full.
    ConnectFailed { elapsed_ms: u64, error: String },
    /// Connected, but the request could not be written (including a write
    /// timeout). The handler was never reached, so never blame it or the database.
    RequestFailed { elapsed_ms: u64, error: String },
    /// Request written, but no bytes came back before the read timeout or the
    /// peer closed. Covers both a wedged runtime and a live one waiting on
    /// Postgres; [`RuntimeLiveness`] tells them apart.
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

/// The actual socket observation time precedes diagnostic snapshots and logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeObservation {
    pub(crate) outcome: HealthProbeOutcome,
    pub(crate) observed_at: Instant,
}

/// Uses one absolute deadline across connect, request writes and the first byte.
/// Blocking socket IO keeps the probe independent of stalled runtime workers.
pub(crate) fn probe_health_once(addr: &str, host: &str, deadline: Instant) -> ProbeObservation {
    use std::io::{ErrorKind, Read, Write};

    let started = Instant::now();
    let elapsed_ms = |at: Instant| at.saturating_duration_since(started).as_millis() as u64;
    let failed = |stage: fn(u64, String) -> HealthProbeOutcome, error: String| {
        let observed_at = Instant::now();
        ProbeObservation {
            outcome: stage(elapsed_ms(observed_at), error),
            observed_at,
        }
    };
    let connect_failed =
        |elapsed_ms, error| HealthProbeOutcome::ConnectFailed { elapsed_ms, error };
    let request_failed =
        |elapsed_ms, error| HealthProbeOutcome::RequestFailed { elapsed_ms, error };
    let no_response = |elapsed_ms, error| HealthProbeOutcome::NoResponse { elapsed_ms, error };
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|time| !time.is_zero())
    };
    let expired = || "probe deadline reached".to_string();

    if remaining().is_none() {
        return failed(connect_failed, expired());
    }
    let socket_addr = match addr.parse() {
        Ok(parsed) => parsed,
        Err(e) => return failed(connect_failed, format!("bad addr {addr}: {e}")),
    };
    let mut stream = loop {
        let Some(timeout) = remaining() else {
            return failed(connect_failed, expired());
        };
        match std::net::TcpStream::connect_timeout(&socket_addr, timeout) {
            Ok(stream) => break stream,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return failed(connect_failed, e.to_string()),
        }
    };
    let request = format!("GET /api/health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    let mut pending = request.as_bytes();
    while !pending.is_empty() {
        let Some(timeout) = remaining() else {
            return failed(request_failed, expired());
        };
        // Partial writes and interrupted syscalls retain the original deadline.
        if let Err(e) = stream.set_write_timeout(Some(timeout)) {
            return failed(request_failed, e.to_string());
        }
        if remaining().is_none() {
            return failed(request_failed, expired());
        }
        match stream.write(pending) {
            Ok(0) => return failed(request_failed, "peer closed during request".to_string()),
            Ok(written) => pending = &pending[written..],
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return failed(request_failed, e.to_string()),
        }
    }

    let mut buf = [0u8; 512];
    loop {
        let Some(timeout) = remaining() else {
            return failed(no_response, expired());
        };
        if let Err(e) = stream.set_read_timeout(Some(timeout)) {
            return failed(no_response, e.to_string());
        }
        if remaining().is_none() {
            return failed(no_response, expired());
        }
        let read = stream.read(&mut buf);
        let observed_at = Instant::now();
        let outcome = match read {
            Ok(n) if n > 0 => HealthProbeOutcome::Responded {
                elapsed_ms: elapsed_ms(observed_at),
            },
            Ok(_) => HealthProbeOutcome::NoResponse {
                elapsed_ms: elapsed_ms(observed_at),
                error: "peer closed without responding".to_string(),
            },
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => HealthProbeOutcome::NoResponse {
                elapsed_ms: elapsed_ms(observed_at),
                error: e.to_string(),
            },
        };
        return ProbeObservation {
            outcome,
            observed_at,
        };
    }
}

/// Serializes tests that assert on deltas of the process-global counters, so a
/// parallel test cannot move them mid-assertion. Outside `mod tests` because
/// `services::health_diagnostics` tests must share the same lock.
#[cfg(test)]
pub(crate) fn counter_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static COUNTER_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    COUNTER_TESTS.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests;
