use serde::Serialize;
use sqlx::PgPool;

use super::issue_body::build_issue_body_pg;
use super::patterns::{
    API_FRICTION_MIN_REPEAT_COUNT, ApiFrictionPattern, DEFAULT_PATTERN_LIMIT,
    load_pattern_candidates_pg,
};
use crate::github::CreatedIssue;
use crate::services::github_issue_creation::{
    GitHubIssueCreateRequest, GitHubIssueCreationResult, IssueAnnouncementSyncOutcome,
    IssueCreationMode, KanbanCardSyncOutcome, create_github_issue_with_side_effects,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ProcessedApiFrictionIssue {
    pub fingerprint: String,
    pub endpoint: String,
    pub friction_type: String,
    pub repo_id: String,
    pub event_count: usize,
    pub issue_number: i64,
    pub issue_url: String,
    pub issue_creation_mode: IssueCreationMode,
    pub kanban_card_sync: KanbanCardSyncOutcome,
    pub announcement_sync: IssueAnnouncementSyncOutcome,
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

    let issue_create_request = GitHubIssueCreateRequest::github_only(
        "api_friction_issue_processing",
        pattern.repo_id.clone(),
        issue_title.clone(),
        issue_body.clone(),
        "API-friction issue processing persists issue_url in api_friction_issues and intentionally skips kanban/announcement sync",
    );

    match create_github_issue_with_side_effects(Some(pg_pool), issue_create_request).await {
        Ok(result) => {
            upsert_created_issue_pg(pg_pool, &pattern, &issue_title, &issue_body, &result.issue)
                .await?;
            Ok(ApiFrictionPatternProcessResult::Created(processed_issue(
                &pattern, result,
            )))
        }
        Err(error) => {
            let error = error.to_string();
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

fn processed_issue(
    pattern: &ApiFrictionPattern,
    result: GitHubIssueCreationResult,
) -> ProcessedApiFrictionIssue {
    let issue = result.issue;
    ProcessedApiFrictionIssue {
        fingerprint: pattern.fingerprint.clone(),
        endpoint: pattern.endpoint.clone(),
        friction_type: pattern.friction_type.clone(),
        repo_id: pattern.repo_id.clone(),
        event_count: pattern.event_count,
        issue_number: issue.number,
        issue_url: issue.url,
        issue_creation_mode: result.mode,
        kanban_card_sync: result.kanban,
        announcement_sync: result.announcement,
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

#[cfg(test)]
mod retention_pg_tests {
    use super::*;

    async fn exec(pool: &PgPool, sql: &str) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
            .execute(pool)
            .await
            .unwrap_or_else(|err| panic!("fixture `{sql}`: {err}"));
    }

    async fn issue_url_of(pool: &PgPool, fingerprint: &str) -> Option<String> {
        load_pattern_candidates_pg(pool, API_FRICTION_MIN_REPEAT_COUNT, DEFAULT_PATTERN_LIMIT)
            .await
            .expect("pattern candidates")
            .into_iter()
            .find(|pattern| pattern.fingerprint == fingerprint)
            .and_then(|pattern| pattern.issue_url)
    }

    /// An issue row goes only when no event of its fingerprint is left and its last record is
    /// a year old, so a friction that keeps recurring is never filed as a second issue.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retention_keeps_the_issue_of_a_recurring_friction() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_api_friction_issue_retention",
            "api friction issue retention",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        // `fp-recurring` was filed 400 days ago and its friction is still being reported.
        exec(
            &pool,
            "INSERT INTO api_friction_issues (fingerprint, repo_id, endpoint, friction_type, title,
                 body, issue_url, last_event_at, updated_at, created_at) VALUES
             ('fp-gone', 'r', '/x', 't', 't', 'b', 'u-gone',
                 NOW() - INTERVAL '400 days', NOW() - INTERVAL '400 days', NOW() - INTERVAL '400 days'),
             ('fp-updated-only', 'r', '/x', 't', 't', 'b', 'u-upd',
                 NULL, NOW() - INTERVAL '400 days', NOW() - INTERVAL '10 days'),
             ('fp-recurring', 'r', '/x', 't', 't', 'b', 'u-recurring',
                 NOW() - INTERVAL '400 days', NOW() - INTERVAL '400 days', NOW() - INTERVAL '400 days'),
             ('fp-recent', 'r', '/x', 't', 't', 'b', 'u-recent',
                 NOW() - INTERVAL '100 days', NOW() - INTERVAL '100 days', NOW() - INTERVAL '100 days'),
             ('fp-last-event-new', 'r', '/x', 't', 't', 'b', 'u-new',
                 NOW() - INTERVAL '10 days', NOW() - INTERVAL '400 days', NOW() - INTERVAL '400 days'),
             ('fp-no-time', 'r', '/x', 't', 't', 'b', 'u-none', NULL, NULL, NULL);
             INSERT INTO api_friction_events (id, fingerprint, endpoint, friction_type, summary,
                 payload_json, created_at) VALUES
             ('e1', 'fp-recurring', '/x', 't', 's', '{}'::jsonb, NOW() - INTERVAL '3 days'),
             ('e2', 'fp-recurring', '/x', 't', 's', '{}'::jsonb, NOW() - INTERVAL '2 days')",
        )
        .await;
        let before = issue_url_of(&pool, "fp-recurring").await;
        assert_eq!(before.as_deref(), Some("u-recurring"));

        let report =
            crate::services::maintenance::jobs::db_retention::db_retention_job(&pool, false)
                .await
                .expect("retention pass");
        assert_eq!(
            issue_url_of(&pool, "fp-recurring").await,
            before,
            "the recurring friction's pattern still sees its issue"
        );
        // The friction recurs again; each processing pass still skips it instead of refiling.
        for (round, id) in [(1, "e3"), (2, "e4")] {
            exec(
                &pool,
                &format!(
                    "INSERT INTO api_friction_events (id, fingerprint, endpoint, friction_type,
                         summary, payload_json) VALUES ('{id}', 'fp-recurring', '/x', 't', 's',
                         '{{}}'::jsonb)"
                ),
            )
            .await;
            let pattern = load_pattern_candidates_pg(&pool, 2, DEFAULT_PATTERN_LIMIT)
                .await
                .expect("pattern candidates")
                .into_iter()
                .find(|pattern| pattern.fingerprint == "fp-recurring")
                .expect("recurring pattern");
            // Checked before processing so a regression cannot reach the real GitHub call.
            assert!(
                pattern.issue_url.is_some(),
                "pass {round}: the dedupe record is still there"
            );
            assert!(matches!(
                process_pattern_issue_pg(&pool, pattern).await,
                Ok(ApiFrictionPatternProcessResult::Skipped(_))
            ));
        }
        let left: Vec<String> =
            sqlx::query_scalar("SELECT fingerprint FROM api_friction_issues ORDER BY 1")
                .fetch_all(&pool)
                .await
                .expect("issues left");
        assert_eq!(
            left,
            [
                "fp-last-event-new",
                "fp-no-time",
                "fp-recent",
                "fp-recurring"
            ]
            .map(String::from),
            "only year-old issues with no event left go"
        );
        assert!(
            report
                .summary()
                .contains(&"api_friction_issues:delete=2".to_string()),
            "the pass actually deleted the two eligible issues"
        );

        pool.close().await;
        db.drop().await;
    }
}
