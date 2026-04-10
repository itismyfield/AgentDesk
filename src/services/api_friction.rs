use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::services::discord::settings::{
    MemoryBackendKind, ResolvedMemorySettings, resolve_memory_settings,
};
use crate::services::memory::{MementoBackend, MementoRememberRequest, TokenUsage};

const DEFAULT_API_FRICTION_REPO: &str = "itismyfield/AgentDesk";
const API_FRICTION_MIN_REPEAT_COUNT: usize = 2;
const DEFAULT_PATTERN_LIMIT: usize = 20;
const MAX_REPORT_FIELD_CHARS: usize = 240;
const MAX_MEMORY_CONTENT_CHARS: usize = 900;
const MAX_ISSUE_EVIDENCE_ITEMS: usize = 5;
const DEFAULT_EVENT_LIST_LIMIT: usize = 20;

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
pub(crate) struct ApiFrictionEvent {
    pub id: String,
    pub fingerprint: String,
    pub endpoint: String,
    pub friction_type: String,
    pub summary: String,
    pub workaround: Option<String>,
    pub suggested_fix: Option<String>,
    pub docs_category: Option<String>,
    pub task_summary: Option<String>,
    pub repo_id: Option<String>,
    pub card_id: Option<String>,
    pub github_issue_number: Option<i64>,
    pub dispatch_id: Option<String>,
    pub issue_url: Option<String>,
    pub created_at: String,
    pub memory_status: String,
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
struct SourceContext {
    card_id: Option<String>,
    repo_id: Option<String>,
    issue_number: Option<i64>,
    task_summary: Option<String>,
    agent_id: Option<String>,
}

#[derive(Clone, Debug)]
struct EventMemoryDraft {
    event_id: String,
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
    db: &Db,
    memory_settings: &ResolvedMemorySettings,
    context: ApiFrictionRecordContext<'_>,
    reports: &[ApiFrictionReport],
) -> Result<ApiFrictionRecordResult, String> {
    if reports.is_empty() {
        return Ok(ApiFrictionRecordResult::default());
    }

    let inserted_events = {
        let mut conn = db.lock().map_err(|err| format!("db lock: {err}"))?;
        let source_context = load_source_context(
            &mut conn,
            context.dispatch_id,
            context.session_key,
            context.channel_id,
        )?;
        persist_event_rows(&mut conn, &source_context, &context, reports)?
    };

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
                &memory_draft.event_id,
                "skipped_backend",
                Some("memento backend is not active for API friction".to_string()),
            );
            continue;
        };

        match backend.remember(memory_draft.request).await {
            Ok(token_usage) => {
                result.memory_stored_count += 1;
                result.token_usage.saturating_add_assign(token_usage);
                mark_event_memory_status(db, &memory_draft.event_id, "stored", None);
            }
            Err(error) => {
                result.memory_errors.push(error.clone());
                mark_event_memory_status(db, &memory_draft.event_id, "failed", Some(error));
            }
        }
    }

    Ok(result)
}

pub(crate) fn list_recent_api_friction_events(
    db: &Db,
    limit: Option<usize>,
) -> Result<Vec<ApiFrictionEvent>, String> {
    let limit = limit.unwrap_or(DEFAULT_EVENT_LIST_LIMIT).clamp(1, 100) as i64;
    let conn = db
        .read_conn()
        .map_err(|err| format!("db read_conn: {err}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT e.id,
                    e.fingerprint,
                    e.endpoint,
                    e.friction_type,
                    e.summary,
                    e.workaround,
                    e.suggested_fix,
                    e.docs_category,
                    e.task_summary,
                    e.repo_id,
                    e.card_id,
                    e.github_issue_number,
                    e.dispatch_id,
                    e.created_at,
                    e.memory_status,
                    i.issue_url
             FROM api_friction_events e
             LEFT JOIN api_friction_issues i
               ON i.fingerprint = e.fingerprint
             ORDER BY e.created_at DESC, e.rowid DESC
             LIMIT ?1",
        )
        .map_err(|err| format!("prepare api_friction_events: {err}"))?;

    let rows = stmt
        .query_map([limit], |row| {
            Ok(ApiFrictionEvent {
                id: row.get(0)?,
                fingerprint: row.get(1)?,
                endpoint: row.get(2)?,
                friction_type: row.get(3)?,
                summary: row.get(4)?,
                workaround: row.get(5)?,
                suggested_fix: row.get(6)?,
                docs_category: row.get(7)?,
                task_summary: row.get(8)?,
                repo_id: row.get(9)?,
                card_id: row.get(10)?,
                github_issue_number: row.get(11)?,
                dispatch_id: row.get(12)?,
                created_at: row.get(13)?,
                memory_status: row.get(14)?,
                issue_url: row.get(15)?,
            })
        })
        .map_err(|err| format!("query api_friction_events: {err}"))?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|err| format!("collect api_friction_events: {err}"))
}

pub(crate) fn list_api_friction_patterns(
    db: &Db,
    min_events: Option<usize>,
    limit: Option<usize>,
) -> Result<Vec<ApiFrictionPattern>, String> {
    let conn = db
        .read_conn()
        .map_err(|err| format!("db read_conn: {err}"))?;
    load_pattern_candidates(
        &conn,
        min_events.unwrap_or(API_FRICTION_MIN_REPEAT_COUNT),
        limit.unwrap_or(DEFAULT_PATTERN_LIMIT),
    )
}

pub(crate) async fn process_api_friction_patterns(
    db: &Db,
    min_events: Option<usize>,
    limit: Option<usize>,
) -> Result<ApiFrictionProcessSummary, String> {
    let patterns = {
        let conn = db
            .read_conn()
            .map_err(|err| format!("db read_conn: {err}"))?;
        load_pattern_candidates(
            &conn,
            min_events.unwrap_or(API_FRICTION_MIN_REPEAT_COUNT),
            limit.unwrap_or(DEFAULT_PATTERN_LIMIT),
        )?
    };

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
        let issue_body = build_issue_body(db, &pattern)?;

        match crate::github::create_issue(&pattern.repo_id, &issue_title, &issue_body).await {
            Ok(issue) => {
                {
                    let conn = db.lock().map_err(|err| format!("db lock: {err}"))?;
                    conn.execute(
                        "INSERT INTO api_friction_issues (
                            fingerprint, repo_id, endpoint, friction_type, title, body, issue_number,
                            issue_url, event_count, first_event_at, last_event_at, last_error,
                            created_at, updated_at
                         ) VALUES (
                            ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                            ?8, ?9, ?10, ?11, NULL,
                            datetime('now'), datetime('now')
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
                            updated_at = datetime('now')",
                        params![
                            pattern.fingerprint,
                            pattern.repo_id,
                            pattern.endpoint,
                            pattern.friction_type,
                            issue_title,
                            issue_body,
                            issue.number,
                            issue.url,
                            pattern.event_count as i64,
                            pattern.first_seen_at,
                            pattern.last_seen_at,
                        ],
                    )
                    .map_err(|err| format!("upsert api_friction_issues: {err}"))?;
                }

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
                {
                    let conn = db.lock().map_err(|err| format!("db lock: {err}"))?;
                    conn.execute(
                        "INSERT INTO api_friction_issues (
                            fingerprint, repo_id, endpoint, friction_type, title, body, issue_number,
                            issue_url, event_count, first_event_at, last_event_at, last_error,
                            created_at, updated_at
                         ) VALUES (
                            ?1, ?2, ?3, ?4, ?5, ?6, NULL,
                            NULL, ?7, ?8, ?9, ?10,
                            datetime('now'), datetime('now')
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
                            updated_at = datetime('now')",
                        params![
                            pattern.fingerprint,
                            pattern.repo_id,
                            pattern.endpoint,
                            pattern.friction_type,
                            issue_title,
                            issue_body,
                            pattern.event_count as i64,
                            pattern.first_seen_at,
                            pattern.last_seen_at,
                            error,
                        ],
                    )
                    .map_err(|err| format!("record api_friction_issues failure: {err}"))?;
                }

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

fn load_source_context(
    conn: &mut Connection,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    _channel_id: u64,
) -> Result<SourceContext, String> {
    if let Some(dispatch_id) = dispatch_id
        && let Some(context) = conn
            .query_row(
                "SELECT td.kanban_card_id,
                        kc.repo_id,
                        kc.github_issue_number,
                        COALESCE(NULLIF(TRIM(kc.title), ''), NULLIF(TRIM(td.title), '')),
                        td.to_agent_id
                 FROM task_dispatches td
                 LEFT JOIN kanban_cards kc
                   ON kc.id = td.kanban_card_id
                 WHERE td.id = ?1",
                [dispatch_id],
                |row| {
                    Ok(SourceContext {
                        card_id: row.get(0)?,
                        repo_id: row.get(1)?,
                        issue_number: row.get(2)?,
                        task_summary: row.get(3)?,
                        agent_id: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(|err| format!("load task_dispatches source context: {err}"))?
    {
        return Ok(context);
    }

    if let Some(session_key) = session_key
        && let Some(context) = conn
            .query_row(
                "SELECT agent_id, active_dispatch_id, session_info
                 FROM sessions
                 WHERE session_key = ?1",
                [session_key],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|err| format!("load sessions source context: {err}"))?
    {
        let (agent_id, active_dispatch_id, session_info) = context;
        if let Some(active_dispatch_id) = active_dispatch_id {
            let mut active = load_source_context(conn, Some(active_dispatch_id.as_str()), None, 0)?;
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

fn persist_event_rows(
    conn: &mut Connection,
    source_context: &SourceContext,
    context: &ApiFrictionRecordContext<'_>,
    reports: &[ApiFrictionReport],
) -> Result<Vec<EventMemoryDraft>, String> {
    let tx = conn
        .transaction()
        .map_err(|err| format!("begin api_friction transaction: {err}"))?;
    let mut memory_drafts = Vec::new();

    for report in reports {
        let fingerprint = build_fingerprint(&report.endpoint, &report.friction_type);
        let id = uuid::Uuid::new_v4().to_string();
        let payload_json = serde_json::to_string(report)
            .map_err(|err| format!("serialize api_friction payload: {err}"))?;

        tx.execute(
            "INSERT INTO api_friction_events (
                id, fingerprint, endpoint, friction_type, summary, workaround, suggested_fix,
                docs_category, keywords_json, payload_json, session_key, channel_id, provider,
                dispatch_id, card_id, repo_id, github_issue_number, task_summary, agent_id,
                memory_backend, memory_status, memory_error, created_at
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                ?8, ?9, ?10, ?11, ?12, ?13,
                ?14, ?15, ?16, ?17, ?18, ?19,
                ?20, 'pending', NULL, datetime('now')
             )",
            params![
                id,
                fingerprint,
                report.endpoint,
                report.friction_type,
                report.summary,
                report.workaround,
                report.suggested_fix,
                report.docs_category,
                serde_json::to_string(&report.keywords)
                    .map_err(|err| format!("serialize api_friction keywords: {err}"))?,
                payload_json,
                context.session_key,
                context.channel_id.to_string(),
                context.provider,
                context.dispatch_id,
                source_context.card_id,
                source_context
                    .repo_id
                    .clone()
                    .unwrap_or_else(|| DEFAULT_API_FRICTION_REPO.to_string()),
                source_context.issue_number,
                source_context.task_summary,
                source_context.agent_id,
                "memento",
            ],
        )
        .map_err(|err| format!("insert api_friction_events: {err}"))?;

        memory_drafts.push(EventMemoryDraft {
            event_id: id,
            request: build_memento_request(
                source_context,
                report,
                &fingerprint,
                context.dispatch_id,
            ),
        });
    }

    tx.commit()
        .map_err(|err| format!("commit api_friction transaction: {err}"))?;
    Ok(memory_drafts)
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

fn mark_event_memory_status(db: &Db, event_id: &str, status: &str, error: Option<String>) {
    let Ok(conn) = db.lock() else {
        return;
    };
    let _ = conn.execute(
        "UPDATE api_friction_events
         SET memory_status = ?1, memory_error = ?2
         WHERE id = ?3",
        params![status, error, event_id],
    );
}

fn load_pattern_candidates(
    conn: &Connection,
    min_events: usize,
    limit: usize,
) -> Result<Vec<ApiFrictionPattern>, String> {
    let min_events = min_events.max(API_FRICTION_MIN_REPEAT_COUNT) as i64;
    let limit = limit.clamp(1, 100) as i64;
    let mut stmt = conn
        .prepare(
            "SELECT e.fingerprint,
                    COUNT(*) AS event_count,
                    MIN(e.created_at) AS first_seen_at,
                    MAX(e.created_at) AS last_seen_at,
                    i.issue_number,
                    i.issue_url,
                    i.last_error
             FROM api_friction_events e
             LEFT JOIN api_friction_issues i
               ON i.fingerprint = e.fingerprint
             GROUP BY e.fingerprint
             HAVING COUNT(*) >= ?1
             ORDER BY event_count DESC, last_seen_at DESC
             LIMIT ?2",
        )
        .map_err(|err| format!("prepare api_friction pattern aggregate: {err}"))?;

    let rows = stmt
        .query_map(params![min_events, limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })
        .map_err(|err| format!("query api_friction pattern aggregate: {err}"))?;

    let mut patterns = Vec::new();
    for row in rows {
        let (
            fingerprint,
            event_count,
            first_seen_at,
            last_seen_at,
            issue_number,
            issue_url,
            last_error,
        ) = row.map_err(|err| format!("collect api_friction pattern aggregate: {err}"))?;
        let latest = conn
            .query_row(
                "SELECT endpoint, friction_type, docs_category, summary, workaround, suggested_fix,
                        COALESCE(repo_id, ?2), task_summary
                 FROM api_friction_events
                 WHERE fingerprint = ?1
                 ORDER BY created_at DESC, rowid DESC
                 LIMIT 1",
                params![fingerprint, DEFAULT_API_FRICTION_REPO],
                |row| {
                    Ok(ApiFrictionPattern {
                        fingerprint: String::new(),
                        endpoint: row.get(0)?,
                        friction_type: row.get(1)?,
                        docs_category: row.get(2)?,
                        summary: row.get(3)?,
                        workaround: row.get(4)?,
                        suggested_fix: row.get(5)?,
                        repo_id: row.get(6)?,
                        event_count: 0,
                        first_seen_at: String::new(),
                        last_seen_at: String::new(),
                        task_summary: row.get(7)?,
                        github_issue_number: None,
                        issue_url: None,
                        last_error: None,
                    })
                },
            )
            .map_err(|err| format!("load latest api_friction pattern row: {err}"))?;

        patterns.push(ApiFrictionPattern {
            fingerprint,
            event_count: event_count as usize,
            first_seen_at,
            last_seen_at,
            github_issue_number: issue_number,
            issue_url,
            last_error,
            ..latest
        });
    }

    Ok(patterns)
}

fn build_issue_body(db: &Db, pattern: &ApiFrictionPattern) -> Result<String, String> {
    let conn = db
        .read_conn()
        .map_err(|err| format!("db read_conn: {err}"))?;
    let evidence = load_pattern_evidence(&conn, &pattern.fingerprint)?;
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

fn load_pattern_evidence(
    conn: &Connection,
    fingerprint: &str,
) -> Result<Vec<PatternEvidence>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT repo_id, github_issue_number, card_id, dispatch_id, summary
             FROM api_friction_events
             WHERE fingerprint = ?1
             ORDER BY created_at DESC, rowid DESC
             LIMIT ?2",
        )
        .map_err(|err| format!("prepare api_friction evidence: {err}"))?;
    let rows = stmt
        .query_map(
            params![fingerprint, MAX_ISSUE_EVIDENCE_ITEMS as i64],
            |row| {
                Ok(PatternEvidence {
                    repo_id: row.get(0)?,
                    issue_number: row.get(1)?,
                    card_id: row.get(2)?,
                    dispatch_id: row.get(3)?,
                    summary: row.get(4)?,
                })
            },
        )
        .map_err(|err| format!("query api_friction evidence: {err}"))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|err| format!("collect api_friction evidence: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::services::discord::settings::{MemoryBackendKind, ResolvedMemorySettings};
    use serde_json::json;

    #[test]
    fn extract_api_friction_reports_strips_valid_markers() {
        let input = "검증 완료\nAPI_FRICTION: {\"endpoint\":\"/api/docs/kanban\",\"friction_type\":\"docs-bypass\",\"summary\":\"카테고리를 모르고 시행착오\",\"workaround\":\"sqlite3\",\"keywords\":[\"kanban\"]}\n후속 작업 없음";
        let extracted = extract_api_friction_reports(input);

        assert_eq!(extracted.reports.len(), 1);
        assert!(extracted.cleaned_response.contains("검증 완료"));
        assert!(extracted.cleaned_response.contains("후속 작업 없음"));
        assert!(!extracted.cleaned_response.contains("API_FRICTION"));
        assert_eq!(extracted.reports[0].endpoint, "/api/docs/kanban");
        assert_eq!(extracted.reports[0].friction_type, "docs-bypass");
        assert!(
            extracted.reports[0]
                .keywords
                .iter()
                .any(|value| value == "sqlite3")
        );
    }

    #[test]
    fn extract_api_friction_reports_keeps_invalid_markers_visible() {
        let input = "API_FRICTION: {not-json}";
        let extracted = extract_api_friction_reports(input);

        assert!(extracted.reports.is_empty());
        assert_eq!(extracted.cleaned_response, input);
        assert_eq!(extracted.parse_errors.len(), 1);
    }

    #[test]
    fn list_api_friction_patterns_counts_repeated_rows() {
        let db = crate::db::test_db();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO api_friction_events (
                id, fingerprint, endpoint, friction_type, summary, keywords_json, payload_json,
                channel_id, provider, repo_id, memory_backend, memory_status, created_at
             ) VALUES (
                'event-1', 'api-docs-kanban::docs-bypass', '/api/docs/kanban', 'docs-bypass', 'first',
                '[]', '{}', '1', 'codex', 'itismyfield/AgentDesk', 'memento', 'stored', datetime('now', '-2 minutes')
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO api_friction_events (
                id, fingerprint, endpoint, friction_type, summary, keywords_json, payload_json,
                channel_id, provider, repo_id, memory_backend, memory_status, created_at
             ) VALUES (
                'event-2', 'api-docs-kanban::docs-bypass', '/api/docs/kanban', 'docs-bypass', 'second',
                '[]', '{}', '1', 'codex', 'itismyfield/AgentDesk', 'memento', 'stored', datetime('now', '-1 minutes')
             )",
            [],
        )
        .unwrap();
        drop(conn);

        let patterns = list_api_friction_patterns(&db, None, None).unwrap();
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].event_count, 2);
        assert_eq!(patterns[0].summary, "second");
    }

    #[tokio::test]
    async fn record_api_friction_reports_syncs_to_memento() {
        #[derive(Clone)]
        struct MockHttpResponse {
            status_line: &'static str,
            headers: Vec<(&'static str, &'static str)>,
            body: String,
        }

        async fn spawn_response_sequence_server(
            responses: Vec<MockHttpResponse>,
        ) -> (
            String,
            tokio::sync::oneshot::Receiver<Vec<String>>,
            tokio::task::JoinHandle<()>,
        ) {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (requests_tx, requests_rx) = tokio::sync::oneshot::channel();
            let handle = tokio::spawn(async move {
                let mut requests = Vec::new();
                for response in responses {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        break;
                    };
                    let mut buf = [0u8; 32768];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    requests.push(String::from_utf8_lossy(&buf[..n]).to_string());

                    let mut raw_response = format!(
                        "HTTP/1.1 {}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n",
                        response.status_line,
                        response.body.len()
                    );
                    for (header, value) in response.headers {
                        raw_response.push_str(&format!("{header}: {value}\r\n"));
                    }
                    raw_response.push_str("\r\n");
                    raw_response.push_str(&response.body);

                    let _ = stream.write_all(raw_response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                }
                let _ = requests_tx.send(requests);
            });

            (format!("http://{}", addr), requests_rx, handle)
        }

        fn restore_env(name: &str, previous: Option<std::ffi::OsString>) {
            match previous {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }

        let initialize_response = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": "2025-11-25"
            }
        }))
        .unwrap();
        let remember_response = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "content": [
                    {
                        "type": "text",
                        "text": serde_json::to_string(&json!({"usage": {"input_tokens": 8, "output_tokens": 3}})).unwrap()
                    }
                ],
                "isError": false
            }
        }))
        .unwrap();
        let (base_url, requests_rx, handle) = spawn_response_sequence_server(vec![
            MockHttpResponse {
                status_line: "200 OK",
                headers: vec![("MCP-Session-Id", "session-1")],
                body: initialize_response,
            },
            MockHttpResponse {
                status_line: "200 OK",
                headers: vec![("MCP-Session-Id", "session-1")],
                body: remember_response,
            },
        ])
        .await;

        let lock = crate::services::discord::runtime_store::test_env_lock();
        let _guard = lock.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_key = std::env::var_os("MEMENTO_TEST_KEY");
        let config_dir = temp.path().join("config");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("agentdesk.yaml"),
            format!(
                "server:\n  port: 8791\nmemory:\n  backend: memento\n  mcp:\n    endpoint: {base_url}\n    access_key_env: MEMENTO_TEST_KEY\n"
            ),
        )
        .unwrap();
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", temp.path());
            std::env::set_var("MEMENTO_TEST_KEY", "memento-key");
        }

        let db = crate::db::test_db();
        let result = record_api_friction_reports(
            &db,
            &ResolvedMemorySettings {
                backend: MemoryBackendKind::Memento,
                ..ResolvedMemorySettings::default()
            },
            ApiFrictionRecordContext {
                channel_id: 1,
                session_key: Some("host:session"),
                dispatch_id: None,
                provider: "codex",
            },
            &[ApiFrictionReport {
                endpoint: "/api/docs/kanban".to_string(),
                friction_type: "docs-bypass".to_string(),
                summary: "category guessing".to_string(),
                workaround: Some("sqlite3".to_string()),
                suggested_fix: Some("document a single endpoint".to_string()),
                docs_category: Some("kanban".to_string()),
                keywords: vec!["/api/docs/kanban".to_string(), "sqlite3".to_string()],
            }],
        )
        .await
        .unwrap();

        handle.abort();
        restore_env("AGENTDESK_ROOT_DIR", previous_root);
        restore_env("MEMENTO_TEST_KEY", previous_key);

        assert_eq!(result.stored_event_count, 1);
        assert_eq!(result.memory_stored_count, 1);
        assert_eq!(result.token_usage.input_tokens, 8);
        assert_eq!(result.token_usage.output_tokens, 3);

        let requests = requests_rx.await.unwrap();
        assert!(requests[1].contains("\"name\":\"remember\""));
        assert!(requests[1].contains("\"topic\":\"api-friction\""));
        assert!(requests[1].contains("\"type\":\"error\""));

        let conn = db.lock().unwrap();
        let memory_status: String = conn
            .query_row(
                "SELECT memory_status FROM api_friction_events LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(memory_status, "stored");
    }

    #[tokio::test]
    async fn process_api_friction_patterns_creates_issue_once() {
        let lock = crate::services::discord::runtime_store::lock_test_env();
        let dir = tempfile::tempdir().unwrap();
        let gh_path = dir.path().join("gh");
        fs::write(
            &gh_path,
            "#!/usr/bin/env bash\nif [ \"$1\" = \"issue\" ] && [ \"$2\" = \"create\" ]; then\n  echo \"https://github.com/itismyfield/AgentDesk/issues/999\"\n  exit 0\nfi\nexit 1\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&gh_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&gh_path, perms).unwrap();
        }

        let old_gh_path = std::env::var_os("AGENTDESK_GH_PATH");
        unsafe {
            std::env::set_var("AGENTDESK_GH_PATH", &gh_path);
        }

        let db = crate::db::test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO api_friction_events (
                    id, fingerprint, endpoint, friction_type, summary, keywords_json, payload_json,
                    channel_id, provider, repo_id, memory_backend, memory_status, created_at
                 ) VALUES (
                    'event-1', 'api-docs-kanban::docs-bypass', '/api/docs/kanban', 'docs-bypass', 'first',
                    '[]', '{}', '1', 'codex', 'itismyfield/AgentDesk', 'memento', 'stored', datetime('now', '-2 minutes')
                 )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO api_friction_events (
                    id, fingerprint, endpoint, friction_type, summary, keywords_json, payload_json,
                    channel_id, provider, repo_id, memory_backend, memory_status, created_at
                 ) VALUES (
                    'event-2', 'api-docs-kanban::docs-bypass', '/api/docs/kanban', 'docs-bypass', 'second',
                    '[]', '{}', '1', 'codex', 'itismyfield/AgentDesk', 'memento', 'stored', datetime('now', '-1 minutes')
                 )",
                [],
            )
            .unwrap();
        }

        let summary = process_api_friction_patterns(&db, None, None)
            .await
            .unwrap();

        if let Some(value) = old_gh_path {
            unsafe { std::env::set_var("AGENTDESK_GH_PATH", value) };
        } else {
            unsafe { std::env::remove_var("AGENTDESK_GH_PATH") };
        }
        drop(lock);

        assert_eq!(summary.created_issues.len(), 1);
        assert!(summary.failed_patterns.is_empty());
        let conn = db.lock().unwrap();
        let issue_number: i64 = conn
            .query_row(
                "SELECT issue_number FROM api_friction_issues WHERE fingerprint = 'api-docs-kanban::docs-bypass'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(issue_number, 999);
    }
}
