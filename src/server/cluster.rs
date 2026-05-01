use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use sqlx::{PgPool, Row};

use crate::config::{ClusterConfig, Config};
use crate::db::postgres::AdvisoryLockLease;

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
    let capabilities = serde_json::Value::Object(config.cluster.capabilities.clone());
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

    spawn_heartbeat_loop(
        pool,
        instance_id.clone(),
        hostname,
        pid,
        configured_role,
        effective_role,
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
    effective_role: ClusterRole,
    labels: serde_json::Value,
    capabilities: serde_json::Value,
    heartbeat_interval_secs: u64,
    leader_active: Arc<AtomicBool>,
    mut leader_lease: Option<AdvisoryLockLease>,
) {
    let interval_secs = heartbeat_interval_secs.max(1);
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
            }
            let current_effective_role = if leader_active.load(Ordering::Acquire) {
                effective_role
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

#[cfg(test)]
mod tests {
    use super::{ClusterRole, resolve_instance_id};
    use crate::config::ClusterConfig;

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
            last_heartbeat_at,
            started_at,
            updated_at
        FROM worker_nodes
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
                "capabilities": row
                    .try_get::<Option<serde_json::Value>, _>("capabilities")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| serde_json::json!({})),
                "last_heartbeat_at": row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_heartbeat_at").ok().flatten(),
                "started_at": row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("started_at").ok().flatten(),
                "updated_at": row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("updated_at").ok().flatten(),
            })
        })
        .collect())
}
