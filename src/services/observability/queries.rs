//! #2049: public `query_*` façade split out of `mod.rs`. Holds the
//! orchestration logic (filter normalization, live counter fold-in,
//! fallback chains) — actual SQL lives in `pg_io`, alert pipeline in
//! `quality_alert`. Public surface unchanged.

use std::collections::HashSet;

use anyhow::{Result, anyhow};
use sqlx::PgPool;

use super::helpers::{
    counter_snapshot_from_values, normalized_counter_limit, normalized_event_limit,
    normalized_invariant_limit, normalized_quality_daily_limit, normalized_quality_days,
    normalized_quality_limit, normalized_quality_ranking_limit, now_kst,
};
use super::pg_io::{
    pick_ranking_metric_value, query_agent_quality_daily_pg, query_agent_quality_events_pg,
    query_agent_quality_ranking_pg, query_counter_snapshots_pg, query_events_pg,
    query_invariant_counts_pg, query_invariant_events_pg, ranking_window_sample_size,
    synth_agent_quality_daily_from_events_pg, upsert_agent_quality_daily_pg,
};
use super::quality_alert::enqueue_quality_regression_alerts_pg;
use super::worker::snapshot_rows;
use super::{
    AgentQualityDailyRecord, AgentQualityEventRecord, AgentQualityFilters,
    AgentQualityRankingEntry, AgentQualityRankingResponse, AgentQualityRollupReport,
    AgentQualitySummary, AnalyticsCounterSnapshot, AnalyticsEventRecord, AnalyticsFilters,
    AnalyticsResponse, InvariantAnalyticsFilters, InvariantAnalyticsResponse,
    InvariantViolationCount, InvariantViolationRecord, QUALITY_SAMPLE_GUARD, QualityRankingMetric,
    QualityRankingWindow, runtime,
};

pub async fn query_analytics(
    pg_pool: Option<&PgPool>,
    filters: &AnalyticsFilters,
) -> Result<AnalyticsResponse> {
    let event_limit = normalized_event_limit(filters.event_limit);
    let counter_limit = normalized_counter_limit(filters.counter_limit);
    let live_counter_rows = snapshot_rows(&runtime(), Some(filters))
        .into_iter()
        .take(counter_limit)
        .collect::<Vec<_>>();
    let mut counters = live_counter_rows
        .into_iter()
        .map(|row| {
            counter_snapshot_from_values(
                &row.provider,
                &row.channel_id,
                row.values,
                "live",
                now_kst(),
            )
        })
        .collect::<Vec<_>>();

    let persisted_counters = query_counter_snapshots_db(pg_pool, filters, counter_limit).await?;
    let mut seen = counters
        .iter()
        .map(|snapshot| (snapshot.provider.clone(), snapshot.channel_id.clone()))
        .collect::<HashSet<_>>();
    for snapshot in persisted_counters {
        if seen.insert((snapshot.provider.clone(), snapshot.channel_id.clone())) {
            counters.push(snapshot);
        }
    }
    counters.truncate(counter_limit);

    let events = query_events_db(pg_pool, filters, event_limit).await?;
    Ok(AnalyticsResponse {
        generated_at: now_kst(),
        counters,
        events,
    })
}

pub async fn query_agent_quality_events(
    pg_pool: Option<&PgPool>,
    filters: &AgentQualityFilters,
) -> Result<Vec<AgentQualityEventRecord>> {
    let days = normalized_quality_days(filters.days);
    let limit = normalized_quality_limit(filters.limit);
    let Some(pool) = pg_pool else {
        return Err(anyhow!(
            "postgres pool unavailable for agent quality events"
        ));
    };
    query_agent_quality_events_pg(pool, filters.agent_id.as_deref(), days, limit).await
}

pub async fn run_agent_quality_rollup_pg(pool: &PgPool) -> Result<AgentQualityRollupReport> {
    let upserted_rows = upsert_agent_quality_daily_pg(pool).await?;
    let alert_count = enqueue_quality_regression_alerts_pg(pool).await?;
    Ok(AgentQualityRollupReport {
        upserted_rows,
        alert_count,
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

pub async fn query_agent_quality_ranking(
    pg_pool: Option<&PgPool>,
    limit: usize,
) -> Result<AgentQualityRankingResponse> {
    query_agent_quality_ranking_with(
        pg_pool,
        limit,
        QualityRankingMetric::TurnSuccessRate,
        QualityRankingWindow::Seven,
        QUALITY_SAMPLE_GUARD,
    )
    .await
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

pub async fn query_invariant_analytics(
    pg_pool: Option<&PgPool>,
    filters: &InvariantAnalyticsFilters,
) -> Result<InvariantAnalyticsResponse> {
    let limit = normalized_invariant_limit(filters.limit);
    let counts = query_invariant_counts_db(pg_pool, filters).await?;
    let total_violations = counts.iter().map(|count| count.count).sum();
    let recent = query_invariant_events_db(pg_pool, filters, limit).await?;

    Ok(InvariantAnalyticsResponse {
        generated_at: now_kst(),
        total_violations,
        counts,
        recent,
    })
}

async fn query_invariant_counts_db(
    pg_pool: Option<&PgPool>,
    filters: &InvariantAnalyticsFilters,
) -> Result<Vec<InvariantViolationCount>> {
    let Some(pool) = pg_pool else {
        return Err(anyhow!("postgres pool unavailable for invariant counts"));
    };
    query_invariant_counts_pg(pool, filters).await
}

async fn query_invariant_events_db(
    pg_pool: Option<&PgPool>,
    filters: &InvariantAnalyticsFilters,
    limit: usize,
) -> Result<Vec<InvariantViolationRecord>> {
    let Some(pool) = pg_pool else {
        return Err(anyhow!("postgres pool unavailable for invariant events"));
    };
    query_invariant_events_pg(pool, filters, limit).await
}

async fn query_events_db(
    pg_pool: Option<&PgPool>,
    filters: &AnalyticsFilters,
    limit: usize,
) -> Result<Vec<AnalyticsEventRecord>> {
    let Some(pool) = pg_pool else {
        return Ok(Vec::new());
    };
    query_events_pg(pool, filters, limit).await
}

async fn query_counter_snapshots_db(
    pg_pool: Option<&PgPool>,
    filters: &AnalyticsFilters,
    limit: usize,
) -> Result<Vec<AnalyticsCounterSnapshot>> {
    let Some(pool) = pg_pool else {
        return Ok(Vec::new());
    };
    query_counter_snapshots_pg(pool, filters, limit).await
}
