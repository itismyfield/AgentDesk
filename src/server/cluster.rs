use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Row};

use crate::config::{ClusterConfig, Config};
use crate::db::postgres::AdvisoryLockLease;
use crate::server::cluster_session_routing::{
    cluster_capabilities_with_worker_api, worker_api_base_url_from_capabilities,
};

pub(crate) const CLUSTER_LEADER_ADVISORY_LOCK_ID: i64 = 7_801_100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ClusterRole {
    Leader,
    Worker,
    Auto,
}

impl ClusterRole {
    pub(crate) fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "leader" => Self::Leader,
            "worker" => Self::Worker,
            _ => Self::Auto,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Leader => "leader",
            Self::Worker => "worker",
            Self::Auto => "auto",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ClusterRuntime {
    enabled: bool,
    instance_id: String,
    configured_role: ClusterRole,
    effective_role: ClusterRole,
    leader_active: Arc<AtomicBool>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct CapabilityRouteDecision {
    pub(crate) instance_id: Option<String>,
    pub(crate) eligible: bool,
    pub(crate) reasons: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct CapabilityRouteCandidate {
    pub(crate) decision: CapabilityRouteDecision,
    pub(crate) score: i64,
    pub(crate) last_heartbeat_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl ClusterRuntime {
    pub(crate) fn single_node() -> Self {
        Self {
            enabled: false,
            instance_id: "single-node".to_string(),
            configured_role: ClusterRole::Leader,
            effective_role: ClusterRole::Leader,
            leader_active: Arc::new(AtomicBool::new(true)),
        }
    }

    pub(crate) fn is_leader(&self) -> bool {
        !self.enabled || self.leader_active.load(Ordering::Acquire)
    }

    pub(crate) fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub(crate) async fn wait_until_not_leader(&self) {
        if !self.enabled {
            std::future::pending::<()>().await;
            return;
        }
        loop {
            if !self.is_leader() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    pub(crate) fn describe_for_log(&self) -> serde_json::Value {
        serde_json::json!({
            "enabled": self.enabled,
            "instance_id": self.instance_id,
            "configured_role": self.configured_role.as_str(),
            "effective_role": self.effective_role.as_str(),
            "is_leader": self.is_leader(),
        })
    }
}

pub(crate) async fn bootstrap(config: &Config, pg_pool: Option<PgPool>) -> ClusterRuntime {
    if !config.cluster.enabled {
        tracing::info!("[cluster] disabled; running in single-node leader-compatible mode");
        return ClusterRuntime::single_node();
    }

    let Some(pool) = pg_pool else {
        tracing::warn!("[cluster] enabled but PostgreSQL pool is unavailable; disabling cluster");
        return ClusterRuntime::single_node();
    };

    let instance_id = resolve_instance_id(&config.cluster);
    let hostname = crate::services::platform::hostname_short();
    let configured_role = ClusterRole::parse(&config.cluster.role);
    let mut leader_lease = match configured_role {
        ClusterRole::Worker => None,
        ClusterRole::Leader | ClusterRole::Auto => {
            match AdvisoryLockLease::try_acquire(
                &pool,
                CLUSTER_LEADER_ADVISORY_LOCK_ID,
                "cluster-leader",
            )
            .await
            {
                Ok(lease) => lease,
                Err(error) => {
                    tracing::warn!("[cluster] leader lease acquisition failed: {error}");
                    None
                }
            }
        }
    };
    let effective_role = if leader_lease.is_some() {
        ClusterRole::Leader
    } else {
        ClusterRole::Worker
    };
    let leader_active = Arc::new(AtomicBool::new(leader_lease.is_some()));
    let labels = serde_json::Value::Array(
        config
            .cluster
            .labels
            .iter()
            .map(|label| serde_json::Value::String(label.clone()))
            .collect(),
    );
    let capabilities = cluster_capabilities_with_worker_api(&config.cluster);
    let pid = std::process::id() as i32;

    if let Err(error) = upsert_worker_node(
        &pool,
        &instance_id,
        &hostname,
        pid,
        configured_role,
        effective_role,
        &labels,
        &capabilities,
    )
    .await
    {
        tracing::warn!("[cluster] worker node registration failed: {error}");
    }
    if let Err(error) = upsert_worker_mcp_endpoints(&pool, &instance_id, &capabilities).await {
        tracing::warn!("[cluster] worker MCP endpoint registration failed: {error}");
    }

    spawn_heartbeat_loop(
        pool,
        instance_id.clone(),
        hostname,
        pid,
        configured_role,
        labels,
        capabilities,
        config.cluster.heartbeat_interval_secs,
        leader_active.clone(),
        leader_lease.take(),
    );

    let runtime = ClusterRuntime {
        enabled: true,
        instance_id,
        configured_role,
        effective_role,
        leader_active,
    };
    tracing::info!(cluster = %runtime.describe_for_log(), "[cluster] runtime bootstrapped");
    runtime
}

#[allow(clippy::too_many_arguments)]
fn spawn_heartbeat_loop(
    pool: PgPool,
    instance_id: String,
    hostname: String,
    pid: i32,
    configured_role: ClusterRole,
    labels: serde_json::Value,
    capabilities: serde_json::Value,
    heartbeat_interval_secs: u64,
    leader_active: Arc<AtomicBool>,
    mut leader_lease: Option<AdvisoryLockLease>,
) {
    let interval_secs = heartbeat_interval_secs.max(1);
    let leader_eligible = matches!(configured_role, ClusterRole::Leader | ClusterRole::Auto);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Some(lease) = leader_lease.as_mut()
                && let Err(error) = lease.keepalive().await
            {
                tracing::warn!("[cluster] leader lease keepalive failed: {error}");
                leader_active.store(false, Ordering::Release);
                leader_lease = None;
            }
            // Live failover: if this node is eligible to lead and currently is
            // not leader, retry the advisory lock. Picks up leadership when the
            // previous leader's session is gone (Postgres releases the lock on
            // session disconnect), without waiting for a dcserver restart.
            if leader_eligible && leader_lease.is_none() && !leader_active.load(Ordering::Acquire) {
                match AdvisoryLockLease::try_acquire(
                    &pool,
                    CLUSTER_LEADER_ADVISORY_LOCK_ID,
                    "cluster-leader",
                )
                .await
                {
                    Ok(Some(new_lease)) => {
                        tracing::info!(
                            instance_id,
                            "[cluster] acquired leader advisory lock via failover"
                        );
                        leader_lease = Some(new_lease);
                        leader_active.store(true, Ordering::Release);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!("[cluster] leader lease retry failed: {error}");
                    }
                }
            }
            let current_effective_role = if leader_active.load(Ordering::Acquire) {
                ClusterRole::Leader
            } else {
                ClusterRole::Worker
            };
            if let Err(error) = upsert_worker_node(
                &pool,
                &instance_id,
                &hostname,
                pid,
                configured_role,
                current_effective_role,
                &labels,
                &capabilities,
            )
            .await
            {
                tracing::warn!("[cluster] heartbeat failed: {error}");
            }
            if let Err(error) =
                upsert_worker_mcp_endpoints(&pool, &instance_id, &capabilities).await
            {
                tracing::warn!("[cluster] heartbeat MCP endpoint sync failed: {error}");
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn upsert_worker_node(
    pool: &PgPool,
    instance_id: &str,
    hostname: &str,
    pid: i32,
    configured_role: ClusterRole,
    effective_role: ClusterRole,
    labels: &serde_json::Value,
    capabilities: &serde_json::Value,
) -> Result<(), String> {
    sqlx::query(
        r#"
        INSERT INTO worker_nodes (
            instance_id, hostname, process_id, role, effective_role, status,
            labels, capabilities, last_heartbeat_at, started_at, updated_at
        )
        VALUES ($1, $2, $3, $4, $5, 'online', $6, $7, NOW(), NOW(), NOW())
        ON CONFLICT (instance_id) DO UPDATE SET
            hostname = EXCLUDED.hostname,
            process_id = EXCLUDED.process_id,
            role = EXCLUDED.role,
            effective_role = EXCLUDED.effective_role,
            status = 'online',
            labels = EXCLUDED.labels,
            capabilities = EXCLUDED.capabilities,
            last_heartbeat_at = NOW(),
            updated_at = NOW()
        "#,
    )
    .bind(instance_id)
    .bind(hostname)
    .bind(pid)
    .bind(configured_role.as_str())
    .bind(effective_role.as_str())
    .bind(labels)
    .bind(capabilities)
    .execute(pool)
    .await
    .map(|_| ())
    .map_err(|error| format!("upsert worker_nodes: {error}"))
}

async fn upsert_worker_mcp_endpoints(
    pool: &PgPool,
    instance_id: &str,
    capabilities: &Value,
) -> Result<(), String> {
    let mut endpoint_names = Vec::new();
    let Some(mcp) = capabilities.get("mcp") else {
        sqlx::query("DELETE FROM worker_mcp_endpoints WHERE instance_id = $1")
            .bind(instance_id)
            .execute(pool)
            .await
            .map_err(|error| format!("clear worker_mcp_endpoints: {error}"))?;
        return Ok(());
    };

    match mcp {
        Value::Object(map) => {
            for (name, metadata) in map {
                if name.trim().is_empty() {
                    continue;
                }
                endpoint_names.push(name.clone());
                let healthy = metadata
                    .get("healthy")
                    .and_then(|value| value.as_bool())
                    .or_else(|| metadata.as_bool());
                sqlx::query(
                    r#"
                    INSERT INTO worker_mcp_endpoints (
                        instance_id, endpoint_name, healthy, metadata, last_checked_at, updated_at
                    )
                    VALUES ($1, $2, $3, $4, NOW(), NOW())
                    ON CONFLICT (instance_id, endpoint_name) DO UPDATE SET
                        healthy = EXCLUDED.healthy,
                        metadata = EXCLUDED.metadata,
                        last_checked_at = NOW(),
                        updated_at = NOW()
                    "#,
                )
                .bind(instance_id)
                .bind(name)
                .bind(healthy)
                .bind(metadata)
                .execute(pool)
                .await
                .map_err(|error| format!("upsert worker_mcp_endpoints: {error}"))?;
            }
        }
        Value::Array(names) => {
            for endpoint in names.iter().filter_map(|value| value.as_str()) {
                if endpoint.trim().is_empty() {
                    continue;
                }
                endpoint_names.push(endpoint.to_string());
                sqlx::query(
                    r#"
                    INSERT INTO worker_mcp_endpoints (
                        instance_id, endpoint_name, healthy, metadata, last_checked_at, updated_at
                    )
                    VALUES ($1, $2, NULL, '{}'::jsonb, NOW(), NOW())
                    ON CONFLICT (instance_id, endpoint_name) DO UPDATE SET
                        healthy = EXCLUDED.healthy,
                        metadata = EXCLUDED.metadata,
                        last_checked_at = NOW(),
                        updated_at = NOW()
                    "#,
                )
                .bind(instance_id)
                .bind(endpoint)
                .execute(pool)
                .await
                .map_err(|error| format!("upsert worker_mcp_endpoints: {error}"))?;
            }
        }
        _ => {}
    }

    sqlx::query(
        "DELETE FROM worker_mcp_endpoints
          WHERE instance_id = $1
            AND NOT (endpoint_name = ANY($2))",
    )
    .bind(instance_id)
    .bind(endpoint_names)
    .execute(pool)
    .await
    .map_err(|error| format!("prune worker_mcp_endpoints: {error}"))?;
    Ok(())
}

fn resolve_instance_id(config: &ClusterConfig) -> String {
    if let Some(value) = config
        .instance_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return value.to_string();
    }
    if let Ok(value) = std::env::var("AGENTDESK_INSTANCE_ID")
        && !value.trim().is_empty()
    {
        return value.trim().to_string();
    }
    format!(
        "{}-{}",
        crate::services::platform::hostname_short(),
        std::process::id()
    )
}

pub(crate) async fn list_worker_nodes(
    pool: &PgPool,
    lease_ttl_secs: u64,
) -> Result<Vec<serde_json::Value>, String> {
    let rows = sqlx::query(
        r#"
        SELECT
            instance_id,
            hostname,
            process_id,
            role,
            effective_role,
            CASE
                WHEN last_heartbeat_at < NOW() - ($1::BIGINT * INTERVAL '1 second') THEN 'offline'
                ELSE status
            END AS computed_status,
            labels,
            capabilities,
            COALESCE(active_dispatches.active_dispatch_count, 0)::BIGINT AS active_dispatch_count,
            last_heartbeat_at,
            started_at,
            updated_at
        FROM worker_nodes
        LEFT JOIN (
            SELECT claim_owner, COUNT(*)::BIGINT AS active_dispatch_count
              FROM dispatch_outbox
             WHERE status IN ('claimed', 'processing')
               AND claim_owner IS NOT NULL
             GROUP BY claim_owner
        ) active_dispatches ON active_dispatches.claim_owner = worker_nodes.instance_id
        ORDER BY last_heartbeat_at DESC, instance_id ASC
        "#,
    )
    .bind(lease_ttl_secs.max(1) as i64)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("query worker_nodes: {error}"))?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let capabilities = row
                .try_get::<Option<serde_json::Value>, _>("capabilities")
                .ok()
                .flatten()
                .unwrap_or_else(|| serde_json::json!({}));
            let api_base_url = worker_api_base_url_from_capabilities(&capabilities);
            let session_api_routable = api_base_url.is_some();
            serde_json::json!({
                "instance_id": row.try_get::<String, _>("instance_id").ok(),
                "hostname": row.try_get::<Option<String>, _>("hostname").ok().flatten(),
                "process_id": row.try_get::<Option<i32>, _>("process_id").ok().flatten(),
                "role": row.try_get::<Option<String>, _>("role").ok().flatten(),
                "effective_role": row.try_get::<Option<String>, _>("effective_role").ok().flatten(),
                "status": row.try_get::<Option<String>, _>("computed_status").ok().flatten(),
                "labels": row
                    .try_get::<Option<serde_json::Value>, _>("labels")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| serde_json::json!([])),
                "capabilities": capabilities,
                "active_dispatch_count": row
                    .try_get::<Option<i64>, _>("active_dispatch_count")
                    .ok()
                    .flatten()
                    .unwrap_or(0),
                "api_base_url": api_base_url,
                "session_api_routable": session_api_routable,
                "last_heartbeat_at": row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_heartbeat_at").ok().flatten(),
                "started_at": row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("started_at").ok().flatten(),
                "updated_at": row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("updated_at").ok().flatten(),
            })
        })
        .collect())
}

pub(crate) async fn worker_node_snapshot_by_instance(
    pool: &PgPool,
    instance_id: &str,
    lease_ttl_secs: u64,
) -> Result<Option<Value>, String> {
    Ok(list_worker_nodes(pool, lease_ttl_secs)
        .await?
        .into_iter()
        .find(|node| node.get("instance_id").and_then(|value| value.as_str()) == Some(instance_id)))
}

pub(crate) fn explain_capability_match(
    node: &Value,
    required_capabilities: &Value,
) -> CapabilityRouteDecision {
    let instance_id = node
        .get("instance_id")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let mut reasons = Vec::new();

    let hard_required = hard_required_capabilities(required_capabilities);
    if !hard_required.is_object() {
        return CapabilityRouteDecision {
            instance_id,
            eligible: true,
            reasons,
        };
    }

    let labels = node
        .get("labels")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let labels = labels
        .iter()
        .filter_map(|value| value.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(required_labels) = hard_required
        .get("labels")
        .and_then(|value| value.as_array())
    {
        for label in required_labels.iter().filter_map(|value| value.as_str()) {
            if !labels.contains(label) {
                reasons.push(format!("missing label '{label}'"));
            }
        }
    }

    let capabilities = node.get("capabilities").unwrap_or(&Value::Null);
    let providers = capabilities
        .get("providers")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let providers = providers
        .iter()
        .filter_map(|value| value.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(required_providers) = hard_required
        .get("providers")
        .and_then(|value| value.as_array())
    {
        for provider in required_providers.iter().filter_map(|value| value.as_str()) {
            if !providers.contains(provider) {
                reasons.push(format!("missing provider '{provider}'"));
            }
        }
    }

    if let Some(required_mcp) = hard_required.get("mcp") {
        match required_mcp {
            Value::Array(names) => {
                for endpoint in names.iter().filter_map(|value| value.as_str()) {
                    if capabilities
                        .get("mcp")
                        .and_then(|mcp| mcp.get(endpoint))
                        .is_none()
                    {
                        reasons.push(format!("missing MCP endpoint '{endpoint}'"));
                    }
                }
            }
            Value::Object(map) => {
                for (endpoint, requirement) in map {
                    let actual = capabilities.get("mcp").and_then(|mcp| mcp.get(endpoint));
                    let Some(actual) = actual else {
                        reasons.push(format!("missing MCP endpoint '{endpoint}'"));
                        continue;
                    };
                    let requires_healthy = requirement
                        .get("healthy")
                        .and_then(|value| value.as_bool())
                        .or_else(|| requirement.as_bool())
                        .unwrap_or(false);
                    let actual_healthy = actual
                        .get("healthy")
                        .and_then(|value| value.as_bool())
                        .or_else(|| actual.as_bool())
                        .unwrap_or(false);
                    if requires_healthy && !actual_healthy {
                        reasons.push(format!("MCP endpoint '{endpoint}' is not healthy"));
                    }
                }
            }
            _ => {}
        }
    }

    CapabilityRouteDecision {
        instance_id,
        eligible: reasons.is_empty(),
        reasons,
    }
}

pub(crate) fn select_capability_route(
    nodes: &[Value],
    required_capabilities: &Value,
) -> Vec<CapabilityRouteCandidate> {
    let preferred = preferred_capabilities(required_capabilities);
    let mut candidates = nodes
        .iter()
        .filter(|node| node.get("status").and_then(|value| value.as_str()) == Some("online"))
        .map(|node| {
            let decision = explain_capability_match(node, required_capabilities);
            let score = if decision.eligible {
                capability_preference_score(node, preferred)
            } else {
                0
            };
            CapabilityRouteCandidate {
                decision,
                score,
                last_heartbeat_at: parse_last_heartbeat(node),
            }
        })
        .filter(|candidate| candidate.decision.eligible)
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| right.last_heartbeat_at.cmp(&left.last_heartbeat_at))
            .then_with(|| left.decision.instance_id.cmp(&right.decision.instance_id))
    });
    candidates
}

fn hard_required_capabilities(capabilities: &Value) -> &Value {
    capabilities
        .get("required")
        .filter(|value| value.is_object())
        .unwrap_or(capabilities)
}

fn preferred_capabilities(capabilities: &Value) -> Option<&Value> {
    capabilities
        .get("preferred")
        .filter(|value| value.is_object())
}

fn capability_preference_score(node: &Value, preferred: Option<&Value>) -> i64 {
    let Some(preferred) = preferred else {
        return 0;
    };
    let mut score = 0;

    let labels = string_set(node.get("labels"));
    if let Some(preferred_labels) = preferred.get("labels").and_then(|value| value.as_array()) {
        let preferred_count = preferred_labels.len() as i64;
        score += preferred_labels
            .iter()
            .enumerate()
            .filter_map(|(index, value)| value.as_str().map(|label| (index, label)))
            .filter(|(_, label)| labels.contains(label))
            .map(|(index, _)| preferred_count.saturating_sub(index as i64))
            .sum::<i64>();
    }

    let capabilities = node.get("capabilities").unwrap_or(&Value::Null);
    let providers = string_set(capabilities.get("providers"));
    if let Some(preferred_providers) = preferred
        .get("providers")
        .and_then(|value| value.as_array())
    {
        score += preferred_providers
            .iter()
            .filter_map(|value| value.as_str())
            .filter(|provider| providers.contains(provider))
            .count() as i64;
    }

    if let Some(preferred_mcp) = preferred.get("mcp") {
        match preferred_mcp {
            Value::Array(names) => {
                score += names
                    .iter()
                    .filter_map(|value| value.as_str())
                    .filter(|endpoint| {
                        capabilities
                            .get("mcp")
                            .and_then(|mcp| mcp.get(*endpoint))
                            .is_some()
                    })
                    .count() as i64;
            }
            Value::Object(map) => {
                for (endpoint, preference) in map {
                    let actual = capabilities.get("mcp").and_then(|mcp| mcp.get(endpoint));
                    let Some(actual) = actual else {
                        continue;
                    };
                    let prefers_healthy = preference
                        .get("healthy")
                        .and_then(|value| value.as_bool())
                        .or_else(|| preference.as_bool())
                        .unwrap_or(false);
                    let actual_healthy = actual
                        .get("healthy")
                        .and_then(|value| value.as_bool())
                        .or_else(|| actual.as_bool())
                        .unwrap_or(false);
                    if !prefers_healthy || actual_healthy {
                        score += 1;
                    }
                }
            }
            _ => {}
        }
    }

    score
}

fn string_set(value: Option<&Value>) -> std::collections::BTreeSet<&str> {
    value
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .collect()
}

fn parse_last_heartbeat(node: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    node.get("last_heartbeat_at")
        .and_then(|value| value.as_str())
        .and_then(|value| {
            chrono::DateTime::parse_from_rfc3339(value)
                .ok()
                .map(|value| value.with_timezone(&chrono::Utc))
        })
}

#[cfg(test)]
mod tests {
    use super::{
        ClusterRole, explain_capability_match, resolve_instance_id, select_capability_route,
    };
    use crate::config::ClusterConfig;
    use serde_json::json;

    #[test]
    fn cluster_role_parses_known_values_and_defaults_to_auto() {
        assert_eq!(ClusterRole::parse("leader"), ClusterRole::Leader);
        assert_eq!(ClusterRole::parse("WORKER"), ClusterRole::Worker);
        assert_eq!(ClusterRole::parse("anything-else"), ClusterRole::Auto);
    }

    #[test]
    fn configured_instance_id_wins() {
        let config = ClusterConfig {
            instance_id: Some("mac-mini-release".to_string()),
            ..ClusterConfig::default()
        };
        assert_eq!(resolve_instance_id(&config), "mac-mini-release");
    }

    #[test]
    fn capability_match_accepts_labels_providers_and_healthy_mcp() {
        let node = json!({
            "instance_id": "mac-book-release",
            "labels": ["mac-book"],
            "capabilities": {
                "providers": ["codex"],
                "mcp": {"filesystem": {"healthy": true}}
            }
        });
        let required = json!({
            "labels": ["mac-book"],
            "providers": ["codex"],
            "mcp": {"filesystem": {"healthy": true}}
        });

        let decision = explain_capability_match(&node, &required);
        assert!(decision.eligible, "{:?}", decision.reasons);
    }

    #[test]
    fn capability_match_reports_exclusion_reasons() {
        let node = json!({
            "instance_id": "mac-mini-release",
            "labels": ["mac-mini"],
            "capabilities": {
                "providers": ["claude"],
                "mcp": {"filesystem": {"healthy": false}}
            }
        });
        let required = json!({
            "labels": ["mac-book"],
            "providers": ["codex"],
            "mcp": {"filesystem": {"healthy": true}, "unreal": true}
        });

        let decision = explain_capability_match(&node, &required);
        assert!(!decision.eligible);
        assert!(
            decision
                .reasons
                .iter()
                .any(|reason| reason.contains("mac-book"))
        );
        assert!(
            decision
                .reasons
                .iter()
                .any(|reason| reason.contains("codex"))
        );
        assert!(
            decision
                .reasons
                .iter()
                .any(|reason| reason.contains("filesystem"))
        );
        assert!(
            decision
                .reasons
                .iter()
                .any(|reason| reason.contains("unreal"))
        );
    }

    #[test]
    fn required_namespace_remains_hard_and_preferred_namespace_is_soft() {
        let mac_mini = json!({
            "instance_id": "mac-mini-release",
            "status": "online",
            "labels": ["mac-mini"],
            "capabilities": {"providers": ["codex"]},
            "last_heartbeat_at": "2026-05-03T00:00:00Z"
        });
        let mac_book = json!({
            "instance_id": "mac-book-release",
            "status": "online",
            "labels": ["mac-book"],
            "capabilities": {"providers": ["claude"]},
            "last_heartbeat_at": "2026-05-03T00:00:01Z"
        });
        let route = json!({
            "required": {"providers": ["codex"]},
            "preferred": {"labels": ["mac-book"]}
        });

        let candidates = select_capability_route(&[mac_mini, mac_book], &route);
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].decision.instance_id.as_deref(),
            Some("mac-mini-release")
        );
        assert_eq!(candidates[0].score, 0);
    }

    #[test]
    fn preferred_label_ranks_online_match_but_falls_back_to_online_candidate() {
        let offline_preferred = json!({
            "instance_id": "mac-book-release",
            "status": "offline",
            "labels": ["mac-book"],
            "capabilities": {"providers": ["codex"]},
            "last_heartbeat_at": "2026-05-03T00:00:02Z"
        });
        let fallback = json!({
            "instance_id": "mac-mini-release",
            "status": "online",
            "labels": ["mac-mini"],
            "capabilities": {"providers": ["codex"]},
            "last_heartbeat_at": "2026-05-03T00:00:01Z"
        });
        let preferred_online = json!({
            "instance_id": "mac-book-release",
            "status": "online",
            "labels": ["mac-book"],
            "capabilities": {"providers": ["codex"]},
            "last_heartbeat_at": "2026-05-03T00:00:02Z"
        });
        let route = json!({"preferred": {"labels": ["mac-book"]}});

        let fallback_candidates =
            select_capability_route(&[offline_preferred, fallback.clone()], &route);
        assert_eq!(
            fallback_candidates[0].decision.instance_id.as_deref(),
            Some("mac-mini-release")
        );
        assert_eq!(fallback_candidates[0].score, 0);

        let preferred_candidates = select_capability_route(&[fallback, preferred_online], &route);
        assert_eq!(
            preferred_candidates[0].decision.instance_id.as_deref(),
            Some("mac-book-release")
        );
        assert_eq!(preferred_candidates[0].score, 1);
    }

    #[test]
    fn preferred_label_order_beats_newer_heartbeat() {
        let first_label = json!({
            "instance_id": "mac-book-release",
            "status": "online",
            "labels": ["mac-book"],
            "capabilities": {"providers": ["codex"]},
            "last_heartbeat_at": "2026-05-03T00:00:01Z"
        });
        let second_label_newer = json!({
            "instance_id": "mac-mini-release",
            "status": "online",
            "labels": ["mac-mini"],
            "capabilities": {"providers": ["codex"]},
            "last_heartbeat_at": "2026-05-03T00:00:02Z"
        });
        let route = json!({"preferred": {"labels": ["mac-book", "mac-mini"]}});

        let candidates = select_capability_route(&[second_label_newer, first_label], &route);
        assert_eq!(
            candidates[0].decision.instance_id.as_deref(),
            Some("mac-book-release")
        );
        assert!(candidates[0].score > candidates[1].score);
    }

    #[test]
    fn equally_preferred_candidates_tie_break_by_latest_heartbeat() {
        let stale = json!({
            "instance_id": "mac-mini-release",
            "status": "online",
            "labels": ["mac"],
            "capabilities": {"providers": ["codex"]},
            "last_heartbeat_at": "2026-05-03T00:00:01Z"
        });
        let fresh = json!({
            "instance_id": "mac-book-release",
            "status": "online",
            "labels": ["mac"],
            "capabilities": {"providers": ["codex"]},
            "last_heartbeat_at": "2026-05-03T00:00:02Z"
        });
        let route = json!({"preferred": {"labels": ["mac"]}});

        let candidates = select_capability_route(&[stale, fresh], &route);
        assert_eq!(
            candidates[0].decision.instance_id.as_deref(),
            Some("mac-book-release")
        );
    }
}
