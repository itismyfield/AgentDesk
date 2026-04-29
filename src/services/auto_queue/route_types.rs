const RESET_GLOBAL_CONFIRMATION_TOKEN: &str = "confirm-global-reset";

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct GenerateEntryBody {
    pub issue_number: i64,
    pub batch_phase: Option<i64>,
    pub thread_group: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct GenerateBody {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
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
    /// Canonical: `ordered_ids` (snake_case).
    /// Legacy `orderedIds` (camelCase) still accepted via serde alias.
    #[serde(alias = "orderedIds")]
    pub ordered_ids: Vec<String>,
    /// Canonical: `agent_id` (snake_case).
    /// Legacy `agentId` (camelCase) still accepted via serde alias.
    #[serde(default, alias = "agentId")]
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
pub struct DispatchGroupBody {
    pub issues: Vec<i64>,
    pub sequential: Option<bool>,
    pub batch_phase: Option<i64>,
    pub thread_group: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct DispatchBody {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub groups: Vec<DispatchGroupBody>,
    pub review_mode: Option<String>,
    pub unified_thread: Option<bool>,
    pub activate: Option<bool>,
    pub auto_assign_agent: Option<bool>,
    pub max_concurrent_threads: Option<i64>,
    pub deploy_phases: Option<Vec<i64>>,
    pub force: Option<bool>,
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
struct GenerateCandidate {
    card_id: String,
    agent_id: String,
    priority: String,
    description: Option<String>,
    metadata: Option<String>,
    github_issue_number: Option<i64>,
}

#[derive(Debug, Clone)]
struct PlannedEntry {
    card_idx: usize,
    thread_group: i64,
    priority_rank: i64,
    batch_phase: i64,
    reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DependencyParseResult {
    numbers: Vec<i64>,
    signals: Vec<String>,
}

const AUTO_QUEUE_REVIEW_MODE_ENABLED: &str = "enabled";
const AUTO_QUEUE_REVIEW_MODE_DISABLED: &str = "disabled";
