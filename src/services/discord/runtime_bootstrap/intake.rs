use super::*;

/// Phase 5.1 of intake-node-routing (issue #2007): when intake routing is in
/// Observe/Enforce mode and a PG pool exists, spawn the REST-only intake_worker
/// poll loop (resolves `target_instance_id` inside the task to avoid racing
/// `cluster::bootstrap`). Observe mode keeps the consumer warm so a later
/// observe→enforce config reload does not strand forwarded rows. The caller
/// invokes this only after a gateway or confirmed-standby role is registered.
pub(super) fn run_bot_maybe_spawn_intake_worker(
    shared: &Arc<SharedData>,
    token: &str,
    provider: &ProviderKind,
) {
    let intake_routing =
        crate::services::cluster::intake_router_hook::effective_intake_routing_config();
    if intake_routing.worker_consumer_should_spawn() {
        if let Some(pool_for_intake_worker) = shared.pg_pool.clone() {
            crate::services::cluster::node_registry::register_intake_worker_provider(
                provider.as_str(),
            );
            let intake_worker_http = std::sync::Arc::new(serenity::http::Http::new(token));
            let intake_worker_shared = shared.clone();
            let intake_worker_token = token.to_string();
            let intake_worker_provider = provider.as_str().to_string();
            let intake_worker_cancel = shared.restart.shutting_down.clone();
            // The intake_worker spawn runs concurrently with `cluster::bootstrap`
            // which is the writer of `SELF_INSTANCE_ID`. Resolving
            // `target_instance_id` eagerly here would race and pick up the
            // hostname+PID fallback (e.g. `itismyfieldui-Macmini-46662`)
            // instead of the configured cluster id (e.g. `mac-mini-release`).
            // The leader hook (`intake_router_hook::try_route_intake`) resolves
            // the same function later, by which time bootstrap has populated
            // the OnceLock — the two ids must match or every claim misses.
            // Bridge the race by awaiting the OnceLock inside the spawned task
            // before the worker logs "poll loop started".
            tokio::spawn(async move {
                let resolved_target_id =
                    crate::services::cluster::node_registry::wait_for_self_instance_id(
                        std::time::Duration::from_secs(30),
                    )
                    .await;
                if let Err(error) =
                    crate::services::cluster::node_registry::refresh_worker_node_runtime_capabilities(
                        &pool_for_intake_worker,
                        &resolved_target_id,
                    )
                    .await
                {
                    tracing::warn!("[intake_worker] runtime capability refresh failed: {error}");
                }
                // claim_owner appends provider so multi-bot deployments
                // surface which token's worker holds a row in
                // observability dashboards.
                let resolved_claim_owner =
                    format!("{}:{}", resolved_target_id, intake_worker_provider);
                crate::services::cluster::intake_worker::run_intake_worker_loop(
                    pool_for_intake_worker,
                    intake_worker_http,
                    intake_worker_shared,
                    intake_worker_token,
                    resolved_target_id,
                    intake_worker_provider,
                    resolved_claim_owner,
                    crate::services::cluster::intake_worker::IntakeWorkerConfig::default(),
                    intake_worker_cancel,
                )
                .await;
            });
        } else {
            tracing::info!(
                mode = intake_routing.mode.as_str(),
                source = intake_routing.source.as_str(),
                "[intake_worker] postgres pool unavailable — intake-node-routing worker not started"
            );
        }
    } else {
        tracing::debug!(
            mode = intake_routing.mode.as_str(),
            source = intake_routing.source.as_str(),
            "[intake_worker] intake routing disabled — worker not started"
        );
    }
}
