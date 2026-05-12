//! #2049 Finding 9: retention sweep split out of `mod.rs`. Prunes old
//! observability rows so disk and index growth stay bounded on long-lived
//! single-node deployments. Schedule lives in the worker loop; this module
//! is pure I/O.

use std::sync::Arc;

use sqlx::PgPool;

use super::worker::storage_handles;
use super::{
    DEFAULT_COUNTER_SNAPSHOT_RETENTION_DAYS, DEFAULT_OBSERVABILITY_EVENT_RETENTION_DAYS,
    DEFAULT_QUALITY_EVENT_RETENTION_DAYS, ObservabilityRuntime,
};

/// #2049 Finding 9: prune old rows from observability tables to bound disk
/// and index growth on long-lived single-node deployments. Retention windows
/// are conservative; `observability_counter_snapshots` is pruned aggressively
/// (7d) because analytics only queries the latest snapshot per
/// `(provider, channel_id)`.
pub(super) async fn run_retention_sweep(runtime: &Arc<ObservabilityRuntime>) {
    let handles = storage_handles(runtime);
    let Some(pool) = handles.pg_pool.as_ref() else {
        return;
    };

    let event_days = env_retention_days(
        "ADK_OBSERVABILITY_EVENT_RETENTION_DAYS",
        DEFAULT_OBSERVABILITY_EVENT_RETENTION_DAYS,
    );
    let quality_days = env_retention_days(
        "ADK_OBSERVABILITY_QUALITY_RETENTION_DAYS",
        DEFAULT_QUALITY_EVENT_RETENTION_DAYS,
    );
    let snapshot_days = env_retention_days(
        "ADK_OBSERVABILITY_COUNTER_SNAPSHOT_RETENTION_DAYS",
        DEFAULT_COUNTER_SNAPSHOT_RETENTION_DAYS,
    );

    if event_days > 0 {
        prune_table_by_created_at(pool, "observability_events", event_days).await;
    }
    if quality_days > 0 {
        prune_table_by_created_at(pool, "agent_quality_event", quality_days).await;
    }
    if snapshot_days > 0 {
        prune_table_by_snapshot_at(pool, "observability_counter_snapshots", snapshot_days).await;
    }
}

fn env_retention_days(var: &str, default_days: i64) -> i64 {
    std::env::var(var)
        .ok()
        .and_then(|raw| raw.trim().parse::<i64>().ok())
        .filter(|value| *value >= 0)
        .unwrap_or(default_days)
}

async fn prune_table_by_created_at(pool: &PgPool, table: &'static str, days: i64) {
    // `table` is sourced from a `&'static str` whitelist — never user input.
    let sql =
        format!("DELETE FROM {table} WHERE created_at < NOW() - ($1::bigint || ' days')::interval");
    match sqlx::query(&sql).bind(days).execute(pool).await {
        Ok(result) => {
            let affected = result.rows_affected();
            if affected > 0 {
                tracing::info!(
                    "[observability] retention sweep deleted {affected} rows from {table} (>{days}d)"
                );
            }
        }
        Err(error) => {
            tracing::warn!(
                "[observability] retention sweep on {table} failed (days={days}): {error}"
            );
        }
    }
}

async fn prune_table_by_snapshot_at(pool: &PgPool, table: &'static str, days: i64) {
    let sql = format!(
        "DELETE FROM {table} WHERE snapshot_at < NOW() - ($1::bigint || ' days')::interval"
    );
    match sqlx::query(&sql).bind(days).execute(pool).await {
        Ok(result) => {
            let affected = result.rows_affected();
            if affected > 0 {
                tracing::info!(
                    "[observability] retention sweep deleted {affected} rows from {table} (>{days}d)"
                );
            }
        }
        Err(error) => {
            tracing::warn!(
                "[observability] retention sweep on {table} failed (days={days}): {error}"
            );
        }
    }
}
