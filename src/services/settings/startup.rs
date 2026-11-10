//! Leader-owned startup configuration synchronization, separate from schema migration.
//! Runners validate the shared baseline without applying local configuration.

use super::{KvSeedAction, RETIRED_CONFIG_KEY_PREFIXES, config_default_seed_actions};
use crate::config::{ClusterRole, Config};
use crate::db::postgres::shared_config::{
    delete_kv_meta, delete_kv_meta_prefix, put_kv_meta_if_absent, shared_config_initialized,
    upsert_kv_meta,
};
use crate::db::postgres::{connect_for_startup, register_repo, sync_agents_from_config_pg};
use sqlx::PgPool;
use std::collections::BTreeSet;

pub(crate) async fn startup_reseed(pool: &PgPool, config: &Config) -> Result<(), String> {
    // Startup precedes leader election; runner-local values cannot seed shared configuration.
    if !shared_config_sync_enabled(config) {
        if !shared_config_initialized(pool).await? {
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
    for prefix in RETIRED_CONFIG_KEY_PREFIXES {
        delete_kv_meta_prefix(pool, prefix).await?;
    }
    upsert_kv_meta(pool, "server_port", &config.server.port.to_string()).await?;
    super::seed_runtime_config_defaults_pg(pool, config).await?;

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

/// Single-node and hub configurations own shared reseeding, including pre-seed config audit.
pub(crate) fn shared_config_sync_enabled(config: &Config) -> bool {
    !config.cluster.enabled || config.cluster.role == ClusterRole::Hub
}

async fn apply_kv_seed_actions(pool: &PgPool, actions: &[KvSeedAction]) -> Result<(), String> {
    for action in actions {
        match action {
            KvSeedAction::Put { key, value } => upsert_kv_meta(pool, key, value).await?,
            KvSeedAction::PutIfAbsent { key, value } => {
                put_kv_meta_if_absent(pool, key, value).await?;
            }
            KvSeedAction::Delete { key } => delete_kv_meta(pool, key).await?,
        }
    }
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

#[cfg(test)]
#[path = "startup_tests.rs"]
mod tests;
