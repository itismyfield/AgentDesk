use super::*;

const DISCORD_GATEWAY_LEASE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const DISCORD_GATEWAY_LOCK_PREFIX: u64 = 0x0443_0000_0000_0000;
/// How often a yielding node re-checks whether the preferred gateway node has
/// taken the lease, and how often the preferred node retries acquiring it.
const GATEWAY_PREFERENCE_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// #4351: resolved view of `cluster.gateway_preferred_instance_id` for this node.
struct GatewayPreference {
    preferred_instance_id: String,
    self_instance_id: String,
    yield_grace: Duration,
}

impl GatewayPreference {
    fn self_is_preferred(&self) -> bool {
        self.preferred_instance_id == self.self_instance_id
    }
}

/// `None` when clustering is off or no preference is configured — in that case
/// the lease stays pure first-come, exactly as before #4351.
fn resolve_gateway_preference(
    cluster: &crate::config::ClusterConfig,
    self_instance_id: String,
) -> Option<GatewayPreference> {
    if !cluster.enabled {
        return None;
    }
    let preferred_instance_id = cluster
        .gateway_preferred_instance_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    Some(GatewayPreference {
        preferred_instance_id,
        self_instance_id,
        yield_grace: Duration::from_secs(cluster.gateway_yield_grace_secs),
    })
}

/// Which instance are we, as far as the gateway preference is concerned?
///
/// #4356: **not** `resolve_self_instance_id_without_config()`. That falls back to
/// `hostname-pid` whenever the `SELF_INSTANCE_ID` cell is still empty, and gateway
/// acquisition runs *before* `cluster::bootstrap` fills it:
///
/// ```text
/// 19:15:25.925  GATEWAY-LEASE: Claude launch skipped — singleton lease held elsewhere
/// 19:15:26.543  [cluster] runtime bootstrapped instance_id="mac-book-release"
/// ```
///
/// The preferred node therefore never recognized itself, never registered as a
/// waiter, and the holder never saw a reason to yield.
///
/// `cluster.instance_id` is the same value bootstrap publishes, and we already hold
/// the config here, so reading it directly removes the race rather than timing
/// around it. Only an unset (`auto`) id needs to wait for bootstrap to derive one.
async fn resolve_self_instance_id_for_preference(cluster: &crate::config::ClusterConfig) -> String {
    if let Some(configured) = cluster
        .instance_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return configured.to_string();
    }
    crate::services::cluster::node_registry::wait_for_self_instance_id(Duration::from_secs(10))
        .await
}

async fn gateway_preference() -> Option<GatewayPreference> {
    let config = crate::config::load_graceful();
    // Cheap exits first: no clustering, or no preference configured. Neither needs
    // a self-id, and the `auto`-id path would otherwise block on bootstrap for
    // every node that never opted into a preference.
    if !config.cluster.enabled || config.cluster.gateway_preferred_instance_id.is_none() {
        return None;
    }
    let self_instance_id = resolve_self_instance_id_for_preference(&config.cluster).await;
    resolve_gateway_preference(&config.cluster, self_instance_id)
}

/// May we hand the gateway over to the preferred node?
///
/// Being `online` in `worker_nodes` is **not** sufficient. A node whose dcserver
/// is up and heartbeating may have no token for this provider, may have failed
/// before gateway startup, or may simply not be contending for the lease. Yielding
/// to such a node hands the gateway to nobody: we release, self-fence, restart,
/// re-acquire, and yield again — a gateway outage loop.
///
/// So the preferred node must also *advertise* that it wants this gateway, via the
/// `discord_gateway.waiting_providers` capability that `register_gateway_waiter`
/// publishes on every heartbeat. That signal only exists while a `run_bot` on that
/// node is actually waiting for, or holding, the lease for this provider.
fn should_yield_to_preferred(
    nodes: &[serde_json::Value],
    preferred_instance_id: &str,
    provider: &str,
) -> bool {
    nodes.iter().any(|node| {
        node.get("instance_id").and_then(|v| v.as_str()) == Some(preferred_instance_id)
            && node
                .get("status")
                .and_then(|v| v.as_str())
                .is_some_and(|status| status.eq_ignore_ascii_case("online"))
            && crate::services::cluster::node_registry::node_awaits_gateway(node, provider)
    })
}

/// Is the preferred node online *and* actually contending for this gateway?
///
/// A DB error answers `false`. That is deliberate: this gates *yielding*, and
/// yielding on a DB blip would take a healthy gateway down for a node we cannot
/// actually see. The same reasoning as the keepalive's `Err` arm.
async fn preferred_gateway_is_waiting(
    pool: &sqlx::PgPool,
    preferred_instance_id: &str,
    provider: &ProviderKind,
) -> bool {
    let lease_ttl_secs = crate::config::load_graceful().cluster.lease_ttl_secs.max(1);
    match crate::services::cluster::node_registry::list_worker_nodes(pool, lease_ttl_secs).await {
        Ok(nodes) => should_yield_to_preferred(&nodes, preferred_instance_id, provider.as_str()),
        Err(error) => {
            tracing::warn!(
                "GATEWAY-LEASE: could not read worker_nodes to check preferred gateway: {error}"
            );
            false
        }
    }
}

/// A non-preferred node waits here before it even tries to acquire, giving the
/// preferred node a head start. Returns as soon as the preferred node goes
/// offline (nothing to wait for — take the lease) or the grace expires (the
/// preferred node is up but not taking it; take the lease rather than leave
/// Discord unserved — the keepalive yield will hand it over later).
async fn yield_to_preferred_gateway(
    pool: &sqlx::PgPool,
    preference: &GatewayPreference,
    provider: &ProviderKind,
    shared: &Arc<SharedData>,
) {
    if !preferred_gateway_is_waiting(pool, &preference.preferred_instance_id, provider).await {
        return;
    }
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] ⏸ GATEWAY-LEASE: preferred node {} is waiting for this gateway — standing by for up to {}s",
        preference.preferred_instance_id,
        preference.yield_grace.as_secs()
    );

    let deadline = tokio::time::Instant::now() + preference.yield_grace;
    while tokio::time::Instant::now() < deadline {
        if shared
            .restart
            .shutting_down
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        tokio::time::sleep(GATEWAY_PREFERENCE_POLL_INTERVAL).await;
        if !preferred_gateway_is_waiting(pool, &preference.preferred_instance_id, provider).await {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ▶ GATEWAY-LEASE: preferred node {} is no longer waiting for this gateway — acquiring",
                preference.preferred_instance_id
            );
            return;
        }
    }
    // The preferred node claims to want the lease but has not taken it. Take it
    // rather than leave Discord unserved; the keepalive yield hands it over the
    // moment the preferred node is genuinely ready.
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ▶ GATEWAY-LEASE: preferred node {} still waiting after {}s — acquiring anyway",
        preference.preferred_instance_id,
        preference.yield_grace.as_secs()
    );
}

/// The preferred node is an **unbounded** waiter.
///
/// It must not give up: a non-preferred holder only learns it has to yield on its
/// next keepalive tick, and it yields *because* this node advertises that it is
/// waiting. If this loop bailed after a grace period and stopped advertising, the
/// holder would keep the gateway (correct) — but if it bailed while still
/// advertising, the holder would yield to a node that is no longer acquiring, and
/// the gateway would be lost. Waiting forever keeps the two sides consistent.
///
/// The waiter signal is registered before the first attempt and cleared only on a
/// DB error or shutdown, so a peer never yields to a node that is not acquiring.
async fn acquire_as_preferred_gateway(
    pool: &sqlx::PgPool,
    token_hash: &str,
    provider: &ProviderKind,
    shared: &Arc<SharedData>,
) -> Result<Option<crate::db::postgres::AdvisoryLockLease>, String> {
    crate::services::cluster::node_registry::register_gateway_waiter(provider.as_str());
    // Publish the intent immediately rather than waiting for the next heartbeat,
    // so a peer holding the lease can start yielding right away.
    if let Err(error) =
        crate::services::cluster::node_registry::refresh_worker_node_runtime_capabilities(
            pool,
            &crate::services::cluster::node_registry::resolve_self_instance_id_without_config(),
        )
        .await
    {
        tracing::warn!("GATEWAY-LEASE: could not publish gateway waiter capability: {error}");
    }

    let mut attempts: u64 = 0;
    loop {
        match try_acquire_discord_gateway_lease(pool, token_hash, provider).await {
            // Keep the waiter signal registered: we now hold the gateway, and a
            // peer that sees it will not try to take it from us anyway.
            Ok(Some(lease)) => return Ok(Some(lease)),
            Ok(None) => {
                if shared
                    .restart
                    .shutting_down
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    crate::services::cluster::node_registry::deregister_gateway_waiter(
                        provider.as_str(),
                    );
                    return Ok(None);
                }
                // ~1 log/minute at a 5s poll: enough to see a stuck hand-off,
                // quiet enough to leave running.
                if attempts % 12 == 0 {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ⏳ GATEWAY-LEASE: {} is the preferred gateway but the lease is held — waiting for the holder to yield",
                        provider.display_name()
                    );
                }
                attempts += 1;
                tokio::time::sleep(GATEWAY_PREFERENCE_POLL_INTERVAL).await;
            }
            Err(error) => {
                // We cannot acquire and must not keep peers yielding to us.
                crate::services::cluster::node_registry::deregister_gateway_waiter(
                    provider.as_str(),
                );
                return Err(error);
            }
        }
    }
}

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
    /// Another node owns the lease, so this provider is a confirmed standby.
    /// The startup diagnostic has already run; run_bot must expose the standby
    /// runtime and leave its shutdown-barrier slot for the marker poller.
    Standby,
    /// Lease ownership is unknown because acquisition failed. The startup
    /// diagnostic has already run; run_bot must fail closed and return without
    /// classifying this provider as standby.
    Failed,
}

impl GatewayLeaseOutcome {
    pub(super) fn starts_provider_runtime(&self) -> bool {
        !matches!(self, Self::Failed)
    }
}

/// Acquire the Discord gateway singleton lease (advisory lock) when a PG pool
/// is present. Returns `Proceed(Some(lease))` on success, `Proceed(None)` when
/// there is no PG pool (standalone path), `Standby` when the lease is confirmed
/// held elsewhere, or `Failed` when ownership could not be determined. Both
/// non-proceed paths run the post-reconcile startup diagnostic exactly as the
/// original early-returns did before run_bot decrements the shutdown barrier.
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
        Some(pool) => {
            // #4351: honor the configured gateway owner before racing for the lock.
            let preference = gateway_preference().await;
            let acquired = match preference.as_ref() {
                Some(pref) if pref.self_is_preferred() => {
                    acquire_as_preferred_gateway(pool, token_hash, provider, shared).await
                }
                Some(pref) => {
                    yield_to_preferred_gateway(pool, pref, provider, shared).await;
                    try_acquire_discord_gateway_lease(pool, token_hash, provider).await
                }
                None => try_acquire_discord_gateway_lease(pool, token_hash, provider).await,
            };
            match acquired {
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
                    GatewayLeaseOutcome::Standby
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
                    GatewayLeaseOutcome::Failed
                }
            }
        }
        None => GatewayLeaseOutcome::Proceed(None),
    }
}

/// Stand the gateway down: stop serving Discord, cancel tmux watchers, persist
/// the pending queues and last-message checkpoints, then shut every shard.
///
/// Two callers: the split-brain guard (another instance took the lock out from
/// under us) and the #4351 yield (the preferred gateway node came online and we
/// are handing it back). `restart_pending` is set so launchd brings the process
/// back — it re-enters `run_bot`, finds the lease held, and settles into standby.
///
/// Does NOT release the advisory lock — the split-brain caller no longer holds
/// one, and the yield caller unlocks first so the preferred node can take it.
async fn self_fence_gateway(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    shard_manager: &Arc<serenity::gateway::ShardManager>,
) {
    shared
        .bot_connected
        .store(false, std::sync::atomic::Ordering::SeqCst);
    shared
        .restart
        .shutting_down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    shared
        .restart
        .restart_pending
        .store(true, std::sync::atomic::Ordering::SeqCst);

    for entry in shared.tmux_watchers.iter() {
        entry
            .value()
            .cancel
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    let drain = mailbox_restart_drain_all(shared, provider).await;
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

    let ids: std::collections::HashMap<u64, u64> = shared
        .last_message_ids
        .iter()
        .map(|entry| (entry.key().get(), *entry.value()))
        .collect();
    if !ids.is_empty() {
        runtime_store::save_all_last_message_ids(provider.as_str(), &ids);
    }

    shard_manager.shutdown_all().await;
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
        // Resolved once, inside the task because it may await bootstrap (#4356):
        // the self-id is stable for the life of the process, and a hot config
        // reload that changed the preferred node mid-flight would race the yield
        // against the acquire.
        let preference = gateway_preference().await;
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

            // #4351: we are holding the gateway but we are not the node that is
            // supposed to. Hand it back as soon as the preferred node is up.
            // This is what makes the preference survive deploy/boot ordering:
            // `deploy-release.sh` restarts the local node first, so a
            // non-preferred node routinely wins the initial race.
            if let Some(pref) = preference.as_ref().filter(|p| !p.self_is_preferred()) {
                if current_lease.is_some() {
                    if let Some(pool) = shared_for_lease.pg_pool.as_ref() {
                        if preferred_gateway_is_waiting(
                            pool,
                            &pref.preferred_instance_id,
                            &provider_for_lease,
                        )
                        .await
                        {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] 🤝 GATEWAY-LEASE: {} yielding gateway to preferred node {} — self-fencing",
                                provider_for_lease.display_name(),
                                pref.preferred_instance_id
                            );
                            // Release first: the preferred node is an unbounded
                            // waiter and can only win once the lock is free.
                            if let Some(lease) = current_lease.take() {
                                let _ = lease.unlock().await;
                            }
                            self_fence_gateway(
                                &shared_for_lease,
                                &provider_for_lease,
                                &shard_manager,
                            )
                            .await;
                            return;
                        }
                    }
                }
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
                    self_fence_gateway(&shared_for_lease, &provider_for_lease, &shard_manager)
                        .await;
                    return;
                }
            }
        }
    })
}

#[cfg(test)]
#[path = "gateway_lease_tests.rs"]
mod tests;
