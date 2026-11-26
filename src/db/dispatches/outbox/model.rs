use chrono::{DateTime, Utc};
use serde_json::Value;

/// Tuple shape returned by `claim_pending_dispatch_outbox_batch_pg`.
/// Mirrors the (id, dispatch_id, action, agent_id, card_id, title,
/// retry_count, required_capabilities, claim_owner, claimed_at) column layout
/// of `dispatch_outbox`.
pub type DispatchOutboxRow = (
    i64,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    Option<Value>,
    String,
    DateTime<Utc>,
);

#[derive(Clone, Debug)]
pub struct DispatchOutboxClaimCandidate {
    pub id: i64,
    pub dispatch_id: String,
    pub(crate) action: String,
    pub(crate) agent_id: Option<String>,
    pub(crate) card_id: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) retry_count: i64,
    pub required_capabilities: Option<Value>,
}

impl DispatchOutboxClaimCandidate {
    pub fn into_outbox_row(
        self,
        claim_owner: String,
        claimed_at: DateTime<Utc>,
    ) -> DispatchOutboxRow {
        (
            self.id,
            self.dispatch_id,
            self.action,
            self.agent_id,
            self.card_id,
            self.title,
            self.retry_count,
            self.required_capabilities,
            claim_owner,
            claimed_at,
        )
    }
}

#[derive(Clone, Debug)]
pub struct StaleDispatchOutboxClaimOwnerCandidate {
    pub id: i64,
    pub dispatch_id: String,
    pub action: String,
    pub required_capabilities: Option<Value>,
    pub stale_claim_owner: String,
    pub stale_owner_last_heartbeat_at: Option<DateTime<Utc>>,
}

/// Snapshot of a completed dispatch row used to build followup summaries.
#[derive(Clone, Debug)]
pub struct CompletedDispatchInfo {
    pub dispatch_type: String,
    pub status: String,
    pub card_id: String,
    pub result_json: Option<String>,
    pub context_json: Option<String>,
    pub thread_id: Option<String>,
    pub(crate) duration_seconds: Option<i64>,
}
