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
//! So this module records the three facts the dump structurally cannot carry.
//!
//! ### 1. Which stage of the probe failed — [`HealthProbeOutcome`]
//!
//! `ConnectFailed` means the TCP handshake never completed; `NoResponse` means
//! the connection was established and the request written, but no bytes came
//! back before the read timeout.
//!
//! **This field on its own does not separate a wedged runtime from a slow
//! handler, and it must not be read that way.** The listen backlog completes
//! the handshake *in the kernel*, with no participation from the accept loop, so
//! a runtime that never polls its acceptor still lets `connect()` succeed —
//! measured at 0.1 ms — and then times out on read, classifying as `NoResponse`
//! exactly like a healthy runtime waiting on a slow query. The backlog depth is
//! a kernel limit, not a tokio one (`mio` asks for `-1` and the kernel caps it:
//! `kern.ipc.somaxconn`, measured at **128** on the release host). The exact
//! depth does not matter; one free slot completes a handshake the acceptor
//! never sees. So:
//!
//! * `ConnectFailed` means the handshake did not complete: the listening socket
//!   is gone — **or its backlog is full**. The second case matters, and an
//!   earlier draft of this file denied it ("not that the runtime is wedged").
//!   A runtime that never polls its acceptor holds every slot it is given, so
//!   with enough clients it eventually fills all 128 and the next `connect()`
//!   fails. `ConnectFailed` therefore does not acquit the runtime; it only says
//!   the handshake failed. That is why [`verdict`] still consults the beacon
//!   for this stage, and why `undetermined_no_beacon` — not `listener_gone` —
//!   is the honest answer when no beacon ran.
//! * `NoResponse` covers **both** "the runtime is wedged" and "the handler is
//!   waiting on Postgres".
//! * `RequestFailed` means the connection was established and then the request
//!   could not be written. The kernel keeps an accepted socket writable with no
//!   help from the executor, so a *refused* write is a peer-side reset and not
//!   a scheduling symptom — which is why this is the one failure stage
//!   [`verdict`] settles without the beacon.
//!
//!   State the limit, because the stage is a fold and the conclusion is not:
//!   [`probe_health_once`] maps **every** `write_all` error here, so a write
//!   that merely timed out is *not* a peer reset. The request is 70 bytes to
//!   loopback against a far larger socket buffer, so that has never been
//!   observed — but if it happens, `verdict` prints
//!   `connection_reset_before_request` with no evidence behind it, in the one
//!   place that skips the beacon. `err=` on the same line distinguishes them.
//!
//! ### 2. Whether the runtime is scheduling tasks — [`RuntimeLiveness`]
//!
//! This is the discriminator `stage=` cannot be, and why the two are always
//! reported together. [`spawn_runtime_liveness_beacon`] runs one tokio task
//! storing a monotonic timestamp every [`RUNTIME_TICK_PERIOD`]. It depends on
//! the timer driver and a worker thread and nothing else — not the acceptor,
//! not the HTTP stack, not Postgres. A stale tick is positive evidence the
//! runtime stopped polling; a fresh one, that it did not. [`verdict`] combines
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
//! ### 3. What the database was doing — [`Breadcrumbs`]
//!
//! Every Postgres await on the **public `GET /api/health` path** — the endpoint
//! the watchdog probes — brackets itself with a [`DbProbeGuard`]. Enumerated,
//! because a partial claim would misread as `db_in_flight=0` == "the database is
//! innocent" (all in `services::health_diagnostics`):
//!
//! The site-by-site enumeration and the rule for what counts as one acquire
//! live in `health_diagnostics`' module docs, next to the code. Headline:
//! **9 unconditional sequential acquires**, a 10th with a `health_registry`, an
//! 11th on a providerless cluster standby, each able to block for the pool's
//! 10s `acquire_timeout` against a 5s probe timeout.
//!
//! **Not covered**, stated explicitly so a future reader does not infer more
//! than the counters carry:
//!
//! * `load_channel_session_state`, reached once per mailbox from
//!   `enrich_mailbox_session_state` on `GET /api/health/detail` only. It is
//!   shared with two non-health repair routes, so bracketing it *inside the
//!   function* would count DB work no health probe performed. Bracketing its
//!   health-only call site instead is open and the only obstacle is small — it
//!   returns `Option`, so the predicate has to be `|_| true` rather than
//!   `Result::is_ok`, which would score "no row" as a failure. Left undone
//!   because `/api/health/detail` is not the probed endpoint, **not** because
//!   it cannot be done. That endpoint's active-session audit query is bracketed
//!   anyway, being health-only.
//!
//! The enumeration above is held by the *types* in `health_diagnostics`, not by
//! a test: every health-path function there shadows its `Option<&PgPool>` with
//! a [`ProbedPool`] and the two that take a pool directly take a `ProbedPool`,
//! so no raw handle is in scope to await unbracketed. The exempt routes are
//! exactly the ones that still hold a `&PgPool`. That scope is the **module**,
//! not the health *path*: an await added to a health route elsewhere
//! (`server::routes::health_api`) is outside anything here can see and would
//! silently reopen the `db_in_flight=0` misreading. `ProbedPool`'s own docs
//! list the rest of what the construction does not cover.
//!
//! ## What the log evidence actually supports
//!
//! Stated as an enumeration with **the predicate, the window and the base rate
//! attached**, because the tempting summary ("a watchdog failure means the
//! database was slow") is not true as a universal, and because a bare "X
//! happened just before Y" is not evidence at all until X is shown to be rare
//! at a random moment.
//!
//! ### How to reproduce every number below
//!
//! An earlier draft gave figures without saying what it had counted, and three
//! of them then turned out to be wrong in ways nobody could check from the
//! text. So, exactly:
//!
//! 1. **Corpus.** `~/.adk/release/logs/dcserver.stdout.log` plus its 10 rotated
//!    siblings `.1`–`.10`, **restricted to
//!    `[2026-07-12T23:50:43.386222Z, 2026-08-07T03:18:28.232513Z]`** — 11 files,
//!    **603.462 h** of covered wall clock (the union of each file's first→last
//!    stamp; the files abut to within a second and do not overlap).
//!
//!    ⚠️ **These logs rotate, so the figures below reproduce only inside that
//!    window** and are not stable properties of the deployment. Two
//!    recomputations four days apart disagreed on corpus length (638.0 h vs
//!    603.5 h) purely because `.10` had aged out. Re-derive rather than trust;
//!    if the window is gone, say so instead of quoting these numbers.
//!
//!    Strip the ANSI SGR escapes, then read the RFC3339 stamp at the head of
//!    each line. **That
//!    stamp is UTC**; the `[HH:MM:SS]` inside the message is KST (UTC+9), and
//!    so are the `adk-hang-<pid>-<YYYYMMDD>-<HHMMSS>.txt` dump names. Parse the
//!    head stamp as an *aware* UTC instant — an earlier analysis parsed it
//!    naive, re-rendered through `utcfromtimestamp`, and shifted every reported
//!    time by −9 h. **Six of the 13 kills land on a different calendar day
//!    under that shift**, including the 2026-08-03 one an earlier draft
//!    therefore reported as 08-02.
//! 2. **Watchdog events.** `watchdog: health check failed (n/3)`,
//!    `watchdog: runtime unresponsive`, `watchdog: health recovered`. A *streak*
//!    opens at `(1/3)` and closes at the next `recovered` or `unresponsive`.
//!    Corpus totals: **33 streaks, 13 of them ending in a kill, 20 recovering**.
//! 3. **Postgres error — `P_pg`.** The pool-acquisition failures the watchdog
//!    can actually be delayed by: a line matching
//!    `pool timed out|PoolTimedOut|Connection refused \(os error 61\)` —
//!    **13 061 lines** in the window.
//!
//!    ⚠️ **`P_pg` is the predicate for every Postgres figure below, with no
//!    per-figure exceptions.** An earlier draft published this regex but
//!    computed three bullets from `pool timed out|PoolTimedOut` alone (944
//!    lines) and one from a fourth; each was individually reproducible and none
//!    reproducible *from the text*, the only property this section is for. A
//!    different predicate, where needed, is named at the point of use.
//! 4. **Base rate.** Not 20 000 Monte-Carlo draws: at these hit counts that
//!    estimator has ±30 % noise and is not reproducible without the seed.
//!    Compute it *analytically* as the covered fraction of the union of
//!    `[error, error + W]` windows — **for every control in this section, not
//!    just the Postgres ones**. Same definition, deterministic answer.
//!
//!    ⚠️ **What that comparison assumes.** Comparing 11/13 against a base rate
//!    over *uniformly random covered instants* treats kill times as drawn from
//!    that same distribution — as carrying no information about when in the
//!    corpus they fall, independently of Postgres. Not free: both are clumped,
//!    so a common cause raising both densities in the same hours (a busy period,
//!    a restart storm) would produce lift without either causing the other. The
//!    lift is large enough that this is unlikely to explain all of it, but it is
//!    why the module ships `db_in_flight` and the beacon, not just correlation.
//!
//! ### The findings
//!
//! * **Kills preceded by a `P_pg` line: 11 of 13, within 5 s.** The 11 deltas
//!   are 0.06, 0.12, 0.44, 0.46, 0.92, 1.04, 1.16, 1.27, 2.56, 2.62 and 4.04 s.
//!   Base rate of that predicate at a uniformly random covered instant:
//!   **0.07975 %** (W = 5 s), so the lift is **≈1060×**. It survives the window
//!   choice — at W = 6 s still 11 of 13 against 0.08589 % (≈985×), at W = 20 s
//!   still 11 of 13 against 0.15772 % (≈537×). That lift is what makes this the
//!   thing the module is built around. The three most recent kills
//!   (`2026-08-04T12:41:08Z`, `2026-08-04T16:43:06Z`, `2026-08-05T16:05:13Z`)
//!   are 3 of 3, at **0.44 s, 4.04 s and 1.04 s** — named events rather than a
//!   trailing-window rate, because an earlier draft's "most recent 58.8 h"
//!   stopped being computable the moment the corpus rotated.
//! * **The two exceptions are not database-quiet — they are a different
//!   database failure.** `2026-08-02T13:21:36Z` and `2026-08-03T03:21:51Z`
//!   (dump names `adk-hang-84772-20260802-222136` and
//!   `adk-hang-32033-20260803-122151`, which are KST — an earlier draft read
//!   both as 08-02) had gone **5.2 h and 19.2 h** with no `P_pg` line. That on
//!   its own is unremarkable — `P_pg` lines are heavily clumped, and the corpus
//!   holds 54 `P_pg`-free stretches of an hour or more, the longest 77.4 h.
//!   (**Endpoint rule**: gaps *between consecutive `P_pg` lines* only. The
//!   right-censored tail — last line to end of coverage, 33.00 h — is excluded;
//!   include it and it is 55. The left head is 0.00 h.) What
//!   singles these two out is only that they are the two kills the 11-of-13
//!   above does not cover.
//!
//!   What they do have is `EADDRNOTAVAIL`, the kernel refusing a *new outbound
//!   connection*. Predicate `P_eaddr` = `Can't assign requested address`:
//!   **20 lines, and every one of them falls between `2026-08-02T13:17:42Z` and
//!   `2026-08-03T03:21:30Z`** — nowhere else in 603 h. Twelve precede the two
//!   kills (13:17:42–13:21:10Z, 03:20:31–03:21:30Z); the other eight are
//!   `[startup] postgres warmup pool unavailable` / `[config-audit]` pairs from
//!   processes that had just restarted (13:22:24, 13:32:36, 15:06:28, 20:51:26),
//!   i.e. downstream of a kill, not before one. ⚠️ An earlier draft said "the
//!   only **9**, in exactly two bursts (7 and 2)" — those 9 are `P_eaddr` **also
//!   restricted to `[policy-tick] advisory lock failed`**, an undisclosed
//!   filter. "Confined to these two days" survives; "two bursts" does not.
//! * **…and those same two streaks own all four instant-fail probes.** Of the
//!   29 consecutive-failure gaps, 23 are 35.02–35.32 s (below) and **4 are
//!   30.07–30.19 s — all four inside these two kills** (08-02: 30.13, 30.15;
//!   08-03: 30.07, 30.19). A 30.1 s gap against a 30 s `CHECK_INTERVAL` means
//!   the probe returned in ~0.1 s: it consumed none of its 5 s timeout, which
//!   is what a `connect()` that fails immediately looks like. So the two facts
//!   an earlier draft listed as separate bullets — "kills with no Postgres
//!   error" and "probes that failed without waiting" — are the same two events.
//!
//!   ⚠️ **That co-location is all this says. n = 2, and no cause is claimed.**
//!   An earlier draft called both "consistent with the host having run out of
//!   the thing a new connection needs" — a mechanism sketched to fit two
//!   observations, and "consistent with" is how a hypothesis gets quoted back
//!   as a conclusion. With two events there is no base rate to test it against,
//!   the direction is untested (the port shortage could as easily be a
//!   *symptom*), and one episode has `P_eaddr` lines both before and after its
//!   kill. The last confident correlation here (`routines::loader`, below) was
//!   noise. What it buys is a prediction: if the ephemeral-port reading is
//!   right, the next such event renders `stage=connect_failed` with `verdict` at
//!   `listener_gone` or `undetermined_no_beacon`, never `handler_*`.
//! * **First failures whose last `P_pg` line was an hour or more earlier:
//!   11 of 33** — 2.0, 5.2, 5.2, 5.2, 11.2, 18.5, 19.2, 19.9, 23.2, 23.3,
//!   27.7 h. **Two** of those escalated to a kill (the two above). (An earlier
//!   draft listed 2.0, 13.1, 18.5, 19.9, 23.2, 23.3, 30.7, 30.7, 30.8, 44.8,
//!   53.3 h — the same 11 streaks against `pool timed out|PoolTimedOut` instead
//!   of `P_pg`; same count, same membership, same two kills, only shorter gaps,
//!   because `P_pg` also sees the refused connections. And an earlier draft said
//!   *four* kills, from a `kill_time - first_failure < 180 s` proximity test
//!   that also matched two streaks which *recovered* after a single failure —
//!   2026-08-02T13:18:04Z and 13:19:05Z — because a later, unrelated kill fell
//!   inside the window.) Attribute a kill to the streak that logged it, not to
//!   the clock.
//!   Something other than Postgres delays the probe often enough to matter, it
//!   is not modelled here, and the beacon plus `db_in_flight` are what will
//!   tell the two apart next time.
//! * **No second trigger has been identified.** An earlier draft attributed
//!   those hour-gap failures to a `routines::loader` burst. That is **withdrawn
//!   as a base-rate error**, and recorded rather than deleted because the next
//!   investigator will find the same correlation and should not have to
//!   re-derive that it is noise. In the 60 s before each of the 11 hour-gap
//!   failures the loader emitted 28, 44, 52, 56, 56, 56, 56, 56, 56, 56, 56
//!   lines, against a control of **p20 = 52, median = 56, p90 = 58** at random
//!   covered instants: eight sit exactly at the modal density and three sit
//!   *below* it. There is no burst. Across all 33 first failures the sign
//!   reverses — 6.1 % (2 of 33) have a loader line within 5 s, versus
//!   **15.0 %** of random covered instants (15.0019 %: analytic union of
//!   `[event, event+5 s]` over 1 788 762 `routines::loader` lines ÷ 603.462 h).
//!   An earlier draft said 15.2 %, from sampling the timeline on a 10 s grid
//!   instead of the analytic union this section mandates. Same predicate, wrong
//!   method; the section's own rule catches it. Two corrections travel with this: the control's p10 is **0**,
//!   not 50 (10.4 % of the corpus — 62.7 h — has no loader line in the preceding
//!   minute at all, in stretches beginning at the kills themselves, e.g. 17.9 h
//!   from `2026-08-04T16:42Z`), and the loader's mean rate of 0.62 line/s is
//!   dropped rather than quoted, because 0.62 × 60 = 37 is below every observed
//!   count and so makes a *modal* minute look like a burst. The output is
//!   clumped; only the density-controlled comparison discriminates.
//! * **Failing probes wait the timeout out: 23 of 29** consecutive-failure gaps
//!   are 35.02–35.32 s against a 30 s `CHECK_INTERVAL` — the probe's own 5 s
//!   read timeout elapsing in full, so those probes *waited* rather than being
//!   refused: "connected, handler never answered", and not a dead listening
//!   socket. The other 6 are **not** that, and the earlier "~35 s vs ~30 s"
//!   phrasing hid them: 4 are the 30.1 s instant-fails above, 2 are 60.4 s and
//!   90.7 s (2× and 3× the interval, with a successful check between).
//!   ⚠️ The denominator 29 is "diffs between consecutive failure LINES ≤ 100 s".
//!   Every *within-streak* gap with no cutoff gives 31: the 60.4/90.7 s pair
//!   drops out (a `health recovered` sits between them) and four long ones come
//!   in — 238, 962, 1375, 1865 s. Three are in recovering streaks; **the 962 s
//!   one is in a kill streak** (2026-07-14T17:16:04Z, first failure→kill 997.6 s
//!   against ~70 s for every other kill) — the corpus's only evidence that the
//!   watchdog thread's own scheduling is not always prompt.
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

/// A `PgPool` whose only *ordinary* use is an await under a [`DbProbeGuard`].
///
/// #5147: `services::health_diagnostics` wraps its `Option<&PgPool>` parameter
/// in one of these at the top of every function on the public `GET /api/health`
/// path, **shadowing the raw pool out of scope**. What that buys, exactly: in
/// those function bodies there is no `&PgPool` binding left to hand to
/// `.fetch_one`/`.fetch_optional`/`.fetch_all`, so *writing another await the
/// normal way* stops compiling instead of going unrecorded. That is the
/// accident this is for.
///
/// It is **not a capability boundary**, and an earlier draft said it was ("the
/// handle never escapes"). [`probe`](ProbedPool::probe) hands the raw
/// `&'p PgPool` to its closure with no bound on the return type, so
/// `|p| ready(p)` returns the borrow and `|p| ready(p.clone())` an owned pool;
/// both compile and either can then be awaited with no guard. True statement:
/// a future *built* by that closure is awaited under the guard, because the
/// closure is not `async`.
///
/// This replaces a test that counted `observe_db(` occurrences against
/// `.fetch_*(` occurrences in the module source. Counting cannot prove
/// *pairing*: adversarial review deleted the bracket from the second
/// `dispatch_outbox` query and added one to an exempt repair route, the totals
/// matched, and the guard stayed green while a health-path await went
/// unrecorded. Nothing here counts anything.
///
/// **What this does not cover**, stated so nobody reads it as total:
///
/// * A *new* sibling in that module taking `Option<&PgPool>` instead of
///   wrapping it can still await unbracketed — nothing forces the parameter
///   type. The converted functions are additionally pinned behaviourally by
///   `assert_bracketed!`; a new one would have neither.
/// * One [`probe`](ProbedPool::probe) whose closure `await`s twice records one
///   probe for two round trips. Every call site issues exactly one query — a
///   convention, not a constraint.
/// * It covers only *this* module, not the health *path*
///   (`server::routes::health_api` is the obvious gap).
/// * **The handle is extractable** — see above. A sealed bound on `T` was tried
///   and dropped: every real call site returns `Result<_, _>` and so does
///   `|p| ready(Ok(p.clone()))`, so it moves the leak without closing it.
///   Closing it means never handing the closure a pool, which the foreign
///   `fn(&PgPool)` this path calls in `auto_queue::cleanup_tasks` rules out.
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
/// `discord::health::self_watchdog::spawn_watchdog_thread` takes one by value
/// and the only way to obtain one is to call the arming function, so **at that
/// call site** deleting the arming, or moving it into the spawned closure,
/// stops compiling.
///
/// Read that scope literally; three nearby readings are false. It is `Copy`, so
/// "by value" is not linear consumption — one token can start two threads. It
/// says nothing about other ways to start one: `std::thread::spawn` compiles
/// anywhere with no token in sight. And it is issued on the failure path too
/// (`workers: None`), so it records that arming was *attempted*, not that it
/// succeeded — [`BeaconArmed::boot_report`] is what distinguishes those, logged
/// at ERROR by `spawn_watchdog_thread`. One edge of one call graph, not a
/// crate-wide invariant.
///
/// It replaces an `include_str!` byte-offset guard on `self_watchdog.rs` that
/// adversarial review broke six ways and that also failed on correct code; that
/// file's module doc enumerates the seven inputs, none of which can affect a
/// name-resolution error. There is deliberately no public constructor and no
/// `Default`.
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

/// The one conclusion the watchdog is entitled to draw, from the probe stage
/// and the beacon together.
///
/// Neither input is sufficient alone, which is why this is a function and not a
/// note to eyeball `stage=`: `stage=` cannot see a wedged runtime (the backlog
/// answers `connect()` without the acceptor), and the beacon cannot see a dead
/// listener or a stuck handler. `db_in_flight` splits the remainder — waiting on
/// Postgres vs slow for another reason — but is process-wide (see
/// [`Breadcrumbs::db_in_flight`]), so `handler_blocked_on_db` names *a* health
/// request stuck in a query, not necessarily this one.
///
/// The seven values this returns, and what each is worth:
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
/// Seven, not the five an earlier draft listed: that count omitted `responsive`
/// and predates `connection_reset_before_request`. The set is pinned as a table
/// by `tests::every_stage_and_liveness_combination_has_a_pinned_verdict`.
///
/// Matched **stage-first and exhaustively, no `_` arm**. The previous shape
/// ended in `_ => "handler_slow_db_idle"` under a `Scheduling` guard, sweeping
/// every `request_failed` cell into the two database buckets — a connection
/// that never reached the handler, reported as "blocked on Postgres". Adding a
/// stage is now a compile error here, not a silent DB verdict.
pub(crate) fn verdict(outcome: &HealthProbeOutcome, crumbs: &Breadcrumbs) -> &'static str {
    use HealthProbeOutcome as Stage;
    match outcome {
        Stage::Responded { .. } => "responsive",
        // Beacon-independent by construction. The connection was established,
        // so the listener existed and the backlog had room; the write was then
        // refused, which means the peer reset it. A wedged runtime does not do
        // that — the kernel keeps an accepted socket writable with no help from
        // the executor — so this conclusion neither needs the beacon nor may be
        // downgraded to `undetermined_no_beacon` when there is none.
        //
        // The narrow case where that reasoning does not hold: `RequestFailed`
        // also absorbs a write *timeout*, which is not a peer reset. See the
        // variant's docs — unreachable in practice for a 70-byte loopback
        // write, but this is the one arm that would print a conclusion with no
        // evidence behind it, so `err=` has to be read before believing it.
        Stage::RequestFailed { .. } => "connection_reset_before_request",
        // `ConnectFailed` does NOT settle this on its own: a runtime that never
        // polls its acceptor eventually fills the 128-slot backlog, and the
        // next `connect()` then fails exactly like a closed socket. So the
        // beacon still decides, and with no beacon the honest answer is that we
        // do not know — a wrong `runtime_stalled` (or a wrong `listener_gone`)
        // here would send the next investigation down the same dead end the
        // last three took.
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
    /// backlog is full, or the address is unroutable.
    ///
    /// This is not the *first* place a wedged runtime shows up. The kernel
    /// completes the handshake from the listen backlog without the accept loop
    /// running at all, so a runtime that never polls its acceptor lands in
    /// `NoResponse` while the backlog still has room. It only reaches here once
    /// the backlog is exhausted — so this stage cannot acquit the runtime
    /// either, and [`verdict`] reads the beacon before concluding. See the
    /// module docs for the backlog depth.
    ConnectFailed { elapsed_ms: u64, error: String },
    /// The connection was established but the request could not be written.
    ///
    /// The handler was never reached, so this must never be classified as a
    /// database or handler problem. It is also the only failure stage that
    /// concludes without the beacon: an accepted socket stays writable in the
    /// kernel whatever the executor is doing, so a **refused** write (RST /
    /// `EPIPE`) is evidence about the peer, not about scheduling.
    ///
    /// Every `write_all` error folds into this variant, including the write
    /// timeout [`probe_health_once`] also arms — and a timeout is *not* a peer
    /// reset, so the beacon-free conclusion would not be earned. A 70-byte
    /// loopback write into a socket buffer orders of magnitude larger has never
    /// produced one, but `err=` is the field that tells them apart if it ever
    /// does.
    RequestFailed { elapsed_ms: u64, error: String },
    /// The connection was established and the request written, but no bytes
    /// came back before the read timeout.
    ///
    /// This bucket holds **two different failures** and cannot separate them on
    /// its own: a wedged runtime (accepted by the backlog, never polled) and a
    /// live runtime whose `/api/health` handler is waiting on Postgres.
    /// [`RuntimeLiveness`] is what tells them apart.
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

    /// A failed probe that reached the read timeout. Shared so the verdict
    /// tests below vary only the field under test.
    fn no_response() -> HealthProbeOutcome {
        HealthProbeOutcome::NoResponse {
            elapsed_ms: 5_000,
            error: "timed out".to_string(),
        }
    }

    /// How stale the beacon can be by the time the watchdog kills, measured
    /// **from the last check that succeeded** — three `CHECK_INTERVAL`s (30s):
    /// success, then failures at +30s, +60s and +90s, the last of which exits.
    ///
    /// Read it that way and only that way. It is *not* the length of the
    /// failure streak: three failures 30s apart span **two** intervals, not
    /// three. Measured over the preserved corpus, first-failure→kill is
    /// 60.26–60.28 s when the probes fail instantly and 70.14–70.45 s when each
    /// consumes its full 5s read timeout — never 90 s. The beacon's real age at
    /// kill is therefore somewhere in [60s, 90s] depending on when the runtime
    /// stopped ticking, and 90s is the upper bound this constant names.
    ///
    /// A literal, deliberately — see
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

    // ── The discriminating test ────────────────────────────────────────────
    // These reproduce the two failure modes the production watchdog cannot
    // currently tell apart.

    #[test]
    fn accepted_but_silent_server_is_classified_as_no_response() {
        // Production shape: the runtime accepts the connection but the
        // `/api/health` handler is blocked awaiting Postgres, so nothing is
        // written back before the read timeout. The old `-> bool` probe
        // reported this identically to a dead port.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let accepted = std::thread::spawn(move || {
            // Accept and then hold the connection open, answering nothing.
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

    /// MY-1: nothing exercised the `RequestFailed` arm of `stage`, `elapsed_ms`,
    /// `error` or `is_ok`, so any of them could be swapped without a test
    /// noticing. A live `RequestFailed` needs the peer to vanish between
    /// `connect` and `write`, which is a race, so pin the whole table by
    /// construction instead — that is where the mutable behaviour lives.
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

    /// MY-2: `read` returning `Ok(0)` means the peer completed the handshake,
    /// took the request and then closed without answering. That is a failure,
    /// and it must not be mistaken for the healthy `Ok(n > 0)` path.
    ///
    /// The server drains the request before shutting down on purpose: closing
    /// with unread bytes still buffered makes the kernel send RST, and the
    /// client would take the `Err` arm instead of the `Ok(0)` one this pins.
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

    /// The reviewer's finding, pinned as an executable fact: a listener whose
    /// `accept` is NEVER called still completes the TCP handshake, because the
    /// kernel does it from the listen backlog. This
    /// is the shape of a runtime that has stopped polling its acceptor, and it
    /// classifies as `no_response` — identical to a healthy runtime waiting on
    /// Postgres, and nothing like `connect_failed`.
    ///
    /// If this ever starts returning `connect_failed`, the module docs and
    /// [`verdict`] are both wrong and must be revisited.
    #[test]
    fn a_never_accepted_connection_is_no_response_not_connect_failed() {
        // Bound but never accepted. Held for the whole probe so the socket
        // stays open and the backlog stays available.
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
            // A wedged runtime cannot poll the beacon either. Absolute, not
            // `RUNTIME_TICK_STALE_MS + 1`: this is the age the beacon actually
            // has when the watchdog kills, and an expectation written in terms
            // of the constant it is testing moves with it and proves nothing.
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
    //
    // #5147. Every threshold assertion in this file was originally
    // phrased relative to the constant it was checking — `RUNTIME_TICK_STALE_MS
    // + 1`, `RUNTIME_TICK_PERIOD * 3` — so the expectation moved with the
    // constant and the constant had no coverage at all. Two independent
    // mutations survived a 23/23 green run:
    //
    //   * `RUNTIME_TICK_STALE_MS: 5_000 -> 86_400_000` makes `runtime_stalled`
    //     unreachable in production. The watchdog exits three 30s intervals
    //     after the last successful check — 90s, and the failure streak itself
    //     is only the last two of those — so a threshold of 24 hours can never
    //     be crossed before the process is dead.
    //   * `RUNTIME_TICK_PERIOD: 1s -> 3600s` makes the beacon stale at every
    //     check, so *every* failure renders as `runtime_stalled` and the
    //     discriminator discriminates nothing.
    //
    // That is the #5177 defect class: an oracle computed from the
    // implementation. Nothing below may name a constant on the expectation
    // side.

    /// The literals themselves, plus the one relation the doc comment claims
    /// ("Five periods"). The relation alone would still be satisfiable by any
    /// pair in a 1:5 ratio, which is why both absolutes are pinned first.
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
    ///
    /// **Every expectation here is a literal on both sides.** The watchdog's
    /// constants are *read* (so a change to either side is caught) but never
    /// used to compute an expectation — `assert_eq!(A, B)` between two
    /// constants that move together is the #5177 defect class and cannot fail.
    ///
    /// While these were function-local `const`s the only available technique
    /// was `include_str!("discord/health/recovery.rs")` plus
    /// `str::contains("… from_secs(5);")`, and that was measured to break both
    /// ways: `str::contains` sees a file as one flat string and cannot tell
    /// code from a comment, so commenting the real declaration out, leaving the
    /// same literal behind in the comment and setting the live value to 60s
    /// stays green, while rewriting it as the identical
    /// `Duration::from_millis(5_000)` turns red. `self_watchdog` exists so
    /// these can simply be imported.
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
        // The other side, so a threshold mutated *down* is caught too: a beacon
        // that is ticking must never be reported as a stalled runtime.
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

    /// #5147: the whole verdict table, every cell.
    ///
    /// Two defects hid in the cells nothing exercised. `verdict` used to test
    /// the beacon first and end in `_ =>`, so **all six `request_failed`
    /// cells** — a stage whose connection was reset before the handler was ever
    /// reached — fell into `handler_blocked_on_db` / `handler_slow_db_idle`,
    /// and there was not one `request_failed` × verdict test to notice. And a
    /// `connect_failed` with no beacon has to stay `undetermined_no_beacon`,
    /// because a wedged acceptor fills the backlog and then *also* fails
    /// `connect()` — the module docs used to assert the opposite.
    ///
    /// Enumerated as a table so a new stage or a new liveness state cannot be
    /// added without a row here, and so the two claims above are readable as
    /// data rather than reconstructed from control flow.
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

        // Every verdict the watchdog can print, in one place. Six is the
        // count an earlier draft gave; it omitted `responsive` and predates
        // `connection_reset_before_request`.
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
    /// callable. Time is paused, so tokio auto-advances once the test task
    /// idles and this costs no wall-clock.
    ///
    /// The window is **5 000 ms as a literal**, not `RUNTIME_TICK_PERIOD * n`.
    /// The point of the beacon is to resolve the runtime's state *within one
    /// probe timeout*, so the property worth pinning is "several ticks fit
    /// inside 5s" — a property a period of 3600s fails. Phrased relative to the
    /// period it would pass at any period whatsoever, which is exactly how
    /// `RUNTIME_TICK_PERIOD: 1s -> 3600s` survived a 23/23 green run.
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
        // No age assertion here: `PROCESS_START` is a real `Instant`, so under
        // `start_paused` every tick lands within a millisecond of real "now"
        // and any age bound would pass vacuously. The age/threshold behaviour
        // is pinned on constructed `Breadcrumbs` instead, by
        // `a_runtime_that_stopped_ticking_reads_as_stalled_before_the_watchdog_kills`.
        after
            .runtime_tick_age_ms
            .expect("a running beacon must leave a timestamp");
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

    /// This parses rather than searches: it splits the section into lines,
    /// drops comment lines, splits on the first `=` and **truncates the value
    /// at a trailing `#`**. That last step
    /// is not cosmetic — without it `strip = true # keep symbols` yields the
    /// value `true # keep symbols`, which is `!= "true"`, and
    /// [`release_profile_keeps_the_mach_o_symbol_table`] goes green on a fully
    /// stripped binary. TOML has no `#` inside a bare value, and the two keys
    /// read here are a bare bool/int and a quoted string with no `#` in it, so
    /// truncating is exact for this table.
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
        // The regression this closes: `split_once('=')` used to keep the
        // trailing comment in the value, so `strip = true # …` compared unequal
        // to `"true"` and the symbol-table guard passed on a stripped binary.
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
        // #5147: `sample`/`atos` have TWO independent name sources — the
        // binary's own Mach-O symbol table (LC_SYMTAB) and a UUID-matched
        // .dSYM. Either one alone resolves our frames; measured on a fully
        // stripped binary, the .dSYM by itself still recovers both the symbol
        // name and `recovery.rs:1677`.
        //
        // `strip = true` (== `strip = "symbols"`) deletes LC_SYMTAB, which
        // leaves the .dSYM as a single point of failure — and it is a separate
        // 276 MB directory that deploy ships fail-open, that is ignored when
        // its UUID does not match, and that no dump carries with it. In
        // #4756/#4770/#5147 both sources were absent at once, which is why
        // every one of our frames rendered as `load address 0x… + 0x…`.
        // Keeping the symbol table is the layer that cannot get separated from
        // the binary.
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
        // Function names come from the symbol table; file:line and inlined
        // frames need debug info, which on macOS must be collected into a
        // .dSYM by `split-debuginfo = "packed"`. Without `debug` there is
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
