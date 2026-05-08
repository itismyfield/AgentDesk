use sqlx::Row as SqlxRow;

pub mod crud;
pub mod listing;
pub mod metadata;
pub mod transitions;

pub use crud::*;
pub use listing::*;
pub use metadata::*;
pub use transitions::*;

#[derive(Debug, Clone, Default)]
pub struct ListCardsFilter {
    pub status: Option<String>,
    pub repo_id: Option<String>,
    pub assigned_agent_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct KanbanCardRecord {
    pub id: String,
    pub repo_id: Option<String>,
    pub title: String,
    pub status: String,
    pub priority: String,
    pub assigned_agent_id: Option<String>,
    pub github_issue_url: Option<String>,
    pub github_issue_number: Option<i64>,
    pub latest_dispatch_id: Option<String>,
    pub review_round: i64,
    pub metadata_raw: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub description: Option<String>,
    pub blocked_reason: Option<String>,
    pub review_notes: Option<String>,
    pub review_status: Option<String>,
    pub started_at: Option<String>,
    pub requested_at: Option<String>,
    pub completed_at: Option<String>,
    pub pipeline_stage_id: Option<String>,
    pub owner_agent_id: Option<String>,
    pub requester_agent_id: Option<String>,
    pub parent_card_id: Option<String>,
    pub sort_order: i64,
    pub depth: i64,
    pub latest_dispatch_status: Option<String>,
    pub latest_dispatch_title: Option<String>,
    pub latest_dispatch_type: Option<String>,
    pub latest_dispatch_result_summary: Option<String>,
    pub latest_dispatch_chain_depth: Option<i64>,
}

pub(crate) const CARD_SELECT_SQL_PG: &str = "SELECT kc.id, kc.repo_id, kc.title, kc.status, kc.priority, kc.assigned_agent_id, \
    kc.github_issue_url, kc.github_issue_number::bigint AS github_issue_number, kc.latest_dispatch_id, kc.review_round::bigint AS review_round, kc.metadata::text AS metadata, \
    kc.created_at::text AS created_at, kc.updated_at::text AS updated_at, \
    td.status AS d_status, td.dispatch_type AS d_type, td.title AS d_title, td.chain_depth::bigint AS d_depth, \
    td.result AS d_result, td.context AS d_context, \
    kc.description, kc.blocked_reason, kc.review_notes, kc.review_status, \
    kc.started_at::text AS started_at, kc.requested_at::text AS requested_at, kc.completed_at::text AS completed_at, kc.pipeline_stage_id, \
    kc.owner_agent_id, kc.requester_agent_id, kc.parent_card_id, kc.sort_order::bigint AS sort_order, kc.depth::bigint AS depth \
    FROM kanban_cards kc LEFT JOIN task_dispatches td ON td.id = kc.latest_dispatch_id";

#[derive(Debug, Clone, Default)]
pub struct IssueCardUpsert {
    pub repo_id: String,
    pub issue_number: i64,
    pub issue_url: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub priority: Option<String>,
    pub assigned_agent_id: Option<String>,
    pub metadata_json: Option<String>,
    pub status_on_create: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueCardUpsertResult {
    pub card_id: String,
    pub created: bool,
}

pub(crate) fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
}

pub(crate) fn normalize_optional_description(value: Option<String>) -> Option<String> {
    value
        .map(|raw| raw.trim_end().to_string())
        .filter(|raw| !raw.trim().is_empty())
}

#[derive(Debug, Clone)]
pub struct ActiveTurnTarget {
    pub session_key: String,
    pub provider: Option<String>,
    pub thread_channel_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateCardFields {
    pub title: Option<String>,
    pub priority: Option<String>,
    pub assigned_agent_id: Option<String>,
    pub repo_id: Option<String>,
    pub github_issue_url: Option<String>,
    pub description: Option<String>,
    pub metadata_json: Option<String>,
    pub review_status: Option<String>,
    pub review_notes: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RetryDispatchSpec {
    pub agent_id: String,
    pub dispatch_type: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct DodStateRecord {
    pub deferred_dod_json: Option<String>,
    pub status: String,
    pub review_status: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GithubIssueRef {
    pub repo_id: Option<String>,
    pub issue_number: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct PmDecisionCardInfo {
    pub status: String,
    pub review_status: Option<String>,
    pub blocked_reason: Option<String>,
    pub agent_id: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct RereviewCardInfo {
    pub status: String,
    pub assigned_agent_id: Option<String>,
    pub title: String,
    pub github_issue_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CardPipelineContext {
    pub repo_id: Option<String>,
    pub assigned_agent_id: Option<String>,
}

pub(crate) fn kanban_card_row_to_record_pg(
    row: &sqlx::postgres::PgRow,
) -> Result<KanbanCardRecord, String> {
    let latest_dispatch_status = row
        .try_get::<Option<String>, _>("d_status")
        .map_err(|error| format!("decode d_status: {error}"))?;
    let latest_dispatch_type = row
        .try_get::<Option<String>, _>("d_type")
        .map_err(|error| format!("decode d_type: {error}"))?;
    let latest_dispatch_result_raw = row
        .try_get::<Option<String>, _>("d_result")
        .map_err(|error| format!("decode d_result: {error}"))?;
    let latest_dispatch_context_raw = row
        .try_get::<Option<String>, _>("d_context")
        .map_err(|error| format!("decode d_context: {error}"))?;

    Ok(KanbanCardRecord {
        id: row
            .try_get::<String, _>("id")
            .map_err(|error| format!("decode id: {error}"))?,
        repo_id: row
            .try_get::<Option<String>, _>("repo_id")
            .map_err(|error| format!("decode repo_id: {error}"))?,
        title: row
            .try_get::<String, _>("title")
            .map_err(|error| format!("decode title: {error}"))?,
        status: row
            .try_get::<String, _>("status")
            .map_err(|error| format!("decode status: {error}"))?,
        priority: row
            .try_get::<Option<String>, _>("priority")
            .map_err(|error| format!("decode priority: {error}"))?
            .unwrap_or_else(|| "medium".to_string()),
        assigned_agent_id: row
            .try_get::<Option<String>, _>("assigned_agent_id")
            .map_err(|error| format!("decode assigned_agent_id: {error}"))?,
        github_issue_url: row
            .try_get::<Option<String>, _>("github_issue_url")
            .map_err(|error| format!("decode github_issue_url: {error}"))?,
        github_issue_number: row
            .try_get::<Option<i64>, _>("github_issue_number")
            .map_err(|error| format!("decode github_issue_number: {error}"))?,
        latest_dispatch_id: row
            .try_get::<Option<String>, _>("latest_dispatch_id")
            .map_err(|error| format!("decode latest_dispatch_id: {error}"))?,
        review_round: row
            .try_get::<Option<i64>, _>("review_round")
            .map_err(|error| format!("decode review_round: {error}"))?
            .unwrap_or(0),
        metadata_raw: row
            .try_get::<Option<String>, _>("metadata")
            .map_err(|error| format!("decode metadata: {error}"))?,
        created_at: row
            .try_get::<Option<String>, _>("created_at")
            .map_err(|error| format!("decode created_at: {error}"))?,
        updated_at: row
            .try_get::<Option<String>, _>("updated_at")
            .map_err(|error| format!("decode updated_at: {error}"))?,
        latest_dispatch_status: latest_dispatch_status.clone(),
        latest_dispatch_type: latest_dispatch_type.clone(),
        latest_dispatch_title: row
            .try_get::<Option<String>, _>("d_title")
            .map_err(|error| format!("decode d_title: {error}"))?,
        latest_dispatch_chain_depth: row
            .try_get::<Option<i64>, _>("d_depth")
            .map_err(|error| format!("decode d_depth: {error}"))?,
        latest_dispatch_result_summary: crate::dispatch::summarize_dispatch_from_text(
            latest_dispatch_type.as_deref(),
            latest_dispatch_status.as_deref(),
            latest_dispatch_result_raw.as_deref(),
            latest_dispatch_context_raw.as_deref(),
        ),
        description: row
            .try_get::<Option<String>, _>("description")
            .map_err(|error| format!("decode description: {error}"))?,
        blocked_reason: row
            .try_get::<Option<String>, _>("blocked_reason")
            .map_err(|error| format!("decode blocked_reason: {error}"))?,
        review_notes: row
            .try_get::<Option<String>, _>("review_notes")
            .map_err(|error| format!("decode review_notes: {error}"))?,
        review_status: row
            .try_get::<Option<String>, _>("review_status")
            .map_err(|error| format!("decode review_status: {error}"))?,
        started_at: row
            .try_get::<Option<String>, _>("started_at")
            .map_err(|error| format!("decode started_at: {error}"))?,
        requested_at: row
            .try_get::<Option<String>, _>("requested_at")
            .map_err(|error| format!("decode requested_at: {error}"))?,
        completed_at: row
            .try_get::<Option<String>, _>("completed_at")
            .map_err(|error| format!("decode completed_at: {error}"))?,
        pipeline_stage_id: row
            .try_get::<Option<String>, _>("pipeline_stage_id")
            .map_err(|error| format!("decode pipeline_stage_id: {error}"))?,
        owner_agent_id: row
            .try_get::<Option<String>, _>("owner_agent_id")
            .map_err(|error| format!("decode owner_agent_id: {error}"))?,
        requester_agent_id: row
            .try_get::<Option<String>, _>("requester_agent_id")
            .map_err(|error| format!("decode requester_agent_id: {error}"))?,
        parent_card_id: row
            .try_get::<Option<String>, _>("parent_card_id")
            .map_err(|error| format!("decode parent_card_id: {error}"))?,
        sort_order: row
            .try_get::<Option<i64>, _>("sort_order")
            .map_err(|error| format!("decode sort_order: {error}"))?
            .unwrap_or(0),
        depth: row
            .try_get::<Option<i64>, _>("depth")
            .map_err(|error| format!("decode depth: {error}"))?
            .unwrap_or(0),
    })
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn card_row_to_json(row: &sqlite_test::Row) -> sqlite_test::Result<serde_json::Value> {
    use crate::utils::github_links::{
        normalize_optional_github_issue_url, normalize_optional_github_repo_id,
    };
    use serde_json::json;
    let repo_id = normalize_optional_github_repo_id(row.get::<_, Option<String>>(1)?);
    let assigned_agent_id = row.get::<_, Option<String>>(5)?;
    let metadata_raw = row.get::<_, Option<String>>(10).unwrap_or(None);
    let metadata_parsed = metadata_raw
        .as_ref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
    let latest_dispatch_status = row.get::<_, Option<String>>(13).unwrap_or(None);
    let latest_dispatch_type = row.get::<_, Option<String>>(14).unwrap_or(None);
    let latest_dispatch_result_raw = row.get::<_, Option<String>>(17).unwrap_or(None);
    let latest_dispatch_context_raw = row.get::<_, Option<String>>(18).unwrap_or(None);
    let latest_dispatch_result_summary = crate::dispatch::summarize_dispatch_from_text(
        latest_dispatch_type.as_deref(),
        latest_dispatch_status.as_deref(),
        latest_dispatch_result_raw.as_deref(),
        latest_dispatch_context_raw.as_deref(),
    );

    let description = row.get::<_, Option<String>>(19).unwrap_or(None);
    let blocked_reason = row.get::<_, Option<String>>(20).unwrap_or(None);
    let review_notes = row.get::<_, Option<String>>(21).unwrap_or(None);
    let review_status = row.get::<_, Option<String>>(22).unwrap_or(None);
    let started_at = row.get::<_, Option<String>>(23).unwrap_or(None);
    let requested_at = row.get::<_, Option<String>>(24).unwrap_or(None);
    let completed_at = row.get::<_, Option<String>>(25).unwrap_or(None);
    let pipeline_stage_id = row.get::<_, Option<String>>(26).unwrap_or(None);
    let owner_agent_id = row.get::<_, Option<String>>(27).unwrap_or(None);
    let requester_agent_id = row.get::<_, Option<String>>(28).unwrap_or(None);
    let parent_card_id = row.get::<_, Option<String>>(29).unwrap_or(None);
    let sort_order = row.get::<_, i64>(30).unwrap_or(0);
    let depth = row.get::<_, i64>(31).unwrap_or(0);
    let review_entered_at = row.get::<_, Option<String>>(32).unwrap_or(None);

    let github_issue_number = row.get::<_, Option<i64>>(7)?;
    let github_issue_url = normalize_optional_github_issue_url(
        row.get::<_, Option<String>>(6)?,
        repo_id.as_deref(),
        github_issue_number,
    );

    Ok(json!({
        "id": row.get::<_, String>(0)?,
        "repo_id": repo_id,
        "title": row.get::<_, String>(2)?,
        "status": row.get::<_, String>(3)?,
        "priority": row.get::<_, String>(4)?,
        "assigned_agent_id": assigned_agent_id,
        "github_issue_url": github_issue_url,
        "github_issue_number": github_issue_number,
        "latest_dispatch_id": row.get::<_, Option<String>>(8)?,
        "review_round": row.get::<_, i64>(9).unwrap_or(0),
        "metadata": metadata_parsed,
        "created_at": row.get::<_, Option<String>>(11).ok().flatten().or_else(|| row.get::<_, Option<i64>>(11).ok().flatten().map(|v| v.to_string())),
        "updated_at": row.get::<_, Option<String>>(12).ok().flatten().or_else(|| row.get::<_, Option<i64>>(12).ok().flatten().map(|v| v.to_string())),
        "github_repo": repo_id,
        "assignee_agent_id": assigned_agent_id,
        "metadata_json": metadata_raw,
        "description": description,
        "blocked_reason": blocked_reason,
        "review_notes": review_notes,
        "review_status": review_status,
        "started_at": started_at,
        "requested_at": requested_at,
        "completed_at": completed_at,
        "pipeline_stage_id": pipeline_stage_id,
        "owner_agent_id": owner_agent_id,
        "requester_agent_id": requester_agent_id,
        "parent_card_id": parent_card_id,
        "sort_order": sort_order,
        "depth": depth,
        "review_entered_at": review_entered_at,
        "latest_dispatch_status": latest_dispatch_status.clone(),
        "latest_dispatch_title": row.get::<_, Option<String>>(15).unwrap_or(None),
        "latest_dispatch_type": latest_dispatch_type.clone(),
        "latest_dispatch_result_summary": latest_dispatch_result_summary,
        "latest_dispatch_chain_depth": row.get::<_, Option<i64>>(16).unwrap_or(None),
        "child_count": 0,
    }))
}
