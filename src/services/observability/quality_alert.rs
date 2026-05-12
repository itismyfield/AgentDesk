//! #2049 Finding 7: agent-quality regression alert pipeline split out of
//! `mod.rs`. Owns the dedupe slot claim, alert content formatting, and the
//! outbox enqueue path. Driven by the rollup helper exposed via
//! `queries::run_agent_quality_rollup_pg`.

use anyhow::{Result, anyhow};
use sqlx::{PgPool, Row};

use super::{QUALITY_ALERT_DEDUPE_MS, QUALITY_REVIEW_DROP_THRESHOLD, QUALITY_TURN_DROP_THRESHOLD};

fn normalize_channel_target(channel: &str) -> Option<String> {
    let channel = channel.trim();
    if channel.is_empty() {
        return None;
    }
    Some(if channel.starts_with("channel:") {
        channel.to_string()
    } else {
        format!("channel:{channel}")
    })
}

async fn quality_alert_target_pg(pool: &PgPool) -> Result<Option<String>> {
    let value = sqlx::query_scalar::<_, String>(
        "SELECT value
         FROM kv_meta
         WHERE key IN ('agent_quality_monitoring_channel_id', 'kanban_human_alert_channel_id')
           AND value IS NOT NULL
           AND btrim(value) <> ''
         ORDER BY CASE key
                      WHEN 'agent_quality_monitoring_channel_id' THEN 0
                      ELSE 1
                  END
         LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow!("load agent quality alert target: {error}"))?;
    Ok(value.as_deref().and_then(normalize_channel_target))
}

#[allow(dead_code)] // #2049 Finding 7: superseded by claim_quality_alert_slot_pg.
// Kept for unit tests and operator-facing ad-hoc tooling.
async fn quality_alert_recently_sent_pg(pool: &PgPool, key: &str, now_ms: i64) -> Result<bool> {
    let last_ms =
        sqlx::query_scalar::<_, Option<String>>("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
            .bind(key)
            .fetch_optional(pool)
            .await
            .map_err(|error| anyhow!("load quality alert dedupe key {key}: {error}"))?
            .flatten()
            .and_then(|value| value.parse::<i64>().ok());
    Ok(last_ms.is_some_and(|last_ms| now_ms.saturating_sub(last_ms) < QUALITY_ALERT_DEDUPE_MS))
}

#[allow(dead_code)] // #2049 Finding 7: superseded by claim_quality_alert_slot_pg.
async fn mark_quality_alert_sent_pg(pool: &PgPool, key: &str, now_ms: i64) -> Result<()> {
    sqlx::query(
        "INSERT INTO kv_meta (key, value)
         VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(key)
    .bind(now_ms.to_string())
    .execute(pool)
    .await
    .map_err(|error| anyhow!("mark quality alert dedupe key {key}: {error}"))?;
    Ok(())
}

/// #2049 Finding 7: atomically claim the dedupe slot for `key` if and only if
/// the previous claim is older than `QUALITY_ALERT_DEDUPE_MS`. Returns `true`
/// when this caller won the race and may proceed to enqueue the outbox row;
/// `false` when another concurrent rollup already claimed the slot. Compresses
/// the prior `quality_alert_recently_sent_pg` + `mark_quality_alert_sent_pg`
/// two-step (TOCTOU) into a single statement using `INSERT ... ON CONFLICT ...
/// WHERE`.
async fn claim_quality_alert_slot_pg(pool: &PgPool, key: &str, now_ms: i64) -> Result<bool> {
    let dedupe_ms = QUALITY_ALERT_DEDUPE_MS;
    // Defensive cast: kv_meta.value is shared by other writers and may hold
    // non-numeric legacy strings (operator scripts, older code). A raw
    // `value::bigint` cast would raise `invalid input syntax for type bigint`
    // and break dedupe entirely for the alert key (#2049 F7 review). Match
    // `^[0-9]+$` first and treat non-numeric values as "no prior claim" so
    // the UPDATE proceeds and writes a fresh numeric stamp.
    let claimed = sqlx::query_scalar::<_, i32>(
        "INSERT INTO kv_meta (key, value)
         VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE
             SET value = EXCLUDED.value
             WHERE CASE
                 WHEN kv_meta.value ~ '^[0-9]+$'
                     THEN kv_meta.value::bigint + $3 <= ($2)::bigint
                 ELSE TRUE
             END
         RETURNING 1",
    )
    .bind(key)
    .bind(now_ms.to_string())
    .bind(dedupe_ms)
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow!("claim quality alert dedupe key {key}: {error}"))?;
    Ok(claimed.is_some())
}

/// #2049 Finding 7: best-effort rollback for `claim_quality_alert_slot_pg`
/// when the subsequent outbox INSERT fails. Deletes the freshly-claimed
/// dedupe row so the next rollup cycle can retry.
async fn release_quality_alert_slot_pg(pool: &PgPool, key: &str) -> Result<()> {
    sqlx::query("DELETE FROM kv_meta WHERE key = $1")
        .bind(key)
        .execute(pool)
        .await
        .map_err(|error| anyhow!("release quality alert dedupe key {key}: {error}"))?;
    Ok(())
}

fn format_rate_for_alert(value: f64) -> String {
    format!("{:.1}%", value * 100.0)
}

fn quality_alert_content(
    agent_id: &str,
    metric_label: &str,
    rate_7d: f64,
    rate_30d: f64,
    sample_7d: i64,
    sample_30d: i64,
) -> String {
    let drop_points = (rate_30d - rate_7d) * 100.0;
    format!(
        "에이전트 품질 회귀 감지: `{agent_id}` {metric_label} 7d {} / 30d {} ({drop_points:.1}%p 하락, sample {sample_7d}/{sample_30d})",
        format_rate_for_alert(rate_7d),
        format_rate_for_alert(rate_30d),
    )
}

async fn enqueue_quality_alert_pg(
    pool: &PgPool,
    target: &str,
    dedupe_key: &str,
    content: &str,
    now_ms: i64,
) -> Result<bool> {
    // #2049 Finding 7: claim the dedupe slot atomically *before* enqueueing the
    // outbox row. Previously (SELECT, ENQUEUE, MARK) could interleave across
    // concurrent rollup callers and double-post the regression alert.
    if !claim_quality_alert_slot_pg(pool, dedupe_key, now_ms).await? {
        return Ok(false);
    }

    let enqueued = match crate::services::message_outbox::enqueue_outbox_pg(
        pool,
        crate::services::message_outbox::OutboxMessage {
            target,
            content,
            bot: "notify",
            source: "agent_quality_rollup",
            reason_code: Some("agent_quality.regression"),
            session_key: Some(dedupe_key),
        },
    )
    .await
    {
        Ok(enqueued) => enqueued,
        Err(error) => {
            // Slot was claimed but outbox INSERT failed: roll back the claim
            // so we don't suppress the next dedupe window.
            if let Err(rollback_err) = release_quality_alert_slot_pg(pool, dedupe_key).await {
                tracing::warn!(
                    "[quality] failed to release dedupe slot {dedupe_key} after outbox error: {rollback_err}"
                );
            }
            return Err(anyhow!("enqueue quality regression alert: {error}"));
        }
    };

    Ok(enqueued)
}

pub(super) async fn enqueue_quality_regression_alerts_pg(pool: &PgPool) -> Result<u64> {
    let Some(target) = quality_alert_target_pg(pool).await? else {
        return Ok(0);
    };

    let rows = sqlx::query(
        "WITH latest AS (
             SELECT DISTINCT ON (agent_id)
                    agent_id,
                    day,
                    turn_success_rate_7d,
                    turn_success_rate_30d,
                    review_pass_rate_7d,
                    review_pass_rate_30d,
                    turn_sample_size_7d,
                    turn_sample_size_30d,
                    review_sample_size_7d,
                    review_sample_size_30d,
                    measurement_unavailable_7d,
                    measurement_unavailable_30d
             FROM agent_quality_daily
             ORDER BY agent_id, day DESC
         )
         SELECT *
         FROM latest
         WHERE measurement_unavailable_7d = FALSE
           AND measurement_unavailable_30d = FALSE
           AND (
               (review_pass_rate_7d IS NOT NULL
                AND review_pass_rate_30d IS NOT NULL
                AND review_pass_rate_30d - review_pass_rate_7d > $1)
            OR (turn_success_rate_7d IS NOT NULL
                AND turn_success_rate_30d IS NOT NULL
                AND turn_success_rate_30d - turn_success_rate_7d > $2)
           )",
    )
    .bind(QUALITY_REVIEW_DROP_THRESHOLD)
    .bind(QUALITY_TURN_DROP_THRESHOLD)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("query quality regression alert candidates: {error}"))?;

    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut alert_count = 0u64;
    for row in rows {
        let agent_id: String = row
            .try_get("agent_id")
            .map_err(|error| anyhow!("decode alert agent_id: {error}"))?;
        let review_7d: Option<f64> = row
            .try_get("review_pass_rate_7d")
            .map_err(|error| anyhow!("decode alert review_pass_rate_7d: {error}"))?;
        let review_30d: Option<f64> = row
            .try_get("review_pass_rate_30d")
            .map_err(|error| anyhow!("decode alert review_pass_rate_30d: {error}"))?;
        let turn_7d: Option<f64> = row
            .try_get("turn_success_rate_7d")
            .map_err(|error| anyhow!("decode alert turn_success_rate_7d: {error}"))?;
        let turn_30d: Option<f64> = row
            .try_get("turn_success_rate_30d")
            .map_err(|error| anyhow!("decode alert turn_success_rate_30d: {error}"))?;
        let review_sample_7d: i64 = row
            .try_get("review_sample_size_7d")
            .map_err(|error| anyhow!("decode alert review_sample_size_7d: {error}"))?;
        let review_sample_30d: i64 = row
            .try_get("review_sample_size_30d")
            .map_err(|error| anyhow!("decode alert review_sample_size_30d: {error}"))?;
        let turn_sample_7d: i64 = row
            .try_get("turn_sample_size_7d")
            .map_err(|error| anyhow!("decode alert turn_sample_size_7d: {error}"))?;
        let turn_sample_30d: i64 = row
            .try_get("turn_sample_size_30d")
            .map_err(|error| anyhow!("decode alert turn_sample_size_30d: {error}"))?;

        if let (Some(rate_7d), Some(rate_30d)) = (review_7d, review_30d)
            && rate_30d - rate_7d > QUALITY_REVIEW_DROP_THRESHOLD
        {
            let key = format!("agent_quality_alert:{agent_id}:review_pass_rate");
            let content = quality_alert_content(
                &agent_id,
                "review pass rate",
                rate_7d,
                rate_30d,
                review_sample_7d,
                review_sample_30d,
            );
            if enqueue_quality_alert_pg(pool, &target, &key, &content, now_ms).await? {
                alert_count = alert_count.saturating_add(1);
            }
        }

        if let (Some(rate_7d), Some(rate_30d)) = (turn_7d, turn_30d)
            && rate_30d - rate_7d > QUALITY_TURN_DROP_THRESHOLD
        {
            let key = format!("agent_quality_alert:{agent_id}:turn_success_rate");
            let content = quality_alert_content(
                &agent_id,
                "turn success rate",
                rate_7d,
                rate_30d,
                turn_sample_7d,
                turn_sample_30d,
            );
            if enqueue_quality_alert_pg(pool, &target, &key, &content, now_ms).await? {
                alert_count = alert_count.saturating_add(1);
            }
        }
    }

    Ok(alert_count)
}
