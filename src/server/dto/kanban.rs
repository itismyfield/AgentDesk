use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::kanban_cards::KanbanCardRecord;

#[derive(Debug, Deserialize)]
pub struct ListCardsQuery {
    pub status: Option<String>,
    pub repo_id: Option<String>,
    pub assigned_agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateCardBody {
    pub title: String,
    pub repo_id: Option<String>,
    pub priority: Option<String>,
    pub github_issue_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateCardBody {
    pub title: Option<String>,
    pub status: Option<String>,
    pub priority: Option<String>,
    /// Canonical: `assignee_agent_id`.
    /// Legacy `assigned_agent_id` still accepted via serde alias during migration (#1065).
    #[serde(default, alias = "assigned_agent_id")]
    pub assignee_agent_id: Option<String>,
    pub repo_id: Option<String>,
    pub github_issue_url: Option<String>,
    pub metadata: Option<Value>,
    pub description: Option<String>,
    pub metadata_json: Option<String>,
    pub review_status: Option<String>,
    pub review_notes: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AssignCardBody {
    pub agent_id: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct RetryCardBody {
    pub assignee_agent_id: Option<String>,
    pub request_now: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct RedispatchCardBody {
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeferDodBody {
    pub items: Option<Vec<String>>,
    pub verify: Option<Vec<String>>,
    pub unverify: Option<Vec<String>>,
    pub remove: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct BulkActionBody {
    pub action: String,
    pub card_ids: Vec<String>,
    /// Target status for "transition" action (e.g. "ready", "backlog").
    pub target_status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AssignIssueBody {
    pub github_repo: String,
    pub github_issue_number: i64,
    pub github_issue_url: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub assignee_agent_id: String,
}

#[derive(Debug, Deserialize)]
pub struct PmDecisionBody {
    pub card_id: String,
    pub decision: String,
    pub comment: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RereviewBody {
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BatchRereviewBody {
    pub issues: Vec<i64>,
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ReopenBody {
    pub review_status: Option<String>,
    pub dispatch_type: Option<String>,
    pub reason: Option<String>,
    pub reset_full: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct BatchTransitionBody {
    pub issue_numbers: Option<Vec<i64>>,
    pub card_ids: Option<Vec<String>>,
    pub status: String,
    pub cancel_dispatches: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ForceTransitionBody {
    pub status: String,
    pub cancel_dispatches: Option<bool>,
    /// #1444: explicit opt-in to cancel a card's active dispatch when the
    /// target_status is `ready`. Without `force=true` (and without legacy
    /// `cancel_dispatches=true`), `/transition` returns 409 Conflict if the
    /// card already has a pending/dispatched dispatch.
    pub force: Option<bool>,
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
