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

/// The three artifact-writing side effects the post-barrier decision can
/// perform. Injected rather than called directly so a test can hold the skip
/// write open at the exact instant the standby path registers, and observe which
/// writer ran, without a real artifact write or a loopback HTTP server
/// (#5071 S0 r2 F1).
#[async_trait::async_trait]
pub(super) trait StartupDoctorEffects: Send + Sync + 'static {
    async fn record_skip(&self);
    async fn run_now(&self);
    async fn upgrade_skip(&self);
}

struct ProcessStartupDoctorEffects {
    api_port: u16,
}

#[async_trait::async_trait]
impl StartupDoctorEffects for ProcessStartupDoctorEffects {
    async fn record_skip(&self) {
        record_startup_diagnostic_skip().await;
    }

    async fn run_now(&self) {
        run_startup_diagnostic_now(self.api_port).await;
    }

    async fn upgrade_skip(&self) {
        upgrade_skipped_startup_diagnostic(self.api_port).await;
    }
}

/// Barrier arrival that is NOT a provider finishing its own reconcile: a launch
/// skip, a standby lease, or a gateway backend that ended. §7.2-4's tag reads
/// `unknown` for these rather than implying a reconcile that never ran.
pub(super) async fn run_startup_diagnostic_after_reconcile_barrier(
    remaining: Arc<std::sync::atomic::AtomicUsize>,
    started: Arc<std::sync::atomic::AtomicBool>,
    health_registry: Arc<health::HealthRegistry>,
    api_port: u16,
) {
    // The rearm handle is deliberately dropped: the watcher is detached (see
    // `spawn_startup_doctor_rearm`). Only tests await it.
    let _rearm = run_startup_diagnostic_after_reconcile_barrier_with(
        None,
        remaining,
        started,
        health_registry,
        Arc::new(ProcessStartupDoctorEffects { api_port }),
    )
    .await;
}

/// Barrier arrival for the provider whose reconcile just completed (#5462 S5
/// §7.2-4). `waiting for N provider reconcile(s)` counted the outstanding ones
/// and named no arrival at all, so the waiting lines said nothing about who had
/// gotten there.
///
/// What the tag answers is narrower than "which provider is missing": the
/// arrivals that are NOT a provider reconcile join as `unknown` (see
/// `run_startup_diagnostic_after_reconcile_barrier`), so a boot stuck at N=1
/// still cannot separate "X never arrived" from "X arrived as a skip". Naming
/// those would mean tagging a reconcile that never ran, which is the confusion
/// this tag exists to avoid; the residual is deliberate.
pub(super) async fn run_startup_diagnostic_after_reconcile_barrier_for_provider(
    provider: &ProviderKind,
    remaining: Arc<std::sync::atomic::AtomicUsize>,
    started: Arc<std::sync::atomic::AtomicBool>,
    health_registry: Arc<health::HealthRegistry>,
    api_port: u16,
) {
    let _rearm = run_startup_diagnostic_after_reconcile_barrier_with(
        Some(provider.as_str().to_string()),
        remaining,
        started,
        health_registry,
        Arc::new(ProcessStartupDoctorEffects { api_port }),
    )
    .await;
}

/// Returns the rearm watcher's handle when the skip branch armed one, so a test
/// can await the watcher instead of racing it.
///
/// `arriving_provider` is owned rather than borrowed because this future is
/// spawned, so a borrowed tag would not be `'static`.
async fn run_startup_diagnostic_after_reconcile_barrier_with(
    arriving_provider: Option<String>,
    remaining: Arc<std::sync::atomic::AtomicUsize>,
    started: Arc<std::sync::atomic::AtomicBool>,
    health_registry: Arc<health::HealthRegistry>,
    effects: Arc<dyn StartupDoctorEffects>,
) -> Option<tokio::task::JoinHandle<()>> {
    match startup_doctor_barrier_arrive(&remaining, &started) {
        StartupDoctorBarrier::Waiting(waiting) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                provider = arriving_provider.as_deref().unwrap_or("unknown"),
                "  [{ts}] ⏳ startup_doctor waiting for {waiting} provider reconcile(s)"
            );
            return None;
        }
        StartupDoctorBarrier::AlreadyReleased => return None,
        StartupDoctorBarrier::Released => {}
    }

    // #5071 S0 r2 F1: read the rearm baseline BEFORE the count observation
    // below, and never re-read it afterwards. Both the count observation and the
    // skip write are await points, so a baseline captured after either of them
    // can absorb the very registration the rearm exists to notice — the poll
    // then sees `current == recorded` for the whole window, gives up, and this
    // boot's stale no-provider skip becomes permanent even though a provider
    // runtime is registered. Captured first, a registration landing in either
    // gap is either already visible in the count (the diagnostic runs now) or
    // strictly ahead of this baseline (the rearm fires).
    let baseline_generation = health_registry.registration_generation();

    if health_registry.registered_provider_count().await == 0 {
        health::note_startup_doctor_saw_empty_registry();
        effects.record_skip().await;
        // #5449: on the standby path the registry is empty here by static
        // ordering, not by race — the lease branch awaits this call and only
        // then calls `register_standby`, so "no providers" is not yet final. The
        // skip above stays this boot's immediate artifact because deploy
        // readiness reads its `skipped_reason`; the rearm below replaces it with
        // a real report if a provider runtime does register.
        return Some(spawn_startup_doctor_rearm(
            health_registry,
            effects,
            baseline_generation,
        ));
    }

    effects.run_now().await;
    None
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
/// released with an empty registry. Deliberately the same constant as the
/// reconcile-stall promotion so both bounds are tuned in one place (#5449).
///
/// #5071 S0 r2 F4: that is a shared bound, NOT a claim that this window closes
/// before health can name an unfinished reconcile as stalled. The stall age is
/// measured from the provider's `SharedData` construction and this window from
/// the barrier's release, so the two can interleave; see
/// `health::RECONCILE_STALL_AFTER` for what the pairing does guarantee.
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
///
/// `recorded_generation` is supplied by the caller instead of read here: it must
/// pre-date the empty-registry observation that decided to skip, and this
/// function runs after both that observation and the skip write (#5071 S0 r2 F1).
fn spawn_startup_doctor_rearm(
    health_registry: Arc<health::HealthRegistry>,
    effects: Arc<dyn StartupDoctorEffects>,
    recorded_generation: u64,
) -> tokio::task::JoinHandle<()> {
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
                    effects.upgrade_skip().await;
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
    })
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
    use super::{
        STARTUP_DOCTOR_REARM_WINDOW, StartupDoctorRearm, health, startup_doctor_rearm_due,
    };
    use std::sync::Arc;
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

    /// Fake effects that park inside the skip write. `record_skip` announces that
    /// the empty-registry count has already been observed and then blocks until
    /// the test releases it, which reproduces the exact window the standby lease
    /// branch registers in; `run_now` / `upgrade_skip` only record that they ran,
    /// so no artifact is written and no loopback HTTP server is needed.
    struct PausedSkipEffects {
        entered_skip: Arc<tokio::sync::Notify>,
        release_skip: Arc<tokio::sync::Notify>,
        ran_now: Arc<AtomicUsize>,
        upgraded: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl super::StartupDoctorEffects for PausedSkipEffects {
        async fn record_skip(&self) {
            self.entered_skip.notify_one();
            self.release_skip.notified().await;
        }

        async fn run_now(&self) {
            self.ran_now.fetch_add(1, Ordering::AcqRel);
        }

        async fn upgrade_skip(&self) {
            self.upgraded.fetch_add(1, Ordering::AcqRel);
        }
    }

    // T-S0-4 (#5071 S0 r2 F1): a registration that completes AFTER the
    // empty-registry count was observed but BEFORE the skip write returns must
    // still rearm the doctor. The generation baseline is captured ahead of the
    // count observation precisely so this registration stays strictly ahead of
    // it; a baseline captured at rearm-spawn time absorbs it, holds for the whole
    // window, and leaves the stale no-provider skip in place forever.
    #[tokio::test(start_paused = true)]
    async fn a_registration_during_the_skip_write_still_rearms_the_doctor() {
        let registry = Arc::new(health::HealthRegistry::new());
        let entered_skip = Arc::new(tokio::sync::Notify::new());
        let release_skip = Arc::new(tokio::sync::Notify::new());
        let ran_now = Arc::new(AtomicUsize::new(0));
        let upgraded = Arc::new(AtomicUsize::new(0));

        let barrier = tokio::spawn(super::run_startup_diagnostic_after_reconcile_barrier_with(
            Some("claude".to_string()),
            Arc::new(AtomicUsize::new(1)),
            Arc::new(AtomicBool::new(false)),
            registry.clone(),
            Arc::new(PausedSkipEffects {
                entered_skip: entered_skip.clone(),
                release_skip: release_skip.clone(),
                ran_now: ran_now.clone(),
                upgraded: upgraded.clone(),
            }),
        ));

        // The barrier has released and read the registry as empty; the skip
        // artifact is mid-write.
        entered_skip.notified().await;
        registry
            .register_standby(
                "codex".to_string(),
                crate::services::discord::make_shared_data_for_tests(),
            )
            .await;
        assert!(registry.registration_generation() > 0);
        release_skip.notify_one();

        let rearm = barrier
            .await
            .expect("barrier task")
            .expect("the skip branch arms a rearm watcher");
        // The clock is paused, so a watcher that decided to Hold burns its whole
        // window in virtual time and returns here without upgrading — which the
        // assertion below is what catches.
        rearm.await.expect("rearm task");

        assert_eq!(
            upgraded.load(Ordering::Acquire),
            1,
            "a registration accepted while the skip was being written must replace the skip"
        );
        assert_eq!(
            ran_now.load(Ordering::Acquire),
            0,
            "the count was observed as empty, so the immediate diagnostic must not have run"
        );
    }
}
