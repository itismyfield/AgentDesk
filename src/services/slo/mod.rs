//! Turn lifecycle SLO pipeline (#1072 / Epic #905 Phase 1).
//!
//! Computes 3 SLO metrics from `observability_events` over a sliding 5-minute
//! window, persists the aggregates into `slo_aggregates`, and — when a
//! threshold is crossed and the per-channel cooldown has elapsed — enqueues a
//! Discord alert through `message_outbox` (delivered via `/api/send`).
//!
//! The 3 metrics (as defined in Epic #905):
//!   * `TurnSuccessRate`      — successful turns / total attempts
//!   * `DuplicateRelayCount`  — duplicate-relay guard fires inside the window
//!   * `AvgTurnLatencyMs`     — average `turn_finished.payload.duration_ms`
//!
//! Storage layout — see migration `0015_slo_aggregates.sql` (postgres) and
//! `ensure_slo_schema` in `db/schema.rs` (sqlite parity).

use std::fmt;

use anyhow::Result;
use libsql_rusqlite::OptionalExtension;
use serde::Serialize;
use sqlx::{PgPool, Row};

use crate::db::Db;

/// Window length applied by the aggregation tick (5 minutes).
pub const DEFAULT_WINDOW_MS: i64 = 5 * 60 * 1000;
/// Per-(metric, channel) cooldown so back-to-back tick ticks do not spam.
pub const ALERT_COOLDOWN_MS: i64 = 30 * 60 * 1000;

/// Hardcoded thresholds for the first slice. Epic #905 Phase 2 will promote
/// these to a configurable policy; see DoD gap note in the issue body.
pub const TURN_SUCCESS_RATE_MIN: f64 = 0.80;
pub const DUPLICATE_RELAY_MAX: i64 = 3;
pub const AVG_TURN_LATENCY_MAX_MS: f64 = 60_000.0;

/// Fallback alert channel (adk-cc) used when `ADK_SLO_ALERT_CHANNEL` is unset.
pub const FALLBACK_ALERT_CHANNEL: &str = "1479671298497183835";
const ALERT_CHANNEL_ENV: &str = "ADK_SLO_ALERT_CHANNEL";

/// The 3 SLO metrics tracked in this slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum SloMetric {
    TurnSuccessRate,
    DuplicateRelayCount,
    AvgTurnLatencyMs,
}

impl SloMetric {
    pub const ALL: &'static [SloMetric] = &[
        SloMetric::TurnSuccessRate,
        SloMetric::DuplicateRelayCount,
        SloMetric::AvgTurnLatencyMs,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            SloMetric::TurnSuccessRate => "turn_success_rate",
            SloMetric::DuplicateRelayCount => "duplicate_relay_count",
            SloMetric::AvgTurnLatencyMs => "avg_turn_latency_ms",
        }
    }
}

impl fmt::Display for SloMetric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `[window_start_ms, window_end_ms)` window bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SloWindow {
    pub window_start_ms: i64,
    pub window_end_ms: i64,
}

impl SloWindow {
    pub fn ending_at(now_ms: i64, width_ms: i64) -> Self {
        let width = width_ms.max(1);
        Self {
            window_start_ms: now_ms.saturating_sub(width),
            window_end_ms: now_ms,
        }
    }
}

/// Result of computing a single metric in a window. `sample_size` is the
/// denominator (number of attempts / number of finished turns / etc.) so the
/// alert rendering can say "3/5 failed" without re-querying.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SloWindowAggregate {
    pub window_start_ms: i64,
    pub window_end_ms: i64,
    pub metric: SloMetric,
    pub value: f64,
    pub sample_size: i64,
}

/// Evaluation verdict for a single aggregate against its threshold.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ThresholdVerdict {
    Ok,
    Breach { threshold: f64 },
}

impl ThresholdVerdict {
    pub fn is_breach(&self) -> bool {
        matches!(self, ThresholdVerdict::Breach { .. })
    }

    pub fn threshold(&self) -> Option<f64> {
        match self {
            ThresholdVerdict::Breach { threshold } => Some(*threshold),
            ThresholdVerdict::Ok => None,
        }
    }
}

/// Compare an aggregate against its hardcoded threshold.
pub fn evaluate_threshold(aggregate: &SloWindowAggregate) -> ThresholdVerdict {
    match aggregate.metric {
        // success-rate is an inverse threshold: low values are bad.  A window
        // with zero attempts is *not* a breach — it just means the platform is
        // idle and we don't want to page anyone at 3am.
        SloMetric::TurnSuccessRate => {
            if aggregate.sample_size == 0 {
                ThresholdVerdict::Ok
            } else if aggregate.value < TURN_SUCCESS_RATE_MIN {
                ThresholdVerdict::Breach {
                    threshold: TURN_SUCCESS_RATE_MIN,
                }
            } else {
                ThresholdVerdict::Ok
            }
        }
        SloMetric::DuplicateRelayCount => {
            if aggregate.value > DUPLICATE_RELAY_MAX as f64 {
                ThresholdVerdict::Breach {
                    threshold: DUPLICATE_RELAY_MAX as f64,
                }
            } else {
                ThresholdVerdict::Ok
            }
        }
        SloMetric::AvgTurnLatencyMs => {
            if aggregate.sample_size == 0 {
                ThresholdVerdict::Ok
            } else if aggregate.value > AVG_TURN_LATENCY_MAX_MS {
                ThresholdVerdict::Breach {
                    threshold: AVG_TURN_LATENCY_MAX_MS,
                }
            } else {
                ThresholdVerdict::Ok
            }
        }
    }
}

/// Resolve the configured alert channel (env override or fallback const).
pub fn resolve_alert_channel() -> String {
    std::env::var(ALERT_CHANNEL_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| FALLBACK_ALERT_CHANNEL.to_string())
}

/// Human-readable Discord message for a breach.
pub fn format_alert_message(aggregate: &SloWindowAggregate, threshold: f64) -> String {
    let window_minutes = (aggregate.window_end_ms - aggregate.window_start_ms).max(0) / 60_000;
    match aggregate.metric {
        SloMetric::TurnSuccessRate => format!(
            "[SLO] turn success rate {:.2}% < {:.0}% over last {}m (sample={})",
            aggregate.value * 100.0,
            threshold * 100.0,
            window_minutes.max(1),
            aggregate.sample_size
        ),
        SloMetric::DuplicateRelayCount => format!(
            "[SLO] duplicate-relay guard fired {} times > {} in last {}m",
            aggregate.value as i64,
            threshold as i64,
            window_minutes.max(1)
        ),
        SloMetric::AvgTurnLatencyMs => format!(
            "[SLO] avg turn latency {:.0}ms > {:.0}ms over last {}m (sample={})",
            aggregate.value,
            threshold,
            window_minutes.max(1),
            aggregate.sample_size
        ),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SQL compute functions
// ─────────────────────────────────────────────────────────────────────────────

/// Compute turn success rate = turn_finished(status='completed'|'tmux_handoff')
/// / turn_finished(any status) within the window.
pub async fn compute_turn_success_rate_pg(
    pool: &PgPool,
    window: SloWindow,
) -> Result<SloWindowAggregate> {
    let row = sqlx::query(
        "SELECT COUNT(*) FILTER (WHERE status IN ('completed','tmux_handoff'))::bigint AS ok,
                COUNT(*)::bigint AS total
         FROM observability_events
         WHERE event_type = 'turn_finished'
           AND created_at >= to_timestamp($1::bigint / 1000.0)
           AND created_at <  to_timestamp($2::bigint / 1000.0)",
    )
    .bind(window.window_start_ms)
    .bind(window.window_end_ms)
    .fetch_one(pool)
    .await?;
    let ok: i64 = row.try_get("ok").unwrap_or(0);
    let total: i64 = row.try_get("total").unwrap_or(0);
    Ok(success_rate_aggregate(window, ok, total))
}

pub fn compute_turn_success_rate_sqlite(db: &Db, window: SloWindow) -> Result<SloWindowAggregate> {
    let conn = db.lock().map_err(|_| anyhow::anyhow!("sqlite poisoned"))?;
    let start_seconds = window.window_start_ms as f64 / 1000.0;
    let end_seconds = window.window_end_ms as f64 / 1000.0;
    let (ok, total): (i64, i64) = conn
        .query_row(
            "SELECT COALESCE(SUM(CASE WHEN status IN ('completed','tmux_handoff') THEN 1 ELSE 0 END), 0) AS ok,
                    COUNT(*) AS total
             FROM observability_events
             WHERE event_type = 'turn_finished'
               AND strftime('%s', created_at) >= CAST(?1 AS INTEGER)
               AND strftime('%s', created_at) <  CAST(?2 AS INTEGER)",
            libsql_rusqlite::params![start_seconds as i64, end_seconds as i64],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?
        .unwrap_or((0, 0));
    Ok(success_rate_aggregate(window, ok, total))
}

fn success_rate_aggregate(window: SloWindow, ok: i64, total: i64) -> SloWindowAggregate {
    let value = if total <= 0 {
        1.0
    } else {
        ok as f64 / total as f64
    };
    SloWindowAggregate {
        window_start_ms: window.window_start_ms,
        window_end_ms: window.window_end_ms,
        metric: SloMetric::TurnSuccessRate,
        value,
        sample_size: total,
    }
}

/// Count duplicate-relay events inside the window. We tolerate two event_type
/// spellings for forward-compat: an explicit `duplicate_relay` event and
/// `guard_fired` with `status` containing `duplicate_relay`. This matches the
/// tmux.rs guard that currently only logs — the event emission will be added
/// in the follow-up slice documented in the issue `Budget` note.
pub async fn compute_duplicate_relay_pg(
    pool: &PgPool,
    window: SloWindow,
) -> Result<SloWindowAggregate> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint
         FROM observability_events
         WHERE (event_type = 'duplicate_relay'
                OR (event_type = 'guard_fired' AND status LIKE 'duplicate_relay%'))
           AND created_at >= to_timestamp($1::bigint / 1000.0)
           AND created_at <  to_timestamp($2::bigint / 1000.0)",
    )
    .bind(window.window_start_ms)
    .bind(window.window_end_ms)
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    Ok(SloWindowAggregate {
        window_start_ms: window.window_start_ms,
        window_end_ms: window.window_end_ms,
        metric: SloMetric::DuplicateRelayCount,
        value: count as f64,
        sample_size: count,
    })
}

pub fn compute_duplicate_relay_sqlite(db: &Db, window: SloWindow) -> Result<SloWindowAggregate> {
    let conn = db.lock().map_err(|_| anyhow::anyhow!("sqlite poisoned"))?;
    let start_seconds = window.window_start_ms / 1000;
    let end_seconds = window.window_end_ms / 1000;
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM observability_events
             WHERE (event_type = 'duplicate_relay'
                    OR (event_type = 'guard_fired' AND status LIKE 'duplicate_relay%'))
               AND strftime('%s', created_at) >= CAST(?1 AS INTEGER)
               AND strftime('%s', created_at) <  CAST(?2 AS INTEGER)",
            libsql_rusqlite::params![start_seconds, end_seconds],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0);
    Ok(SloWindowAggregate {
        window_start_ms: window.window_start_ms,
        window_end_ms: window.window_end_ms,
        metric: SloMetric::DuplicateRelayCount,
        value: count as f64,
        sample_size: count,
    })
}

/// Average duration across `turn_finished` events in the window, reading
/// `payload_json.duration_ms` emitted by `observability::emit_turn_finished`.
pub async fn compute_avg_latency_pg(
    pool: &PgPool,
    window: SloWindow,
) -> Result<SloWindowAggregate> {
    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS n,
                COALESCE(AVG((payload_json->>'duration_ms')::double precision), 0.0) AS avg_ms
         FROM observability_events
         WHERE event_type = 'turn_finished'
           AND payload_json ? 'duration_ms'
           AND created_at >= to_timestamp($1::bigint / 1000.0)
           AND created_at <  to_timestamp($2::bigint / 1000.0)",
    )
    .bind(window.window_start_ms)
    .bind(window.window_end_ms)
    .fetch_one(pool)
    .await?;
    let n: i64 = row.try_get("n").unwrap_or(0);
    let avg_ms: f64 = row.try_get("avg_ms").unwrap_or(0.0);
    Ok(SloWindowAggregate {
        window_start_ms: window.window_start_ms,
        window_end_ms: window.window_end_ms,
        metric: SloMetric::AvgTurnLatencyMs,
        value: avg_ms,
        sample_size: n,
    })
}

pub fn compute_avg_latency_sqlite(db: &Db, window: SloWindow) -> Result<SloWindowAggregate> {
    let conn = db.lock().map_err(|_| anyhow::anyhow!("sqlite poisoned"))?;
    let start_seconds = window.window_start_ms / 1000;
    let end_seconds = window.window_end_ms / 1000;
    // sqlite has no JSON1 by default on every build; read as TEXT and let rust
    // parse.
    let mut stmt = conn.prepare(
        "SELECT payload_json FROM observability_events
         WHERE event_type = 'turn_finished'
           AND strftime('%s', created_at) >= CAST(?1 AS INTEGER)
           AND strftime('%s', created_at) <  CAST(?2 AS INTEGER)",
    )?;
    let rows = stmt.query_map(
        libsql_rusqlite::params![start_seconds, end_seconds],
        |row| row.get::<_, String>(0),
    )?;
    let mut total_ms: f64 = 0.0;
    let mut n: i64 = 0;
    for payload in rows.flatten() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&payload)
            && let Some(duration) = value.get("duration_ms").and_then(|v| v.as_i64())
        {
            total_ms += duration.max(0) as f64;
            n += 1;
        }
    }
    let avg = if n == 0 { 0.0 } else { total_ms / n as f64 };
    Ok(SloWindowAggregate {
        window_start_ms: window.window_start_ms,
        window_end_ms: window.window_end_ms,
        metric: SloMetric::AvgTurnLatencyMs,
        value: avg,
        sample_size: n,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Persistence + cooldown
// ─────────────────────────────────────────────────────────────────────────────

pub async fn persist_aggregate_pg(
    pool: &PgPool,
    aggregate: &SloWindowAggregate,
    verdict: ThresholdVerdict,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO slo_aggregates
             (window_start_ms, window_end_ms, metric, value, sample_size, threshold, breached)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(aggregate.window_start_ms)
    .bind(aggregate.window_end_ms)
    .bind(aggregate.metric.as_str())
    .bind(aggregate.value)
    .bind(aggregate.sample_size)
    .bind(verdict.threshold())
    .bind(verdict.is_breach())
    .execute(pool)
    .await?;
    Ok(())
}

pub fn persist_aggregate_sqlite(
    db: &Db,
    aggregate: &SloWindowAggregate,
    verdict: ThresholdVerdict,
) -> Result<()> {
    let conn = db.lock().map_err(|_| anyhow::anyhow!("sqlite poisoned"))?;
    conn.execute(
        "INSERT INTO slo_aggregates
             (window_start_ms, window_end_ms, metric, value, sample_size, threshold, breached)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        libsql_rusqlite::params![
            aggregate.window_start_ms,
            aggregate.window_end_ms,
            aggregate.metric.as_str(),
            aggregate.value,
            aggregate.sample_size,
            verdict.threshold(),
            if verdict.is_breach() { 1_i64 } else { 0_i64 },
        ],
    )?;
    Ok(())
}

/// Returns `true` if cooldown has expired (i.e. the alert should fire) for the
/// given `(metric, channel)` and `now_ms`. When the alert is fired the caller
/// must call [`record_alert_sent_sqlite`] / `_pg` to advance the cooldown.
pub fn cooldown_allows_alert_sqlite(
    db: &Db,
    metric: SloMetric,
    channel_id: &str,
    now_ms: i64,
    cooldown_ms: i64,
) -> Result<bool> {
    let conn = db.lock().map_err(|_| anyhow::anyhow!("sqlite poisoned"))?;
    let last: Option<i64> = conn
        .query_row(
            "SELECT alerted_at_ms FROM slo_alert_cooldowns
             WHERE metric = ?1 AND channel_id = ?2",
            libsql_rusqlite::params![metric.as_str(), channel_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    Ok(match last {
        None => true,
        Some(previous) => now_ms.saturating_sub(previous) >= cooldown_ms,
    })
}

pub fn record_alert_sent_sqlite(
    db: &Db,
    metric: SloMetric,
    channel_id: &str,
    now_ms: i64,
    value: f64,
) -> Result<()> {
    let conn = db.lock().map_err(|_| anyhow::anyhow!("sqlite poisoned"))?;
    conn.execute(
        "INSERT INTO slo_alert_cooldowns (metric, channel_id, alerted_at_ms, last_value)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(metric, channel_id) DO UPDATE SET
             alerted_at_ms = excluded.alerted_at_ms,
             last_value    = excluded.last_value",
        libsql_rusqlite::params![metric.as_str(), channel_id, now_ms, value],
    )?;
    Ok(())
}

pub async fn cooldown_allows_alert_pg(
    pool: &PgPool,
    metric: SloMetric,
    channel_id: &str,
    now_ms: i64,
    cooldown_ms: i64,
) -> Result<bool> {
    let last: Option<i64> = sqlx::query_scalar(
        "SELECT alerted_at_ms FROM slo_alert_cooldowns
         WHERE metric = $1 AND channel_id = $2",
    )
    .bind(metric.as_str())
    .bind(channel_id)
    .fetch_optional(pool)
    .await?;
    Ok(match last {
        None => true,
        Some(previous) => now_ms.saturating_sub(previous) >= cooldown_ms,
    })
}

pub async fn record_alert_sent_pg(
    pool: &PgPool,
    metric: SloMetric,
    channel_id: &str,
    now_ms: i64,
    value: f64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO slo_alert_cooldowns (metric, channel_id, alerted_at_ms, last_value)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (metric, channel_id) DO UPDATE SET
             alerted_at_ms = EXCLUDED.alerted_at_ms,
             last_value    = EXCLUDED.last_value",
    )
    .bind(metric.as_str())
    .bind(channel_id)
    .bind(now_ms)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// Enqueue a Discord alert message via the `message_outbox` queue (sqlite
/// backend). The outbox worker picks it up and POSTs `/api/send`.
pub fn enqueue_alert_sqlite(db: &Db, target: &str, content: &str) -> Result<()> {
    let conn = db.lock().map_err(|_| anyhow::anyhow!("sqlite poisoned"))?;
    conn.execute(
        "INSERT INTO message_outbox (target, content, bot, source, reason_code)
         VALUES (?1, ?2, 'notify', 'slo_alerter', 'slo_threshold_breach')",
        libsql_rusqlite::params![target, content],
    )?;
    Ok(())
}

pub async fn enqueue_alert_pg(pool: &PgPool, target: &str, content: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO message_outbox (target, content, bot, source, reason_code, status)
         VALUES ($1, $2, 'notify', 'slo_alerter', 'slo_threshold_breach', 'pending')",
    )
    .bind(target)
    .bind(content)
    .execute(pool)
    .await?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Aggregation tick — invoked every 5 minutes from the server tick loop.
// ─────────────────────────────────────────────────────────────────────────────

/// Public entry point. Computes all 3 metrics, persists, and (if breach +
/// cooldown elapsed) enqueues a Discord alert. Returns the list of aggregates
/// computed so callers / tests can inspect them.
pub async fn run_aggregation_tick(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    now_ms: i64,
) -> Vec<SloWindowAggregate> {
    let window = SloWindow::ending_at(now_ms, DEFAULT_WINDOW_MS);
    let channel = resolve_alert_channel();
    let mut aggregates = Vec::with_capacity(3);

    let computes: Vec<Result<SloWindowAggregate>> = if let Some(pool) = pg_pool {
        vec![
            compute_turn_success_rate_pg(pool, window).await,
            compute_duplicate_relay_pg(pool, window).await,
            compute_avg_latency_pg(pool, window).await,
        ]
    } else if let Some(db) = db {
        vec![
            compute_turn_success_rate_sqlite(db, window),
            compute_duplicate_relay_sqlite(db, window),
            compute_avg_latency_sqlite(db, window),
        ]
    } else {
        tracing::debug!("[slo] aggregation tick skipped: no db backend available");
        return aggregates;
    };

    for compute_result in computes {
        let aggregate = match compute_result {
            Ok(agg) => agg,
            Err(error) => {
                tracing::warn!("[slo] metric computation failed: {error}");
                continue;
            }
        };
        let verdict = evaluate_threshold(&aggregate);

        // Persistence is best-effort: aggregation should keep going even if
        // one metric's insert fails.
        if let Some(pool) = pg_pool {
            if let Err(error) = persist_aggregate_pg(pool, &aggregate, verdict).await {
                tracing::warn!(
                    "[slo] persist_aggregate_pg({}) failed: {error}",
                    aggregate.metric
                );
            }
        } else if let Some(db) = db
            && let Err(error) = persist_aggregate_sqlite(db, &aggregate, verdict)
        {
            tracing::warn!(
                "[slo] persist_aggregate_sqlite({}) failed: {error}",
                aggregate.metric
            );
        }

        if let ThresholdVerdict::Breach { threshold } = verdict {
            let cooldown_ok = if let Some(pool) = pg_pool {
                cooldown_allows_alert_pg(
                    pool,
                    aggregate.metric,
                    &channel,
                    now_ms,
                    ALERT_COOLDOWN_MS,
                )
                .await
                .unwrap_or(true)
            } else if let Some(db) = db {
                cooldown_allows_alert_sqlite(
                    db,
                    aggregate.metric,
                    &channel,
                    now_ms,
                    ALERT_COOLDOWN_MS,
                )
                .unwrap_or(true)
            } else {
                true
            };

            if cooldown_ok {
                let message = format_alert_message(&aggregate, threshold);
                if let Some(pool) = pg_pool {
                    if let Err(error) = enqueue_alert_pg(pool, &channel, &message).await {
                        tracing::warn!("[slo] enqueue_alert_pg failed: {error}");
                    } else if let Err(error) = record_alert_sent_pg(
                        pool,
                        aggregate.metric,
                        &channel,
                        now_ms,
                        aggregate.value,
                    )
                    .await
                    {
                        tracing::warn!("[slo] record_alert_sent_pg failed: {error}");
                    }
                } else if let Some(db) = db {
                    if let Err(error) = enqueue_alert_sqlite(db, &channel, &message) {
                        tracing::warn!("[slo] enqueue_alert_sqlite failed: {error}");
                    } else if let Err(error) = record_alert_sent_sqlite(
                        db,
                        aggregate.metric,
                        &channel,
                        now_ms,
                        aggregate.value,
                    ) {
                        tracing::warn!("[slo] record_alert_sent_sqlite failed: {error}");
                    }
                }
                tracing::warn!(
                    metric = %aggregate.metric,
                    value = aggregate.value,
                    threshold = threshold,
                    channel = %channel,
                    "[slo] threshold breach alert enqueued"
                );
            } else {
                tracing::debug!(
                    metric = %aggregate.metric,
                    "[slo] threshold breach suppressed by cooldown"
                );
            }
        }

        aggregates.push(aggregate);
    }

    aggregates
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests — use in-memory sqlite so PG is not required.
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, wrap_conn};
    use libsql_rusqlite::Connection;

    fn new_test_db() -> Db {
        let conn = Connection::open_in_memory().expect("open memory db");
        conn.execute_batch(
            "CREATE TABLE observability_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_type TEXT NOT NULL,
                provider TEXT,
                channel_id TEXT,
                dispatch_id TEXT,
                session_key TEXT,
                turn_id TEXT,
                status TEXT,
                payload_json TEXT NOT NULL DEFAULT '{}',
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE slo_aggregates (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                window_start_ms INTEGER NOT NULL,
                window_end_ms INTEGER NOT NULL,
                metric TEXT NOT NULL,
                value REAL NOT NULL,
                sample_size INTEGER NOT NULL DEFAULT 0,
                threshold REAL,
                breached INTEGER NOT NULL DEFAULT 0,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE slo_alert_cooldowns (
                metric TEXT NOT NULL,
                channel_id TEXT NOT NULL,
                alerted_at_ms INTEGER NOT NULL,
                last_value REAL NOT NULL,
                PRIMARY KEY (metric, channel_id)
            );
            CREATE TABLE message_outbox (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                target TEXT NOT NULL,
                content TEXT NOT NULL,
                bot TEXT NOT NULL DEFAULT 'announce',
                source TEXT NOT NULL DEFAULT 'system',
                reason_code TEXT,
                session_key TEXT,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at DATETIME DEFAULT (datetime('now')),
                sent_at DATETIME,
                error TEXT,
                claimed_at DATETIME,
                claim_owner TEXT
            );",
        )
        .expect("create test tables");
        wrap_conn(conn)
    }

    fn insert_turn_finished(db: &Db, status: &str, duration_ms: i64, created_at: &str) {
        let conn = db.lock().expect("lock db");
        conn.execute(
            "INSERT INTO observability_events
                 (event_type, status, payload_json, created_at)
             VALUES ('turn_finished', ?1, ?2, ?3)",
            libsql_rusqlite::params![
                status,
                format!(r#"{{"duration_ms":{duration_ms}}}"#),
                created_at
            ],
        )
        .expect("insert turn_finished");
    }

    fn insert_event(db: &Db, event_type: &str, status: Option<&str>, created_at: &str) {
        let conn = db.lock().expect("lock db");
        conn.execute(
            "INSERT INTO observability_events
                 (event_type, status, payload_json, created_at)
             VALUES (?1, ?2, '{}', ?3)",
            libsql_rusqlite::params![event_type, status, created_at],
        )
        .expect("insert event");
    }

    fn recent_window(now_ms: i64) -> SloWindow {
        SloWindow::ending_at(now_ms, DEFAULT_WINDOW_MS)
    }

    #[test]
    fn success_rate_computes_ratio_over_window() {
        let db = new_test_db();
        // 3 completed, 1 failed → 0.75 success rate
        insert_turn_finished(&db, "completed", 1000, "2026-04-24 00:00:05");
        insert_turn_finished(&db, "completed", 2000, "2026-04-24 00:00:10");
        insert_turn_finished(&db, "tmux_handoff", 1500, "2026-04-24 00:00:20");
        insert_turn_finished(&db, "error", 500, "2026-04-24 00:00:30");

        // Window: 2026-04-24 00:00:00 → 00:05:00
        let window = SloWindow {
            window_start_ms: 1_776_988_800_000, // 2026-04-24 00:00:00 UTC
            window_end_ms: 1_776_989_100_000,   // 2026-04-24 00:05:00 UTC
        };
        let agg = compute_turn_success_rate_sqlite(&db, window).expect("compute");
        assert_eq!(agg.sample_size, 4);
        assert!((agg.value - 0.75).abs() < 1e-9);
        assert_eq!(agg.metric, SloMetric::TurnSuccessRate);
    }

    #[test]
    fn duplicate_relay_counts_within_window() {
        let db = new_test_db();
        insert_event(
            &db,
            "guard_fired",
            Some("duplicate_relay"),
            "2026-04-24 00:01:00",
        );
        insert_event(
            &db,
            "guard_fired",
            Some("duplicate_relay"),
            "2026-04-24 00:02:00",
        );
        insert_event(&db, "duplicate_relay", Some("fired"), "2026-04-24 00:03:00");
        // Event outside window (too old) — must NOT be counted.
        insert_event(
            &db,
            "guard_fired",
            Some("duplicate_relay"),
            "2020-01-01 00:00:00",
        );

        let window = SloWindow {
            window_start_ms: 1_776_988_800_000, // 2026-04-24 00:00:00 UTC
            window_end_ms: 1_776_989_100_000,   // 2026-04-24 00:05:00 UTC
        };
        let agg = compute_duplicate_relay_sqlite(&db, window).expect("compute");
        assert_eq!(agg.value as i64, 3);
        assert_eq!(agg.sample_size, 3);
        assert_eq!(agg.metric, SloMetric::DuplicateRelayCount);
    }

    #[test]
    fn avg_latency_averages_durations_in_window() {
        let db = new_test_db();
        insert_turn_finished(&db, "completed", 1000, "2026-04-24 00:00:10");
        insert_turn_finished(&db, "completed", 3000, "2026-04-24 00:00:20");
        insert_turn_finished(&db, "completed", 5000, "2026-04-24 00:00:30");

        let window = SloWindow {
            window_start_ms: 1_776_988_800_000, // 2026-04-24 00:00:00 UTC
            window_end_ms: 1_776_989_100_000,   // 2026-04-24 00:05:00 UTC
        };
        let agg = compute_avg_latency_sqlite(&db, window).expect("compute");
        assert_eq!(agg.sample_size, 3);
        assert!((agg.value - 3000.0).abs() < 1e-9);
        assert_eq!(agg.metric, SloMetric::AvgTurnLatencyMs);
    }

    #[test]
    fn threshold_evaluator_flags_breach_and_ok() {
        // success rate breach
        let low = SloWindowAggregate {
            window_start_ms: 0,
            window_end_ms: 300_000,
            metric: SloMetric::TurnSuccessRate,
            value: 0.5,
            sample_size: 10,
        };
        assert!(evaluate_threshold(&low).is_breach());

        // success rate idle (sample=0) must NOT page
        let idle = SloWindowAggregate {
            sample_size: 0,
            value: 0.0,
            ..low
        };
        assert_eq!(evaluate_threshold(&idle), ThresholdVerdict::Ok);

        // duplicate-relay > 3
        let dup_breach = SloWindowAggregate {
            metric: SloMetric::DuplicateRelayCount,
            value: 5.0,
            sample_size: 5,
            ..low
        };
        assert!(evaluate_threshold(&dup_breach).is_breach());

        let dup_ok = SloWindowAggregate {
            value: 3.0,
            ..dup_breach
        };
        assert_eq!(evaluate_threshold(&dup_ok), ThresholdVerdict::Ok);

        // latency > 60s
        let lat_breach = SloWindowAggregate {
            metric: SloMetric::AvgTurnLatencyMs,
            value: 90_000.0,
            sample_size: 3,
            ..low
        };
        assert!(evaluate_threshold(&lat_breach).is_breach());
    }

    #[test]
    fn cooldown_suppresses_repeat_alerts_within_window() {
        let db = new_test_db();
        let metric = SloMetric::TurnSuccessRate;
        let now_ms = recent_window(0).window_end_ms + 10_000_000;
        assert!(
            cooldown_allows_alert_sqlite(&db, metric, "12345", now_ms, ALERT_COOLDOWN_MS)
                .expect("cooldown check")
        );
        record_alert_sent_sqlite(&db, metric, "12345", now_ms, 0.1).expect("record");

        // 10 minutes later — still in cooldown.
        assert!(
            !cooldown_allows_alert_sqlite(
                &db,
                metric,
                "12345",
                now_ms + 10 * 60_000,
                ALERT_COOLDOWN_MS
            )
            .expect("cooldown check"),
            "cooldown must suppress alert inside 30 min window",
        );

        // 31 minutes later — cooldown released.
        assert!(
            cooldown_allows_alert_sqlite(
                &db,
                metric,
                "12345",
                now_ms + 31 * 60_000,
                ALERT_COOLDOWN_MS
            )
            .expect("cooldown check"),
        );
    }

    #[test]
    fn alert_formatter_renders_expected_payload() {
        let success = SloWindowAggregate {
            window_start_ms: 0,
            window_end_ms: DEFAULT_WINDOW_MS,
            metric: SloMetric::TurnSuccessRate,
            value: 0.50,
            sample_size: 10,
        };
        let msg = format_alert_message(&success, TURN_SUCCESS_RATE_MIN);
        assert!(msg.contains("turn success rate"));
        assert!(msg.contains("50.00%"));
        assert!(msg.contains("80%"));
        assert!(msg.contains("sample=10"));

        let dup = SloWindowAggregate {
            metric: SloMetric::DuplicateRelayCount,
            value: 5.0,
            ..success
        };
        let msg = format_alert_message(&dup, DUPLICATE_RELAY_MAX as f64);
        assert!(msg.contains("duplicate-relay"));
        assert!(msg.contains(" 5 times"));
        assert!(msg.contains("> 3"));

        let latency = SloWindowAggregate {
            metric: SloMetric::AvgTurnLatencyMs,
            value: 90_000.0,
            ..success
        };
        let msg = format_alert_message(&latency, AVG_TURN_LATENCY_MAX_MS);
        assert!(msg.contains("avg turn latency"));
        assert!(msg.contains("90000ms"));
    }

    #[tokio::test]
    async fn aggregation_tick_persists_and_enqueues_alert_on_breach() {
        let db = new_test_db();
        // 4 duplicate-relay events within window (threshold = 3 → breach)
        insert_event(
            &db,
            "guard_fired",
            Some("duplicate_relay"),
            "2026-04-24 00:00:10",
        );
        insert_event(
            &db,
            "guard_fired",
            Some("duplicate_relay"),
            "2026-04-24 00:00:20",
        );
        insert_event(
            &db,
            "guard_fired",
            Some("duplicate_relay"),
            "2026-04-24 00:00:30",
        );
        insert_event(
            &db,
            "guard_fired",
            Some("duplicate_relay"),
            "2026-04-24 00:00:40",
        );

        // Force the window to 2026-04-24 00:00:00 .. 00:05:00 by choosing now_ms
        // = window_end_ms.  SloWindow::ending_at subtracts DEFAULT_WINDOW_MS
        // internally so this pins both bounds.
        let now_ms = 1_776_989_100_000; // 2026-04-24 00:05:00 UTC
        let aggregates = run_aggregation_tick(Some(&db), None, now_ms).await;
        assert_eq!(aggregates.len(), 3);

        // One row per metric persisted.
        let conn = db.lock().unwrap();
        let persisted: i64 = conn
            .query_row("SELECT COUNT(*) FROM slo_aggregates", [], |row| row.get(0))
            .unwrap();
        assert_eq!(persisted, 3);

        let breached: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM slo_aggregates WHERE breached = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(breached, 1);

        let outbox_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message_outbox WHERE source = 'slo_alerter'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(outbox_rows, 1);
        drop(conn);

        // Re-run immediately — cooldown must suppress a second enqueue.
        let _ = run_aggregation_tick(Some(&db), None, now_ms + 60_000).await;
        let conn = db.lock().unwrap();
        let outbox_rows_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message_outbox WHERE source = 'slo_alerter'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            outbox_rows_after, 1,
            "cooldown must prevent a second alert within 30min"
        );
    }
}
