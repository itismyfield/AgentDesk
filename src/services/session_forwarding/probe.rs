//! Cached, authenticated reachability through the same pinned trusted target
//! as session control. Registry advertisements never grant transport trust.
use super::{ForwardCallerContext, trusted_request, trusted_target::build_trusted_target};
use futures::{StreamExt, stream};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, RwLock};
use std::time::Duration;

const TTL_MS: i64 = 45_000;
struct CachedProbe {
    origin: Option<String>,
    report: Value,
}
static CACHE: LazyLock<RwLock<BTreeMap<String, CachedProbe>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));

pub(crate) fn diagnostics(
    config: &crate::config::ClusterConfig,
    node: &Value,
    local: Option<&str>,
) -> Value {
    let id = node["instance_id"].as_str().unwrap_or_default();
    let advertised = node["api_base_url"].as_str();
    let configured = config
        .nodes
        .get(id)
        .and_then(|n| n.trusted_forward_origin.as_deref())
        .is_some_and(|origin| !origin.trim().is_empty());
    let report = CACHE.read().ok().and_then(|cache| {
        cache
            .get(id)
            .filter(|cached| cached.origin.as_deref() == advertised)
            .map(|cached| cached.report.clone())
    });
    let mut result = report.unwrap_or_else(|| {
        json!({
            "reachability_verified":false,"trust_validated":false,
            "reachability_status":if Some(id) == local {"local"} else {"not_probed"},
        })
    });
    if result["expires_at_ms"]
        .as_i64()
        .is_some_and(|expiry| expiry <= chrono::Utc::now().timestamp_millis())
    {
        result["reachability_verified"] = json!(false);
        result["reachability_status"] = json!("probe_expired");
    }
    result["advertised"] = json!(advertised.is_some());
    result["configured"] = json!(configured);
    result
}

pub(crate) fn readiness_reason(node: &Value) -> Option<String> {
    let id = node["instance_id"].as_str().unwrap_or_default();
    if id == crate::services::cluster::node_registry::resolve_self_instance_id_without_config() {
        return None;
    }
    let Ok(cached) = CACHE.read() else {
        return Some("forwarding_probe_cache_unavailable".into());
    };
    let Some(cached) = cached
        .get(id)
        .filter(|p| p.origin.as_deref() == node["api_base_url"].as_str())
    else {
        return Some("forwarding_not_probed".into());
    };
    if cached.report["expires_at_ms"].as_i64().unwrap_or(0) <= chrono::Utc::now().timestamp_millis()
    {
        return Some("forwarding_probe_expired".into());
    }
    if cached.report["reachability_verified"] == true {
        None
    } else {
        Some(
            cached.report["reachability_status"]
                .as_str()
                .unwrap_or("forwarding_unavailable")
                .into(),
        )
    }
}

pub(crate) fn spawn(config: crate::config::Config, pool: sqlx::PgPool, local: String) {
    tokio::spawn(async move {
        let context = ForwardCallerContext {
            pg_pool: Some(pool.clone()),
            config: Arc::new(config),
            cluster_instance_id: Some(local.clone()),
        };
        loop {
            if let Ok(nodes) = crate::services::cluster::node_registry::list_worker_nodes(
                &pool,
                context.config.cluster.lease_ttl_secs.max(1),
            )
            .await
            {
                let results: Vec<_> =
                    stream::iter(nodes.into_iter().filter(|n| n["instance_id"] != local))
                        .map(|node| {
                            let context = &context;
                            async move {
                                let report = probe(context, &node).await;
                                (
                                    node["instance_id"].as_str().unwrap_or_default().to_owned(),
                                    CachedProbe {
                                        origin: node["api_base_url"].as_str().map(str::to_owned),
                                        report,
                                    },
                                )
                            }
                        })
                        .buffer_unordered(4)
                        .collect()
                        .await;
                if let Ok(mut cache) = CACHE.write() {
                    *cache = results.into_iter().collect();
                }
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    });
}

async fn probe(context: &ForwardCallerContext, node: &Value) -> Value {
    let mut trusted = false;
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        if node["status"] != "online" {
            return Err("node_offline");
        }
        let id = node["instance_id"]
            .as_str()
            .ok_or("node_identity_missing")?;
        let origin = node["api_base_url"]
            .as_str()
            .ok_or("worker_api_base_url_missing")?;
        let target = build_trusted_target(
            &context.config.cluster,
            id,
            origin,
            "node_probe_v1",
            &node["capabilities"],
        )
        .await
        .map_err(|e| e.code())?;
        trusted = true;
        probe_target(context, &target).await
    })
    .await
    .unwrap_or(Err("forwarding_probe_timeout"));
    let now = chrono::Utc::now().timestamp_millis();
    json!({"trust_validated":trusted,"reachability_verified":result.is_ok(),
        "reachability_status":result.err().unwrap_or("verified"),
        "observed_at_ms":now,"expires_at_ms":now + TTL_MS})
}

async fn probe_target(
    context: &ForwardCallerContext,
    target: &super::TrustedForwardTarget,
) -> Result<(), &'static str> {
    let request = trusted_request(
        context,
        target,
        reqwest::Method::GET,
        "/api/internal/node-probe",
    )
    .map_err(|_| "forwarding_probe_headers_invalid")?;
    let mut response = request
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .map_err(|_| "forwarding_probe_transport")?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED
        || response.status() == reqwest::StatusCode::FORBIDDEN
    {
        return Err("forwarding_probe_auth_failed");
    }
    if !response.status().is_success() {
        return Err("forwarding_probe_http_failed");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "forwarding_probe_body_failed")?
    {
        if body.len() + chunk.len() > 4096 {
            return Err("forwarding_probe_body_too_large");
        }
        body.extend_from_slice(&chunk);
    }
    let body: Value = serde_json::from_slice(&body).map_err(|_| "forwarding_probe_protocol")?;
    if body["protocol"] != 1 || body["instance_id"].as_str() != Some(target.owner_instance_id()) {
        return Err("forwarding_probe_identity_mismatch");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
