use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;

use crate::db::kanban::{self, ListCardsFilter};
use crate::db::kanban_cards::KanbanCardRecord;
use crate::services::service_error::{ErrorCode, ServiceError, ServiceResult};
use crate::utils::github_links::{
    normalize_optional_github_issue_url, normalize_optional_github_repo_id,
};

#[derive(Debug, Serialize)]
pub struct ListCardsResponse {
    pub cards: Vec<KanbanCardView>,
}

#[derive(Debug, Serialize)]
pub struct KanbanCardView {
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
    pub metadata: Value,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub github_repo: Option<String>,
    pub assignee_agent_id: Option<String>,
    pub metadata_json: Option<String>,
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
    pub child_count: i64,
}

impl From<KanbanCardRecord> for KanbanCardView {
    fn from(record: KanbanCardRecord) -> Self {
        let metadata = record
            .metadata_raw
            .as_ref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .unwrap_or(Value::Null);
        let repo_id = normalize_optional_github_repo_id(record.repo_id);
        let github_issue_url = normalize_optional_github_issue_url(
            record.github_issue_url,
            repo_id.as_deref(),
            record.github_issue_number,
        );

        Self {
            id: record.id,
            github_repo: repo_id.clone(),
            assignee_agent_id: record.assigned_agent_id.clone(),
            metadata_json: record.metadata_raw.clone(),
            metadata,
            repo_id,
            title: record.title,
            status: record.status,
            priority: record.priority,
            assigned_agent_id: record.assigned_agent_id,
            github_issue_url,
            github_issue_number: record.github_issue_number,
            latest_dispatch_id: record.latest_dispatch_id,
            review_round: record.review_round,
            created_at: record.created_at,
            updated_at: record.updated_at,
            description: record.description,
            blocked_reason: record.blocked_reason,
            review_notes: record.review_notes,
            review_status: record.review_status,
            started_at: record.started_at,
            requested_at: record.requested_at,
            completed_at: record.completed_at,
            pipeline_stage_id: record.pipeline_stage_id,
            owner_agent_id: record.owner_agent_id,
            requester_agent_id: record.requester_agent_id,
            parent_card_id: record.parent_card_id,
            sort_order: record.sort_order,
            depth: record.depth,
            latest_dispatch_status: record.latest_dispatch_status,
            latest_dispatch_title: record.latest_dispatch_title,
            latest_dispatch_type: record.latest_dispatch_type,
            latest_dispatch_result_summary: record.latest_dispatch_result_summary,
            latest_dispatch_chain_depth: record.latest_dispatch_chain_depth,
            child_count: 0,
        }
    }
}

#[derive(Clone)]
pub struct KanbanService {
    pg_pool: Option<PgPool>,
}

#[derive(Debug, Clone, Default)]
pub struct ListCardsInput {
    pub status: Option<String>,
    pub repo_id: Option<String>,
    pub assigned_agent_id: Option<String>,
}

impl KanbanService {
    pub fn new(pg_pool: Option<PgPool>) -> Self {
        Self { pg_pool }
    }

    pub async fn list_cards(&self, input: ListCardsInput) -> ServiceResult<ListCardsResponse> {
        let pool = self.pg_pool.as_ref().ok_or_else(|| {
            ServiceError::internal("postgres pool unavailable for list_cards")
                .with_code(ErrorCode::Database)
                .with_operation("list_cards.pg_pool")
        })?;
        let registered_repo_ids =
            kanban::list_registered_repo_ids_pg(pool)
                .await
                .map_err(|error| {
                    ServiceError::internal(error)
                        .with_code(ErrorCode::Database)
                        .with_operation("list_cards.list_registered_repo_ids_pg")
                })?;
        let records = kanban::list_cards_pg(
            pool,
            &ListCardsFilter {
                status: input.status,
                repo_id: input.repo_id,
                assigned_agent_id: input.assigned_agent_id,
            },
            &registered_repo_ids,
        )
        .await
        .map_err(|error| {
            ServiceError::internal(error)
                .with_code(ErrorCode::Database)
                .with_operation("list_cards.query_pg")
        })?;

        Ok(ListCardsResponse {
            cards: records.into_iter().map(KanbanCardView::from).collect(),
        })
    }
}

// ── Request auth / identity helpers ─────────────────────────────
//
// Relocated from `server/routes/kanban.rs` (#3037 service→server backflow).
// These resolve the requesting agent and gate mutations behind the explicit
// Bearer token. They depend only on config + db (lower layers), so they belong
// in the services layer; server routes call them via `crate::services::kanban`.

use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use serde_json::json;

fn trimmed_header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(crate) fn require_explicit_bearer_token(
    headers: &HeaderMap,
    operation: &str,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let config = crate::config::load_graceful();
    if let Some(expected_token) = config.server.auth_token.as_deref() {
        if !expected_token.is_empty() {
            let provided = trimmed_header_value(headers, "authorization")
                .and_then(|value| value.strip_prefix("Bearer "))
                .map(str::trim);
            if !provided
                .map(|token| crate::utils::auth::constant_time_token_eq(expected_token, token))
                .unwrap_or(false)
            {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": format!("{operation} requires explicit Bearer token")})),
                ));
            }
        }
    }

    if let Some(expected_channel_id) = config
        .kanban
        .manager_channel_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let provided_channel_id = trimmed_header_value(headers, "x-channel-id");
        if provided_channel_id != Some(expected_channel_id) {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": format!("{operation} requires PMD channel authorization")})),
            ));
        }
    }

    Ok(())
}

async fn resolve_agent_id_from_channel_id_with_pg(
    pool: &PgPool,
    channel_id: &str,
) -> Option<String> {
    crate::db::kanban_cards::resolve_agent_id_from_channel_id_with_pg(pool, channel_id).await
}

/// Resolves the caller's requested agent scope from API headers.
///
/// This is not authentication: API clients self-assert `x-agent-id` /
/// `x-channel-id`, and the returned value is spoofable unless a future auth
/// layer binds the request to a verified principal.
pub(crate) async fn resolve_requesting_agent_id_with_pg(
    pool: &PgPool,
    headers: &HeaderMap,
) -> Option<String> {
    if let Some(agent_id) = trimmed_header_value(headers, "x-agent-id") {
        // Preserve the repository-wide self-asserted caller model: a raw
        // `x-agent-id` is accepted even if it does not resolve to an agents row.
        return crate::db::kanban_cards::resolve_existing_agent_id_with_pg(pool, agent_id)
            .await
            .or_else(|| Some(agent_id.to_string()));
    }

    match trimmed_header_value(headers, "x-channel-id") {
        Some(channel_id) => resolve_agent_id_from_channel_id_with_pg(pool, channel_id).await,
        None => None,
    }
}
