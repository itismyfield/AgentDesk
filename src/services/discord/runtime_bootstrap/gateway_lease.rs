use super::*;

const DISCORD_GATEWAY_LEASE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const DISCORD_GATEWAY_LOCK_PREFIX: u64 = 0x0443_0000_0000_0000;

fn discord_gateway_lock_id(token_hash: &str) -> i64 {
    // `discord_token_hash()` returns "discord_<16hex>". Strip the literal prefix
    // so the first 16 chars we sample are actual hex; otherwise the `is_ascii_hexdigit`
    // check fails on non-hex letters in the prefix and every bot collapses onto the
    // same fallback lock id, causing only one bot to acquire the singleton lease.
    let raw = token_hash.strip_prefix("discord_").unwrap_or(token_hash);
    let hex = raw
        .get(..16)
        .filter(|prefix| prefix.chars().all(|ch| ch.is_ascii_hexdigit()))
        .unwrap_or("0");
    let parsed = u64::from_str_radix(hex, 16).unwrap_or(0);
    let suffix = parsed & 0x0000_FFFF_FFFF_FFFF;
    (DISCORD_GATEWAY_LOCK_PREFIX | suffix) as i64
}

async fn try_acquire_discord_gateway_lease(
    pool: &sqlx::PgPool,
    token_hash: &str,
    provider: &ProviderKind,
) -> Result<Option<crate::db::postgres::AdvisoryLockLease>, String> {
    crate::db::postgres::AdvisoryLockLease::try_acquire(
        pool,
        discord_gateway_lock_id(token_hash),
        format!("discord gateway {}", provider.as_str()),
    )
    .await
}

/// Outcome of the gateway singleton-lease acquisition phase.
pub(super) enum GatewayLeaseOutcome {
    /// Either the lease was acquired (`Some`) or there is no PG pool (`None`,
    /// the standalone/no-DB path). Either way, startup proceeds.
    Proceed(Option<crate::db::postgres::AdvisoryLockLease>),
    /// Lease is held elsewhere, or acquisition failed. The startup diagnostic
    /// has already run; run_bot must decrement the shutdown barrier and return.
    Skip,
}

/// Acquire the Discord gateway singleton lease (advisory lock) when a PG pool
/// is present. Returns `Proceed(Some(lease))` on success, `Proceed(None)` when
/// there is no PG pool (standalone path), or `Skip` when the lease is held
/// elsewhere / acquisition failed. On the `Skip` paths this runs the
/// post-reconcile startup diagnostic exactly as the original early-returns did,
/// before returning; run_bot then decrements the shutdown barrier and returns.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_bot_acquire_gateway_lease(
    shared: &Arc<SharedData>,
    token_hash: &str,
    provider: &ProviderKind,
    startup_reconcile_remaining: &Arc<std::sync::atomic::AtomicUsize>,
    startup_doctor_started: &Arc<std::sync::atomic::AtomicBool>,
    health_registry: &Arc<health::HealthRegistry>,
    api_port: u16,
) -> GatewayLeaseOutcome {
    match shared.pg_pool.as_ref() {
        Some(pool) => match try_acquire_discord_gateway_lease(pool, token_hash, provider).await {
            Ok(Some(lease)) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔐 GATEWAY-LEASE: {} acquired singleton lease",
                    provider.display_name()
                );
                GatewayLeaseOutcome::Proceed(Some(lease))
            }
            Ok(None) => {
                run_startup_diagnostic_after_reconcile_barrier(
                    startup_reconcile_remaining.clone(),
                    startup_doctor_started.clone(),
                    health_registry.clone(),
                    api_port,
                )
                .await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⏭ GATEWAY-LEASE: {} launch skipped — singleton lease held elsewhere",
                    provider.display_name()
                );
                GatewayLeaseOutcome::Skip
            }
            Err(error) => {
                run_startup_diagnostic_after_reconcile_barrier(
                    startup_reconcile_remaining.clone(),
                    startup_doctor_started.clone(),
                    health_registry.clone(),
                    api_port,
                )
                .await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⏭ GATEWAY-LEASE: {} launch skipped — failed to acquire singleton lease: {}",
                    provider.display_name(),
                    error
                );
                GatewayLeaseOutcome::Skip
            }
        },
        None => GatewayLeaseOutcome::Proceed(None),
    }
}

/// Spawn the gateway singleton-lease keepalive loop.
///
/// Every tick it keepalives the held advisory lock. A keepalive error means the
/// underlying Postgres connection died, which releases the advisory lock
/// server-side (it is session-scoped) — but that is almost always a TRANSIENT
/// blip (DB restart, brief network drop), not a genuine hand-off to another
/// instance. Fencing on the very first error previously left the process alive
/// forever with a dead, never-reacquired gateway (#3620: a startup DB blip took
/// the Discord relay down for ~2h until a manual restart).
///
/// So instead of fencing on the first error, it re-acquires the lock on a fresh
/// connection — mirroring the cluster-leader lease
/// (`node_registry::spawn_heartbeat_loop`), which auto-recovered from the same
/// blip while this loop did not:
///   * re-acquired (`Ok(Some)`)  → the gateway never went down; keep serving.
///   * held elsewhere (`Ok(None)`) → another instance owns the singleton now,
///     so self-fence to avoid a split-brain (two live gateways).
///   * db unreachable (`Err`)    → nobody can hold the lock while the DB is
///     down, so keep the gateway up and retry on the next tick.
///
/// The self-fence path flips shutdown flags, cancels tmux watchers, drains
/// pending queues, persists last_message_ids, and shuts down all shards.
/// Spawned after the client is built (needs `shard_manager`) and before the
/// gateway backend run. Returns the JoinHandle so run_bot can abort it on
/// backend exit.
pub(super) fn run_bot_spawn_gateway_lease_keepalive(
    lease: crate::db::postgres::AdvisoryLockLease,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    token_hash: String,
    shard_manager: Arc<serenity::gateway::ShardManager>,
) -> tokio::task::JoinHandle<()> {
    let shared_for_lease = shared.clone();
    let provider_for_lease = provider.clone();
    let mut current_lease = Some(lease);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(DISCORD_GATEWAY_LEASE_KEEPALIVE_INTERVAL);
        interval.tick().await;
        loop {
            interval.tick().await;

            if shared_for_lease
                .restart
                .shutting_down
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                if let Some(lease) = current_lease.take() {
                    let _ = lease.unlock().await;
                }
                return;
            }

            // Keepalive the held lease. On error the lock connection died (the
            // advisory lock is released server-side), so drop it and fall
            // through to the re-acquire arm below.
            if let Some(lease) = current_lease.as_mut() {
                match lease.keepalive().await {
                    Ok(()) => continue,
                    Err(error) => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ GATEWAY-LEASE: {} keepalive failed: {} — re-acquiring (gateway stays up)",
                            provider_for_lease.display_name(),
                            error
                        );
                        current_lease = None;
                    }
                }
            }

            // We hold no lease — try to re-acquire on a fresh connection.
            let Some(pool) = shared_for_lease.pg_pool.as_ref() else {
                // No PG pool (standalone/no-DB path): nothing to keepalive.
                return;
            };
            match try_acquire_discord_gateway_lease(pool, &token_hash, &provider_for_lease).await {
                Ok(Some(new_lease)) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 🔐 GATEWAY-LEASE: {} re-acquired singleton lease after transient loss",
                        provider_for_lease.display_name()
                    );
                    current_lease = Some(new_lease);
                }
                Err(error) => {
                    // DB still unreachable — no other instance can hold the lock
                    // while the DB is down, so keep the gateway up and retry.
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ GATEWAY-LEASE: {} re-acquire deferred (db unavailable): {} — retrying next tick",
                        provider_for_lease.display_name(),
                        error
                    );
                }
                Ok(None) => {
                    // A genuine hand-off: another instance now holds the
                    // singleton. Self-fence to avoid two live gateways.
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::error!(
                        "  [{ts}] ⛔ GATEWAY-LEASE: {} singleton lease taken by another instance — self-fencing",
                        provider_for_lease.display_name()
                    );

                    shared_for_lease
                        .bot_connected
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    shared_for_lease
                        .restart
                        .shutting_down
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    shared_for_lease
                        .restart
                        .restart_pending
                        .store(true, std::sync::atomic::Ordering::SeqCst);

                    for entry in shared_for_lease.tmux_watchers.iter() {
                        entry
                            .value()
                            .cancel
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                    }

                    let drain =
                        mailbox_restart_drain_all(&shared_for_lease, &provider_for_lease).await;
                    let queue_count = drain.queued_count;
                    if !drain.persistence_errors.is_empty() {
                        tracing::error!(
                            failures = drain.persistence_errors.len(),
                            "gateway lease self-fence observed pending-queue persistence failure(s)"
                        );
                    }
                    if queue_count > 0 {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 📋 GATEWAY-LEASE: persisted {queue_count} pending queue item(s) before self-fence"
                        );
                    }

                    let ids: std::collections::HashMap<u64, u64> = shared_for_lease
                        .last_message_ids
                        .iter()
                        .map(|entry| (entry.key().get(), *entry.value()))
                        .collect();
                    if !ids.is_empty() {
                        runtime_store::save_all_last_message_ids(provider_for_lease.as_str(), &ids);
                    }

                    shard_manager.shutdown_all().await;
                    return;
                }
            }
        }
    })
}
