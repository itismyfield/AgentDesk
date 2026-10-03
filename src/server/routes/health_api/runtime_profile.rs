//! Runtime-profile health projection and gateway standby semantics.
use super::*;

pub(super) fn attach_runtime_profile(json: &mut serde_json::Value, config: &crate::config::Config) {
    json["runtime_profile"] = serde_json::json!(config.cluster.runtime_profile);
    json["modules"] = serde_json::json!(config.cluster.runtime_profile.modules());
    json["dashboard_required"] =
        serde_json::json!(config.cluster.runtime_profile.modules().dashboard);
    json["herdr"] = herdr_health(config);
}

/// The boot `session_hosts` view and the admission switch. No E7 runs here; `last_e7` is `never`
/// until a launch records one. Admission is read only once a channel is configured.
fn herdr_health(config: &crate::config::Config) -> serde_json::Value {
    use crate::config::session_hosts::{BootSessionHosts, with_boot};
    let view = |boot: &BootSessionHosts| {
        let channels: Vec<String> = boot.channels().keys().map(u64::to_string).collect();
        let admission = match channels.is_empty() {
            true => "not_evaluated".to_string(),
            false => match crate::services::herdr_admission::check() {
                Ok(()) => "open".to_string(),
                Err(cause) => format!("stopped({})", cause.as_str()),
            },
        };
        let mut endpoints = serde_json::Map::new();
        for (key, endpoint) in &boot.config().herdr.endpoints {
            let node = endpoint.execution_node.trim();
            let local = boot.local_node() == Some(node);
            let entry = serde_json::json!({"node": node, "local": local, "last_e7": "never"});
            endpoints.insert(key.clone(), entry);
        }
        serde_json::json!({
            "configured_channels": channels,
            "admission": admission,
            "endpoints": endpoints,
            "restart_required": *boot.config() != config.session_hosts,
        })
    };
    with_boot(|boot| match boot {
        Some(boot) => view(boot),
        None => view(&BootSessionHosts::default()),
    })
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
