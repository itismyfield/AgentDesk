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

/// Reset scope for `POST /api/queue/reset` (#4880). `run_id` is mandatory so a
/// reset can never widen past the run the operator selected; `agent_id` and
/// `repo` only narrow it further. Unknown fields are rejected because the
/// pre-#4880 body silently dropped the dashboard's `run_id`/`repo`, turning a
/// run-scoped request into an agent-wide wipe.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetBody {
    pub run_id: String,
    pub agent_id: Option<String>,
    pub repo: Option<String>,
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

#[cfg(test)]
mod reset_body_tests {
    use super::ResetBody;

    fn parse(raw: &str) -> Result<ResetBody, serde_json::Error> {
        serde_json::from_str::<ResetBody>(raw)
    }

    #[test]
    fn parses_full_run_scope() {
        let body = parse(r#"{"run_id":"run-x","agent_id":"agent-a","repo":"owner/repo-a"}"#)
            .expect("full scope parses");
        assert_eq!(body.run_id, "run-x");
        assert_eq!(body.agent_id.as_deref(), Some("agent-a"));
        assert_eq!(body.repo.as_deref(), Some("owner/repo-a"));
    }

    #[test]
    fn parses_run_id_only_scope() {
        let body = parse(r#"{"run_id":"run-x"}"#).expect("run_id-only scope parses");
        assert_eq!(body.run_id, "run-x");
        assert!(body.agent_id.is_none());
        assert!(body.repo.is_none());
    }

    /// #4880: the pre-fix body accepted `{"agent_id": ...}` and silently
    /// dropped `run_id`/`repo`, so an agent-wide wipe looked like a run reset.
    #[test]
    fn rejects_missing_run_id() {
        let error = parse(r#"{"agent_id":"agent-a","repo":"owner/repo-a"}"#)
            .expect_err("agent-only body must be rejected");
        assert!(
            error.to_string().contains("run_id"),
            "expected a missing-run_id error, got: {error}"
        );
    }

    /// A scope field the server does not understand must fail loudly rather
    /// than widen the blast radius by being ignored.
    #[test]
    fn rejects_unknown_scope_field() {
        let error = parse(r#"{"run_id":"run-x","thread_group":3}"#)
            .expect_err("unknown field must be rejected");
        assert!(
            error.to_string().contains("thread_group"),
            "expected an unknown-field error, got: {error}"
        );
    }

    #[test]
    fn rejects_empty_body() {
        assert!(parse("{}").is_err(), "empty body must not reset anything");
    }
}
