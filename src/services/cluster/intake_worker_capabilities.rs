use std::collections::BTreeSet;
use std::sync::{LazyLock, RwLock};

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::services::cluster::session_routing::cluster_capabilities_with_worker_api;

static ACTIVE_INTAKE_WORKER_PROVIDERS: LazyLock<RwLock<BTreeSet<String>>> =
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
