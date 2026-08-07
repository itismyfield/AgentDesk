//! Process self-watchdog: the thread that force-exits `dcserver` when the HTTP
//! runtime stops answering.
//!
//! #5147 moved this out of `health/recovery.rs` for two reasons.
//!
//! **Cohesion.** `recovery.rs` is about recovering *channels* — watchers,
//! mailboxes, stalled turns. This is about the *process*, it shares no state
//! with anything else in that file, and it runs on its own OS thread precisely
//! so it depends on nothing the rest of the module touches.
//!
//! **Testability of the constants.** [`CHECK_INTERVAL`], [`TCP_TIMEOUT`] and
//! [`MAX_FAILURES`] are at module scope so `services::hang_forensics` can
//! *import* them: it derives its stale threshold from `TCP_TIMEOUT` and its
//! kill deadline from `CHECK_INTERVAL * MAX_FAILURES`, and it has to be able to
//! check that derivation. Against a function-local `const` the only available
//! technique is `include_str!` plus `str::contains` on the declaration text,
//! which was measured to fail in both directions — commenting the real
//! declaration out while leaving the same literal in a comment stays green with
//! the live value at 60s, and an equivalent rewrite to
//! `Duration::from_millis(5_000)` turns red.
//!
//! **Ordering.** [`spawn_watchdog`] arms `hang_forensics`' runtime-liveness
//! beacon and hands the resulting `BeaconArmed` token to
//! `spawn_watchdog_thread`, which takes it by value. Stated at the scope it
//! actually holds: **in this function**, deleting the arming or moving it
//! inside the spawned closure stops compiling, because `armed` is then not in
//! scope. It is not a crate-wide invariant — `std::thread::spawn` compiles
//! anywhere without a token, and `BeaconArmed` is `Copy`, so "by value" does
//! not mean the token is consumed. What it replaces is a guard that
//! `include_str!`-ed this file and compared byte offsets: adversarial review
//! defeated that six ways (decoys in a lifetime-adjacent comment, a plain
//! string, a byte string, a raw string, a `#[cfg(test)]` item and an unused
//! `macro_rules!` body) and it also failed on *correct* code that merely
//! mentioned `std::thread::Builder::new()` in a string earlier in the file.
//! None of those seven inputs can affect a name-resolution error.

use std::time::Duration;

/// How long the watchdog sleeps between probes.
pub(crate) const CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// Connect / write / read timeout for one probe.
///
/// Deliberately shorter than the Postgres pool's `acquire_timeout`
/// (10s, `db::postgres::DEFAULT_PG_ACQUIRE_TIMEOUT_SECS`), which is why a slow
/// database alone can fail this probe while the runtime is perfectly
/// responsive. `hang_forensics::RUNTIME_TICK_STALE_MS` is the same 5s expressed
/// in milliseconds, so "the beacon is stale" means "the runtime failed to run
/// one trivial task for at least as long as the probe waited for a byte".
pub(crate) const TCP_TIMEOUT: Duration = Duration::from_secs(5);

/// Consecutive failed probes before the process force-exits.
pub(crate) const MAX_FAILURES: u32 = 3;

/// Skip checks for the first 30s after startup so the runtime has time to
/// initialise Discord bots and register providers.
pub(crate) const STARTUP_GRACE: Duration = Duration::from_secs(30);

/// Self-watchdog: runs on a dedicated OS thread (not tokio) to detect runtime
/// hangs. Periodically opens a raw TCP connection to the server port and
/// expects a response within [`TCP_TIMEOUT`]. If the check fails
/// [`MAX_FAILURES`] times in a row the process is force-killed so launchd (or
/// systemd) can restart it.
///
/// #5147: every failure carries [`crate::services::hang_forensics`] fields —
/// which stage of the probe failed, how long it took, whether the tokio runtime
/// was still scheduling tasks, and what the Postgres health probes were doing.
///
/// Read `verdict=` first. Do **not** read `stage=` on its own: the kernel
/// completes the TCP handshake from the listen backlog without the accept loop
/// running, so a wedged runtime and a database-blocked handler both surface as
/// `no_response`. `verdict=` is the field that separates them, by combining
/// `stage=` with the runtime beacon (`runtime=`) and `db_in_flight=`. The
/// accompanying `sample` dump cannot settle it either — a task parked in an
/// `await` is not a running thread and does not appear in a thread sample.
///
/// [`crate::services::hang_forensics::verdict`] tabulates all seven values and
/// the two caveats they cannot carry: `runtime=scheduling` means **one** of
/// `runtime_workers=` was free, and `db_in_flight=` is process-wide, so
/// `handler_blocked_on_db` names *a* stuck health request, not this one.
///
/// Must be called from inside the tokio runtime it is meant to watch: the first
/// thing it does is arm the runtime-liveness beacon, which needs a runtime
/// handle.
pub fn spawn_watchdog(port: u16) {
    // #5147: arm the beacon HERE rather than at the boot site, and prove the
    // order with a value rather than with a comment or a source-text guard.
    // `spawn_watchdog_thread` takes the `BeaconArmed` token by value and there
    // is no other way to obtain one, so "armed before the thread exists" is a
    // data dependency the compiler checks: deleting the arming, or moving it
    // inside the spawned closure, does not compile. Must be called from inside
    // the tokio runtime being watched; off a runtime the token says so rather
    // than panicking.
    let armed = crate::services::hang_forensics::spawn_runtime_liveness_beacon();
    spawn_watchdog_thread(port, armed);
}

/// Creates the watchdog's OS thread. Private, and takes the beacon proof by
/// value — see [`spawn_watchdog`].
fn spawn_watchdog_thread(port: u16, armed: crate::services::hang_forensics::BeaconArmed) {
    // A beacon that did not arm is not fatal -- `verdict` degrades to
    // `undetermined_no_beacon` -- but it must not be silent either, because
    // every later kill line then concludes nothing and the next investigation
    // is back where #4756/#4770/#5147 were.
    match armed.boot_report() {
        Ok(line) => tracing::info!("{line}"),
        Err(line) => tracing::error!("{line}"),
    }

    std::thread::Builder::new()
        .name("health-watchdog".into())
        .spawn(move || {
            std::thread::sleep(STARTUP_GRACE);

            let mut consecutive_failures: u32 = 0;

            loop {
                std::thread::sleep(CHECK_INTERVAL);

                // #5147: classify *where* the probe stopped instead of
                // collapsing it to a bool. `connect_failed` (nothing accepted
                // the socket) and `no_response` (accepted, but the handler
                // never answered) have opposite root causes, and the bool lost
                // that distinction — which is why three investigations could
                // not explain these kills from the dump alone.
                let loopback = crate::config::loopback();
                let outcome = crate::services::hang_forensics::probe_health_once(
                    &format!("{loopback}:{port}"),
                    &loopback,
                    TCP_TIMEOUT,
                );

                if outcome.is_ok() {
                    if consecutive_failures > 0 {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 🩺 watchdog: health recovered after {consecutive_failures} failure(s)"
                        );
                    }
                    consecutive_failures = 0;
                } else {
                    consecutive_failures += 1;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    // #5147: what the runtime and the health path's Postgres
                    // awaits were doing at the moment the probe gave up. The
                    // `sample` dump this eventually captures cannot carry
                    // either: a task parked in an `await` is not a running
                    // thread and does not appear in a thread sample at all.
                    let probe = outcome.render();
                    // Snapshot once and derive the verdict from that same
                    // snapshot: re-reading would let the beacon tick in between
                    // and describe a runtime state the probe never saw.
                    let snapshot = crate::services::hang_forensics::snapshot();
                    let probe = format!(
                        "verdict={} {probe}",
                        crate::services::hang_forensics::verdict(&outcome, &snapshot)
                    );
                    let crumbs = snapshot.render();
                    tracing::warn!(
                        "  [{ts}] 🩺 watchdog: health check failed ({consecutive_failures}/{MAX_FAILURES}) {probe} {crumbs}"
                    );
                    if consecutive_failures >= MAX_FAILURES {
                        tracing::warn!(
                            "  [{ts}] 🩺 watchdog: runtime unresponsive — capturing diagnostics before exit {probe} {crumbs}"
                        );
                        // Capture process dump for post-mortem analysis (platform-aware)
                        // Write to runtime root's logs/ dir so dumps survive /tmp cleanup
                        let pid = std::process::id();
                        let dump_dir = crate::agentdesk_runtime_root()
                            .map(|r| r.join("logs"))
                            .unwrap_or_else(|| std::env::temp_dir());
                        let _ = std::fs::create_dir_all(&dump_dir);
                        let dump_path = format!(
                            "{}/adk-hang-{}-{}.txt",
                            dump_dir.display(),
                            pid,
                            chrono::Local::now().format("%Y%m%d-%H%M%S")
                        );
                        match crate::services::platform::capture_process_dump(pid, &dump_path) {
                            Ok(()) => tracing::warn!(
                                "  [{ts}] 🩺 watchdog: dump saved to {dump_path} — forcing exit"
                            ),
                            Err(e) => tracing::warn!(
                                "  [{ts}] 🩺 watchdog: dump capture failed ({e}) — forcing exit without diagnostics"
                            ),
                        }
                        std::process::exit(1);
                    }
                }
            }
        })
        .expect("Failed to spawn watchdog thread");
}
