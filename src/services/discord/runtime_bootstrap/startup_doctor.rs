use super::super::*;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum StartupDoctorBarrier {
    Waiting(usize),
    Released,
    AlreadyReleased,
}

pub(super) fn startup_doctor_barrier_arrive(
    remaining: &std::sync::atomic::AtomicUsize,
    started: &std::sync::atomic::AtomicBool,
) -> StartupDoctorBarrier {
    let mut current = remaining.load(Ordering::Acquire);
    loop {
        if current == 0 {
            return StartupDoctorBarrier::AlreadyReleased;
        }
        let next = current - 1;
        match remaining.compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) if next > 0 => return StartupDoctorBarrier::Waiting(next),
            Ok(_) => {
                return match started.compare_exchange(
                    false,
                    true,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => StartupDoctorBarrier::Released,
                    Err(_) => StartupDoctorBarrier::AlreadyReleased,
                };
            }
            Err(observed) => current = observed,
        }
    }
}

/// Maximum time the startup_doctor will wait for the local HTTP server to
/// finish binding before it begins running self-probe checks. Without this
/// gate, every fresh boot races the doctor against axum's `bind` call and
/// latches a permanent `unhealthy` artifact via cascading Connection-refused
/// failures (see issue #2096).
pub(super) const STARTUP_DOCTOR_HTTP_BIND_TIMEOUT: Duration = Duration::from_secs(30);
const STARTUP_DOCTOR_HTTP_BIND_POLL_INTERVAL: Duration = Duration::from_millis(200);
const STARTUP_DOCTOR_HTTP_BIND_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Poll the loopback HTTP server until it accepts a TCP connection or the
/// deadline expires. We deliberately probe the raw TCP bind rather than an
/// HTTP route so this gate is independent of which routes are mounted by the
/// time the doctor wants to run.
pub(super) async fn wait_for_local_http_bind(api_port: u16) {
    let start = tokio::time::Instant::now();
    let addr = format!("127.0.0.1:{api_port}");
    loop {
        if let Ok(Ok(_stream)) = tokio::time::timeout(
            STARTUP_DOCTOR_HTTP_BIND_PROBE_TIMEOUT,
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let elapsed_ms = start.elapsed().as_millis();
            tracing::info!("  [{ts}] ✓ startup_doctor http bind ready ({addr}, {elapsed_ms}ms)");
            return;
        }
        if start.elapsed() >= STARTUP_DOCTOR_HTTP_BIND_TIMEOUT {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ startup_doctor http bind not observed within {:?} ({addr}) — running anyway",
                STARTUP_DOCTOR_HTTP_BIND_TIMEOUT
            );
            return;
        }
        tokio::time::sleep(STARTUP_DOCTOR_HTTP_BIND_POLL_INTERVAL).await;
    }
}

pub(super) async fn run_startup_diagnostic_after_reconcile_barrier(
    remaining: Arc<std::sync::atomic::AtomicUsize>,
    started: Arc<std::sync::atomic::AtomicBool>,
    health_registry: Arc<health::HealthRegistry>,
    api_port: u16,
) {
    match startup_doctor_barrier_arrive(&remaining, &started) {
        StartupDoctorBarrier::Waiting(waiting) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏳ startup_doctor waiting for {waiting} provider reconcile(s)"
            );
            return;
        }
        StartupDoctorBarrier::AlreadyReleased => return,
        StartupDoctorBarrier::Released => {}
    }

    if health_registry.registered_provider_count().await == 0 {
        health::note_startup_doctor_saw_empty_registry();
        record_startup_diagnostic_skip().await;
        // #5449: on the standby path the registry is empty here by static
        // ordering, not by race — the lease branch awaits this call and only
        // then calls `register_standby`, so "no providers" is not yet final. The
        // skip above stays this boot's immediate artifact because deploy
        // readiness reads its `skipped_reason`; the rearm below replaces it with
        // a real report if a provider runtime does register.
        spawn_startup_doctor_rearm(health_registry, api_port);
        return;
    }

    run_startup_diagnostic_now(api_port).await;
}

async fn record_startup_diagnostic_skip() {
    let ts = chrono::Local::now().format("%H:%M:%S");
    let startup_doctor = tokio::task::spawn_blocking(|| {
        crate::cli::doctor::startup::record_startup_diagnostic_skipped(
            crate::cli::doctor::startup::NO_PROVIDER_RUNTIMES_SKIP_REASON,
        )
    })
    .await;
    match startup_doctor {
        Ok(Ok(Some(path))) => {
            tracing::info!(
                "  [{ts}] ⏭ startup_doctor skipped — no provider runtimes registered; wrote {}",
                path.display()
            );
        }
        Ok(Ok(None)) => {
            tracing::info!(
                "  [{ts}] ⏭ startup_doctor skipped — no provider runtimes registered; already recorded for this boot"
            );
        }
        Ok(Err(error)) => {
            tracing::warn!("  [{ts}] ⚠ startup_doctor skipped but artifact write failed: {error}");
        }
        Err(error) => {
            tracing::warn!("  [{ts}] ⚠ startup_doctor skipped but artifact task failed: {error}");
        }
    }
}

async fn run_startup_diagnostic_now(api_port: u16) {
    // #2096: the doctor's `server` / `discord_bot` / `health_*` checks all
    // hit the loopback HTTP server. If we run before axum binds the port we
    // latch six cascading Connection-refused failures into the artifact and
    // every subsequent `/api/health` call returns 503 until the next boot.
    wait_for_local_http_bind(api_port).await;

    let startup_doctor =
        tokio::task::spawn_blocking(crate::cli::doctor::startup::run_startup_diagnostic_once).await;
    match startup_doctor {
        Ok(Ok(Some(path))) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ✓ startup_doctor wrote {}", path.display());
        }
        Ok(Ok(None)) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ✓ startup_doctor already recorded for this boot");
        }
        Ok(Err(error)) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ startup_doctor_failed: {error}");
        }
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ startup_doctor_failed: {error}");
        }
    }
}

/// How long a boot keeps watching for a provider registration after the barrier
/// released with an empty registry. Same deadline as the reconcile-stall
/// promotion, so the rearm window closes no later than the point at which health
/// starts naming an unfinished reconcile as stalled (#5449).
const STARTUP_DOCTOR_REARM_WINDOW: Duration = health::RECONCILE_STALL_AFTER;
const STARTUP_DOCTOR_REARM_POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum StartupDoctorRearm {
    /// A registration was accepted after the skip decision: run the diagnostic.
    Rearm,
    /// Nothing new registered and the window is still open.
    Hold,
    /// The window closed with nothing registered: the recorded skip stands.
    GiveUp,
}

/// Decide whether a barrier that already released with an empty provider
/// registry should still run the startup diagnostic.
///
/// Window expiry is checked FIRST so the caller's poll loop terminates on
/// `elapsed` alone: a generation that keeps moving cannot hold the loop open
/// past the window. Pure so all three outcomes are testable without a runtime.
pub(super) fn startup_doctor_rearm_due(
    recorded_generation: u64,
    current_generation: u64,
    elapsed: Duration,
) -> StartupDoctorRearm {
    if elapsed >= STARTUP_DOCTOR_REARM_WINDOW {
        return StartupDoctorRearm::GiveUp;
    }
    if current_generation > recorded_generation {
        return StartupDoctorRearm::Rearm;
    }
    StartupDoctorRearm::Hold
}

/// Watch for a provider registration that lands after the barrier released with
/// an empty registry, and upgrade the recorded skip into the diagnostic that
/// registration deserves.
///
/// Detached on purpose. Every caller of
/// `run_startup_diagnostic_after_reconcile_barrier` awaits it, and on the
/// standby path the registration this waits for is issued by that same caller
/// after we return — waiting inline would block the wait against its own
/// precondition. The `started` CAS is left alone: this reuses the barrier's
/// single release rather than re-opening it, and because the barrier releases at
/// most once per process, at most one rearm task exists per boot.
fn spawn_startup_doctor_rearm(health_registry: Arc<health::HealthRegistry>, api_port: u16) {
    let recorded_generation = health_registry.registration_generation();
    tokio::spawn(async move {
        let start = tokio::time::Instant::now();
        loop {
            match startup_doctor_rearm_due(
                recorded_generation,
                health_registry.registration_generation(),
                start.elapsed(),
            ) {
                StartupDoctorRearm::Rearm
                    if health_registry.registered_provider_count().await > 0 =>
                {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 🔁 startup_doctor rearmed — a provider runtime registered after the reconcile barrier released"
                    );
                    upgrade_skipped_startup_diagnostic(api_port).await;
                    return;
                }
                StartupDoctorRearm::GiveUp => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ⏭ startup_doctor rearm window closed with no provider runtime registered — the recorded skip stands"
                    );
                    return;
                }
                // `Rearm` with a still-empty registry falls through and keeps
                // waiting; the window check above bounds that wait.
                StartupDoctorRearm::Rearm | StartupDoctorRearm::Hold => {}
            }
            tokio::time::sleep(STARTUP_DOCTOR_REARM_POLL_INTERVAL).await;
        }
    });
}

/// Replace this boot's no-provider skip with a real report once a provider
/// runtime has registered. Same loopback-bind gate and blocking hop as
/// `run_startup_diagnostic_now`; only the writer differs, because this boot
/// already has an artifact that the registration falsified.
async fn upgrade_skipped_startup_diagnostic(api_port: u16) {
    wait_for_local_http_bind(api_port).await;

    let startup_doctor = tokio::task::spawn_blocking(
        crate::cli::doctor::startup::rerun_startup_diagnostic_after_late_registration,
    )
    .await;
    let ts = chrono::Local::now().format("%H:%M:%S");
    match startup_doctor {
        Ok(Ok(Some(path))) => {
            tracing::info!(
                "  [{ts}] ✓ startup_doctor replaced the no-provider skip with {}",
                path.display()
            );
        }
        Ok(Ok(None)) => {
            tracing::info!(
                "  [{ts}] ✓ startup_doctor left this boot's artifact alone — it is no longer the no-provider skip"
            );
        }
        Ok(Err(error)) => {
            tracing::warn!("  [{ts}] ⚠ startup_doctor_failed: {error}");
        }
        Err(error) => {
            tracing::warn!("  [{ts}] ⚠ startup_doctor_failed: {error}");
        }
    }
}

#[cfg(test)]
mod startup_doctor_rearm_tests {
    use super::{STARTUP_DOCTOR_REARM_WINDOW, StartupDoctorRearm, startup_doctor_rearm_due};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    // T-S0-1: a registration accepted after the skip decision rearms the boot's
    // startup diagnostic, and the barrier's one-shot release is not re-opened.
    #[test]
    fn late_registration_rearms_the_released_barrier() {
        let remaining = AtomicUsize::new(1);
        let started = AtomicBool::new(false);
        assert_eq!(
            super::startup_doctor_barrier_arrive(&remaining, &started),
            super::StartupDoctorBarrier::Released
        );

        assert_eq!(
            startup_doctor_rearm_due(3, 4, Duration::from_secs(1)),
            StartupDoctorRearm::Rearm
        );
        // The rearm rides the single release instead of re-opening the barrier:
        // `started` stays latched and a further arrival still sees it consumed.
        assert!(started.load(Ordering::Acquire));
        assert_eq!(
            super::startup_doctor_barrier_arrive(&remaining, &started),
            super::StartupDoctorBarrier::AlreadyReleased
        );
    }

    #[test]
    fn unchanged_generation_inside_the_window_holds() {
        assert_eq!(
            startup_doctor_rearm_due(3, 3, STARTUP_DOCTOR_REARM_WINDOW - Duration::from_secs(1)),
            StartupDoctorRearm::Hold
        );
    }

    #[test]
    fn closed_window_gives_up_even_while_the_generation_moves() {
        assert_eq!(
            startup_doctor_rearm_due(3, 3, STARTUP_DOCTOR_REARM_WINDOW),
            StartupDoctorRearm::GiveUp
        );
        assert_eq!(
            startup_doctor_rearm_due(3, 9, STARTUP_DOCTOR_REARM_WINDOW),
            StartupDoctorRearm::GiveUp
        );
    }
}
