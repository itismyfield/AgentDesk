use serde::Serialize;
use serde_json::Value;

use crate::db::{
    Db,
    kanban::{self, KanbanCardRecord, ListCardsFilter},
};
use crate::services::service_error::{ErrorCode, ServiceError, ServiceResult};

#[derive(Clone)]
pub struct KanbanService {
    db: Db,
}

#[derive(Debug, Clone, Default)]
pub struct ListCardsInput {
    pub status: Option<String>,
    pub repo_id: Option<String>,
    pub assigned_agent_id: Option<String>,
}

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

impl KanbanService {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn list_cards(&self, input: ListCardsInput) -> ServiceResult<ListCardsResponse> {
        let conn = self.db.read_conn().map_err(|error| {
            ServiceError::internal(format!("{error}"))
                .with_code(ErrorCode::Database)
                .with_operation("list_cards.read_conn")
        })?;
        let registered_repo_ids = kanban::list_registered_repo_ids(&conn).unwrap_or_default();
        let records = kanban::list_cards(
            &conn,
            &ListCardsFilter {
                status: input.status,
                repo_id: input.repo_id,
                assigned_agent_id: input.assigned_agent_id,
            },
            &registered_repo_ids,
        )
        .map_err(|error| {
            ServiceError::internal(format!("list cards: {error}"))
                .with_code(ErrorCode::Database)
                .with_operation("list_cards.query")
        })?;

        Ok(ListCardsResponse {
            cards: records.into_iter().map(KanbanCardView::from).collect(),
        })
    }
}

impl From<KanbanCardRecord> for KanbanCardView {
    fn from(record: KanbanCardRecord) -> Self {
        let metadata = record
            .metadata_raw
            .as_ref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .unwrap_or(Value::Null);

        Self {
            id: record.id,
            github_repo: record.repo_id.clone(),
            assignee_agent_id: record.assigned_agent_id.clone(),
            metadata_json: record.metadata_raw.clone(),
            metadata,
            repo_id: record.repo_id,
            title: record.title,
            status: record.status,
            priority: record.priority,
            assigned_agent_id: record.assigned_agent_id,
            github_issue_url: record.github_issue_url,
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
