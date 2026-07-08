use std::collections::BTreeSet;
use std::sync::{LazyLock, RwLock};

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::services::cluster::session_routing::cluster_capabilities_with_worker_api;

static ACTIVE_INTAKE_WORKER_PROVIDERS: LazyLock<RwLock<BTreeSet<String>>> =
    LazyLock::new(|| RwLock::new(BTreeSet::new()));

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
    let provider = provider.trim().to_ascii_lowercase();
    if provider.is_empty() {
        return false;
    }
    let Some(intake_worker) = node
        .get("capabilities")
        .and_then(|capabilities| capabilities.get("intake_worker"))
    else {
        return false;
    };
    if intake_worker.get("enabled").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    intake_worker
        .get("providers")
        .and_then(Value::as_array)
        .map(|providers| {
            providers
                .iter()
                .filter_map(Value::as_str)
                .any(|candidate| candidate.trim().eq_ignore_ascii_case(&provider))
        })
        .unwrap_or(false)
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
