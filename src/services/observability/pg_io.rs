//! #2049: Postgres I/O primitives (event inserts, snapshot inserts, query
//! helpers, and the daily-rollup queries) split out of `mod.rs`. These are
//! `pub(super)` so the worker, retention sweep, and query façade can share
//! them without touching the global runtime.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};

use super::helpers::{
    counter_snapshot_from_values, invariant_record_from_parts, saturating_i64, value_as_string,
};
use super::{
    AgentQualityDailyRecord, AgentQualityEventRecord, AgentQualityRankingEntry, AgentQualityWindow,
    AnalyticsCounterSnapshot, AnalyticsEventRecord, AnalyticsFilters, CounterValues,
    InvariantAnalyticsFilters, InvariantViolationCount, InvariantViolationRecord, MAX_QUALITY_DAYS,
    QUALITY_SAMPLE_GUARD, QualityRankingMetric, QualityRankingWindow, QueuedEvent,
    QueuedQualityEvent, SnapshotRow,
};

pub(super) async fn query_events_pg(
    pool: &PgPool,
    filters: &AnalyticsFilters,
    limit: usize,
) -> Result<Vec<AnalyticsEventRecord>> {
    let rows = sqlx::query(
        "SELECT id,
                event_type,
                provider,
                channel_id,
                dispatch_id,
                session_key,
                turn_id,
                status,
                payload_json::text AS payload_json,
                to_char(created_at AT TIME ZONE 'Asia/Seoul', 'YYYY-MM-DD HH24:MI:SS') AS created_at_kst
         FROM observability_events
         WHERE ($1::text IS NULL OR provider = $1)
           AND ($2::text IS NULL OR channel_id = $2)
           AND ($3::text IS NULL OR event_type = $3)
         ORDER BY id DESC
         LIMIT $4",
    )
    .bind(filters.provider.as_deref())
    .bind(filters.channel_id.as_deref())
    .bind(filters.event_type.as_deref())
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("query postgres observability events: {error}"))?;

    rows.into_iter()
        .map(|row| {
            let payload_json: Option<String> = row
                .try_get("payload_json")
                .map_err(|error| anyhow!("decode observability payload_json: {error}"))?;
            Ok(AnalyticsEventRecord {
                id: row
                    .try_get("id")
                    .map_err(|error| anyhow!("decode observability event id: {error}"))?,
                event_type: row
                    .try_get("event_type")
                    .map_err(|error| anyhow!("decode observability event_type: {error}"))?,
                provider: row
                    .try_get("provider")
                    .map_err(|error| anyhow!("decode observability provider: {error}"))?,
                channel_id: row
                    .try_get("channel_id")
                    .map_err(|error| anyhow!("decode observability channel_id: {error}"))?,
                dispatch_id: row
                    .try_get("dispatch_id")
                    .map_err(|error| anyhow!("decode observability dispatch_id: {error}"))?,
                session_key: row
                    .try_get("session_key")
                    .map_err(|error| anyhow!("decode observability session_key: {error}"))?,
                turn_id: row
                    .try_get("turn_id")
                    .map_err(|error| anyhow!("decode observability turn_id: {error}"))?,
                status: row
                    .try_get("status")
                    .map_err(|error| anyhow!("decode observability status: {error}"))?,
                payload: payload_json
                    .as_deref()
                    .and_then(|value| serde_json::from_str(value).ok())
                    .unwrap_or_else(|| json!({})),
                created_at: row
                    .try_get("created_at_kst")
                    .map_err(|error| anyhow!("decode observability created_at: {error}"))?,
            })
        })
        .collect()
}

pub(super) async fn query_invariant_counts_pg(
    pool: &PgPool,
    filters: &InvariantAnalyticsFilters,
) -> Result<Vec<InvariantViolationCount>> {
    let rows = sqlx::query(
        "SELECT COALESCE(status, 'unknown') AS invariant,
                COUNT(*) AS violation_count
         FROM observability_events
         WHERE event_type = 'invariant_violation'
           AND ($1::text IS NULL OR provider = $1)
           AND ($2::text IS NULL OR channel_id = $2)
           AND ($3::text IS NULL OR status = $3)
         GROUP BY COALESCE(status, 'unknown')
         ORDER BY violation_count DESC, invariant ASC",
    )
    .bind(filters.provider.as_deref())
    .bind(filters.channel_id.as_deref())
    .bind(filters.invariant.as_deref())
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("query postgres invariant counts: {error}"))?;

    rows.into_iter()
        .map(|row| {
            Ok(InvariantViolationCount {
                invariant: row
                    .try_get("invariant")
                    .map_err(|error| anyhow!("decode invariant: {error}"))?,
                count: row
                    .try_get::<i64, _>("violation_count")
                    .map_err(|error| anyhow!("decode violation_count: {error}"))?
                    .max(0) as u64,
            })
        })
        .collect()
}

pub(super) async fn query_invariant_events_pg(
    pool: &PgPool,
    filters: &InvariantAnalyticsFilters,
    limit: usize,
) -> Result<Vec<InvariantViolationRecord>> {
    let rows = sqlx::query(
        "SELECT id,
                provider,
                channel_id,
                dispatch_id,
                session_key,
                turn_id,
                status,
                payload_json::text AS payload_json,
                to_char(created_at AT TIME ZONE 'Asia/Seoul', 'YYYY-MM-DD HH24:MI:SS') AS created_at_kst
         FROM observability_events
         WHERE event_type = 'invariant_violation'
           AND ($1::text IS NULL OR provider = $1)
           AND ($2::text IS NULL OR channel_id = $2)
           AND ($3::text IS NULL OR status = $3)
         ORDER BY id DESC
         LIMIT $4",
    )
    .bind(filters.provider.as_deref())
    .bind(filters.channel_id.as_deref())
    .bind(filters.invariant.as_deref())
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("query postgres invariant events: {error}"))?;

    rows.into_iter()
        .map(|row| {
            let payload_json: Option<String> = row
                .try_get("payload_json")
                .map_err(|error| anyhow!("decode invariant payload_json: {error}"))?;
            let payload = payload_json
                .as_deref()
                .and_then(|value| serde_json::from_str::<Value>(value).ok())
                .unwrap_or_else(|| json!({}));
            let invariant = row
                .try_get::<Option<String>, _>("status")
                .map_err(|error| anyhow!("decode invariant status: {error}"))?
                .or_else(|| payload.get("invariant").and_then(value_as_string))
                .unwrap_or_else(|| "unknown".to_string());
            Ok(invariant_record_from_parts(
                row.try_get("id")
                    .map_err(|error| anyhow!("decode invariant event id: {error}"))?,
                invariant,
                row.try_get("provider")
                    .map_err(|error| anyhow!("decode invariant provider: {error}"))?,
                row.try_get("channel_id")
                    .map_err(|error| anyhow!("decode invariant channel_id: {error}"))?,
                row.try_get("dispatch_id")
                    .map_err(|error| anyhow!("decode invariant dispatch_id: {error}"))?,
                row.try_get("session_key")
                    .map_err(|error| anyhow!("decode invariant session_key: {error}"))?,
                row.try_get("turn_id")
                    .map_err(|error| anyhow!("decode invariant turn_id: {error}"))?,
                payload,
                row.try_get("created_at_kst")
                    .map_err(|error| anyhow!("decode invariant created_at: {error}"))?,
            ))
        })
        .collect()
}

pub(super) async fn query_agent_quality_events_pg(
    pool: &PgPool,
    agent_id: Option<&str>,
    days: i64,
    limit: usize,
) -> Result<Vec<AgentQualityEventRecord>> {
    let rows = sqlx::query(
        "SELECT id,
                source_event_id,
                correlation_id,
                agent_id,
                provider,
                channel_id,
                card_id,
                dispatch_id,
                event_type::text AS event_type,
                payload::text AS payload_json,
                to_char(created_at AT TIME ZONE 'Asia/Seoul', 'YYYY-MM-DD HH24:MI:SS') AS created_at_kst
         FROM agent_quality_event
         WHERE ($1::text IS NULL OR agent_id = $1)
           AND created_at >= NOW() - ($2::int * INTERVAL '1 day')
         ORDER BY created_at DESC, id DESC
         LIMIT $3",
    )
    .bind(agent_id)
    .bind(days as i32)
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("query postgres agent quality events: {error}"))?;

    rows.into_iter()
        .map(|row| {
            let payload_json: Option<String> = row
                .try_get("payload_json")
                .map_err(|error| anyhow!("decode agent quality payload_json: {error}"))?;
            Ok(AgentQualityEventRecord {
                id: row
                    .try_get("id")
                    .map_err(|error| anyhow!("decode agent quality event id: {error}"))?,
                source_event_id: row
                    .try_get("source_event_id")
                    .map_err(|error| anyhow!("decode agent quality source_event_id: {error}"))?,
                correlation_id: row
                    .try_get("correlation_id")
                    .map_err(|error| anyhow!("decode agent quality correlation_id: {error}"))?,
                agent_id: row
                    .try_get("agent_id")
                    .map_err(|error| anyhow!("decode agent quality agent_id: {error}"))?,
                provider: row
                    .try_get("provider")
                    .map_err(|error| anyhow!("decode agent quality provider: {error}"))?,
                channel_id: row
                    .try_get("channel_id")
                    .map_err(|error| anyhow!("decode agent quality channel_id: {error}"))?,
                card_id: row
                    .try_get("card_id")
                    .map_err(|error| anyhow!("decode agent quality card_id: {error}"))?,
                dispatch_id: row
                    .try_get("dispatch_id")
                    .map_err(|error| anyhow!("decode agent quality dispatch_id: {error}"))?,
                event_type: row
                    .try_get("event_type")
                    .map_err(|error| anyhow!("decode agent quality event_type: {error}"))?,
                payload: payload_json
                    .as_deref()
                    .and_then(|value| serde_json::from_str(value).ok())
                    .unwrap_or_else(|| json!({})),
                created_at: row
                    .try_get("created_at_kst")
                    .map_err(|error| anyhow!("decode agent quality created_at: {error}"))?,
            })
        })
        .collect()
}

pub(super) async fn upsert_agent_quality_daily_pg(pool: &PgPool) -> Result<u64> {
    // #1101 extends #930's rollup with four additional daily metrics:
    //   * avg_rework_count   — avg(review_fail per card) over cards that had
    //                          at least one review_fail that day.
    //   * cost_per_done_card — sum(card_transitioned.payload->>'cost') /
    //                          count(card_transitioned → done).
    //   * latency_p50_ms     — percentile_cont(0.5) over turn_complete
    //                          payload->>'duration_ms'.
    //   * latency_p99_ms     — percentile_cont(0.99) over turn_complete
    //                          payload->>'duration_ms'.
    //
    // All four are nullable; they land as NULL when the requisite events are
    // absent for an agent/day, which is also how the dashboard renders
    // "측정 불가" downstream.
    let row_count = sqlx::query_scalar::<_, i64>(
        "WITH daily_counts AS (
             SELECT agent_id,
                    (created_at AT TIME ZONE 'Asia/Seoul')::date AS day,
                    MAX(provider) FILTER (WHERE provider IS NOT NULL AND btrim(provider) <> '') AS provider,
                    MAX(channel_id) FILTER (WHERE channel_id IS NOT NULL AND btrim(channel_id) <> '') AS channel_id,
                    COUNT(*) FILTER (WHERE event_type = 'turn_complete')::bigint AS turn_success_count,
                    COUNT(*) FILTER (WHERE event_type = 'turn_error')::bigint AS turn_error_count,
                    COUNT(*) FILTER (WHERE event_type = 'review_pass')::bigint AS review_pass_count,
                    COUNT(*) FILTER (WHERE event_type = 'review_fail')::bigint AS review_fail_count
             FROM agent_quality_event
             WHERE agent_id IS NOT NULL
               AND btrim(agent_id) <> ''
               AND created_at >= NOW() - INTERVAL '45 days'
             GROUP BY agent_id, (created_at AT TIME ZONE 'Asia/Seoul')::date
         ),
         rework_per_card AS (
             SELECT agent_id,
                    (created_at AT TIME ZONE 'Asia/Seoul')::date AS day,
                    card_id,
                    COUNT(*)::double precision AS review_fail_count
             FROM agent_quality_event
             WHERE event_type = 'review_fail'
               AND agent_id IS NOT NULL AND btrim(agent_id) <> ''
               AND card_id IS NOT NULL AND btrim(card_id) <> ''
               AND created_at >= NOW() - INTERVAL '45 days'
             GROUP BY agent_id, (created_at AT TIME ZONE 'Asia/Seoul')::date, card_id
         ),
         rework_agg AS (
             SELECT agent_id, day, AVG(review_fail_count) AS avg_rework_count
             FROM rework_per_card
             GROUP BY agent_id, day
         ),
         cost_agg AS (
             SELECT agent_id,
                    (created_at AT TIME ZONE 'Asia/Seoul')::date AS day,
                    SUM(COALESCE(NULLIF(payload->>'cost', ''), '0')::double precision) AS cost_total,
                    COUNT(*) FILTER (
                        WHERE event_type = 'card_transitioned'
                          AND payload->>'to' = 'done'
                    )::bigint AS done_card_count
             FROM agent_quality_event
             WHERE agent_id IS NOT NULL AND btrim(agent_id) <> ''
               AND created_at >= NOW() - INTERVAL '45 days'
             GROUP BY agent_id, (created_at AT TIME ZONE 'Asia/Seoul')::date
         ),
         latency_agg AS (
             SELECT agent_id,
                    (created_at AT TIME ZONE 'Asia/Seoul')::date AS day,
                    percentile_cont(0.5) WITHIN GROUP (
                        ORDER BY NULLIF(payload->>'duration_ms', '')::double precision
                    ) AS latency_p50_ms,
                    percentile_cont(0.99) WITHIN GROUP (
                        ORDER BY NULLIF(payload->>'duration_ms', '')::double precision
                    ) AS latency_p99_ms
             FROM agent_quality_event
             WHERE event_type = 'turn_complete'
               AND agent_id IS NOT NULL AND btrim(agent_id) <> ''
               AND payload ? 'duration_ms'
               AND created_at >= NOW() - INTERVAL '45 days'
             GROUP BY agent_id, (created_at AT TIME ZONE 'Asia/Seoul')::date
         ),
         windowed AS (
             SELECT d.agent_id,
                    d.day,
                    d.provider,
                    d.channel_id,
                    d.turn_success_count,
                    d.turn_error_count,
                    d.review_pass_count,
                    d.review_fail_count,
                    d.turn_success_count + d.turn_error_count AS turn_sample_size,
                    d.review_pass_count + d.review_fail_count AS review_sample_size,
                    d.turn_success_count + d.turn_error_count + d.review_pass_count + d.review_fail_count AS sample_size,
                    COALESCE(SUM(w.turn_success_count) FILTER (WHERE w.day >= d.day - 6), 0)::bigint AS turn_success_count_7d,
                    COALESCE(SUM(w.turn_error_count) FILTER (WHERE w.day >= d.day - 6), 0)::bigint AS turn_error_count_7d,
                    COALESCE(SUM(w.review_pass_count) FILTER (WHERE w.day >= d.day - 6), 0)::bigint AS review_pass_count_7d,
                    COALESCE(SUM(w.review_fail_count) FILTER (WHERE w.day >= d.day - 6), 0)::bigint AS review_fail_count_7d,
                    COALESCE(SUM(w.turn_success_count), 0)::bigint AS turn_success_count_30d,
                    COALESCE(SUM(w.turn_error_count), 0)::bigint AS turn_error_count_30d,
                    COALESCE(SUM(w.review_pass_count), 0)::bigint AS review_pass_count_30d,
                    COALESCE(SUM(w.review_fail_count), 0)::bigint AS review_fail_count_30d
             FROM daily_counts d
             JOIN daily_counts w
               ON w.agent_id = d.agent_id
              AND w.day BETWEEN d.day - 29 AND d.day
             GROUP BY d.agent_id,
                      d.day,
                      d.provider,
                      d.channel_id,
                      d.turn_success_count,
                      d.turn_error_count,
                      d.review_pass_count,
                      d.review_fail_count
         ),
         normalized AS (
             SELECT w.*,
                    w.turn_success_count_7d + w.turn_error_count_7d AS turn_sample_size_7d,
                    w.review_pass_count_7d + w.review_fail_count_7d AS review_sample_size_7d,
                    w.turn_success_count_7d + w.turn_error_count_7d + w.review_pass_count_7d + w.review_fail_count_7d AS sample_size_7d,
                    w.turn_success_count_30d + w.turn_error_count_30d AS turn_sample_size_30d,
                    w.review_pass_count_30d + w.review_fail_count_30d AS review_sample_size_30d,
                    w.turn_success_count_30d + w.turn_error_count_30d + w.review_pass_count_30d + w.review_fail_count_30d AS sample_size_30d,
                    r.avg_rework_count,
                    CASE WHEN COALESCE(c.done_card_count, 0) > 0
                         THEN COALESCE(c.cost_total, 0) / c.done_card_count
                         ELSE NULL END AS cost_per_done_card,
                    l.latency_p50_ms,
                    l.latency_p99_ms
             FROM windowed w
             LEFT JOIN rework_agg  r ON r.agent_id = w.agent_id AND r.day = w.day
             LEFT JOIN cost_agg    c ON c.agent_id = w.agent_id AND c.day = w.day
             LEFT JOIN latency_agg l ON l.agent_id = w.agent_id AND l.day = w.day
         ),
         upserted AS (
             INSERT INTO agent_quality_daily (
                 agent_id,
                 day,
                 provider,
                 channel_id,
                 turn_success_count,
                 turn_error_count,
                 review_pass_count,
                 review_fail_count,
                 turn_sample_size,
                 review_sample_size,
                 sample_size,
                 turn_success_rate,
                 review_pass_rate,
                 turn_success_count_7d,
                 turn_error_count_7d,
                 review_pass_count_7d,
                 review_fail_count_7d,
                 turn_sample_size_7d,
                 review_sample_size_7d,
                 sample_size_7d,
                 turn_success_rate_7d,
                 review_pass_rate_7d,
                 measurement_unavailable_7d,
                 turn_success_count_30d,
                 turn_error_count_30d,
                 review_pass_count_30d,
                 review_fail_count_30d,
                 turn_sample_size_30d,
                 review_sample_size_30d,
                 sample_size_30d,
                 turn_success_rate_30d,
                 review_pass_rate_30d,
                 measurement_unavailable_30d,
                 avg_rework_count,
                 cost_per_done_card,
                 latency_p50_ms,
                 latency_p99_ms,
                 computed_at
             )
             SELECT agent_id,
                    day,
                    provider,
                    channel_id,
                    turn_success_count,
                    turn_error_count,
                    review_pass_count,
                    review_fail_count,
                    turn_sample_size,
                    review_sample_size,
                    sample_size,
                    CASE WHEN turn_sample_size > 0 THEN turn_success_count::double precision / turn_sample_size ELSE NULL END,
                    CASE WHEN review_sample_size > 0 THEN review_pass_count::double precision / review_sample_size ELSE NULL END,
                    turn_success_count_7d,
                    turn_error_count_7d,
                    review_pass_count_7d,
                    review_fail_count_7d,
                    turn_sample_size_7d,
                    review_sample_size_7d,
                    sample_size_7d,
                    CASE WHEN turn_sample_size_7d > 0 THEN turn_success_count_7d::double precision / turn_sample_size_7d ELSE NULL END,
                    CASE WHEN review_sample_size_7d > 0 THEN review_pass_count_7d::double precision / review_sample_size_7d ELSE NULL END,
                    sample_size_7d < $1,
                    turn_success_count_30d,
                    turn_error_count_30d,
                    review_pass_count_30d,
                    review_fail_count_30d,
                    turn_sample_size_30d,
                    review_sample_size_30d,
                    sample_size_30d,
                    CASE WHEN turn_sample_size_30d > 0 THEN turn_success_count_30d::double precision / turn_sample_size_30d ELSE NULL END,
                    CASE WHEN review_sample_size_30d > 0 THEN review_pass_count_30d::double precision / review_sample_size_30d ELSE NULL END,
                    sample_size_30d < $1,
                    avg_rework_count,
                    cost_per_done_card,
                    CASE WHEN latency_p50_ms IS NULL THEN NULL ELSE latency_p50_ms::bigint END,
                    CASE WHEN latency_p99_ms IS NULL THEN NULL ELSE latency_p99_ms::bigint END,
                    NOW()
             FROM normalized
             ON CONFLICT (agent_id, day) DO UPDATE SET
                 provider = EXCLUDED.provider,
                 channel_id = EXCLUDED.channel_id,
                 turn_success_count = EXCLUDED.turn_success_count,
                 turn_error_count = EXCLUDED.turn_error_count,
                 review_pass_count = EXCLUDED.review_pass_count,
                 review_fail_count = EXCLUDED.review_fail_count,
                 turn_sample_size = EXCLUDED.turn_sample_size,
                 review_sample_size = EXCLUDED.review_sample_size,
                 sample_size = EXCLUDED.sample_size,
                 turn_success_rate = EXCLUDED.turn_success_rate,
                 review_pass_rate = EXCLUDED.review_pass_rate,
                 turn_success_count_7d = EXCLUDED.turn_success_count_7d,
                 turn_error_count_7d = EXCLUDED.turn_error_count_7d,
                 review_pass_count_7d = EXCLUDED.review_pass_count_7d,
                 review_fail_count_7d = EXCLUDED.review_fail_count_7d,
                 turn_sample_size_7d = EXCLUDED.turn_sample_size_7d,
                 review_sample_size_7d = EXCLUDED.review_sample_size_7d,
                 sample_size_7d = EXCLUDED.sample_size_7d,
                 turn_success_rate_7d = EXCLUDED.turn_success_rate_7d,
                 review_pass_rate_7d = EXCLUDED.review_pass_rate_7d,
                 measurement_unavailable_7d = EXCLUDED.measurement_unavailable_7d,
                 turn_success_count_30d = EXCLUDED.turn_success_count_30d,
                 turn_error_count_30d = EXCLUDED.turn_error_count_30d,
                 review_pass_count_30d = EXCLUDED.review_pass_count_30d,
                 review_fail_count_30d = EXCLUDED.review_fail_count_30d,
                 turn_sample_size_30d = EXCLUDED.turn_sample_size_30d,
                 review_sample_size_30d = EXCLUDED.review_sample_size_30d,
                 sample_size_30d = EXCLUDED.sample_size_30d,
                 turn_success_rate_30d = EXCLUDED.turn_success_rate_30d,
                 review_pass_rate_30d = EXCLUDED.review_pass_rate_30d,
                 measurement_unavailable_30d = EXCLUDED.measurement_unavailable_30d,
                 avg_rework_count = EXCLUDED.avg_rework_count,
                 cost_per_done_card = EXCLUDED.cost_per_done_card,
                 latency_p50_ms = EXCLUDED.latency_p50_ms,
                 latency_p99_ms = EXCLUDED.latency_p99_ms,
                 computed_at = EXCLUDED.computed_at
             RETURNING 1
         )
         SELECT COUNT(*) FROM upserted",
    )
    .bind(QUALITY_SAMPLE_GUARD)
    .fetch_one(pool)
    .await
    .map_err(|error| anyhow!("upsert postgres agent quality daily: {error}"))?;

    Ok(row_count.max(0) as u64)
}

/// #1102 event-based fallback (postgres): synthesize per-day records from
/// `agent_quality_event` when the daily rollup table is empty for this
/// agent. Only counts turn/review success/fail events, rolling windows are
/// computed in-memory over the returned days.
pub(super) async fn synth_agent_quality_daily_from_events_pg(
    pool: &PgPool,
    agent_id: &str,
    days: i64,
) -> Result<Vec<AgentQualityDailyRecord>> {
    let days = days.clamp(1, MAX_QUALITY_DAYS);
    let rows = sqlx::query(
        "SELECT to_char((created_at AT TIME ZONE 'Asia/Seoul')::date, 'YYYY-MM-DD') AS day_text,
                MAX(provider) FILTER (WHERE provider IS NOT NULL AND btrim(provider) <> '') AS provider,
                MAX(channel_id) FILTER (WHERE channel_id IS NOT NULL AND btrim(channel_id) <> '') AS channel_id,
                COUNT(*) FILTER (WHERE event_type = 'turn_complete')::bigint AS turn_success_count,
                COUNT(*) FILTER (WHERE event_type = 'turn_error')::bigint AS turn_error_count,
                COUNT(*) FILTER (WHERE event_type = 'review_pass')::bigint AS review_pass_count,
                COUNT(*) FILTER (WHERE event_type = 'review_fail')::bigint AS review_fail_count
         FROM agent_quality_event
         WHERE agent_id = $1
           AND created_at >= NOW() - ($2::bigint || ' days')::interval
         GROUP BY day_text
         ORDER BY day_text DESC",
    )
    .bind(agent_id)
    .bind(days)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("synth agent quality daily (pg): {error}"))?;

    let buckets: Vec<SynthDailyBucket> = rows
        .into_iter()
        .map(|row| {
            Ok::<_, anyhow::Error>(SynthDailyBucket {
                day: row
                    .try_get::<String, _>("day_text")
                    .map_err(|e| anyhow!("decode synth day: {e}"))?,
                provider: row.try_get::<Option<String>, _>("provider").ok().flatten(),
                channel_id: row
                    .try_get::<Option<String>, _>("channel_id")
                    .ok()
                    .flatten(),
                turn_success: row
                    .try_get::<i64, _>("turn_success_count")
                    .unwrap_or(0)
                    .max(0),
                turn_error: row
                    .try_get::<i64, _>("turn_error_count")
                    .unwrap_or(0)
                    .max(0),
                review_pass: row
                    .try_get::<i64, _>("review_pass_count")
                    .unwrap_or(0)
                    .max(0),
                review_fail: row
                    .try_get::<i64, _>("review_fail_count")
                    .unwrap_or(0)
                    .max(0),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(buckets_to_synth_records(agent_id, buckets))
}

#[derive(Debug, Clone)]
struct SynthDailyBucket {
    day: String,
    provider: Option<String>,
    channel_id: Option<String>,
    turn_success: i64,
    turn_error: i64,
    review_pass: i64,
    review_fail: i64,
}

fn buckets_to_synth_records(
    agent_id: &str,
    buckets: Vec<SynthDailyBucket>,
) -> Vec<AgentQualityDailyRecord> {
    let now = super::helpers::now_kst();
    // #2049 Finding 6: index buckets by calendar date so rolling windows sum
    // over `target_day - N + 1 ..= target_day` (calendar-day semantics),
    // matching the SQL `upsert_agent_quality_daily_pg` rollup. Previously the
    // window walked `buckets[idx..idx+N]` which used "Nth event-having day"
    // — agents that only had events on 5 of the last 30 days would have
    // their `rolling_7d` silently aggregate over 25+ calendar days.
    let indexed: BTreeMap<chrono::NaiveDate, &SynthDailyBucket> = buckets
        .iter()
        .filter_map(|b| {
            chrono::NaiveDate::parse_from_str(&b.day, "%Y-%m-%d")
                .ok()
                .map(|d| (d, b))
        })
        .collect();
    buckets
        .iter()
        .map(|bucket| {
            let target_day = chrono::NaiveDate::parse_from_str(&bucket.day, "%Y-%m-%d").ok();
            let window = |size: i64| -> (i64, i64, i64, i64) {
                let Some(target) = target_day else {
                    return (
                        bucket.turn_success,
                        bucket.turn_error,
                        bucket.review_pass,
                        bucket.review_fail,
                    );
                };
                let start = target - chrono::Duration::days(size - 1);
                let mut ts = 0i64;
                let mut te = 0i64;
                let mut rp = 0i64;
                let mut rf = 0i64;
                for (_, b) in indexed.range(start..=target) {
                    ts += b.turn_success;
                    te += b.turn_error;
                    rp += b.review_pass;
                    rf += b.review_fail;
                }
                (ts, te, rp, rf)
            };
            let (ts7, te7, rp7, rf7) = window(7);
            let (ts30, te30, rp30, rf30) = window(30);
            let turn_sample = bucket.turn_success + bucket.turn_error;
            let review_sample = bucket.review_pass + bucket.review_fail;
            let sample = turn_sample + review_sample;
            let turn_rate = if turn_sample > 0 {
                Some(bucket.turn_success as f64 / turn_sample as f64)
            } else {
                None
            };
            let review_rate = if review_sample > 0 {
                Some(bucket.review_pass as f64 / review_sample as f64)
            } else {
                None
            };
            let turn_sample_7d = ts7 + te7;
            let review_sample_7d = rp7 + rf7;
            let sample_7d = turn_sample_7d + review_sample_7d;
            let turn_sample_30d = ts30 + te30;
            let review_sample_30d = rp30 + rf30;
            let sample_30d = turn_sample_30d + review_sample_30d;
            let unavailable_7d = sample_7d < QUALITY_SAMPLE_GUARD;
            let unavailable_30d = sample_30d < QUALITY_SAMPLE_GUARD;
            AgentQualityDailyRecord {
                agent_id: agent_id.to_string(),
                day: bucket.day.clone(),
                provider: bucket.provider.clone(),
                channel_id: bucket.channel_id.clone(),
                turn_success_count: bucket.turn_success,
                turn_error_count: bucket.turn_error,
                review_pass_count: bucket.review_pass,
                review_fail_count: bucket.review_fail,
                turn_sample_size: turn_sample,
                review_sample_size: review_sample,
                sample_size: sample,
                turn_success_rate: turn_rate,
                review_pass_rate: review_rate,
                rolling_7d: quality_window(
                    7,
                    sample_7d,
                    unavailable_7d,
                    turn_sample_7d,
                    if turn_sample_7d > 0 {
                        Some(ts7 as f64 / turn_sample_7d as f64)
                    } else {
                        None
                    },
                    review_sample_7d,
                    if review_sample_7d > 0 {
                        Some(rp7 as f64 / review_sample_7d as f64)
                    } else {
                        None
                    },
                ),
                rolling_30d: quality_window(
                    30,
                    sample_30d,
                    unavailable_30d,
                    turn_sample_30d,
                    if turn_sample_30d > 0 {
                        Some(ts30 as f64 / turn_sample_30d as f64)
                    } else {
                        None
                    },
                    review_sample_30d,
                    if review_sample_30d > 0 {
                        Some(rp30 as f64 / review_sample_30d as f64)
                    } else {
                        None
                    },
                ),
                computed_at: now.clone(),
            }
        })
        .collect()
}

fn measurement_label(unavailable: bool) -> Option<String> {
    unavailable.then(|| "측정 불가".to_string())
}

pub(super) fn quality_window(
    days: i64,
    sample_size: i64,
    measurement_unavailable: bool,
    turn_sample_size: i64,
    turn_success_rate: Option<f64>,
    review_sample_size: i64,
    review_pass_rate: Option<f64>,
) -> AgentQualityWindow {
    AgentQualityWindow {
        days,
        sample_size: sample_size.max(0),
        measurement_unavailable,
        measurement_label: measurement_label(measurement_unavailable),
        turn_sample_size: turn_sample_size.max(0),
        turn_success_rate,
        review_sample_size: review_sample_size.max(0),
        review_pass_rate,
    }
}

fn quality_daily_record_from_pg_row(
    row: &sqlx::postgres::PgRow,
) -> Result<AgentQualityDailyRecord> {
    let measurement_unavailable_7d = row
        .try_get::<bool, _>("measurement_unavailable_7d")
        .map_err(|error| anyhow!("decode measurement_unavailable_7d: {error}"))?;
    let measurement_unavailable_30d = row
        .try_get::<bool, _>("measurement_unavailable_30d")
        .map_err(|error| anyhow!("decode measurement_unavailable_30d: {error}"))?;
    Ok(AgentQualityDailyRecord {
        agent_id: row
            .try_get("agent_id")
            .map_err(|error| anyhow!("decode agent quality daily agent_id: {error}"))?,
        day: row
            .try_get("day_text")
            .map_err(|error| anyhow!("decode agent quality daily day: {error}"))?,
        provider: row
            .try_get("provider")
            .map_err(|error| anyhow!("decode agent quality daily provider: {error}"))?,
        channel_id: row
            .try_get("channel_id")
            .map_err(|error| anyhow!("decode agent quality daily channel_id: {error}"))?,
        turn_success_count: row
            .try_get::<i64, _>("turn_success_count")
            .map_err(|error| anyhow!("decode turn_success_count: {error}"))?
            .max(0),
        turn_error_count: row
            .try_get::<i64, _>("turn_error_count")
            .map_err(|error| anyhow!("decode turn_error_count: {error}"))?
            .max(0),
        review_pass_count: row
            .try_get::<i64, _>("review_pass_count")
            .map_err(|error| anyhow!("decode review_pass_count: {error}"))?
            .max(0),
        review_fail_count: row
            .try_get::<i64, _>("review_fail_count")
            .map_err(|error| anyhow!("decode review_fail_count: {error}"))?
            .max(0),
        turn_sample_size: row
            .try_get::<i64, _>("turn_sample_size")
            .map_err(|error| anyhow!("decode turn_sample_size: {error}"))?
            .max(0),
        review_sample_size: row
            .try_get::<i64, _>("review_sample_size")
            .map_err(|error| anyhow!("decode review_sample_size: {error}"))?
            .max(0),
        sample_size: row
            .try_get::<i64, _>("sample_size")
            .map_err(|error| anyhow!("decode sample_size: {error}"))?
            .max(0),
        turn_success_rate: row
            .try_get("turn_success_rate")
            .map_err(|error| anyhow!("decode turn_success_rate: {error}"))?,
        review_pass_rate: row
            .try_get("review_pass_rate")
            .map_err(|error| anyhow!("decode review_pass_rate: {error}"))?,
        rolling_7d: quality_window(
            7,
            row.try_get("sample_size_7d")
                .map_err(|error| anyhow!("decode sample_size_7d: {error}"))?,
            measurement_unavailable_7d,
            row.try_get("turn_sample_size_7d")
                .map_err(|error| anyhow!("decode turn_sample_size_7d: {error}"))?,
            row.try_get("turn_success_rate_7d")
                .map_err(|error| anyhow!("decode turn_success_rate_7d: {error}"))?,
            row.try_get("review_sample_size_7d")
                .map_err(|error| anyhow!("decode review_sample_size_7d: {error}"))?,
            row.try_get("review_pass_rate_7d")
                .map_err(|error| anyhow!("decode review_pass_rate_7d: {error}"))?,
        ),
        rolling_30d: quality_window(
            30,
            row.try_get("sample_size_30d")
                .map_err(|error| anyhow!("decode sample_size_30d: {error}"))?,
            measurement_unavailable_30d,
            row.try_get("turn_sample_size_30d")
                .map_err(|error| anyhow!("decode turn_sample_size_30d: {error}"))?,
            row.try_get("turn_success_rate_30d")
                .map_err(|error| anyhow!("decode turn_success_rate_30d: {error}"))?,
            row.try_get("review_sample_size_30d")
                .map_err(|error| anyhow!("decode review_sample_size_30d: {error}"))?,
            row.try_get("review_pass_rate_30d")
                .map_err(|error| anyhow!("decode review_pass_rate_30d: {error}"))?,
        ),
        computed_at: row
            .try_get("computed_at_kst")
            .map_err(|error| anyhow!("decode computed_at_kst: {error}"))?,
    })
}

fn agent_quality_daily_select_pg() -> &'static str {
    "SELECT agent_id,
            to_char(day, 'YYYY-MM-DD') AS day_text,
            provider,
            channel_id,
            turn_success_count,
            turn_error_count,
            review_pass_count,
            review_fail_count,
            turn_sample_size,
            review_sample_size,
            sample_size,
            turn_success_rate,
            review_pass_rate,
            turn_sample_size_7d,
            sample_size_7d,
            turn_success_rate_7d,
            review_sample_size_7d,
            review_pass_rate_7d,
            measurement_unavailable_7d,
            turn_sample_size_30d,
            sample_size_30d,
            turn_success_rate_30d,
            review_sample_size_30d,
            review_pass_rate_30d,
            measurement_unavailable_30d,
            to_char(computed_at AT TIME ZONE 'Asia/Seoul', 'YYYY-MM-DD HH24:MI:SS') AS computed_at_kst
     FROM agent_quality_daily"
}

pub(super) async fn query_agent_quality_daily_pg(
    pool: &PgPool,
    agent_id: Option<&str>,
    days: i64,
    limit: usize,
) -> Result<Vec<AgentQualityDailyRecord>> {
    let sql = format!(
        "{} WHERE ($1::text IS NULL OR agent_id = $1)
              AND day >= (CURRENT_DATE - $2::int)
            ORDER BY day DESC, agent_id ASC
            LIMIT $3",
        agent_quality_daily_select_pg()
    );
    let rows = sqlx::query(&sql)
        .bind(agent_id)
        .bind(days as i32)
        .bind(limit as i64)
        .fetch_all(pool)
        .await
        .map_err(|error| anyhow!("query postgres agent quality daily: {error}"))?;

    rows.iter().map(quality_daily_record_from_pg_row).collect()
}

fn quality_ranking_entry_from_daily(
    rank: i64,
    record: AgentQualityDailyRecord,
    agent_name: Option<String>,
) -> AgentQualityRankingEntry {
    AgentQualityRankingEntry {
        rank,
        agent_id: record.agent_id,
        agent_name,
        provider: record.provider,
        channel_id: record.channel_id,
        latest_day: record.day,
        rolling_7d: record.rolling_7d,
        rolling_30d: record.rolling_30d,
        metric_value: None,
    }
}

/// Pick the metric value for a ranking entry given the (metric, window)
/// pair. Returns `None` when the window is measurement-unavailable or the
/// underlying rate is NULL.
pub(super) fn pick_ranking_metric_value(
    entry: &AgentQualityRankingEntry,
    metric: QualityRankingMetric,
    window: QualityRankingWindow,
) -> Option<f64> {
    let win = match window {
        QualityRankingWindow::Seven => &entry.rolling_7d,
        QualityRankingWindow::Thirty => &entry.rolling_30d,
    };
    if win.measurement_unavailable {
        return None;
    }
    match metric {
        QualityRankingMetric::TurnSuccessRate => win.turn_success_rate,
        QualityRankingMetric::ReviewPassRate => win.review_pass_rate,
    }
}

/// Return the sample size for the requested window on a ranking entry.
pub(super) fn ranking_window_sample_size(
    entry: &AgentQualityRankingEntry,
    window: QualityRankingWindow,
) -> i64 {
    match window {
        QualityRankingWindow::Seven => entry.rolling_7d.sample_size,
        QualityRankingWindow::Thirty => entry.rolling_30d.sample_size,
    }
}

pub(super) async fn query_agent_quality_ranking_pg(
    pool: &PgPool,
    limit: usize,
) -> Result<Vec<AgentQualityRankingEntry>> {
    let sql = format!(
        "WITH latest AS (
             SELECT DISTINCT ON (agent_id) *
             FROM agent_quality_daily
             ORDER BY agent_id, day DESC
         ),
         ranked AS (
             SELECT row_number() OVER (
                        ORDER BY measurement_unavailable_7d ASC,
                                 turn_success_rate_7d DESC NULLS LAST,
                                 review_pass_rate_7d DESC NULLS LAST,
                                 sample_size_7d DESC,
                                 agent_id ASC
                    )::bigint AS rank,
                    latest.*,
                    COALESCE(a.name_ko, a.name) AS agent_name
             FROM latest
             LEFT JOIN agents a
               ON a.id = latest.agent_id
         )
         SELECT rank,
                agent_id,
                to_char(day, 'YYYY-MM-DD') AS day_text,
                provider,
                channel_id,
                turn_success_count,
                turn_error_count,
                review_pass_count,
                review_fail_count,
                turn_sample_size,
                review_sample_size,
                sample_size,
                turn_success_rate,
                review_pass_rate,
                turn_sample_size_7d,
                sample_size_7d,
                turn_success_rate_7d,
                review_sample_size_7d,
                review_pass_rate_7d,
                measurement_unavailable_7d,
                turn_sample_size_30d,
                sample_size_30d,
                turn_success_rate_30d,
                review_sample_size_30d,
                review_pass_rate_30d,
                measurement_unavailable_30d,
                to_char(computed_at AT TIME ZONE 'Asia/Seoul', 'YYYY-MM-DD HH24:MI:SS') AS computed_at_kst,
                agent_name
         FROM ranked
         ORDER BY rank ASC
         LIMIT $1"
    );
    let rows = sqlx::query(&sql)
        .bind(limit as i64)
        .fetch_all(pool)
        .await
        .map_err(|error| anyhow!("query postgres agent quality ranking: {error}"))?;

    rows.iter()
        .map(|row| {
            let rank = row
                .try_get::<i64, _>("rank")
                .map_err(|error| anyhow!("decode quality rank: {error}"))?;
            let agent_name = row
                .try_get("agent_name")
                .map_err(|error| anyhow!("decode quality ranking agent_name: {error}"))?;
            Ok(quality_ranking_entry_from_daily(
                rank,
                quality_daily_record_from_pg_row(row)?,
                agent_name,
            ))
        })
        .collect()
}

pub(super) async fn query_counter_snapshots_pg(
    pool: &PgPool,
    filters: &AnalyticsFilters,
    limit: usize,
) -> Result<Vec<AnalyticsCounterSnapshot>> {
    let rows = sqlx::query(
        "SELECT provider,
                channel_id,
                turn_attempts,
                guard_fires,
                watcher_replacements,
                recovery_fires,
                turn_successes,
                turn_failures,
                to_char(snapshot_at AT TIME ZONE 'Asia/Seoul', 'YYYY-MM-DD HH24:MI:SS') AS snapshot_at_kst
         FROM (
             SELECT DISTINCT ON (provider, channel_id)
                    provider,
                    channel_id,
                    turn_attempts,
                    guard_fires,
                    watcher_replacements,
                    recovery_fires,
                    turn_successes,
                    turn_failures,
                    snapshot_at,
                    id
             FROM observability_counter_snapshots
             WHERE ($1::text IS NULL OR provider = $1)
               AND ($2::text IS NULL OR channel_id = $2)
             ORDER BY provider, channel_id, id DESC
         ) snapshots
         ORDER BY turn_attempts DESC, provider, channel_id
         LIMIT $3",
    )
    .bind(filters.provider.as_deref())
    .bind(filters.channel_id.as_deref())
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("query postgres observability snapshots: {error}"))?;

    rows.into_iter()
        .map(|row| {
            let values = CounterValues {
                turn_attempts: row
                    .try_get::<i64, _>("turn_attempts")
                    .map_err(|error| anyhow!("decode turn_attempts: {error}"))?
                    .max(0) as u64,
                guard_fires: row
                    .try_get::<i64, _>("guard_fires")
                    .map_err(|error| anyhow!("decode guard_fires: {error}"))?
                    .max(0) as u64,
                watcher_replacements: row
                    .try_get::<i64, _>("watcher_replacements")
                    .map_err(|error| anyhow!("decode watcher_replacements: {error}"))?
                    .max(0) as u64,
                recovery_fires: row
                    .try_get::<i64, _>("recovery_fires")
                    .map_err(|error| anyhow!("decode recovery_fires: {error}"))?
                    .max(0) as u64,
                turn_successes: row
                    .try_get::<i64, _>("turn_successes")
                    .map_err(|error| anyhow!("decode turn_successes: {error}"))?
                    .max(0) as u64,
                turn_failures: row
                    .try_get::<i64, _>("turn_failures")
                    .map_err(|error| anyhow!("decode turn_failures: {error}"))?
                    .max(0) as u64,
            };
            Ok(counter_snapshot_from_values(
                &row.try_get::<String, _>("provider")
                    .map_err(|error| anyhow!("decode snapshot provider: {error}"))?,
                &row.try_get::<String, _>("channel_id")
                    .map_err(|error| anyhow!("decode snapshot channel_id: {error}"))?,
                values,
                "persisted",
                row.try_get::<String, _>("snapshot_at_kst")
                    .map_err(|error| anyhow!("decode snapshot_at: {error}"))?,
            ))
        })
        .collect()
}

pub(super) async fn insert_events_pg(pool: &PgPool, events: &[QueuedEvent]) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| anyhow!("begin postgres observability event tx: {error}"))?;
    for event in events {
        sqlx::query(
            "INSERT INTO observability_events (
                event_type,
                provider,
                channel_id,
                dispatch_id,
                session_key,
                turn_id,
                status,
                payload_json
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, CAST($8 AS jsonb))",
        )
        .bind(&event.event_type)
        .bind(&event.provider)
        .bind(&event.channel_id)
        .bind(&event.dispatch_id)
        .bind(&event.session_key)
        .bind(&event.turn_id)
        .bind(&event.status)
        .bind(&event.payload_json)
        .execute(&mut *tx)
        .await
        .map_err(|error| anyhow!("insert postgres observability event: {error}"))?;
    }
    tx.commit()
        .await
        .map_err(|error| anyhow!("commit postgres observability event tx: {error}"))?;
    Ok(())
}

/// #2049 Finding 1 / Finding 5: When bulk insert rolls back the whole tx,
/// re-attempt each row in its own short tx so good rows still land. Returns
/// the rows that still failed (candidates for dead-letter JSONL or queue
/// push-back).
pub(super) async fn insert_events_pg_row_isolated(
    pool: &PgPool,
    events: &[QueuedEvent],
) -> Vec<QueuedEvent> {
    let mut failed = Vec::new();
    for event in events {
        let result = sqlx::query(
            "INSERT INTO observability_events (
                event_type,
                provider,
                channel_id,
                dispatch_id,
                session_key,
                turn_id,
                status,
                payload_json
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, CAST($8 AS jsonb))",
        )
        .bind(&event.event_type)
        .bind(&event.provider)
        .bind(&event.channel_id)
        .bind(&event.dispatch_id)
        .bind(&event.session_key)
        .bind(&event.turn_id)
        .bind(&event.status)
        .bind(&event.payload_json)
        .execute(pool)
        .await;
        if let Err(error) = result {
            tracing::warn!(
                "[observability] per-row insert failed (event_type={}, dispatch_id={:?}): {error}",
                event.event_type,
                event.dispatch_id
            );
            failed.push(event.clone());
        }
    }
    failed
}

pub(super) async fn insert_quality_events_pg(
    pool: &PgPool,
    events: &[QueuedQualityEvent],
) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| anyhow!("begin postgres agent quality event tx: {error}"))?;
    for event in events {
        sqlx::query(
            "INSERT INTO agent_quality_event (
                source_event_id,
                correlation_id,
                agent_id,
                provider,
                channel_id,
                card_id,
                dispatch_id,
                event_type,
                payload
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8::agent_quality_event_type, CAST($9 AS jsonb))",
        )
        .bind(&event.source_event_id)
        .bind(&event.correlation_id)
        .bind(&event.agent_id)
        .bind(&event.provider)
        .bind(&event.channel_id)
        .bind(&event.card_id)
        .bind(&event.dispatch_id)
        .bind(&event.event_type)
        .bind(&event.payload_json)
        .execute(&mut *tx)
        .await
        .map_err(|error| anyhow!("insert postgres agent quality event: {error}"))?;
    }
    tx.commit()
        .await
        .map_err(|error| anyhow!("commit postgres agent quality event tx: {error}"))?;
    Ok(())
}

/// #2049 Finding 1 / Finding 5: Row-isolated retry for `agent_quality_event`
/// so one enum-cast failure cannot lose the whole 64-row batch.
pub(super) async fn insert_quality_events_pg_row_isolated(
    pool: &PgPool,
    events: &[QueuedQualityEvent],
) -> Vec<QueuedQualityEvent> {
    let mut failed = Vec::new();
    for event in events {
        let result = sqlx::query(
            "INSERT INTO agent_quality_event (
                source_event_id,
                correlation_id,
                agent_id,
                provider,
                channel_id,
                card_id,
                dispatch_id,
                event_type,
                payload
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8::agent_quality_event_type, CAST($9 AS jsonb))",
        )
        .bind(&event.source_event_id)
        .bind(&event.correlation_id)
        .bind(&event.agent_id)
        .bind(&event.provider)
        .bind(&event.channel_id)
        .bind(&event.card_id)
        .bind(&event.dispatch_id)
        .bind(&event.event_type)
        .bind(&event.payload_json)
        .execute(pool)
        .await;
        if let Err(error) = result {
            tracing::warn!(
                "[quality] per-row insert failed (event_type={}, agent_id={:?}): {error}",
                event.event_type,
                event.agent_id
            );
            failed.push(event.clone());
        }
    }
    failed
}

pub(super) async fn insert_snapshots_pg(pool: &PgPool, snapshots: &[SnapshotRow]) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| anyhow!("begin postgres observability snapshot tx: {error}"))?;
    for snapshot in snapshots {
        sqlx::query(
            "INSERT INTO observability_counter_snapshots (
                provider,
                channel_id,
                turn_attempts,
                guard_fires,
                watcher_replacements,
                recovery_fires,
                turn_successes,
                turn_failures
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&snapshot.provider)
        .bind(&snapshot.channel_id)
        .bind(saturating_i64(snapshot.values.turn_attempts))
        .bind(saturating_i64(snapshot.values.guard_fires))
        .bind(saturating_i64(snapshot.values.watcher_replacements))
        .bind(saturating_i64(snapshot.values.recovery_fires))
        .bind(saturating_i64(snapshot.values.turn_successes))
        .bind(saturating_i64(snapshot.values.turn_failures))
        .execute(&mut *tx)
        .await
        .map_err(|error| anyhow!("insert postgres observability snapshot: {error}"))?;
    }
    tx.commit()
        .await
        .map_err(|error| anyhow!("commit postgres observability snapshot tx: {error}"))?;
    Ok(())
}
