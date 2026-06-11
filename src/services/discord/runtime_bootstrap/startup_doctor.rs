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
        let ts = chrono::Local::now().format("%H:%M:%S");
        let startup_doctor = tokio::task::spawn_blocking(|| {
            crate::cli::doctor::startup::record_startup_diagnostic_skipped(
                "no_provider_runtimes_registered",
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
                tracing::warn!(
                    "  [{ts}] ⚠ startup_doctor skipped but artifact write failed: {error}"
                );
            }
            Err(error) => {
                tracing::warn!(
                    "  [{ts}] ⚠ startup_doctor skipped but artifact task failed: {error}"
                );
            }
        }
        return;
    }

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
