use std::collections::{BTreeMap, BTreeSet};
use std::sync::{LazyLock, RwLock};

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::services::cluster::session_routing::cluster_capabilities_with_worker_api;

static ACTIVE_INTAKE_WORKER_PROVIDERS: LazyLock<RwLock<BTreeSet<String>>> =
    LazyLock::new(|| RwLock::new(BTreeSet::new()));

const PRESERVE_ON_CANCEL_V1: &str = "preserve_on_cancel_v1";
const SCHEDULED_MESSAGE_DISCORD_MENTION_CONSUMER_V1: &str = "discord_mention_consumer_v1";

// Count live gateway owners so one bot stopping does not hide another bot's intent.
static GATEWAY_WAITER_PROVIDERS: LazyLock<RwLock<BTreeMap<String, usize>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));

pub(crate) fn register_intake_worker_provider(provider: &str) {
    let provider = provider.trim().to_ascii_lowercase();
    if provider.is_empty() {
        return;
    }
    if let Ok(mut providers) = ACTIVE_INTAKE_WORKER_PROVIDERS.write() {
        providers.insert(provider);
    }
}

pub(super) fn active_intake_worker_providers() -> Vec<String> {
    ACTIVE_INTAKE_WORKER_PROVIDERS
        .read()
        .map(|providers| providers.iter().cloned().collect())
        .unwrap_or_default()
}

/// Keeps gateway intent advertised until acquisition or backend ownership ends.
pub(crate) struct GatewayWaiterGuard {
    provider: String,
}

impl GatewayWaiterGuard {
    pub(crate) fn new(provider: &str) -> Self {
        let provider = provider.trim().to_ascii_lowercase();
        if !provider.is_empty() {
            let mut providers = GATEWAY_WAITER_PROVIDERS
                .write()
                .unwrap_or_else(|error| error.into_inner());
            *providers.entry(provider.clone()).or_default() += 1;
        }
        Self { provider }
    }
}

impl Drop for GatewayWaiterGuard {
    fn drop(&mut self) {
        let mut providers = GATEWAY_WAITER_PROVIDERS
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(count) = providers.get_mut(&self.provider) {
            *count -= 1;
            if *count == 0 {
                providers.remove(&self.provider);
            }
        }
    }
}

fn active_gateway_waiter_providers() -> Vec<String> {
    GATEWAY_WAITER_PROVIDERS
        .read()
        .map(|providers| providers.keys().cloned().collect())
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

pub(crate) fn capabilities_with_runtime_state(base: &Value) -> Value {
    let mut capabilities = base.as_object().cloned().unwrap_or_default();
    super::readiness::publish(&mut capabilities);
    super::machine_resources::publish(&mut capabilities);
    super::execution_capacity::publish(&mut capabilities);
    let providers = active_intake_worker_providers();
    capabilities.insert(
        "intake_worker".to_string(),
        json!({
            "enabled": !providers.is_empty(),
            "providers": providers,
            "features": [PRESERVE_ON_CANCEL_V1, "execution_requirements_v1", super::attachment_transfer::CAPABILITY],
        }),
    );
    let gateway_waiters = active_gateway_waiter_providers();
    capabilities.insert(
        "discord_gateway".to_string(),
        json!({ "waiting_providers": gateway_waiters }),
    );
    let scheduled_messages = capabilities
        .entry("scheduled_messages".to_string())
        .or_insert_with(|| json!({}));
    if !scheduled_messages.is_object() {
        *scheduled_messages = json!({});
    }
    if let Some(features) = scheduled_messages.as_object_mut() {
        features.insert(
            SCHEDULED_MESSAGE_DISCORD_MENTION_CONSUMER_V1.to_string(),
            Value::Bool(true),
        );
    }
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
        assert_eq!(
            capabilities.pointer("/scheduled_messages/discord_mention_consumer_v1"),
            Some(&Value::Bool(true))
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
