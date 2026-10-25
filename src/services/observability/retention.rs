//! #2049 Finding 9: retention sweep split out of `mod.rs`. Prunes old
//! observability rows so disk and index growth stay bounded on long-lived
//! single-node deployments. Schedule lives in the worker loop; this module
//! is pure I/O.

use std::sync::Arc;

use sqlx::{AssertSqlSafe, PgPool};

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
    if let Some(pool) = handles.pg_pool.as_ref() {
        sweep_pool(pool).await;
    }
}

pub(crate) async fn sweep_pool(pool: &PgPool) {
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
        match prune_quality_events_with_turn_rollup(pool, quality_days).await {
            Ok(0) => {}
            Ok(affected) => tracing::info!(
                "[observability] retention sweep deleted {affected} rows from agent_quality_event (>{quality_days}d)"
            ),
            Err(error) => tracing::warn!(
                "[observability] retention sweep on agent_quality_event failed (days={quality_days}): {error}"
            ),
        }
    }
    if snapshot_days > 0 {
        prune_table_by_snapshot_at(pool, "observability_counter_snapshots", snapshot_days).await;
    }
}

/// Deletes aged quality events and adds the deleted `turn_*` events to their month in
/// `turn_analytics_monthly_aggregate`, in one statement: a row counts only for the sweep that removed it.
pub(crate) async fn prune_quality_events_with_turn_rollup(
    pool: &PgPool,
    days: i64,
) -> Result<u64, sqlx::Error> {
    // total_turns counts finished turns (complete + error); start_count counts starts.
    let deleted: i64 = sqlx::query_scalar(
        "WITH removed AS ( \
             DELETE FROM agent_quality_event \
             WHERE created_at < NOW() - ($1::bigint || ' days')::interval \
             RETURNING event_type, created_at \
         ), rolled AS ( \
             INSERT INTO turn_analytics_monthly_aggregate AS a \
                 (month, total_turns, success_count, error_count, start_count, aggregated_at) \
             SELECT date_trunc('month', created_at)::DATE, \
                    COUNT(*) FILTER (WHERE event_type IN ('turn_complete', 'turn_error')), \
                    COUNT(*) FILTER (WHERE event_type = 'turn_complete'), \
                    COUNT(*) FILTER (WHERE event_type = 'turn_error'), \
                    COUNT(*) FILTER (WHERE event_type = 'turn_start'), \
                    NOW() \
             FROM removed \
             WHERE event_type IN ('turn_start', 'turn_complete', 'turn_error') \
             GROUP BY date_trunc('month', created_at) \
             ON CONFLICT (month) DO UPDATE \
             SET total_turns = a.total_turns + EXCLUDED.total_turns, \
                 success_count = a.success_count + EXCLUDED.success_count, \
                 error_count = a.error_count + EXCLUDED.error_count, \
                 start_count = a.start_count + EXCLUDED.start_count, \
                 aggregated_at = EXCLUDED.aggregated_at \
         ) \
         SELECT COUNT(*)::BIGINT FROM removed",
    )
    .bind(days)
    .fetch_one(pool)
    .await?;
    Ok(deleted as u64)
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
    match sqlx::query(AssertSqlSafe(sql))
        .bind(days)
        .execute(pool)
        .await
    {
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
    match sqlx::query(AssertSqlSafe(sql))
        .bind(days)
        .execute(pool)
        .await
    {
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
