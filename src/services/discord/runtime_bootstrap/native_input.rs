//! Native boot custody settles before this runtime opens any intake surface.
use super::*;

/// Retry interval while a native boot restore is held.
#[cfg(unix)]
const NATIVE_RESTORE_RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// Repeat interval of the held-restore warning.
#[cfg(unix)]
const NATIVE_RESTORE_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// One native boot preparation attempt.
#[cfg(unix)]
#[derive(Debug)]
pub(super) enum NativeBoot {
    Ready,
    /// Restore did not finish; rows and custody are untouched, so the next attempt
    /// re-reads the same records (e.g. "native control history busy").
    RestoreHeld(String),
    /// Preparation or the boot reaper refused; retried only by a new process.
    Refused(String),
}

pub(in crate::services::discord::runtime_bootstrap) async fn prepare_native_boot(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) -> bool {
    #[cfg(unix)]
    if *provider == ProviderKind::Claude
        && super::super::super::turn_finalizer::native_prepare::enabled()
    {
        use super::super::super::turn_finalizer::native_prepare::BootInputs;
        let attempt = || async move {
            let prepared = crate::services::discord::runtime_store::runtime_root()
                .ok_or_else(|| "native boot root unavailable".to_string())
                .and_then(|root| BootInputs::prepare(&root, shared.restart.current_generation));
            let plan = match prepared {
                Ok(plan) => plan,
                Err(error) => {
                    tracing::warn!(%error, "native boot preparation refused; rows and custody retained");
                    return NativeBoot::Refused(error);
                }
            };
            // The reaper runs once per process; a retry reads its cached report.
            let reaped = super::super::super::inflight::reap_inflight_rows_at_boot_blocking(
                provider,
                shared.pg_pool.clone(),
            )
            .await;
            if !reaped.native_preparation_complete() {
                tracing::warn!("native boot reaper preparation incomplete; restore kept pending");
                return NativeBoot::Refused("reaper preparation incomplete".into());
            }
            match plan.restore(shared).await {
                Ok(()) => NativeBoot::Ready,
                Err(error) => NativeBoot::RestoreHeld(error),
            }
        };
        return await_native_boot(
            provider,
            &shared.restart.shutting_down,
            &shared.restart.shutdown_counted,
            &shared.restart.shutdown_remaining,
            NATIVE_RESTORE_RETRY_INTERVAL,
            attempt,
        )
        .await;
    }
    super::super::super::inflight::reap_inflight_rows_at_boot_blocking(
        provider,
        shared.pg_pool.clone(),
    )
    .await;
    true
}

/// Repeats `attempt` while restore is held, so a transient hold cannot drop the runtime and its
/// gateway waiter; outcomes go to health. `true` once ready, `false` on refusal or shutdown.
#[cfg(unix)]
pub(super) async fn await_native_boot<F, Fut>(
    provider: &ProviderKind,
    shutting_down: &std::sync::atomic::AtomicBool,
    shutdown_counted: &std::sync::atomic::AtomicBool,
    shutdown_remaining: &std::sync::atomic::AtomicUsize,
    interval: Duration,
    mut attempt: F,
) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = NativeBoot>,
{
    use crate::services::discord::health::boot_hold;
    let mut held = 0u64;
    let mut next_warning = tokio::time::Instant::now();
    loop {
        let error = match attempt().await {
            NativeBoot::Ready => {
                boot_hold::release(provider.as_str());
                if held > 0 {
                    tracing::info!(
                        provider = provider.as_str(),
                        held,
                        "native boot restore completed after retry; runtime startup resumes"
                    );
                }
                return true;
            }
            NativeBoot::Refused(error) => {
                boot_hold::hold(provider.as_str(), "native_boot_refused", &error);
                return false;
            }
            NativeBoot::RestoreHeld(error) => error,
        };
        held += 1;
        boot_hold::hold(provider.as_str(), "native_boot_restore_held", &error);
        let now = tokio::time::Instant::now();
        if now >= next_warning {
            next_warning = now + NATIVE_RESTORE_WARN_INTERVAL;
            tracing::warn!(
                provider = provider.as_str(),
                %error,
                held,
                retry_secs = interval.as_secs(),
                "native boot restore held before intake; retrying (runtime and gateway lease wait)"
            );
        }
        tokio::time::sleep(interval).await;
        if shutting_down.load(Ordering::SeqCst) {
            // Same once-only slot protocol as the SIGTERM handler and deferred restart.
            if shutdown_counted
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                shutdown_remaining.fetch_sub(1, Ordering::AcqRel);
            }
            return false;
        }
    }
}

#[cfg(all(test, unix))]
#[path = "native_input_tests.rs"]
mod tests;
