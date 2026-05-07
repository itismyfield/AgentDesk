use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::BTreeSet;

use crate::db::Db;
use crate::services::discord::settings::{
    MemoryBackendKind, ResolvedMemorySettings, resolve_memory_settings,
};
use crate::services::memory::{MementoBackend, MementoRememberRequest, TokenUsage};
use crate::utils::api::clamp_api_limit;

const DEFAULT_API_FRICTION_REPO: &str = "itismyfield/AgentDesk";
pub(super) const API_FRICTION_MIN_REPEAT_COUNT: usize = 2;
pub(super) const DEFAULT_PATTERN_LIMIT: usize = 20;
const MAX_REPORT_FIELD_CHARS: usize = 240;
const MAX_MEMORY_CONTENT_CHARS: usize = 900;
const MAX_ISSUE_EVIDENCE_ITEMS: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ApiFrictionReport {
    pub endpoint: String,
    pub friction_type: String,
    pub summary: String,
    pub workaround: Option<String>,
    pub suggested_fix: Option<String>,
    pub docs_category: Option<String>,
    pub keywords: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawApiFrictionReport {
    endpoint: Option<String>,
    #[serde(alias = "surface")]
    area: Option<String>,
    friction_type: Option<String>,
    #[serde(alias = "frictionType")]
    friction_type_camel: Option<String>,
    #[serde(alias = "type")]
    kind: Option<String>,
    summary: Option<String>,
    workaround: Option<String>,
    #[serde(alias = "workaround_method")]
    workaround_method: Option<String>,
    suggested_fix: Option<String>,
    #[serde(alias = "suggestedFix")]
    suggested_fix_camel: Option<String>,
    docs_category: Option<String>,
    #[serde(alias = "docsCategory")]
    docs_category_camel: Option<String>,
    keywords: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ApiFrictionExtraction {
    pub cleaned_response: String,
    pub reports: Vec<ApiFrictionReport>,
    pub parse_errors: Vec<String>,
}

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

#[derive(Clone, Debug, Default)]
pub(super) struct SourceContext {
    pub(super) card_id: Option<String>,
    pub(super) repo_id: Option<String>,
    pub(super) issue_number: Option<i64>,
    pub(super) task_summary: Option<String>,
    pub(super) agent_id: Option<String>,
}

#[derive(Clone, Debug)]
struct EventMemoryDraft {
    event_id: String,
    request: MementoRememberRequest,
}

#[derive(Clone, Debug)]
struct PreparedEventRow {
    event_id: String,
    fingerprint: String,
    endpoint: String,
    friction_type: String,
    summary: String,
    workaround: Option<String>,
    suggested_fix: Option<String>,
    docs_category: Option<String>,
    keywords_json_value: serde_json::Value,
    payload_json_value: serde_json::Value,
    card_id: Option<String>,
    repo_id: String,
    github_issue_number_pg: Option<i32>,
    task_summary: Option<String>,
    agent_id: Option<String>,
    request: MementoRememberRequest,
}

pub(crate) fn extract_api_friction_reports(full_response: &str) -> ApiFrictionExtraction {
    let mut cleaned_lines = Vec::new();
    let mut reports = Vec::new();
    let mut parse_errors = Vec::new();

    for line in full_response.lines() {
        let trimmed = line.trim();
        let Some(payload) = trimmed.strip_prefix("API_FRICTION:") else {
            cleaned_lines.push(line.to_string());
            continue;
        };

        match serde_json::from_str::<RawApiFrictionReport>(payload.trim())
            .map_err(|err| err.to_string())
            .and_then(ApiFrictionReport::try_from_raw)
        {
            Ok(report) => reports.push(report),
            Err(error) => {
                parse_errors.push(format!("invalid API_FRICTION marker: {error}"));
                cleaned_lines.push(line.to_string());
            }
        }
    }

    ApiFrictionExtraction {
        cleaned_response: normalize_cleaned_response(&cleaned_lines.join("\n")),
        reports,
        parse_errors,
    }
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
    let source_context =
        load_source_context_pg(pg_pool, context.dispatch_id, context.session_key).await?;
    let prepared_rows = prepare_event_rows(&source_context, &context, reports)?;
    persist_event_rows_pg(pg_pool, &context, &prepared_rows).await?;
    let inserted_events: Vec<EventMemoryDraft> = prepared_rows
        .into_iter()
        .map(|row| EventMemoryDraft {
            event_id: row.event_id,
            request: row.request,
        })
        .collect();

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
            mark_event_memory_status(
                db,
                Some(pg_pool),
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
                mark_event_memory_status(db, Some(pg_pool), &memory_draft.event_id, "stored", None)
                    .await;
            }
            Err(error) => {
                result.memory_errors.push(error.clone());
                mark_event_memory_status(
                    db,
                    Some(pg_pool),
                    &memory_draft.event_id,
                    "failed",
                    Some(error),
                )
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

impl ApiFrictionReport {
    fn try_from_raw(raw: RawApiFrictionReport) -> Result<Self, String> {
        let endpoint = raw
            .endpoint
            .or(raw.area)
            .map(|value| clean_text_field(&value, MAX_REPORT_FIELD_CHARS))
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "endpoint is required".to_string())?;
        let friction_type = raw
            .friction_type
            .or(raw.friction_type_camel)
            .or(raw.kind)
            .map(|value| clean_text_field(&value, 80))
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "friction_type is required".to_string())?;
        let summary = raw
            .summary
            .map(|value| clean_text_field(&value, MAX_REPORT_FIELD_CHARS))
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "summary is required".to_string())?;
        let workaround = raw
            .workaround
            .or(raw.workaround_method)
            .map(|value| clean_text_field(&value, MAX_REPORT_FIELD_CHARS))
            .filter(|value| !value.is_empty());
        let suggested_fix = raw
            .suggested_fix
            .or(raw.suggested_fix_camel)
            .map(|value| clean_text_field(&value, MAX_REPORT_FIELD_CHARS))
            .filter(|value| !value.is_empty());
        let docs_category = raw
            .docs_category
            .or(raw.docs_category_camel)
            .map(|value| clean_text_field(&value, 80))
            .filter(|value| !value.is_empty());

        let keywords = collect_keywords(
            raw.keywords.unwrap_or_default(),
            &endpoint,
            &friction_type,
            workaround.as_deref(),
            docs_category.as_deref(),
        );

        Ok(Self {
            endpoint,
            friction_type,
            summary,
            workaround,
            suggested_fix,
            docs_category,
            keywords,
        })
    }
}

fn normalize_cleaned_response(text: &str) -> String {
    let collapsed = text
        .lines()
        .scan(false, |last_blank, line| {
            let is_blank = line.trim().is_empty();
            if is_blank && *last_blank {
                return Some(None);
            }
            *last_blank = is_blank;
            Some(Some(line))
        })
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");
    collapsed.trim().to_string()
}

fn clean_text_field(value: &str, limit: usize) -> String {
    truncate_chars(
        &value
            .split_whitespace()
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        limit,
    )
}

fn collect_keywords(
    explicit: Vec<String>,
    endpoint: &str,
    friction_type: &str,
    workaround: Option<&str>,
    docs_category: Option<&str>,
) -> Vec<String> {
    let mut set = BTreeSet::new();
    set.insert(clean_text_field(endpoint, 80));
    set.insert(clean_text_field(friction_type, 80));
    if let Some(workaround) = workaround {
        set.insert(clean_text_field(workaround, 80));
    }
    if let Some(docs_category) = docs_category {
        set.insert(clean_text_field(docs_category, 80));
    }
    for keyword in explicit {
        let cleaned = clean_text_field(&keyword, 80);
        if !cleaned.is_empty() {
            set.insert(cleaned);
        }
    }
    set.into_iter().filter(|value| !value.is_empty()).collect()
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{}...", truncated.trim_end())
    } else {
        truncated
    }
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

pub(super) async fn load_dispatch_source_context_pg(
    pg_pool: &PgPool,
    dispatch_id: &str,
) -> Result<Option<SourceContext>, String> {
    sqlx::query_as::<
        _,
        (
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<String>,
        ),
    >(
        "SELECT td.kanban_card_id,
                kc.repo_id,
                kc.github_issue_number,
                COALESCE(NULLIF(TRIM(kc.title), ''), NULLIF(TRIM(td.title), '')),
                td.to_agent_id
         FROM task_dispatches td
         LEFT JOIN kanban_cards kc
           ON kc.id = td.kanban_card_id
         WHERE td.id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pg_pool)
    .await
    .map(|row| {
        row.map(
            |(card_id, repo_id, issue_number, task_summary, agent_id)| SourceContext {
                card_id,
                repo_id,
                issue_number,
                task_summary,
                agent_id,
            },
        )
    })
    .map_err(|err| format!("load task_dispatches source context: {err}"))
}

async fn load_source_context_pg(
    pg_pool: &PgPool,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
) -> Result<SourceContext, String> {
    if let Some(dispatch_id) = dispatch_id
        && let Some(context) = load_dispatch_source_context_pg(pg_pool, dispatch_id).await?
    {
        return Ok(context);
    }

    if let Some(session_key) = session_key
        && let Some((agent_id, active_dispatch_id, session_info)) =
            sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>)>(
                "SELECT agent_id, active_dispatch_id, session_info
             FROM sessions
             WHERE session_key = $1",
            )
            .bind(session_key)
            .fetch_optional(pg_pool)
            .await
            .map_err(|err| format!("load sessions source context: {err}"))?
    {
        if let Some(active_dispatch_id) = active_dispatch_id {
            let mut active = load_dispatch_source_context_pg(pg_pool, active_dispatch_id.as_str())
                .await?
                .unwrap_or_default();
            if active.agent_id.is_none() {
                active.agent_id = agent_id;
            }
            if active.task_summary.is_none() {
                active.task_summary = session_info;
            }
            return Ok(active);
        }

        return Ok(SourceContext {
            agent_id,
            task_summary: session_info,
            ..SourceContext::default()
        });
    }

    Ok(SourceContext::default())
}

fn prepare_event_rows(
    source_context: &SourceContext,
    context: &ApiFrictionRecordContext<'_>,
    reports: &[ApiFrictionReport],
) -> Result<Vec<PreparedEventRow>, String> {
    reports
        .iter()
        .map(|report| {
            let fingerprint = build_fingerprint(&report.endpoint, &report.friction_type);
            let event_id = uuid::Uuid::new_v4().to_string();
            let payload_json_value = serde_json::to_value(report)
                .map_err(|err| format!("serialize api_friction payload: {err}"))?;
            let keywords_json_value = serde_json::to_value(&report.keywords)
                .map_err(|err| format!("serialize api_friction keywords: {err}"))?;
            let repo_id = source_context
                .repo_id
                .clone()
                .unwrap_or_else(|| DEFAULT_API_FRICTION_REPO.to_string());

            Ok(PreparedEventRow {
                event_id: event_id.clone(),
                fingerprint: fingerprint.clone(),
                endpoint: report.endpoint.clone(),
                friction_type: report.friction_type.clone(),
                summary: report.summary.clone(),
                workaround: report.workaround.clone(),
                suggested_fix: report.suggested_fix.clone(),
                docs_category: report.docs_category.clone(),
                keywords_json_value,
                payload_json_value,
                card_id: source_context.card_id.clone(),
                repo_id,
                github_issue_number_pg: source_context
                    .issue_number
                    .and_then(|value| i32::try_from(value).ok()),
                task_summary: source_context.task_summary.clone(),
                agent_id: source_context.agent_id.clone(),
                request: build_memento_request(
                    source_context,
                    report,
                    &fingerprint,
                    context.dispatch_id,
                ),
            })
        })
        .collect()
}

async fn persist_event_rows_pg(
    pg_pool: &PgPool,
    context: &ApiFrictionRecordContext<'_>,
    rows: &[PreparedEventRow],
) -> Result<(), String> {
    let mut tx = pg_pool
        .begin()
        .await
        .map_err(|err| format!("begin api_friction transaction: {err}"))?;

    for row in rows {
        sqlx::query(
            "INSERT INTO api_friction_events (
                id, fingerprint, endpoint, friction_type, summary, workaround, suggested_fix,
                docs_category, keywords_json, payload_json, session_key, channel_id, provider,
                dispatch_id, card_id, repo_id, github_issue_number, task_summary, agent_id,
                memory_backend
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7,
                $8, $9, $10, $11, $12, $13,
                $14, $15, $16, $17, $18, $19,
                $20
             )",
        )
        .bind(&row.event_id)
        .bind(&row.fingerprint)
        .bind(&row.endpoint)
        .bind(&row.friction_type)
        .bind(&row.summary)
        .bind(row.workaround.as_deref())
        .bind(row.suggested_fix.as_deref())
        .bind(row.docs_category.as_deref())
        .bind(&row.keywords_json_value)
        .bind(&row.payload_json_value)
        .bind(context.session_key)
        .bind(context.channel_id.to_string())
        .bind(context.provider)
        .bind(context.dispatch_id)
        .bind(row.card_id.as_deref())
        .bind(&row.repo_id)
        .bind(row.github_issue_number_pg)
        .bind(row.task_summary.as_deref())
        .bind(row.agent_id.as_deref())
        .bind("memento")
        .execute(&mut *tx)
        .await
        .map_err(|err| format!("insert api_friction_events: {err}"))?;
    }

    tx.commit()
        .await
        .map_err(|err| format!("commit api_friction transaction: {err}"))?;
    Ok(())
}

fn build_memento_request(
    source_context: &SourceContext,
    report: &ApiFrictionReport,
    fingerprint: &str,
    dispatch_id: Option<&str>,
) -> MementoRememberRequest {
    let source = [
        dispatch_id.map(|value| format!("dispatch:{value}")),
        source_context
            .card_id
            .as_deref()
            .map(|value| format!("card:{value}")),
        source_context
            .issue_number
            .map(|value| format!("issue:{value}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("/");

    let repo_workspace = source_context
        .repo_id
        .as_deref()
        .and_then(|value| value.split('/').next_back())
        .map(crate::services::memory::sanitize_memento_workspace_segment)
        .unwrap_or_else(|| "agentdesk".to_string());

    let content = truncate_chars(
        &format!(
            "API friction on {} ({})\nSummary: {}\nWorkaround: {}\nSuggested fix: {}\nTask: {}",
            report.endpoint,
            report.friction_type,
            report.summary,
            report.workaround.as_deref().unwrap_or("not provided"),
            report.suggested_fix.as_deref().unwrap_or("not provided"),
            source_context
                .task_summary
                .as_deref()
                .unwrap_or("not provided"),
        ),
        MAX_MEMORY_CONTENT_CHARS,
    );

    MementoRememberRequest {
        content,
        topic: "api-friction".to_string(),
        kind: "error".to_string(),
        importance: None,
        keywords: report.keywords.clone(),
        source: (!source.is_empty()).then_some(source),
        workspace: Some(repo_workspace),
        agent_id: Some("default".to_string()),
        case_id: Some(fingerprint.to_string()),
        goal: Some(format!("Reduce API friction for {}", report.endpoint)),
        outcome: Some("observed".to_string()),
        phase: Some("runtime".to_string()),
        resolution_status: Some("open".to_string()),
        assertion_status: Some("reported".to_string()),
        context_summary: Some(report.summary.clone()),
    }
}

fn build_fingerprint(endpoint: &str, friction_type: &str) -> String {
    let endpoint = endpoint
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let friction_type = friction_type
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    format!("{endpoint}::{friction_type}")
}

async fn mark_event_memory_status(
    _db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    event_id: &str,
    status: &str,
    error: Option<String>,
) {
    if let Some(pg_pool) = pg_pool {
        let _ = sqlx::query(
            "UPDATE api_friction_events
             SET memory_status = $1, memory_error = $2
             WHERE id = $3",
        )
        .bind(status)
        .bind(error.as_deref())
        .bind(event_id)
        .execute(pg_pool)
        .await;
    }
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
