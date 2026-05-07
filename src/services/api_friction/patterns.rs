use serde::Serialize;
use sqlx::PgPool;

use super::storage::DEFAULT_API_FRICTION_REPO;
use crate::utils::api::clamp_api_limit;

pub(super) const API_FRICTION_MIN_REPEAT_COUNT: usize = 2;
pub(super) const DEFAULT_PATTERN_LIMIT: usize = 20;

type PatternAggregateRow = (
    String,
    i64,
    String,
    String,
    Option<i64>,
    Option<String>,
    Option<String>,
);

type LatestPatternEventRow = (
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
);

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ApiFrictionPattern {
    pub fingerprint: String,
    pub endpoint: String,
    pub friction_type: String,
    pub docs_category: Option<String>,
    pub summary: String,
    pub workaround: Option<String>,
    pub suggested_fix: Option<String>,
    pub repo_id: String,
    pub event_count: usize,
    pub first_seen_at: String,
    pub last_seen_at: String,
    pub task_summary: Option<String>,
    pub github_issue_number: Option<i64>,
    pub issue_url: Option<String>,
    pub last_error: Option<String>,
}

pub(super) async fn load_pattern_candidates_pg(
    pg_pool: &PgPool,
    min_events: usize,
    limit: usize,
) -> Result<Vec<ApiFrictionPattern>, String> {
    let min_events = min_events.max(API_FRICTION_MIN_REPEAT_COUNT) as i64;
    let limit = clamp_api_limit(Some(limit)) as i64;
    let rows = sqlx::query_as::<_, PatternAggregateRow>(
        "SELECT e.fingerprint,
                COUNT(*)::BIGINT AS event_count,
                MIN(e.created_at)::TEXT AS first_seen_at,
                MAX(e.created_at)::TEXT AS last_seen_at,
                i.issue_number::BIGINT,
                i.issue_url,
                i.last_error
         FROM api_friction_events e
         LEFT JOIN api_friction_issues i
           ON i.fingerprint = e.fingerprint
         GROUP BY e.fingerprint, i.issue_number, i.issue_url, i.last_error
         HAVING COUNT(*) >= $1
         ORDER BY event_count DESC, last_seen_at DESC
         LIMIT $2",
    )
    .bind(min_events)
    .bind(limit)
    .fetch_all(pg_pool)
    .await
    .map_err(|err| format!("query api_friction pattern aggregate: {err}"))?;

    let mut patterns = Vec::with_capacity(rows.len());
    for aggregate in rows {
        let latest = load_latest_pattern_event_pg(pg_pool, &aggregate.0).await?;
        patterns.push(map_pattern_candidate_row(aggregate, latest));
    }

    Ok(patterns)
}

async fn load_latest_pattern_event_pg(
    pg_pool: &PgPool,
    fingerprint: &str,
) -> Result<LatestPatternEventRow, String> {
    sqlx::query_as::<_, LatestPatternEventRow>(
        "SELECT endpoint,
                    friction_type,
                    docs_category,
                    summary,
                    workaround,
                    suggested_fix,
                    COALESCE(repo_id, $2),
                    task_summary
             FROM api_friction_events
             WHERE fingerprint = $1
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
    )
    .bind(fingerprint)
    .bind(DEFAULT_API_FRICTION_REPO)
    .fetch_one(pg_pool)
    .await
    .map_err(|err| format!("load latest api_friction pattern row: {err}"))
}

fn map_pattern_candidate_row(
    aggregate: PatternAggregateRow,
    latest: LatestPatternEventRow,
) -> ApiFrictionPattern {
    let (
        fingerprint,
        event_count,
        first_seen_at,
        last_seen_at,
        issue_number,
        issue_url,
        last_error,
    ) = aggregate;
    let (
        endpoint,
        friction_type,
        docs_category,
        summary,
        workaround,
        suggested_fix,
        repo_id,
        task_summary,
    ) = latest;

    ApiFrictionPattern {
        fingerprint,
        endpoint,
        friction_type,
        docs_category,
        summary,
        workaround,
        suggested_fix,
        repo_id,
        event_count: event_count as usize,
        first_seen_at,
        last_seen_at,
        task_summary,
        github_issue_number: issue_number,
        issue_url,
        last_error,
    }
}
