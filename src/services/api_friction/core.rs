use serde::Serialize;
use sqlx::PgPool;

use super::markers::ApiFrictionReport;
use super::storage::{
    DEFAULT_API_FRICTION_REPO, mark_event_memory_status_pg, store_api_friction_events_pg,
};
use crate::db::Db;
use crate::services::discord::settings::{
    MemoryBackendKind, ResolvedMemorySettings, resolve_memory_settings,
};
use crate::services::memory::{MementoBackend, TokenUsage};
use crate::utils::api::clamp_api_limit;

pub(super) const API_FRICTION_MIN_REPEAT_COUNT: usize = 2;
pub(super) const DEFAULT_PATTERN_LIMIT: usize = 20;
const MAX_ISSUE_EVIDENCE_ITEMS: usize = 5;

#[derive(Clone, Debug)]
pub(crate) struct ApiFrictionRecordContext<'a> {
    pub channel_id: u64,
    pub session_key: Option<&'a str>,
    pub dispatch_id: Option<&'a str>,
    pub provider: &'a str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ApiFrictionRecordResult {
    pub stored_event_count: usize,
    pub memory_stored_count: usize,
    pub memory_errors: Vec<String>,
    pub token_usage: TokenUsage,
}

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

pub(crate) async fn record_api_friction_reports(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    memory_settings: &ResolvedMemorySettings,
    context: ApiFrictionRecordContext<'_>,
    reports: &[ApiFrictionReport],
) -> Result<ApiFrictionRecordResult, String> {
    if reports.is_empty() {
        return Ok(ApiFrictionRecordResult::default());
    }

    let _ = db;
    let pg_pool = pg_pool.ok_or_else(|| {
        "postgres pool is required for API friction capture; sqlite fallback is unavailable"
            .to_string()
    })?;
    let inserted_events = store_api_friction_events_pg(pg_pool, &context, reports).await?;

    let memory_backend = match resolve_memory_backend_for_friction(memory_settings) {
        Some(settings) => Some(MementoBackend::new(settings)),
        None => None,
    };

    let mut result = ApiFrictionRecordResult {
        stored_event_count: inserted_events.len(),
        ..ApiFrictionRecordResult::default()
    };

    for memory_draft in inserted_events {
        let Some(backend) = memory_backend.as_ref() else {
            mark_event_memory_status_pg(
                pg_pool,
                &memory_draft.event_id,
                "skipped_backend",
                Some("memento backend is not active for API friction".to_string()),
            )
            .await;
            continue;
        };

        match backend.remember(memory_draft.request).await {
            Ok(token_usage) => {
                result.memory_stored_count += 1;
                result.token_usage.saturating_add_assign(token_usage);
                mark_event_memory_status_pg(pg_pool, &memory_draft.event_id, "stored", None).await;
            }
            Err(error) => {
                result.memory_errors.push(error.clone());
                mark_event_memory_status_pg(pg_pool, &memory_draft.event_id, "failed", Some(error))
                    .await;
            }
        }
    }

    Ok(result)
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
        if pattern
            .issue_url
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        {
            summary.skipped_patterns.push(pattern);
            continue;
        }

        let issue_title = format!(
            "api-friction: {} — {}",
            pattern.endpoint, pattern.friction_type
        );
        let issue_body = build_issue_body_pg(pg_pool, &pattern).await?;

        match crate::github::create_issue(&pattern.repo_id, &issue_title, &issue_body).await {
            Ok(issue) => {
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
                .bind(&issue_title)
                .bind(&issue_body)
                .bind(i32::try_from(issue.number).map_err(|_| {
                    format!("github issue number exceeds postgres integer: {}", issue.number)
                })?)
                .bind(&issue.url)
                .bind(i32::try_from(pattern.event_count).map_err(|_| {
                    format!(
                        "api_friction event_count exceeds postgres integer: {}",
                        pattern.event_count
                    )
                })?)
                .bind(&pattern.first_seen_at)
                .bind(&pattern.last_seen_at)
                .execute(pg_pool)
                .await
                .map_err(|err| format!("upsert api_friction_issues: {err}"))?;

                summary.created_issues.push(ProcessedApiFrictionIssue {
                    fingerprint: pattern.fingerprint,
                    endpoint: pattern.endpoint,
                    friction_type: pattern.friction_type,
                    repo_id: pattern.repo_id,
                    event_count: pattern.event_count,
                    issue_number: issue.number,
                    issue_url: issue.url,
                });
            }
            Err(error) => {
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
                .bind(&issue_title)
                .bind(&issue_body)
                .bind(i32::try_from(pattern.event_count).map_err(|_| {
                    format!(
                        "api_friction event_count exceeds postgres integer: {}",
                        pattern.event_count
                    )
                })?)
                .bind(&pattern.first_seen_at)
                .bind(&pattern.last_seen_at)
                .bind(&error)
                .execute(pg_pool)
                .await
                .map_err(|err| format!("record api_friction_issues failure: {err}"))?;

                summary.failed_patterns.push(ApiFrictionPatternFailure {
                    fingerprint: pattern.fingerprint,
                    endpoint: pattern.endpoint,
                    friction_type: pattern.friction_type,
                    repo_id: pattern.repo_id,
                    error,
                });
            }
        }
    }

    Ok(summary)
}

fn resolve_memory_backend_for_friction(
    memory_settings: &ResolvedMemorySettings,
) -> Option<ResolvedMemorySettings> {
    if memory_settings.backend == MemoryBackendKind::Memento {
        return Some(memory_settings.clone());
    }
    let resolved = resolve_memory_settings(None, None);
    (resolved.backend == MemoryBackendKind::Memento).then_some(resolved)
}

pub(super) async fn load_pattern_candidates_pg(
    pg_pool: &PgPool,
    min_events: usize,
    limit: usize,
) -> Result<Vec<ApiFrictionPattern>, String> {
    let min_events = min_events.max(API_FRICTION_MIN_REPEAT_COUNT) as i64;
    let limit = clamp_api_limit(Some(limit)) as i64;
    let rows = sqlx::query_as::<
        _,
        (
            String,
            i64,
            String,
            String,
            Option<i64>,
            Option<String>,
            Option<String>,
        ),
    >(
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
    for (
        fingerprint,
        event_count,
        first_seen_at,
        last_seen_at,
        issue_number,
        issue_url,
        last_error,
    ) in rows
    {
        let latest = sqlx::query_as::<
            _,
            (
                String,
                String,
                Option<String>,
                String,
                Option<String>,
                Option<String>,
                String,
                Option<String>,
            ),
        >(
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
        .bind(&fingerprint)
        .bind(DEFAULT_API_FRICTION_REPO)
        .fetch_one(pg_pool)
        .await
        .map_err(|err| format!("load latest api_friction pattern row: {err}"))?;

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

        patterns.push(ApiFrictionPattern {
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
        });
    }

    Ok(patterns)
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
