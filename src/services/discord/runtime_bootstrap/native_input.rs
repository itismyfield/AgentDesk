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
    /// Restore did not finish (e.g. "native control history busy"); part of it may be done,
    /// and the next attempt re-reads history, custody and ledger to settle the rest.
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
        // Set by a peer runtime's SIGTERM handler; this runtime has none yet and registers none.
        let sigterm = super::super::boot_sigterm::watch();
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
            shared,
            sigterm,
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
    shared: &Arc<SharedData>,
    mut sigterm: tokio::sync::watch::Receiver<bool>,
    interval: Duration,
    mut attempt: F,
) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = NativeBoot>,
{
    use super::super::boot_sigterm;
    use crate::services::discord::health::boot_hold;
    let stop_requested = |sigterm: &tokio::sync::watch::Receiver<bool>| {
        *sigterm.borrow() || shared.restart.shutting_down.load(Ordering::SeqCst)
    };
    let mut held = 0u64;
    let mut next_warning = tokio::time::Instant::now();
    loop {
        let outcome = attempt().await;
        // An attempt that finished after the shutdown request starts nothing, even when ready.
        if stop_requested(&sigterm) {
            boot_sigterm::stop_before_start(shared, *sigterm.borrow());
            return false;
        }
        let error = match outcome {
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
                boot_sigterm::release_boot_slot(shared, *sigterm.borrow());
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
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            () = boot_sigterm::signalled(&mut sigterm) => {}
        }
        if stop_requested(&sigterm) {
            boot_sigterm::stop_before_start(shared, *sigterm.borrow());
            return false;
        }
    }
}

#[cfg(all(test, unix))]
#[path = "native_input_tests.rs"]
mod tests;
