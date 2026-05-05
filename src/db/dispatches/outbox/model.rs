use serde_json::Value;

/// Tuple shape returned by `claim_pending_dispatch_outbox_batch_pg`.
/// Mirrors the (id, dispatch_id, action, agent_id, card_id, title,
/// retry_count, required_capabilities) column layout of `dispatch_outbox`.
pub(crate) type DispatchOutboxRow = (
    i64,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    Option<Value>,
);

#[derive(Clone, Debug)]
pub(crate) struct DispatchOutboxClaimCandidate {
    pub(crate) id: i64,
    pub(crate) dispatch_id: String,
    pub(crate) action: String,
    pub(crate) agent_id: Option<String>,
    pub(crate) card_id: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) retry_count: i64,
    pub(crate) required_capabilities: Option<Value>,
}

impl DispatchOutboxClaimCandidate {
    pub(crate) fn into_outbox_row(self) -> DispatchOutboxRow {
        (
            self.id,
            self.dispatch_id,
            self.action,
            self.agent_id,
            self.card_id,
            self.title,
            self.retry_count,
            self.required_capabilities,
        )
    }
}

/// Snapshot of a completed dispatch row used to build followup summaries.
#[derive(Clone, Debug)]
pub(crate) struct CompletedDispatchInfo {
    pub(crate) dispatch_type: String,
    pub(crate) status: String,
    pub(crate) card_id: String,
    pub(crate) result_json: Option<String>,
    pub(crate) context_json: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) duration_seconds: Option<i64>,
}
