use super::*;

pub(super) const RESET_GLOBAL_CONFIRMATION_TOKEN: &str = "confirm-global-reset";

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct GenerateEntryBody {
    pub issue_number: i64,
    pub batch_phase: Option<i64>,
    pub thread_group: Option<i64>,
    /// User-facing phase-gate kind id (#2125). Must match one of the ids
    /// in `/api/queue/phase-gates/catalog`. When omitted, the catalog's
    /// `default_kind` is used at status-response time.
    pub phase_gate_kind: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GenerateBody {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub auto_assign_agent: Option<bool>,
    pub issue_numbers: Option<Vec<i64>>,
    pub entries: Option<Vec<GenerateEntryBody>>,
    pub review_mode: Option<String>,
    // Legacy compatibility only. Accepted from callers, but ignored.
    #[allow(dead_code)]
    pub mode: Option<String>,
    pub unified_thread: Option<bool>,
    // Legacy compatibility only. Accepted from callers, but ignored.
    #[allow(dead_code)]
    pub parallel: Option<bool>,
    pub max_concurrent_threads: Option<i64>,
    pub force: Option<bool>,
    // Legacy compatibility only. Accepted from callers, but ignored.
    #[allow(dead_code)]
    pub max_concurrent_per_agent: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ActivateBody {
    pub run_id: Option<String>,
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub thread_group: Option<i64>,
    pub unified_thread: Option<bool>,
    /// Internal-only: continue only already-active runs, never promote generated drafts.
    pub active_only: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct StatusQuery {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ReorderBody {
    pub ordered_ids: Vec<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateRunBody {
    pub status: Option<String>,
    pub unified_thread: Option<bool>,
    pub deploy_phases: Option<Vec<i64>>,
    pub max_concurrent_threads: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateEntryBody {
    pub thread_group: Option<i64>,
    pub priority_rank: Option<i64>,
    pub batch_phase: Option<i64>,
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RebindSlotBody {
    pub run_id: String,
    pub thread_group: i64,
}

#[derive(Debug, Default, Deserialize)]
pub struct RepairPhaseGateBody {
    pub phase: Option<i64>,
    pub dispatch_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AddRunEntryBody {
    pub issue_number: i64,
    pub thread_group: Option<i64>,
    pub batch_phase: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResetBody {
    pub agent_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResetGlobalBody {
    pub confirmation_token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PauseBody {
    pub force: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub struct CancelQuery {
    pub run_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct GenerateCandidate {
    pub(super) card_id: String,
    pub(super) agent_id: String,
    pub(super) priority: String,
    pub(super) description: Option<String>,
    pub(super) metadata: Option<String>,
    pub(super) github_issue_number: Option<i64>,
}

#[derive(Debug, Clone)]
pub(super) struct PlannedEntry {
    pub(super) card_idx: usize,
    pub(super) thread_group: i64,
    pub(super) priority_rank: i64,
    pub(super) batch_phase: i64,
    pub(super) reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct DependencyParseResult {
    pub(super) numbers: Vec<i64>,
    pub(super) signals: Vec<String>,
}

pub(super) const AUTO_QUEUE_REVIEW_MODE_ENABLED: &str = "enabled";
pub(super) const AUTO_QUEUE_REVIEW_MODE_DISABLED: &str = "disabled";
