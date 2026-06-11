use super::*;

/// Spawn the SIGTERM graceful-shutdown handler. On SIGTERM it persists queue /
/// inflight / last_message state then quick-exits; tmux/TUI processes survive
/// for the next dcserver instance to rehydrate. Spawned after the lease
/// keepalive task and before the gateway backend run.
pub(super) fn run_bot_spawn_sigterm_handler(
    shared: &Arc<SharedData>,
    provider_for_shutdown: ProviderKind,
) {
    let shared_for_signal = shared.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
                sigterm.recv().await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] 🛑 SIGTERM received — graceful shutdown");

                // Set global shutdown flag
                shared_for_signal
                    .shutting_down
                    .store(true, std::sync::atomic::Ordering::SeqCst);

                // Block dequeue and put router into drain mode so no new
                // queue/checkpoint mutations occur during shutdown.
                shared_for_signal
                    .restart_pending
                    .store(true, std::sync::atomic::Ordering::SeqCst);

                // ── Critical state persistence (MUST run before any I/O) ──
                // Save pending queues and last_message_ids FIRST, before any
                // network calls that might block/timeout and prevent saving.

                let drain =
                    mailbox_restart_drain_all(&shared_for_signal, &provider_for_shutdown).await;
                let queue_count = drain.queued_count;
                if !drain.persistence_errors.is_empty() {
                    tracing::error!(
                        failures = drain.persistence_errors.len(),
                        "SIGTERM initial drain observed pending-queue persistence failure(s)"
                    );
                }
                if queue_count > 0 {
                    let ts3 = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts3}] 📋 mailbox persisted {queue_count} pending queue item(s)"
                    );
                }

                // Persist last_message_ids for catch-up polling after restart
                {
                    let ids: std::collections::HashMap<u64, u64> = shared_for_signal
                        .last_message_ids
                        .iter()
                        .map(|entry| (entry.key().get(), *entry.value()))
                        .collect();
                    if !ids.is_empty() {
                        runtime_store::save_all_last_message_ids(
                            provider_for_shutdown.as_str(),
                            &ids,
                        );
                    }
                }

                // ── Inflight state preservation for silent re-attach ──
                let inflight_states = inflight::load_inflight_states(&provider_for_shutdown);
                if !inflight_states.is_empty() {
                    let ts2 = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts2}] 👁 preserving {} inflight turn(s) for restart recovery",
                        inflight_states.len()
                    );
                    let marked = inflight::mark_all_inflight_states_restart_mode(
                        &provider_for_shutdown,
                        crate::services::discord::InflightRestartMode::DrainRestart,
                    );
                    tracing::info!(
                        "  [{ts2}] 🔖 marked {marked} inflight turn(s) as drain_restart"
                    );
                }

                // ── Final state snapshot (belt-and-suspenders) ──
                // During the HTTP placeholder edits above, active turns may have
                // finished and mutated queues/last_message_ids. Re-save to capture
                // any changes that occurred after the initial save.
                {
                    let drain =
                        mailbox_restart_drain_all(&shared_for_signal, &provider_for_shutdown).await;
                    let queue_count = drain.queued_count;
                    if !drain.persistence_errors.is_empty() {
                        tracing::error!(
                            failures = drain.persistence_errors.len(),
                            "SIGTERM final drain observed pending-queue persistence failure(s)"
                        );
                    }
                    if queue_count > 0 {
                        let ts4 = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts4}] 📋 mailbox final drain: {queue_count} pending queue item(s)"
                        );
                    }
                }
                {
                    let ids: std::collections::HashMap<u64, u64> = shared_for_signal
                        .last_message_ids
                        .iter()
                        .map(|entry| (entry.key().get(), *entry.value()))
                        .collect();
                    if !ids.is_empty() {
                        runtime_store::save_all_last_message_ids(
                            provider_for_shutdown.as_str(),
                            &ids,
                        );
                    }
                }

                // Wait for all providers to finish saving before exiting.
                // CAS guard: skip if this provider already decremented via deferred restart path.
                if shared_for_signal
                    .shutdown_counted
                    .compare_exchange(
                        false,
                        true,
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    if shared_for_signal
                        .shutdown_remaining
                        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
                        == 1
                    {
                        std::process::exit(0);
                    }
                }
            }
        }
    });
}

/// Run the Discord gateway backend (`client.start()`) to completion, classify
/// the exit, run the post-reconcile startup diagnostic on failure, then abort
/// and join the gateway-lease keepalive task. This is the final event-loop
/// entry of run_bot. Consumes `client`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_bot_run_gateway_backend(
    mut client: serenity::Client,
    provider_for_error: &ProviderKind,
    gateway_lease_task: Option<tokio::task::JoinHandle<()>>,
    startup_reconcile_remaining_for_client_start: Arc<std::sync::atomic::AtomicUsize>,
    startup_doctor_started_for_client_start: Arc<std::sync::atomic::AtomicBool>,
    health_registry_for_client_start: Arc<health::HealthRegistry>,
    api_port: u16,
) {
    let gateway_backend_task = tokio::spawn(async move { client.start().await });
    let gateway_backend_failed = match gateway_backend_task.await {
        Ok(Ok(())) => {
            tracing::warn!(
                "  ✗ {} gateway backend exited without error",
                provider_for_error.display_name()
            );
            true
        }
        Ok(Err(error)) => {
            tracing::warn!(
                "  ✗ {} bot error: {error}",
                provider_for_error.display_name()
            );
            true
        }
        Err(join_error) if join_error.is_panic() => {
            tracing::error!(
                "  ✗ {} gateway backend task panicked: {join_error}",
                provider_for_error.display_name()
            );
            true
        }
        Err(join_error) => {
            tracing::warn!(
                "  ✗ {} gateway backend task ended unexpectedly: {join_error}",
                provider_for_error.display_name()
            );
            true
        }
    };
    if gateway_backend_failed {
        run_startup_diagnostic_after_reconcile_barrier(
            startup_reconcile_remaining_for_client_start,
            startup_doctor_started_for_client_start,
            health_registry_for_client_start,
            api_port,
        )
        .await;
    }

    if let Some(handle) = gateway_lease_task {
        handle.abort();
        let _ = handle.await;
    }
}
