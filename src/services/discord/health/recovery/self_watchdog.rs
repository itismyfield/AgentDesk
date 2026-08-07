//! Process self-watchdog: the thread that force-exits `dcserver` when the HTTP
//! runtime stops answering.
//!
//! #5147 moved this out of `health/recovery.rs` unchanged, for two reasons.
//!
//! **Cohesion.** `recovery.rs` is about recovering *channels* — watchers,
//! mailboxes, stalled turns. This is about the *process*, it shares no state
//! with anything else in that file, and it runs on its own OS thread precisely
//! so it depends on nothing the rest of the module touches.
//!
//! **Reach.** `CHECK_INTERVAL`, `TCP_TIMEOUT` and `MAX_FAILURES` are
//! function-local `const`s, so no other module can name them. The follow-up
//! that hangs a runtime-liveness threshold off `TCP_TIMEOUT` needs to assert
//! against the value; at module scope it can import it instead of grepping this
//! file's source text for the declaration.
//!
//! #5147 also arms `services::hang_forensics`' runtime-liveness beacon here
//! rather than at the boot site: `spawn_watchdog` arms it and hands the
//! resulting `BeaconArmed` token to `spawn_watchdog_thread`, so "armed before
//! the thread exists" is a data dependency the compiler checks rather than a
//! comment a reviewer has to believe. Its breadcrumbs are printed on every
//! failure.

/// Self-watchdog: runs on a dedicated OS thread (not tokio) to detect
/// runtime hangs.  Periodically opens a raw TCP connection to the server
/// port and expects a response within a few seconds.  If the check fails
/// `max_failures` times in a row the process is force-killed so launchd
/// (or systemd) can restart it.
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

    const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
    const TCP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    const MAX_FAILURES: u32 = 3;
    // Grace period: skip checks for the first 30s after startup so the
    // runtime has time to initialise Discord bots and register providers.
    const STARTUP_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

    std::thread::Builder::new()
        .name("health-watchdog".into())
        .spawn(move || {
            std::thread::sleep(STARTUP_GRACE);

            let mut consecutive_failures: u32 = 0;

            loop {
                std::thread::sleep(CHECK_INTERVAL);

                let ok = (|| -> bool {
                    use std::io::{Read, Write};
                    let loopback = crate::config::loopback();
                    let addr = format!("{loopback}:{port}");
                    let mut stream =
                        match std::net::TcpStream::connect_timeout(
                            &addr.parse().unwrap(),
                            TCP_TIMEOUT,
                        ) {
                            Ok(s) => s,
                            Err(_) => return false,
                        };
                    let _ = stream.set_read_timeout(Some(TCP_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(TCP_TIMEOUT));
                    let req = format!("GET /api/health HTTP/1.1\r\nHost: {loopback}\r\nConnection: close\r\n\r\n");
                    if stream.write_all(req.as_bytes()).is_err() {
                        return false;
                    }
                    let mut buf = [0u8; 512];
                    match stream.read(&mut buf) {
                        Ok(n) if n > 0 => {
                            // Any HTTP response means the process is alive and serving.
                            // Only TCP failure (Err/_) indicates a true hang/deadlock.
                            // A 503 (degraded/unhealthy state) still means the runtime is
                            // responsive — killing it would create an infinite crash loop
                            // when a provider is temporarily disconnected.
                            true
                        }
                        _ => false,
                    }
                })();

                if ok {
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
                    let crumbs = crate::services::hang_forensics::snapshot().render();
                    tracing::warn!(
                        "  [{ts}] 🩺 watchdog: health check failed ({consecutive_failures}/{MAX_FAILURES}) {crumbs}"
                    );
                    if consecutive_failures >= MAX_FAILURES {
                        tracing::warn!(
                            "  [{ts}] 🩺 watchdog: runtime unresponsive — capturing diagnostics before exit {crumbs}"
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
