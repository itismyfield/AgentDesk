//! Only the census policy receives authenticated health observations; JavaScript gets the report.
use serde_json::{Value, json};
use std::time::Duration;

use super::store::RoutineStore;
use crate::services::{legacy_body_census as census, session_forwarding};

pub(super) const SCRIPT_REF: &str = "monitoring/legacy-body-census.js";
const HOSTS: [&str; 2] = ["mac-mini-release", "mac-book-release"];
const HOST_TIMEOUT: Duration = Duration::from_secs(10);
const TICK_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

pub(super) fn applies(script_ref: &str) -> bool {
    script_ref == SCRIPT_REF
}

fn failed(id: &str, reason: &str) -> Value {
    json!({"host":id,"errors":[reason]})
}

#[cfg(test)]
tokio::task_local! {
    static TEST_INPUT: (session_forwarding::ForwardCallerContext, Value, i64);
}
#[cfg(all(test, unix))]
#[path = "legacy_body_census_tests.rs"]
mod tests;

pub(super) async fn observation(store: &RoutineStore) -> Value {
    let context =
        session_forwarding::ForwardCallerContext::from_live_globals(Some(store.pool().clone()));
    let identity = census::detail_json();
    #[cfg(test)]
    let (context, identity) = TEST_INPUT
        .try_with(|(context, identity, _)| (context.clone(), identity.clone()))
        .unwrap_or((context, identity));
    let inputs = tokio::time::timeout(TICK_TIMEOUT, collect(store, &context))
        .await
        .unwrap_or_else(|_| HOSTS.map(|id| failed(id, "TICK_TIMEOUT")));
    let now = chrono::Utc::now().timestamp_millis();
    #[cfg(test)]
    let now = TEST_INPUT.try_with(|(_, _, now)| *now).unwrap_or(now);
    let report = census::evaluate(
        HOSTS,
        &inputs,
        now,
        identity["build_sha"].as_str().unwrap_or_default(),
        identity["census_hash"].as_str().unwrap_or_default(),
    );
    json!({"kind":"legacy_body_census_report","schema_version":1,
        "observation_policy_version":1,"report":report})
}

async fn collect(
    store: &RoutineStore,
    context: &session_forwarding::ForwardCallerContext,
) -> [Value; 2] {
    let nodes = tokio::time::timeout(
        HOST_TIMEOUT,
        crate::services::cluster::node_registry::list_worker_nodes(
            store.pool(),
            context.config.cluster.lease_ttl_secs,
        ),
    )
    .await;
    let Ok(Ok(nodes)) = nodes else {
        return HOSTS.map(|id| failed(id, "HOST_REGISTRY_UNAVAILABLE"));
    };
    let (first, second) = tokio::join!(
        fetch(context, &nodes, HOSTS[0]),
        fetch(context, &nodes, HOSTS[1]),
    );
    [first, second]
}

async fn fetch(
    context: &session_forwarding::ForwardCallerContext,
    nodes: &[Value],
    id: &str,
) -> Value {
    let result = tokio::time::timeout(HOST_TIMEOUT, async {
        let mut matching = nodes.iter().filter(|node| node["instance_id"] == id);
        let node = matching.next().ok_or("HOST_MISSING")?;
        if matching.next().is_some() || node["status"] != "online" {
            return Err("HOST_REGISTRY_INVALID");
        }
        let origin = node["api_base_url"].as_str().ok_or("HOST_ORIGIN_MISSING")?;
        #[cfg(all(test, unix))]
        let loopback = TEST_INPUT
            .try_with(|_| {
                origin
                    .strip_prefix("http://")
                    .and_then(|raw| raw.parse().ok())
                    .map(session_forwarding::loopback_seam::allow_loopback_for_test)
            })
            .ok()
            .flatten();
        let target = session_forwarding::build_trusted_target(
            &context.config.cluster,
            id,
            origin,
            "node_probe_v1",
            &node["capabilities"],
        )
        .await;
        #[cfg(all(test, unix))]
        drop(loopback);
        let target = target.map_err(|_| "HOST_TRUST_REJECTED")?;
        let request = session_forwarding::trusted_request(
            context,
            &target,
            reqwest::Method::GET,
            "/api/health/detail",
        )
        .map_err(|_| "HOST_AUTH_UNAVAILABLE")?;
        let mut response = request
            .timeout(HOST_TIMEOUT)
            .send()
            .await
            .map_err(|_| "HOST_GET_FAILED")?;
        if !response.status().is_success() {
            return Err("HOST_HTTP_REJECTED");
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| "HOST_BODY_TRUNCATED")? {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err("HOST_BODY_TOO_LARGE");
            }
            bytes.extend_from_slice(&chunk);
        }
        let mut body: Value = serde_json::from_slice(&bytes).map_err(|_| "HOST_BODY_INVALID")?;
        let mut observation = body
            .get_mut("legacy_body_census")
            .map(Value::take)
            .ok_or("HOST_OBSERVATION_MISSING")?;
        if observation["host"] != id
            && let Some(errors) = observation.get_mut("errors").and_then(Value::as_array_mut)
        {
            errors.push(json!("HOST_ENDPOINT_IDENTITY_MISMATCH"));
        }
        Ok(observation)
    })
    .await
    .unwrap_or(Err("HOST_TIMEOUT"));
    result.unwrap_or_else(|reason| failed(id, reason))
}
