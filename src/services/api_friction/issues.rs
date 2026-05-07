use serde::Serialize;
use sqlx::PgPool;

use super::issue_body::build_issue_body_pg;
use super::patterns::{
    API_FRICTION_MIN_REPEAT_COUNT, ApiFrictionPattern, DEFAULT_PATTERN_LIMIT,
    load_pattern_candidates_pg,
};
use crate::github::CreatedIssue;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ProcessedApiFrictionIssue {
    pub fingerprint: String,
    pub endpoint: String,
    pub friction_type: String,
    pub repo_id: String,
    pub event_count: usize,
    pub issue_number: i64,
    pub issue_url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct ApiFrictionProcessSummary {
    pub processed_patterns: usize,
    pub created_issues: Vec<ProcessedApiFrictionIssue>,
    pub skipped_patterns: Vec<ApiFrictionPattern>,
    pub failed_patterns: Vec<ApiFrictionPatternFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ApiFrictionPatternFailure {
    pub fingerprint: String,
    pub endpoint: String,
    pub friction_type: String,
    pub repo_id: String,
    pub error: String,
}

enum ApiFrictionPatternProcessResult {
    Created(ProcessedApiFrictionIssue),
    Skipped(ApiFrictionPattern),
    Failed(ApiFrictionPatternFailure),
}

pub(crate) async fn process_api_friction_patterns(
    pg_pool: Option<&PgPool>,
    min_events: Option<usize>,
    limit: Option<usize>,
) -> Result<ApiFrictionProcessSummary, String> {
    let pg_pool = pg_pool.ok_or_else(|| {
        "postgres pool is required for API friction processing; sqlite fallback is unavailable"
            .to_string()
    })?;
    let patterns = load_pattern_candidates_pg(
        pg_pool,
        min_events.unwrap_or(API_FRICTION_MIN_REPEAT_COUNT),
        limit.unwrap_or(DEFAULT_PATTERN_LIMIT),
    )
    .await?;

    let mut summary = ApiFrictionProcessSummary {
        processed_patterns: patterns.len(),
        ..ApiFrictionProcessSummary::default()
    };

    for pattern in patterns {
        match process_pattern_issue_pg(pg_pool, pattern).await? {
            ApiFrictionPatternProcessResult::Created(issue) => {
                summary.created_issues.push(issue);
            }
            ApiFrictionPatternProcessResult::Skipped(pattern) => {
                summary.skipped_patterns.push(pattern);
            }
            ApiFrictionPatternProcessResult::Failed(failure) => {
                summary.failed_patterns.push(failure);
            }
        }
    }

    Ok(summary)
}

async fn process_pattern_issue_pg(
    pg_pool: &PgPool,
    pattern: ApiFrictionPattern,
) -> Result<ApiFrictionPatternProcessResult, String> {
    if pattern
        .issue_url
        .as_deref()
        .is_some_and(|value| !value.is_empty())
    {
        return Ok(ApiFrictionPatternProcessResult::Skipped(pattern));
    }

    let issue_title = issue_title(&pattern);
    let issue_body = build_issue_body_pg(pg_pool, &pattern).await?;

    match crate::github::create_issue(&pattern.repo_id, &issue_title, &issue_body).await {
        Ok(issue) => {
            upsert_created_issue_pg(pg_pool, &pattern, &issue_title, &issue_body, &issue).await?;
            Ok(ApiFrictionPatternProcessResult::Created(processed_issue(
                &pattern, issue,
            )))
        }
        Err(error) => {
            record_issue_creation_failure_pg(pg_pool, &pattern, &issue_title, &issue_body, &error)
                .await?;
            Ok(ApiFrictionPatternProcessResult::Failed(pattern_failure(
                &pattern, error,
            )))
        }
    }
}

fn issue_title(pattern: &ApiFrictionPattern) -> String {
    format!(
        "api-friction: {} — {}",
        pattern.endpoint, pattern.friction_type
    )
}

async fn upsert_created_issue_pg(
    pg_pool: &PgPool,
    pattern: &ApiFrictionPattern,
    issue_title: &str,
    issue_body: &str,
    issue: &CreatedIssue,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO api_friction_issues (
                fingerprint, repo_id, endpoint, friction_type, title, body, issue_number,
                issue_url, event_count, first_event_at, last_event_at, last_error,
                created_at, updated_at
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7,
                $8, $9, $10::timestamptz, $11::timestamptz, NULL,
                NOW(), NOW()
             )
             ON CONFLICT(fingerprint) DO UPDATE SET
                repo_id = excluded.repo_id,
                endpoint = excluded.endpoint,
                friction_type = excluded.friction_type,
                title = excluded.title,
                body = excluded.body,
                issue_number = excluded.issue_number,
                issue_url = excluded.issue_url,
                event_count = excluded.event_count,
                first_event_at = excluded.first_event_at,
                last_event_at = excluded.last_event_at,
                last_error = NULL,
                updated_at = NOW()",
    )
    .bind(&pattern.fingerprint)
    .bind(&pattern.repo_id)
    .bind(&pattern.endpoint)
    .bind(&pattern.friction_type)
    .bind(issue_title)
    .bind(issue_body)
    .bind(issue_number_i32(issue.number)?)
    .bind(&issue.url)
    .bind(event_count_i32(pattern.event_count)?)
    .bind(&pattern.first_seen_at)
    .bind(&pattern.last_seen_at)
    .execute(pg_pool)
    .await
    .map(|_| ())
    .map_err(|err| format!("upsert api_friction_issues: {err}"))
}

async fn record_issue_creation_failure_pg(
    pg_pool: &PgPool,
    pattern: &ApiFrictionPattern,
    issue_title: &str,
    issue_body: &str,
    error: &str,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO api_friction_issues (
                fingerprint, repo_id, endpoint, friction_type, title, body, issue_number,
                issue_url, event_count, first_event_at, last_event_at, last_error,
                created_at, updated_at
             ) VALUES (
                $1, $2, $3, $4, $5, $6, NULL,
                NULL, $7, $8::timestamptz, $9::timestamptz, $10,
                NOW(), NOW()
             )
             ON CONFLICT(fingerprint) DO UPDATE SET
                repo_id = excluded.repo_id,
                endpoint = excluded.endpoint,
                friction_type = excluded.friction_type,
                title = excluded.title,
                body = excluded.body,
                event_count = excluded.event_count,
                first_event_at = excluded.first_event_at,
                last_event_at = excluded.last_event_at,
                last_error = excluded.last_error,
                updated_at = NOW()",
    )
    .bind(&pattern.fingerprint)
    .bind(&pattern.repo_id)
    .bind(&pattern.endpoint)
    .bind(&pattern.friction_type)
    .bind(issue_title)
    .bind(issue_body)
    .bind(event_count_i32(pattern.event_count)?)
    .bind(&pattern.first_seen_at)
    .bind(&pattern.last_seen_at)
    .bind(error)
    .execute(pg_pool)
    .await
    .map(|_| ())
    .map_err(|err| format!("record api_friction_issues failure: {err}"))
}

fn issue_number_i32(issue_number: i64) -> Result<i32, String> {
    i32::try_from(issue_number)
        .map_err(|_| format!("github issue number exceeds postgres integer: {issue_number}"))
}

fn event_count_i32(event_count: usize) -> Result<i32, String> {
    i32::try_from(event_count)
        .map_err(|_| format!("api_friction event_count exceeds postgres integer: {event_count}"))
}

fn processed_issue(pattern: &ApiFrictionPattern, issue: CreatedIssue) -> ProcessedApiFrictionIssue {
    ProcessedApiFrictionIssue {
        fingerprint: pattern.fingerprint.clone(),
        endpoint: pattern.endpoint.clone(),
        friction_type: pattern.friction_type.clone(),
        repo_id: pattern.repo_id.clone(),
        event_count: pattern.event_count,
        issue_number: issue.number,
        issue_url: issue.url,
    }
}

fn pattern_failure(pattern: &ApiFrictionPattern, error: String) -> ApiFrictionPatternFailure {
    ApiFrictionPatternFailure {
        fingerprint: pattern.fingerprint.clone(),
        endpoint: pattern.endpoint.clone(),
        friction_type: pattern.friction_type.clone(),
        repo_id: pattern.repo_id.clone(),
        error,
    }
}
