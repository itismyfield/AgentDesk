//! #2049: public `query_*` façade split out of `mod.rs`. Holds the
//! orchestration logic (filter normalization and fallback chains) — actual SQL
//! lives in `pg_io`. This façade is aggregation-only; regression alerting is
//! owned by `services::agent_quality::regression_alerts`.

use anyhow::{Result, anyhow};
use sqlx::PgPool;

use super::helpers::{
    normalized_quality_daily_limit, normalized_quality_days, normalized_quality_ranking_limit,
    now_kst,
};
use super::pg_io::{
    pick_ranking_metric_value, query_agent_quality_daily_pg, query_agent_quality_ranking_pg,
    ranking_window_sample_size, synth_agent_quality_daily_from_events_pg,
    upsert_agent_quality_daily_pg,
};
use super::{
    AgentQualityDailyRecord, AgentQualityRankingEntry, AgentQualityRankingResponse,
    AgentQualityRollupReport, AgentQualitySummary, QualityRankingMetric, QualityRankingWindow,
};

pub async fn run_agent_quality_rollup_pg(pool: &PgPool) -> Result<AgentQualityRollupReport> {
    let upserted_rows = upsert_agent_quality_daily_pg(pool).await?;
    Ok(AgentQualityRollupReport {
        upserted_rows,
        // Kept for API compatibility with callers compiled against the old
        // rollup report. The rollup no longer owns an alert producer; the
        // scheduler invokes `quality_regression_alerter` after this job.
        alert_count: 0,
    })
}

pub async fn query_agent_quality_summary(
    pg_pool: Option<&PgPool>,
    agent_id: &str,
    days: i64,
    limit: usize,
) -> Result<AgentQualitySummary> {
    let days = normalized_quality_days(days);
    let limit = normalized_quality_daily_limit(limit);
    let Some(pool) = pg_pool else {
        return Err(anyhow!(
            "postgres pool unavailable for agent quality summary"
        ));
    };
    let daily = match query_agent_quality_daily_pg(pool, Some(agent_id), days, limit).await {
        Ok(records) => records,
        Err(error) => {
            tracing::warn!("[quality] postgres daily query failed: {error}");
            Vec::new()
        }
    };

    // #1102 fallback: when the daily rollup is empty (e.g. the rollup job
    // from #1101 hasn't run yet in this environment), synthesize a mini
    // rollup directly from `agent_quality_event`.
    let (daily, fallback_from_events) = if daily.is_empty() {
        let synthetic = synth_agent_quality_daily_from_events_pg(pool, agent_id, 30)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!("[quality] pg event-fallback mini-rollup failed: {error}");
                Vec::new()
            });
        let fallback = !synthetic.is_empty();
        (synthetic, fallback)
    } else {
        (daily, false)
    };

    let trend_7d = slice_daily_trend(&daily, 7);
    let trend_30d = slice_daily_trend(&daily, 30);
    let latest = daily.first().cloned();

    Ok(AgentQualitySummary {
        generated_at: now_kst(),
        agent_id: agent_id.to_string(),
        current: latest.clone(),
        latest,
        daily,
        trend_7d,
        trend_30d,
        fallback_from_events,
    })
}

fn slice_daily_trend(
    daily: &[AgentQualityDailyRecord],
    max_days: usize,
) -> Vec<AgentQualityDailyRecord> {
    daily.iter().take(max_days).cloned().collect()
}

pub async fn query_agent_quality_ranking_with(
    pg_pool: Option<&PgPool>,
    limit: usize,
    metric: QualityRankingMetric,
    window: QualityRankingWindow,
    min_sample_size: i64,
) -> Result<AgentQualityRankingResponse> {
    let limit = normalized_quality_ranking_limit(limit);
    let min_sample_size = min_sample_size.max(0);
    let Some(pool) = pg_pool else {
        return Err(anyhow!(
            "postgres pool unavailable for agent quality ranking"
        ));
    };
    let agents = match query_agent_quality_ranking_pg(pool, limit).await {
        Ok(records) => records,
        Err(error) => {
            tracing::warn!("[quality] postgres ranking query failed: {error}");
            Vec::new()
        }
    };

    // Filter by sample_size >= min_sample_size (#1102 DoD), then attach
    // metric_value and re-rank by the requested (metric, window) pair.
    let mut filtered: Vec<AgentQualityRankingEntry> = agents
        .into_iter()
        .filter(|entry| ranking_window_sample_size(entry, window) >= min_sample_size)
        .map(|mut entry| {
            entry.metric_value = pick_ranking_metric_value(&entry, metric, window);
            entry
        })
        .collect();

    // Sort by the chosen metric desc, NULLs last; tiebreak on sample size desc,
    // then agent_id asc for determinism.
    filtered.sort_by(|a, b| {
        let av = a.metric_value;
        let bv = b.metric_value;
        match (av, bv) {
            (Some(ax), Some(bx)) => bx
                .partial_cmp(&ax)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    ranking_window_sample_size(b, window)
                        .cmp(&ranking_window_sample_size(a, window))
                })
                .then_with(|| a.agent_id.cmp(&b.agent_id)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.agent_id.cmp(&b.agent_id),
        }
    });

    // Re-rank 1..n after filtering/sorting.
    for (idx, entry) in filtered.iter_mut().enumerate() {
        entry.rank = (idx + 1) as i64;
    }

    Ok(AgentQualityRankingResponse {
        generated_at: now_kst(),
        metric: metric.label().to_string(),
        window: window.label().to_string(),
        min_sample_size,
        agents: filtered,
    })
}

#[cfg(test)]
mod alert_authority_tests {
    use super::*;

    #[tokio::test]
    async fn rollup_is_aggregation_only_and_rule_engine_is_sole_alerter_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_quality_alert_authority",
            "agent quality alert authority test",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        sqlx::query(
            "INSERT INTO agent_quality_daily
                (agent_id, day,
                 turn_success_rate_7d, turn_success_rate_30d,
                 turn_sample_size_7d, turn_sample_size_30d,
                 measurement_unavailable_7d, measurement_unavailable_30d)
             VALUES ('agent-4448', CURRENT_DATE, 0.50, 0.90, 20, 40, FALSE, FALSE)",
        )
        .execute(&pool)
        .await
        .expect("seed regression-shaped daily quality row");
        sqlx::query(
            "INSERT INTO kv_meta (key, value)
             VALUES ('agent_quality_monitoring_channel_id', 'quality-alerts')",
        )
        .execute(&pool)
        .await
        .expect("configure the canonical alerter target");

        let report = run_agent_quality_rollup_pg(&pool)
            .await
            .expect("run aggregation-only quality rollup");
        assert_eq!(report.alert_count, 0);
        let after_rollup: i64 = sqlx::query_scalar("SELECT COUNT(*)::bigint FROM message_outbox")
            .fetch_one(&pool)
            .await
            .expect("count outbox rows after rollup");
        assert_eq!(after_rollup, 0, "rollup must not enqueue regression alerts");

        let sent =
            crate::services::agent_quality::regression_alerts::run_regression_alerter_pg(&pool)
                .await
                .expect("run canonical regression alerter");
        assert_eq!(sent, 1);
        let row: (String, String) = sqlx::query_as(
            "SELECT source, reason_code
               FROM message_outbox
              WHERE source = 'quality_regression_alerter'",
        )
        .fetch_one(&pool)
        .await
        .expect("load canonical quality alert row");
        assert_eq!(
            row,
            (
                "quality_regression_alerter".to_string(),
                "agent_quality.regression".to_string(),
            )
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
