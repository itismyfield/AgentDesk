//! One statement per health read: the dispatch outbox and cleanup counts as conditional
//! aggregates, the kv_meta reports by key list, and the latest session per mailbox channel.

use std::collections::HashMap;

use sqlx::Row;

use super::{AutoQueueCleanupBacklog, ChannelSessionState, DispatchOutboxStats, session_state};
use crate::services::hang_forensics::ProbedPool;

/// The age is over due pending rows only, as the dispatcher would pick them up.
pub(crate) const OUTBOX_STATS_QUERY: &str = "SELECT
        (COUNT(*) FILTER (WHERE status = 'pending'))::BIGINT AS pending,
        (COUNT(*) FILTER (WHERE status = 'pending' AND retry_count > 0))::BIGINT AS retrying,
        (COUNT(*) FILTER (WHERE status = 'failed'))::BIGINT AS failed,
        COALESCE(
            CAST(
                EXTRACT(
                    EPOCH FROM (NOW() - MIN(COALESCE(next_attempt_at, created_at)) FILTER (
                        WHERE status = 'pending'
                          AND (next_attempt_at IS NULL OR next_attempt_at <= NOW())
                    ))
                ) AS BIGINT
            ),
            0
        ) AS oldest_pending_age
   FROM dispatch_outbox";

pub(crate) const CLEANUP_BACKLOG_QUERY: &str = "SELECT
        (COUNT(*) FILTER (WHERE dead_lettered_at IS NULL))::BIGINT AS pending,
        (COUNT(*) FILTER (WHERE dead_lettered_at IS NOT NULL))::BIGINT AS dead_lettered
   FROM auto_queue_run_cleanup_tasks";

pub(crate) const HEALTH_KV_QUERY: &str = "SELECT key, value FROM kv_meta WHERE key = ANY($1)";

/// The latest row per channel, in the single-channel lookup's order.
pub(crate) const MAILBOX_SESSIONS_QUERY: &str = "SELECT DISTINCT ON (thread_channel_id)
            thread_channel_id, agent_id, provider, status, active_dispatch_id
       FROM sessions
      WHERE thread_channel_id = ANY($1::text[])
      ORDER BY thread_channel_id, last_heartbeat DESC NULLS LAST, id DESC";

const CONFIG_AUDIT_KEY: &str = "config_audit_report";
const PIPELINE_OVERRIDE_KEY: &str = "pipeline_override_health_report";
const RUNTIME_CONFIG_KEY: &str = "runtime-config";

/// Per-channel session lookups; `Err` is a failed measurement, `Ok(None)` a channel with no row.
pub(crate) type SessionLookups = HashMap<u64, Result<Option<ChannelSessionState>, String>>;

/// The kv_meta values health reads; a missing, NULL or unparsable value is `None`.
#[derive(Debug, Default)]
pub(crate) struct HealthKvMeta {
    pub(crate) config_audit: Option<serde_json::Value>,
    pub(crate) pipeline_overrides: Option<serde_json::Value>,
    pub(crate) runtime_config: Option<serde_json::Value>,
}

impl HealthKvMeta {
    /// The dispatch-gate enabled and danger overrides persisted in `runtime-config`.
    pub(crate) fn dispatch_gate_overrides(&self) -> (Option<bool>, Option<u64>) {
        let (enabled, danger, _stale) = crate::services::dispatch_gate::persisted_runtime_overrides(
            self.runtime_config.as_ref(),
        );
        (enabled, danger)
    }
}

pub(super) async fn load_dispatch_outbox_stats_pg(
    probed: ProbedPool<'_>,
) -> Option<DispatchOutboxStats> {
    let row = probed
        .probe(
            |pool| sqlx::query(OUTBOX_STATS_QUERY).fetch_one(pool),
            Result::is_ok,
        )
        .await
        .ok()?;
    Some(DispatchOutboxStats {
        pending: row.try_get("pending").ok()?,
        retrying: row.try_get("retrying").ok()?,
        permanent_failures: row.try_get("failed").ok()?,
        oldest_pending_age: row.try_get("oldest_pending_age").ok()?,
    })
}

pub(crate) async fn load_auto_queue_cleanup_backlog_pg(
    probed: ProbedPool<'_>,
) -> Result<AutoQueueCleanupBacklog, String> {
    let row = probed
        .probe(
            |pool| sqlx::query(CLEANUP_BACKLOG_QUERY).fetch_one(pool),
            Result::is_ok,
        )
        .await
        .map_err(|error| format!("count auto-queue run cleanup tasks: {error}"))?;
    let count = |column| {
        row.try_get::<i64, _>(column)
            .map_err(|error| format!("decode auto-queue cleanup {column}: {error}"))
    };
    Ok(AutoQueueCleanupBacklog {
        pending: count("pending")?,
        dead_lettered: count("dead_lettered")?,
    })
}

/// `None` when the read failed, so an unknown override is not mistaken for an absent one.
pub(super) async fn load_health_kv_meta(probed: ProbedPool<'_>) -> Option<HealthKvMeta> {
    let keys = [CONFIG_AUDIT_KEY, PIPELINE_OVERRIDE_KEY, RUNTIME_CONFIG_KEY];
    let rows = probed
        .probe(
            |pool| sqlx::query(HEALTH_KV_QUERY).bind(&keys[..]).fetch_all(pool),
            Result::is_ok,
        )
        .await
        .ok()?;
    let mut values = HashMap::new();
    for row in rows {
        if let (Ok(key), Ok(Some(raw))) = (
            row.try_get::<String, _>("key"),
            row.try_get::<Option<String>, _>("value"),
        ) {
            values.insert(key, raw);
        }
    }
    let parsed = |key: &str| {
        values
            .get(key)
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
    };
    Some(HealthKvMeta {
        config_audit: parsed(CONFIG_AUDIT_KEY),
        pipeline_overrides: parsed(PIPELINE_OVERRIDE_KEY),
        runtime_config: parsed(RUNTIME_CONFIG_KEY),
    })
}

pub(super) async fn load_channel_session_states(
    probed: ProbedPool<'_>,
    channels: &[u64],
) -> SessionLookups {
    let mut keys: Vec<String> = channels.iter().map(u64::to_string).collect();
    keys.sort();
    keys.dedup();
    let rows = probed
        .probe(
            |pool| {
                sqlx::query(MAILBOX_SESSIONS_QUERY)
                    .bind(&keys)
                    .fetch_all(pool)
            },
            Result::is_ok,
        )
        .await
        .map_err(|error| format!("query channel sessions: {error}"));
    map_session_rows(channels, rows, |row: &sqlx::postgres::PgRow, field| {
        row.try_get::<Option<String>, _>(field)
    })
}

/// A query error fails every channel; a row that fails to decode fails only its channel, or
/// every channel without a decoded row when its channel column itself is unreadable.
pub(super) fn map_session_rows<R>(
    channels: &[u64],
    rows: Result<Vec<R>, String>,
    read: impl Fn(&R, &str) -> Result<Option<String>, sqlx::Error>,
) -> SessionLookups {
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => {
            return channels
                .iter()
                .map(|channel| (*channel, Err(error.clone())))
                .collect();
        }
    };
    let mut found = HashMap::new();
    let mut unattributed = None;
    for row in &rows {
        match read(row, "thread_channel_id") {
            Ok(Some(key)) => {
                found.insert(key, session_state::decode(row, &read));
            }
            Ok(None) => {}
            Err(error) => {
                unattributed = Some(format!("decode session thread_channel_id: {error}"));
            }
        }
    }
    channels
        .iter()
        .map(|channel| {
            let lookup = match found.get(&channel.to_string()) {
                Some(decoded) => decoded.clone().map(Some),
                None => unattributed.clone().map_or(Ok(None), Err),
            };
            (*channel, lookup)
        })
        .collect()
}

#[cfg(test)]
#[path = "request_batch_pg_tests.rs"]
mod pg_tests;
#[cfg(test)]
#[path = "request_batch_tests.rs"]
mod tests;
