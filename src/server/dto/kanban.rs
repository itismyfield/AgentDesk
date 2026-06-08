use serde::Deserialize;
use serde_json::Value;

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
pub struct ForceTransitionBody {
    pub status: String,
    pub cancel_dispatches: Option<bool>,
    /// #1444: explicit opt-in to cancel a card's active dispatch when the
    /// target_status is `ready`. Without `force=true` (and without legacy
    /// `cancel_dispatches=true`), `/transition` returns 409 Conflict if the
    /// card already has a pending/dispatched dispatch.
    pub force: Option<bool>,
}
