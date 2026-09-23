//! Leader-owned configuration synchronization, separate from schema migration.
//! Runners validate the shared baseline without applying their local YAML.

use super::{connect_for_startup, register_repo, sync_agents_from_config_pg};
use crate::config::{ClusterRole, Config};
use crate::services::settings::{KvSeedAction, config_default_seed_actions};
use sqlx::PgPool;
use std::collections::BTreeSet;

pub async fn startup_reseed(pool: &PgPool, config: &Config) -> Result<(), String> {
    // Startup runs before leader election. A runner's local paths, port and reset
    // flags must never become the shared configuration, even on the first boot.
    if !shared_config_sync_enabled(config) {
        let initialized: bool = sqlx::query_scalar(
            "SELECT count(*) = 2 FROM kv_meta WHERE key IN ('server_port', 'runtime-config')",
        )
        .fetch_one(pool)
        .await
        .map_err(|error| format!("check hub-owned shared configuration: {error}"))?;
        if !initialized {
            return Err(
                "shared configuration is not initialized; start a node configured as \
                 cluster.role=hub before starting runner/auto nodes (missing server_port \
                 or runtime-config); this node will not seed the shared database"
                    .to_string(),
            );
        }
        tracing::info!(
            role = %config.cluster.role,
            "[startup] using hub-owned shared configuration; skipping all local YAML reseeding"
        );
        return Ok(());
    }

    apply_kv_seed_actions(pool, &config_default_seed_actions(config)).await?;
    upsert_kv_meta(pool, "server_port", &config.server.port.to_string()).await?;
    crate::services::settings::seed_runtime_config_defaults_pg(pool, config).await?;
    crate::server::routes::escalation::seed_escalation_defaults_pg(pool, config).await?;
    let pipeline_path = config.policies.dir.join("default-pipeline.yaml");
    crate::db::table_metadata::sync_pipeline_stages_from_yaml_pg(pool, &pipeline_path)
        .await
        .map_err(|error| {
            format!(
                "sync pipeline_stages from {}: {error}",
                pipeline_path.display()
            )
        })?;

    for repo_id in normalized_repo_ids(&config.github.repos) {
        register_repo(pool, &repo_id).await?;
    }

    sync_agents_from_config_pg(pool, &config.agents).await?;
    Ok(())
}

pub async fn startup_reseed_with_warmup_pool(
    runtime_pool: &PgPool,
    config: &Config,
) -> Result<(), String> {
    if !shared_config_sync_enabled(config) {
        return startup_reseed(runtime_pool, config).await;
    }
    let startup_pg_pool = match connect_for_startup(config).await {
        Ok(pool) => pool,
        Err(error) => {
            tracing::warn!(
                "[startup] postgres warmup pool unavailable; falling back to runtime pool: {error}"
            );
            None
        }
    };
    let startup_pool = startup_pg_pool.as_ref().unwrap_or(runtime_pool);
    startup_reseed(startup_pool, config).await?;
    drop(startup_pg_pool);
    Ok(())
}

/// Whether this node owns synchronization of YAML into shared configuration.
/// True for single-node deployments (cluster disabled) and for the node
/// explicitly configured as `cluster.role: hub`. Runner/auto nodes return
/// false. Schema migration and node-local initialization use separate paths.
/// Config audit and explicit imports share this ownership check because they
/// can update the agent roster before `startup_reseed` runs.
pub(crate) fn shared_config_sync_enabled(config: &Config) -> bool {
    !config.cluster.enabled || config.cluster.role == ClusterRole::Hub
}

async fn apply_kv_seed_actions(pool: &PgPool, actions: &[KvSeedAction]) -> Result<(), String> {
    for action in actions {
        match action {
            KvSeedAction::Put { key, value } => {
                upsert_kv_meta(pool, key, value).await?;
            }
            KvSeedAction::PutIfAbsent { key, value } => {
                sqlx::query(
                    "INSERT INTO kv_meta (key, value)
                     VALUES ($1, $2)
                     ON CONFLICT (key) DO NOTHING",
                )
                .bind(key)
                .bind(value)
                .execute(pool)
                .await
                .map_err(|error| format!("seed kv_meta {key}: {error}"))?;
            }
            KvSeedAction::Delete { key } => {
                sqlx::query("DELETE FROM kv_meta WHERE key = $1")
                    .bind(key)
                    .execute(pool)
                    .await
                    .map_err(|error| format!("delete retired kv_meta {key}: {error}"))?;
            }
        }
    }
    Ok(())
}

pub(super) async fn upsert_kv_meta(pool: &PgPool, key: &str, value: &str) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO kv_meta (key, value)
         VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE
         SET value = EXCLUDED.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await
    .map_err(|error| format!("upsert kv_meta {key}: {error}"))?;
    Ok(())
}

fn normalized_repo_ids(repo_ids: &[String]) -> Vec<String> {
    let mut deduped = BTreeSet::new();
    for raw_repo_id in repo_ids {
        let repo_id = raw_repo_id.trim();
        if repo_id.is_empty() {
            continue;
        }
        if !repo_id.contains('/') {
            tracing::warn!(
                "[startup] skipping invalid github.repos entry {:?}: expected owner/repo",
                raw_repo_id
            );
            continue;
        }
        deduped.insert(repo_id.to_string());
    }
    deduped.into_iter().collect()
}
