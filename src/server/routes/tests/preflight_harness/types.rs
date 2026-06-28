use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PreflightFixture {
    pub(crate) fixture_id: String,
    pub(crate) repo: String,
    pub(crate) group: String,
    pub(crate) pipeline_id: String,
    #[serde(default = "default_scenario_kind")]
    pub(crate) scenario_kind: String,
    #[serde(default = "default_agent_mode")]
    pub(crate) agent_mode: String,
    pub(crate) agent_id: String,
    #[serde(default)]
    pub(crate) agent_name: Option<String>,
    #[serde(default = "default_auth_token")]
    pub(crate) auth_token: String,
    #[serde(default = "default_review_mode")]
    pub(crate) review_mode: String,
    #[serde(default = "default_max_concurrent_threads")]
    pub(crate) max_concurrent_threads: i64,
    #[serde(default)]
    pub(crate) pipeline_config: Option<Value>,
    #[serde(default)]
    pub(crate) required_transitions: Vec<PreflightTransitionExpectation>,
    #[serde(default)]
    pub(crate) expected_preflight_failures: Vec<String>,
    pub(crate) entries: Vec<PreflightFixtureEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PreflightFixtureEntry {
    pub(crate) issue_number: i64,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default = "default_priority")]
    pub(crate) priority: String,
    #[serde(default)]
    pub(crate) batch_phase: Option<i64>,
    #[serde(default)]
    pub(crate) thread_group: Option<i64>,
    #[serde(default = "default_phase_gate_kind")]
    pub(crate) phase_gate_kind: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PreflightTransitionExpectation {
    pub(crate) from: String,
    pub(crate) to: String,
    #[serde(default = "default_transition_supported")]
    pub(crate) supported: bool,
    #[serde(default)]
    pub(crate) label: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct PreflightIdentity {
    pub(crate) fixture_id: String,
    pub(crate) repo: String,
    pub(crate) group: String,
    pub(crate) pipeline_id: String,
    pub(crate) scenario_kind: String,
    pub(crate) agent_id: String,
    pub(crate) agent_mode: String,
    pub(crate) review_mode: String,
    pub(crate) real_provider_contacted: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct EndpointObservation {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) status: u16,
    pub(crate) ok: bool,
    pub(crate) body: Value,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub(crate) struct SlotId {
    pub(crate) agent_id: String,
    pub(crate) slot_index: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct EntryTerminal {
    pub(crate) id: String,
    pub(crate) status: String,
    pub(crate) dispatch_id: Option<String>,
    pub(crate) slot_index: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct DispatchTerminal {
    pub(crate) id: String,
    pub(crate) status: String,
    pub(crate) dispatch_type: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct TerminalStatus {
    pub(crate) run_status: Option<String>,
    pub(crate) entries: Vec<EntryTerminal>,
    pub(crate) dispatches: Vec<DispatchTerminal>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct SafetyProof {
    pub(crate) production_card_count: i64,
    pub(crate) github_pr_tracking_count: i64,
    pub(crate) live_session_count: i64,
    pub(crate) dispatch_delivery_sent_count: i64,
    pub(crate) message_outbox_count: i64,
    pub(crate) message_outbox_rows: Vec<Value>,
    pub(crate) dispatch_outbox_notify_count: i64,
    pub(crate) dispatch_outbox_notify_rows: Vec<Value>,
    pub(crate) worktree_or_branch_context_count: i64,
    pub(crate) worktree_or_branch_context_rows: Vec<Value>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct PreflightReport {
    pub(crate) identity: PreflightIdentity,
    pub(crate) run_id: Option<String>,
    pub(crate) entry_ids: Vec<String>,
    pub(crate) dispatch_ids: Vec<String>,
    pub(crate) slot_ids: Vec<SlotId>,
    pub(crate) phase_gate_state: Vec<Value>,
    pub(crate) scenario_observations: Vec<Value>,
    pub(crate) preflight_failure_reasons: Vec<String>,
    pub(crate) terminal_status: TerminalStatus,
    pub(crate) raw_failure_reasons: Vec<String>,
    pub(crate) endpoint_observations: Vec<EndpointObservation>,
    pub(crate) safety: SafetyProof,
    pub(crate) status_inflight: Option<Value>,
    pub(crate) status_final: Option<Value>,
    pub(crate) history_final: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PreflightSnapshot {
    pub(crate) run_id: Option<String>,
    pub(crate) run_status: Option<String>,
    pub(crate) entries: Vec<EntrySnapshot>,
    pub(crate) dispatches: Vec<DispatchSnapshot>,
    pub(crate) reserved_slots: Vec<SlotId>,
    pub(crate) phase_gates: Vec<Value>,
    pub(crate) diagnostics: Vec<Value>,
    pub(crate) safety: SafetyProof,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct EntrySnapshot {
    pub(crate) id: String,
    pub(crate) status: String,
    pub(crate) dispatch_id: Option<String>,
    pub(crate) slot_index: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DispatchSnapshot {
    pub(crate) id: String,
    pub(crate) status: String,
    pub(crate) dispatch_type: Option<String>,
}

impl PreflightReport {
    pub(crate) fn new(fixture: &PreflightFixture) -> Self {
        Self {
            identity: PreflightIdentity {
                fixture_id: fixture.fixture_id.clone(),
                repo: fixture.repo.clone(),
                group: fixture.group.clone(),
                pipeline_id: fixture.pipeline_id.clone(),
                scenario_kind: fixture.scenario_kind.clone(),
                agent_id: fixture.agent_id.clone(),
                agent_mode: fixture.agent_mode.clone(),
                review_mode: fixture.review_mode.clone(),
                real_provider_contacted: false,
            },
            ..Self::default()
        }
    }
}

fn default_auth_token() -> String {
    "auto-queue-preflight-token".to_string()
}

fn default_review_mode() -> String {
    "disabled".to_string()
}

fn default_scenario_kind() -> String {
    "basic_roundtrip".to_string()
}

fn default_agent_mode() -> String {
    "none".to_string()
}

fn default_priority() -> String {
    "high".to_string()
}

fn default_phase_gate_kind() -> String {
    "pr-confirm".to_string()
}

fn default_max_concurrent_threads() -> i64 {
    1
}

fn default_transition_supported() -> bool {
    true
}
