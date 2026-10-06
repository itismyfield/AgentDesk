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
                shared_for_signal.restart.legacy_sigterm();

                persist_sigterm_state(&shared_for_signal, &provider_for_shutdown).await;

                crate::services::opencode::shutdown_warm_servers();

                // Wait for all providers to finish saving before exiting.
                // CAS guard: skip if this provider already decremented via deferred restart path.
                if shared_for_signal
                    .restart
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
                        .restart
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

/// Persist both restart snapshots around inflight marking without exiting the process.
async fn persist_sigterm_state(
    shared: &SharedData,
    provider: &ProviderKind,
) -> [crate::services::turn_orchestrator::RestartDrainAllResult; 2] {
    persist_sigterm_state_with_boundaries(shared, provider, || {}, || {}).await
}

async fn persist_sigterm_state_with_boundaries(
    shared: &SharedData,
    provider: &ProviderKind,
    after_initial: impl FnOnce(),
    before_final: impl FnOnce(),
) -> [crate::services::turn_orchestrator::RestartDrainAllResult; 2] {
    // Save queues and catch-up checkpoints before network I/O can block shutdown.

    let initial_drain = mailbox_restart_drain_all(shared, provider).await;
    let queue_count = initial_drain.queued_count;
    if !initial_drain.persistence_errors.is_empty() {
        tracing::error!(
            failures = initial_drain.persistence_errors.len(),
            "SIGTERM initial drain observed pending-queue persistence failure(s)"
        );
    }
    if queue_count > 0 {
        let ts3 = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts3}] 📋 mailbox persisted {queue_count} pending queue item(s)");
    }

    // Persist last_message_ids for catch-up polling after restart
    {
        let ids: std::collections::HashMap<u64, u64> = shared
            .last_message_ids
            .iter()
            .map(|entry| (entry.key().get(), *entry.value()))
            .collect();
        if !ids.is_empty() {
            runtime_store::save_all_last_message_ids(provider.as_str(), &ids);
        }
    }

    after_initial();

    // Preserve inflight state for silent re-attach.
    let inflight_states = inflight::load_inflight_states(provider);
    if !inflight_states.is_empty() {
        let ts2 = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts2}] 👁 preserving {} inflight turn(s) for restart recovery",
            inflight_states.len()
        );
        let marked = inflight::mark_all_inflight_states_restart_mode(
            provider,
            crate::services::discord::InflightRestartMode::DrainRestart,
        );
        tracing::info!("  [{ts2}] 🔖 marked {marked} inflight turn(s) as drain_restart");
    }

    before_final();

    // Re-save queues and checkpoints to capture turns finishing during the initial pass.
    let final_drain_block = {
        let final_drain = mailbox_restart_drain_all(shared, provider).await;
        let queue_count = final_drain.queued_count;
        if !final_drain.persistence_errors.is_empty() {
            tracing::error!(
                failures = final_drain.persistence_errors.len(),
                "SIGTERM final drain observed pending-queue persistence failure(s)"
            );
        }
        if queue_count > 0 {
            let ts4 = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts4}] 📋 mailbox final drain: {queue_count} pending queue item(s)");
        }
        final_drain
    };
    {
        let ids: std::collections::HashMap<u64, u64> = shared
            .last_message_ids
            .iter()
            .map(|entry| (entry.key().get(), *entry.value()))
            .collect();
        if !ids.is_empty() {
            runtime_store::save_all_last_message_ids(provider.as_str(), &ids);
        }
    }
    [initial_drain, final_drain_block]
}

async fn abort_and_join_task(handle: Option<tokio::task::JoinHandle<()>>) {
    if let Some(handle) = handle {
        handle.abort();
        let _ = handle.await;
    }
}

async fn release_catalog_before_diagnostic<F>(
    model_catalog_refresh_task: Option<tokio::task::JoinHandle<()>>,
    diagnostic: F,
) where
    F: std::future::Future<Output = ()>,
{
    abort_and_join_task(model_catalog_refresh_task).await;
    diagnostic.await;
}

async fn finish_gateway_backend<E, F>(
    gateway_backend_task: tokio::task::JoinHandle<Result<(), E>>,
    provider_for_error: &ProviderKind,
    gateway_waiter: Option<GatewayWaiterGuard>,
    gateway_lease_task: Option<tokio::task::JoinHandle<()>>,
    model_catalog_refresh_task: Option<tokio::task::JoinHandle<()>>,
    diagnostic: F,
) where
    E: std::fmt::Display,
    F: std::future::Future<Output = ()>,
{
    match gateway_backend_task.await {
        Ok(Ok(())) => {
            tracing::warn!(
                "  ✗ {} gateway backend exited without error",
                provider_for_error.display_name()
            );
        }
        Ok(Err(error)) => {
            tracing::warn!(
                "  ✗ {} bot error: {error}",
                provider_for_error.display_name()
            );
        }
        Err(join_error) if join_error.is_panic() => {
            tracing::error!(
                "  ✗ {} gateway backend task panicked: {join_error}",
                provider_for_error.display_name()
            );
        }
        Err(join_error) => {
            tracing::warn!(
                "  ✗ {} gateway backend task ended unexpectedly: {join_error}",
                provider_for_error.display_name()
            );
        }
    }
    // The backend is gone: close O admission, even against a late re-acquisition, before the
    // lease task is aborted and drops the lease.
    crate::services::tui_o::ownership::gate(provider_for_error.as_str()).close();
    drop(gateway_waiter);
    release_catalog_before_diagnostic(model_catalog_refresh_task, diagnostic).await;
    abort_and_join_task(gateway_lease_task).await;
}

/// Run the gateway backend and clear its waiter intent before diagnostics or
/// lease release so peers stop handing back to a stopped backend.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_bot_run_gateway_backend(
    mut client: serenity::Client,
    provider_for_error: &ProviderKind,
    gateway_waiter: Option<GatewayWaiterGuard>,
    gateway_lease_task: Option<tokio::task::JoinHandle<()>>,
    model_catalog_refresh_task: Option<tokio::task::JoinHandle<()>>,
    startup_reconcile_remaining_for_client_start: Arc<std::sync::atomic::AtomicUsize>,
    startup_doctor_started_for_client_start: Arc<std::sync::atomic::AtomicBool>,
    health_registry_for_client_start: Arc<health::HealthRegistry>,
    api_port: u16,
) {
    let gateway_backend_task = tokio::spawn(async move { client.start().await });
    finish_gateway_backend(
        gateway_backend_task,
        provider_for_error,
        gateway_waiter,
        gateway_lease_task,
        model_catalog_refresh_task,
        run_startup_diagnostic_after_reconcile_barrier(
            startup_reconcile_remaining_for_client_start,
            startup_doctor_started_for_client_start,
            health_registry_for_client_start,
            api_port,
        ),
    )
    .await;
}

#[cfg(test)]
#[path = "shutdown_input_fence_tests.rs"]
mod input_fence_tests;

#[cfg(test)]
mod lifecycle_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::{abort_and_join_task, release_catalog_before_diagnostic};

    #[test]
    fn invariant_gateway_exit_releases_catalog_owner_before_diagnostics_and_takeover() {
        crate::services::discord::model_catalog::with_test_claude_model_catalog_refresh_state(
            || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    let refreshes = Arc::new(AtomicUsize::new(0));
                    let (owner_tx, owner_rx) = tokio::sync::oneshot::channel();
                    let owner_tx = std::sync::Mutex::new(Some(owner_tx));
                    let owner = crate::services::discord::model_catalog::spawn_test_claude_model_catalog_refresh_after_claim(
                        Arc::clone(&refreshes),
                        move || {
                            if let Some(owner_tx) = owner_tx.lock().unwrap().take() {
                                let _ = owner_tx.send(());
                            }
                        },
                    );
                    tokio::time::timeout(Duration::from_millis(100), owner_rx)
                        .await
                        .expect("the initial supervisor must claim refresh ownership")
                        .expect("initial supervisor ended before claiming");
                    assert_eq!(refreshes.load(Ordering::Acquire), 1);

                    let (takeover_tx, takeover_rx) = tokio::sync::oneshot::channel();
                    let takeover_tx = std::sync::Mutex::new(Some(takeover_tx));
                    let survivor = crate::services::discord::model_catalog::spawn_test_claude_model_catalog_refresh_after_claim(
                        Arc::clone(&refreshes),
                        move || {
                            if let Some(takeover_tx) = takeover_tx.lock().unwrap().take() {
                                let _ = takeover_tx.send(());
                            }
                        },
                    );
                    tokio::task::yield_now().await;
                    assert_eq!(refreshes.load(Ordering::Acquire), 1);

                    release_catalog_before_diagnostic(Some(owner), async {
                        assert_eq!(
                            refreshes.load(Ordering::Acquire),
                            1,
                            "the original owner must be released before diagnostics"
                        );
                        tokio::time::timeout(Duration::from_millis(1_100), takeover_rx)
                            .await
                            .expect("a live supervisor must claim before diagnostics continue")
                            .expect("takeover supervisor ended before claiming");
                    })
                    .await;
                    assert_eq!(
                        refreshes.load(Ordering::Acquire),
                        2,
                        "a live supervisor must take over within the one-second standby interval"
                    );

                    abort_and_join_task(Some(survivor)).await;
                    assert!(
                        !crate::services::discord::model_catalog::test_claude_model_catalog_refresh_running()
                    );
                });
            },
        );
    }
}

#[cfg(test)]
mod gateway_waiter_tests {
    use super::*;
    use crate::services::cluster::intake_worker_capabilities::capabilities_with_runtime_state;
    use crate::services::cluster::node_registry::node_awaits_gateway;

    fn advertised(provider: &str) -> bool {
        node_awaits_gateway(
            &serde_json::json!({
                "capabilities": capabilities_with_runtime_state(&serde_json::json!({}))
            }),
            provider,
        )
    }

    #[tokio::test]
    async fn gateway_waiter_exit_arms_withdraw_before_diagnostic_and_lease_abort() {
        for exit in 0..4 {
            let provider = format!("gateway-waiter-exit-{exit}");
            let waiter = GatewayWaiterGuard::new(&provider);
            let backend = tokio::spawn(async move {
                match exit {
                    0 => Ok(()),
                    1 => Err("backend error"),
                    2 => panic!("backend panic"),
                    _ => std::future::pending::<Result<(), &str>>().await,
                }
            });
            if exit == 3 {
                backend.abort();
            }
            let lease = tokio::spawn(std::future::pending());
            let lease_watch = lease.abort_handle();
            let mut diagnosed = false;
            assert!(advertised(&provider));
            finish_gateway_backend(
                backend,
                &ProviderKind::Codex,
                Some(waiter),
                Some(lease),
                None,
                async {
                    assert!(
                        !advertised(&provider),
                        "waiter survived backend exit {exit}"
                    );
                    assert!(
                        !lease_watch.is_finished(),
                        "lease released before diagnostic"
                    );
                    diagnosed = true;
                },
            )
            .await;
            assert!(diagnosed);
            assert!(lease_watch.is_finished());
            assert!(!advertised(&provider));
        }
    }

    /// Records the ownership its lease task saw when the abort dropped it.
    struct LeaseDropProbe(std::sync::Arc<std::sync::Mutex<Option<GatewayOwnership>>>);

    impl Drop for LeaseDropProbe {
        fn drop(&mut self) {
            let seen = crate::services::tui_o::ownership::gate("qwen").current();
            *self.0.lock().unwrap() = Some(seen);
        }
    }

    use crate::services::tui_o::ownership::GatewayOwnership;

    #[tokio::test]
    async fn backend_exit_closes_o_admission_before_the_lease_task_drops_the_lease() {
        let gate = crate::services::tui_o::ownership::gate(ProviderKind::Qwen.as_str());
        gate.acquired();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let probe = LeaseDropProbe(std::sync::Arc::clone(&seen));
        let lease = tokio::spawn(async move {
            let _probe = probe;
            std::future::pending::<()>().await;
        });
        let backend = tokio::spawn(async { Ok::<(), &str>(()) });
        finish_gateway_backend(
            backend,
            &ProviderKind::Qwen,
            None,
            Some(lease),
            None,
            async {
                assert_eq!(
                    gate.admit(|epoch| epoch),
                    None,
                    "admission open after backend exit"
                );
            },
        )
        .await;
        assert_eq!(*seen.lock().unwrap(), Some(GatewayOwnership::Lost));
    }

    /// Guards that a re-acquisition landing while backend-exit diagnostics run keeps O closed.
    #[tokio::test]
    async fn a_lease_reacquired_during_the_exit_diagnostic_keeps_o_admission_closed() {
        let provider = ProviderKind::Unsupported("o-late-reacquire".into());
        let gate = crate::services::tui_o::ownership::gate(provider.as_str());
        gate.acquired();
        let lease = tokio::spawn(std::future::pending::<()>());
        let backend = tokio::spawn(async { Ok::<(), &str>(()) });
        finish_gateway_backend(backend, &provider, None, Some(lease), None, async {
            // The lease task's in-flight re-acquisition completes here.
            assert_eq!(gate.reacquired(), None);
        })
        .await;
        assert_eq!(gate.admit(|epoch| epoch), None);
    }

    #[tokio::test]
    async fn gateway_waiter_cancel_and_prebackend_panic_withdraw_intent() {
        for panic_before_backend in [false, true] {
            let provider = format!("gateway-waiter-unwind-{panic_before_backend}");
            let task_provider = provider.clone();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                let _waiter = GatewayWaiterGuard::new(&task_provider);
                started_tx.send(()).unwrap();
                release_rx.await.unwrap();
                if panic_before_backend {
                    panic!("failure before backend creation");
                }
                std::future::pending::<()>().await;
            });
            started_rx.await.unwrap();
            assert!(advertised(&provider));
            if panic_before_backend {
                release_tx.send(()).unwrap();
            } else {
                task.abort();
            }
            let error = task.await.unwrap_err();
            assert_eq!(error.is_panic(), panic_before_backend);
            assert_eq!(error.is_cancelled(), !panic_before_backend);
            assert!(!advertised(&provider));
        }
    }

    #[tokio::test]
    async fn gateway_waiter_last_same_provider_owner_controls_advertisement() {
        for cancel_first in [false, true] {
            let provider = format!("gateway-waiter-count-{cancel_first}");
            let first = GatewayWaiterGuard::new(&format!(" {} ", provider.to_uppercase()));
            let survivor = GatewayWaiterGuard::new(&provider);
            if cancel_first {
                let task = tokio::spawn(async move {
                    let _first = first;
                    std::future::pending::<()>().await;
                });
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                drop(first);
            }
            assert!(
                advertised(&provider),
                "surviving bot lost its waiter advertisement"
            );
            drop(survivor);
            assert!(!advertised(&provider));
        }
    }
}
