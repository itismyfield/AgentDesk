use std::collections::BTreeSet;
use std::sync::{LazyLock, RwLock};

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::services::cluster::session_routing::cluster_capabilities_with_worker_api;

static ACTIVE_INTAKE_WORKER_PROVIDERS: LazyLock<RwLock<BTreeSet<String>>> =
    LazyLock::new(|| RwLock::new(BTreeSet::new()));

const PRESERVE_ON_CANCEL_V1: &str = "preserve_on_cancel_v1";

/// Providers whose `run_bot` on this node is actively trying to take the Discord
/// gateway lease (#4351). Advertised so a non-preferred holder can tell "the
/// preferred node is heartbeating" apart from "the preferred node actually wants
/// and is able to run this gateway".
///
/// Without the distinction, a preferred node whose dcserver is up but whose bot
/// never starts (no token for this provider, startup failure, acquire gave up)
/// would make the non-preferred holder yield to nobody, self-fence, restart,
/// re-acquire, and yield again — a gateway outage loop.
static GATEWAY_WAITER_PROVIDERS: LazyLock<RwLock<BTreeSet<String>>> =
    LazyLock::new(|| RwLock::new(BTreeSet::new()));

pub(crate) fn register_intake_worker_provider(provider: &str) {
    let provider = provider.trim().to_ascii_lowercase();
    if provider.is_empty() {
        return;
    }
    if let Ok(mut providers) = ACTIVE_INTAKE_WORKER_PROVIDERS.write() {
        providers.insert(provider);
    }
}

fn active_intake_worker_providers() -> Vec<String> {
    ACTIVE_INTAKE_WORKER_PROVIDERS
        .read()
        .map(|providers| providers.iter().cloned().collect())
        .unwrap_or_default()
}

/// Called by the preferred node right before it starts waiting for the gateway
/// lease, and held for as long as it keeps waiting or holds it. The next
/// heartbeat (≤ `heartbeat_interval_secs`) publishes it.
pub(crate) fn register_gateway_waiter(provider: &str) {
    let provider = provider.trim().to_ascii_lowercase();
    if provider.is_empty() {
        return;
    }
    if let Ok(mut providers) = GATEWAY_WAITER_PROVIDERS.write() {
        providers.insert(provider);
    }
}

/// Called when this node stops wanting the gateway for `provider` (acquire error,
/// shutdown). Clears the signal so peers stop yielding to us.
pub(crate) fn deregister_gateway_waiter(provider: &str) {
    let provider = provider.trim().to_ascii_lowercase();
    if let Ok(mut providers) = GATEWAY_WAITER_PROVIDERS.write() {
        providers.remove(&provider);
    }
}

fn active_gateway_waiter_providers() -> Vec<String> {
    GATEWAY_WAITER_PROVIDERS
        .read()
        .map(|providers| providers.iter().cloned().collect())
        .unwrap_or_default()
}

/// Is `node` currently waiting for (or holding) the gateway for `provider`?
///
/// A node that does not advertise this must never be yielded to: it is not
/// contending for the lease, so handing the gateway over would leave Discord
/// unserved.
pub(crate) fn node_awaits_gateway(node: &Value, provider: &str) -> bool {
    let provider = provider.trim().to_ascii_lowercase();
    if provider.is_empty() {
        return false;
    }
    node.get("capabilities")
        .and_then(|capabilities| capabilities.get("discord_gateway"))
        .and_then(|gateway| gateway.get("waiting_providers"))
        .and_then(Value::as_array)
        .map(|providers| {
            providers
                .iter()
                .filter_map(Value::as_str)
                .any(|candidate| candidate.trim().eq_ignore_ascii_case(&provider))
        })
        .unwrap_or(false)
}

pub(super) fn capabilities_with_runtime_state(base: &Value) -> Value {
    let mut capabilities = base.as_object().cloned().unwrap_or_default();
    let providers = active_intake_worker_providers();
    capabilities.insert(
        "intake_worker".to_string(),
        json!({
            "enabled": !providers.is_empty(),
            "providers": providers,
            "features": [PRESERVE_ON_CANCEL_V1],
        }),
    );
    let gateway_waiters = active_gateway_waiter_providers();
    capabilities.insert(
        "discord_gateway".to_string(),
        json!({ "waiting_providers": gateway_waiters }),
    );
    Value::Object(capabilities)
}

pub(crate) fn node_supports_intake_provider(node: &Value, provider: &str) -> bool {
    node_intake_worker(node, provider).is_some()
}

/// Returns whether a worker can safely consume this request's protocol shape.
/// Legacy provider-capable workers remain eligible for non-preserving requests,
/// while preserving requests require an explicit versioned feature advertisement.
pub(crate) fn node_supports_intake_request(
    node: &Value,
    provider: &str,
    preserve_on_cancel: bool,
) -> bool {
    let Some(intake_worker) = node_intake_worker(node, provider) else {
        return false;
    };
    !preserve_on_cancel
        || intake_worker
            .get("features")
            .and_then(Value::as_array)
            .is_some_and(|features| {
                features
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|feature| feature.trim().eq_ignore_ascii_case(PRESERVE_ON_CANCEL_V1))
            })
}

fn node_intake_worker<'a>(node: &'a Value, provider: &str) -> Option<&'a Value> {
    let provider = provider.trim().to_ascii_lowercase();
    if provider.is_empty() {
        return None;
    }
    let intake_worker = node.get("capabilities")?.get("intake_worker")?;
    if intake_worker.get("enabled").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    intake_worker
        .get("providers")
        .and_then(Value::as_array)
        .is_some_and(|providers| {
            providers
                .iter()
                .filter_map(Value::as_str)
                .any(|candidate| candidate.trim().eq_ignore_ascii_case(&provider))
        })
        .then_some(intake_worker)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(features: Option<Value>) -> Value {
        let mut intake_worker = serde_json::Map::from_iter([
            ("enabled".to_string(), Value::Bool(true)),
            ("providers".to_string(), json!(["claude"])),
        ]);
        if let Some(features) = features {
            intake_worker.insert("features".to_string(), features);
        }
        json!({ "capabilities": { "intake_worker": intake_worker } })
    }

    #[test]
    fn preserving_request_requires_versioned_feature() {
        let legacy = node(None);
        let capable = node(Some(json!([PRESERVE_ON_CANCEL_V1])));

        assert!(!node_supports_intake_request(&legacy, "claude", true));
        assert!(node_supports_intake_request(&capable, "claude", true));
    }

    #[test]
    fn preserving_request_rejects_malformed_or_wrong_features() {
        assert!(!node_supports_intake_request(
            &node(Some(Value::String(PRESERVE_ON_CANCEL_V1.to_string()))),
            "claude",
            true
        ));
        assert!(!node_supports_intake_request(
            &node(Some(json!(["other_feature"]))),
            "claude",
            true
        ));
    }

    #[test]
    fn non_preserving_request_allows_legacy_provider_worker() {
        assert!(node_supports_intake_request(&node(None), "claude", false));
    }

    #[test]
    fn runtime_capability_advertises_preservation_protocol() {
        register_intake_worker_provider("claude");
        let capabilities = capabilities_with_runtime_state(&json!({}));
        assert_eq!(
            capabilities
                .pointer("/intake_worker/features/0")
                .and_then(Value::as_str),
            Some(PRESERVE_ON_CANCEL_V1)
        );
    }
}

pub(crate) async fn refresh_worker_node_runtime_capabilities(
    pool: &PgPool,
    instance_id: &str,
) -> Result<(), String> {
    let base = cluster_capabilities_with_worker_api(&crate::config::load_graceful().cluster);
    let capabilities = capabilities_with_runtime_state(&base);
    sqlx::query(
        "UPDATE worker_nodes SET capabilities = $2, updated_at = NOW() WHERE instance_id = $1",
    )
    .bind(instance_id)
    .bind(capabilities)
    .execute(pool)
    .await
    .map(|_| ())
    .map_err(|error| format!("refresh worker_node runtime capabilities: {error}"))
}
