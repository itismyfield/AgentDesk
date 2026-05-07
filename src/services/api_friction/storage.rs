use sqlx::PgPool;

use super::core::ApiFrictionRecordContext;
use super::markers::{ApiFrictionReport, truncate_chars};
use crate::services::memory::MementoRememberRequest;

pub(super) const DEFAULT_API_FRICTION_REPO: &str = "itismyfield/AgentDesk";
const MAX_MEMORY_CONTENT_CHARS: usize = 900;

#[derive(Clone, Debug, Default)]
pub(super) struct SourceContext {
    pub(super) card_id: Option<String>,
    pub(super) repo_id: Option<String>,
    pub(super) issue_number: Option<i64>,
    pub(super) task_summary: Option<String>,
    pub(super) agent_id: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct EventMemoryDraft {
    pub(super) event_id: String,
    pub(super) request: MementoRememberRequest,
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

pub(super) async fn store_api_friction_events_pg(
    pg_pool: &PgPool,
    context: &ApiFrictionRecordContext<'_>,
    reports: &[ApiFrictionReport],
) -> Result<Vec<EventMemoryDraft>, String> {
    let source_context =
        load_source_context_pg(pg_pool, context.dispatch_id, context.session_key).await?;
    let prepared_rows = prepare_event_rows(&source_context, context, reports)?;
    persist_event_rows_pg(pg_pool, context, &prepared_rows).await?;

    Ok(prepared_rows
        .into_iter()
        .map(|row| EventMemoryDraft {
            event_id: row.event_id,
            request: row.request,
        })
        .collect())
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

pub(super) async fn mark_event_memory_status_pg(
    pg_pool: &PgPool,
    event_id: &str,
    status: &str,
    error: Option<String>,
) {
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
