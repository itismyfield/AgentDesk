//! Runtime-profile health projection and gateway standby semantics.
use super::*;

pub(super) fn attach_runtime_profile(json: &mut serde_json::Value, config: &crate::config::Config) {
    json["runtime_profile"] = serde_json::json!(config.cluster.runtime_profile);
    json["modules"] = serde_json::json!(config.cluster.runtime_profile.modules());
    json["dashboard_required"] =
        serde_json::json!(config.cluster.runtime_profile.modules().dashboard);
}

pub(super) async fn cluster_standby_without_gateway(
    state: &AppState,
    server_up: bool,
    degraded_reasons: &[serde_json::Value],
) -> bool {
    if !server_up
        || !state.config.cluster.enabled
        || !state.config.cluster.runtime_profile.modules().gateway
    {
        return false;
    }
    if !degraded_reasons
        .iter()
        .any(|reason| reason.as_str() == Some("no_providers_registered"))
    {
        return false;
    }
    let instance_id = state
        .config
        .cluster
        .instance_id
        .as_deref()
        .unwrap_or("")
        .trim();
    if instance_id.is_empty() {
        return false;
    }
    health_diagnostics::is_recent_cluster_worker(
        state.pg_pool_ref(),
        instance_id,
        state.config.cluster.lease_ttl_secs,
    )
    .await
}
