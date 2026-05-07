use serde::Serialize;
use sqlx::PgPool;

use super::patterns::{
    API_FRICTION_MIN_REPEAT_COUNT, ApiFrictionPattern, DEFAULT_PATTERN_LIMIT,
    load_pattern_candidates_pg,
};
use crate::github::CreatedIssue;

const MAX_ISSUE_EVIDENCE_ITEMS: usize = 5;

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

async fn build_issue_body_pg(
    pg_pool: &PgPool,
    pattern: &ApiFrictionPattern,
) -> Result<String, String> {
    let evidence = load_pattern_evidence_pg(pg_pool, &pattern.fingerprint).await?;
    build_issue_body_from_evidence(pattern, evidence)
}

fn build_issue_body_from_evidence(
    pattern: &ApiFrictionPattern,
    evidence: Vec<PatternEvidence>,
) -> Result<String, String> {
    let mut lines = vec![
        "## Summary".to_string(),
        format!("- Endpoint/Surface: `{}`", pattern.endpoint),
        format!("- Friction type: `{}`", pattern.friction_type),
        format!("- Repeated count: {}", pattern.event_count),
    ];
    if let Some(docs_category) = pattern.docs_category.as_deref() {
        lines.push(format!("- Docs category: `{docs_category}`"));
    }
    if let Some(task_summary) = pattern.task_summary.as_deref() {
        lines.push(format!("- Latest task: {}", task_summary));
    }

    lines.extend([
        String::new(),
        "## Friction Pattern".to_string(),
        format!("- Summary: {}", pattern.summary),
        format!(
            "- Workaround: {}",
            pattern.workaround.as_deref().unwrap_or("not provided")
        ),
        format!(
            "- Proposed improvement: {}",
            pattern
                .suggested_fix
                .as_deref()
                .unwrap_or("Provide a clearer single API path or docs entry")
        ),
        String::new(),
        "## Evidence".to_string(),
    ]);

    if evidence.is_empty() {
        lines.push("- No card-linked evidence was captured.".to_string());
    } else {
        for item in evidence {
            let mut parts = Vec::new();
            if let Some(repo_id) = item.repo_id.as_deref() {
                if let Some(issue_number) = item.issue_number {
                    parts.push(format!("{repo_id}#{issue_number}"));
                } else {
                    parts.push(repo_id.to_string());
                }
            } else if let Some(card_id) = item.card_id.as_deref() {
                parts.push(format!("card {card_id}"));
            }
            if let Some(dispatch_id) = item.dispatch_id.as_deref() {
                parts.push(format!("dispatch {dispatch_id}"));
            }
            if parts.is_empty() {
                parts.push("runtime observation".to_string());
            }
            lines.push(format!("- {}: {}", parts.join(", "), item.summary));
        }
    }

    lines.extend([
        String::new(),
        "## Suggested Next Step".to_string(),
        "- Add or clarify the canonical `/api` endpoint/docs path so agents do not need trial-and-error or DB bypass.".to_string(),
    ]);

    Ok(lines.join("\n"))
}

#[derive(Clone, Debug)]
struct PatternEvidence {
    repo_id: Option<String>,
    issue_number: Option<i64>,
    card_id: Option<String>,
    dispatch_id: Option<String>,
    summary: String,
}

async fn load_pattern_evidence_pg(
    pg_pool: &PgPool,
    fingerprint: &str,
) -> Result<Vec<PatternEvidence>, String> {
    sqlx::query_as::<
        _,
        (
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<String>,
            String,
        ),
    >(
        "SELECT repo_id, github_issue_number::BIGINT, card_id, dispatch_id, summary
         FROM api_friction_events
         WHERE fingerprint = $1
         ORDER BY created_at DESC, id DESC
         LIMIT $2",
    )
    .bind(fingerprint)
    .bind(MAX_ISSUE_EVIDENCE_ITEMS as i64)
    .fetch_all(pg_pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(
                |(repo_id, issue_number, card_id, dispatch_id, summary)| PatternEvidence {
                    repo_id,
                    issue_number,
                    card_id,
                    dispatch_id,
                    summary,
                },
            )
            .collect()
    })
    .map_err(|err| format!("query api_friction evidence: {err}"))
}
