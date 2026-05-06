//! Turn lifecycle SLO pipeline (#1072 / Epic #905 Phase 1).
//!
//! Computes 3 SLO metrics from `observability_events` over a sliding 5-minute
//! window, persists the aggregates into `slo_aggregates`, and — when a
//! threshold is crossed and the per-channel cooldown has elapsed — enqueues a
//! Discord alert through `message_outbox` (delivered via `/api/discord/send`).
//!
//! The 3 metrics (as defined in Epic #905):
//!   * `TurnSuccessRate`      — successful turns / total attempts
//!   * `DuplicateRelayCount`  — duplicate-relay guard fires inside the window
//!   * `AvgTurnLatencyMs`     — average `turn_finished.payload.duration_ms`
//!
//! Storage layout — see migration `0015_slo_aggregates.sql` (postgres).

use std::fmt;

use anyhow::Result;
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
const AUTO_QUEUE_CONTEXT_RE: &str = r#""auto_queue"\s*:\s*true"#;

// task_dispatches.id and observability_events.dispatch_id are both TEXT in the
// Postgres schema; the join backfills dispatch classification for older events
// emitted before turn_finished.payload.dispatch_kind existed.
const AVG_LATENCY_SQL: &str = r#"
SELECT COUNT(*)::bigint AS n,
       COALESCE(AVG((e.payload_json->>'duration_ms')::double precision), 0.0) AS avg_ms
FROM observability_events e
LEFT JOIN task_dispatches d ON d.id = e.dispatch_id
WHERE e.event_type = 'turn_finished'
  AND e.payload_json ? 'duration_ms'
  AND COALESCE(e.payload_json->>'dispatch_kind', '') NOT IN ('auto_queue', 'review_decision')
  AND COALESCE(d.dispatch_type, '') <> 'review-decision'
  AND COALESCE(d.context, '') !~ $3
  AND e.created_at >= to_timestamp($1::bigint / 1000.0)
  AND e.created_at <  to_timestamp($2::bigint / 1000.0)
"#;

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

    /// Whether a threshold breach should enqueue a Discord alert.
    ///
    /// `TurnSuccessRate` and `AvgTurnLatencyMs` were demoted to record-only
    /// after operational review showed both produced predominantly noise: the
    /// 5-minute success-rate window flips to 0%/50% on `sample=1..2` even when
    /// nothing is wrong, and average latency over the same window is dominated
    /// by a single legitimately-long coding turn (often 5–20 min). Aggregates
    /// are still persisted into `slo_aggregates` for dashboards / postmortems;
    /// only the Discord page-out is suppressed. Stuck-turn detection lives in
    /// `dispatch_watchdog` where `idle_minutes >= 60` is genuinely actionable.
    pub fn alerts_enabled(&self) -> bool {
        match self {
            SloMetric::TurnSuccessRate => false,
            SloMetric::AvgTurnLatencyMs => false,
            SloMetric::DuplicateRelayCount => true,
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

/// Average duration across `turn_finished` events in the window, reading
/// `payload_json.duration_ms` emitted by `observability::emit_turn_finished`.
pub async fn compute_avg_latency_pg(
    pool: &PgPool,
    window: SloWindow,
) -> Result<SloWindowAggregate> {
    let row = sqlx::query(AVG_LATENCY_SQL)
        .bind(window.window_start_ms)
        .bind(window.window_end_ms)
        .bind(AUTO_QUEUE_CONTEXT_RE)
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

/// Returns `true` if cooldown has expired (i.e. the alert should fire) for the
/// given `(metric, channel)` and `now_ms`. When the alert is fired the caller
/// must call [`record_alert_sent_pg`] to advance the cooldown.

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
    _db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    now_ms: i64,
) -> Vec<SloWindowAggregate> {
    let window = SloWindow::ending_at(now_ms, DEFAULT_WINDOW_MS);
    let channel = resolve_alert_channel();
    let mut aggregates = Vec::with_capacity(3);

    let Some(pool) = pg_pool else {
        tracing::debug!("[slo] aggregation tick skipped: postgres backend unavailable");
        return aggregates;
    };
    let computes: Vec<Result<SloWindowAggregate>> = vec![
        compute_turn_success_rate_pg(pool, window).await,
        compute_duplicate_relay_pg(pool, window).await,
        compute_avg_latency_pg(pool, window).await,
    ];

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
        if let Err(error) = persist_aggregate_pg(pool, &aggregate, verdict).await {
            tracing::warn!(
                "[slo] persist_aggregate_pg({}) failed: {error}",
                aggregate.metric
            );
        }

        if let ThresholdVerdict::Breach { threshold } = verdict {
            if !aggregate.metric.alerts_enabled() {
                tracing::debug!(
                    metric = %aggregate.metric,
                    value = aggregate.value,
                    threshold = threshold,
                    "[slo] threshold breach recorded (alerts disabled for metric)"
                );
                aggregates.push(aggregate);
                continue;
            }
            let cooldown_ok = cooldown_allows_alert_pg(
                pool,
                aggregate.metric,
                &channel,
                now_ms,
                ALERT_COOLDOWN_MS,
            )
            .await
            .unwrap_or(true);

            if cooldown_ok {
                let message = format_alert_message(&aggregate, threshold);
                if let Err(error) = enqueue_alert_pg(pool, &channel, &message).await {
                    tracing::warn!("[slo] enqueue_alert_pg failed: {error}");
                } else if let Err(error) =
                    record_alert_sent_pg(pool, aggregate.metric, &channel, now_ms, aggregate.value)
                        .await
                {
                    tracing::warn!("[slo] record_alert_sent_pg failed: {error}");
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
// Tests — keep pure threshold/formatting coverage here. Storage and tick
// coverage depends on Postgres fixtures after #868.
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod avg_latency_tests {
    use super::*;

    #[test]
    fn avg_latency_query_excludes_dispatch_noise_sources() {
        assert!(AVG_LATENCY_SQL.contains("LEFT JOIN task_dispatches"));
        assert!(AVG_LATENCY_SQL.contains("payload_json->>'dispatch_kind'"));
        assert!(AVG_LATENCY_SQL.contains("'auto_queue'"));
        assert!(AVG_LATENCY_SQL.contains("'review_decision'"));
        assert!(AVG_LATENCY_SQL.contains("d.dispatch_type"));
        assert!(AVG_LATENCY_SQL.contains("d.context"));
        assert_eq!(AUTO_QUEUE_CONTEXT_RE, r#""auto_queue"\s*:\s*true"#);
    }

    #[test]
    fn noisy_metrics_have_alerts_disabled() {
        // The two metrics demoted to record-only after the post-#1750 noise
        // review. Re-enabling either should be an explicit, reviewed change —
        // this test guards against accidental regressions.
        assert!(!SloMetric::TurnSuccessRate.alerts_enabled());
        assert!(!SloMetric::AvgTurnLatencyMs.alerts_enabled());
        // Duplicate-relay remains a real signal: the guard only fires when an
        // upstream invariant is broken, so a 5-min spike is actionable.
        assert!(SloMetric::DuplicateRelayCount.alerts_enabled());
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;

    #[test]
    #[ignore = "SQLite SLO storage/tick service path removed for #868; metric SQL, cooldown, persistence, and outbox coverage must move to Postgres integration fixtures."]
    fn sqlite_slo_storage_unit_coverage_obsolete_after_pg_migration() {}

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
    async fn aggregation_tick_without_postgres_skips() {
        let aggregates = run_aggregation_tick(None, None, 1_776_989_100_000).await;
        assert!(aggregates.is_empty());
    }
}
