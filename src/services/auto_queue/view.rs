use super::*;

#[derive(Debug, Serialize)]
pub(super) struct AutoQueueHistoryRun {
    pub(super) id: String,
    pub(super) repo: Option<String>,
    pub(super) agent_id: Option<String>,
    pub(super) status: String,
    pub(super) timeout_minutes: i64,
    pub(super) timeout_exceeded: bool,
    pub(super) timeout_overrun_ms: i64,
    pub(super) created_at: i64,
    pub(super) completed_at: Option<i64>,
    pub(super) duration_ms: i64,
    pub(super) entry_count: i64,
    pub(super) done_count: i64,
    pub(super) skipped_count: i64,
    pub(super) pending_count: i64,
    pub(super) dispatched_count: i64,
    pub(super) success_rate: f64,
    pub(super) failure_rate: f64,
}

#[derive(Debug, Serialize)]
pub(super) struct AutoQueueHistorySummary {
    pub(super) total_runs: usize,
    pub(super) completed_runs: usize,
    pub(super) success_rate: f64,
    pub(super) failure_rate: f64,
}

#[derive(Debug, Clone)]
pub(super) struct GroupPlan {
    pub(super) entries: Vec<PlannedEntry>,
    pub(super) thread_group_count: i64,
    pub(super) recommended_parallel_threads: i64,
    pub(super) dependency_edges: usize,
    pub(super) similarity_edges: usize,
    pub(super) path_backed_card_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GroupKind {
    Independent,
    Similarity,
    Dependency,
    Mixed,
}

#[derive(Debug, Clone)]
pub(super) struct RequestedGenerateEntry {
    pub(super) issue_number: i64,
    pub(super) batch_phase: i64,
    pub(super) thread_group: Option<i64>,
    /// Validated phase-gate kind id (#2125). `None` falls back to catalog
    /// `default_kind` at status-response time.
    pub(super) phase_gate_kind: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct ResolvedDispatchCard {
    pub(super) issue_number: i64,
    pub(super) card_id: String,
    pub(super) repo_id: Option<String>,
    pub(super) status: String,
    pub(super) assigned_agent_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct ActivateCardState {
    pub(super) status: String,
    pub(super) title: String,
    pub(super) latest_dispatch_id: Option<String>,
    pub(super) latest_dispatch_status: Option<String>,
    /// #3605 (codex R2): dispatch_type of `latest_dispatch_id`. Needed so the
    /// auto-queue attach decision can distinguish an attachable IMPLEMENTATION
    /// dispatch from an inert side-path (consultation / scope-assessment) that
    /// merely became latest_dispatch_id. `None` when there is no latest dispatch
    /// or it is not pending/dispatched.
    pub(super) latest_dispatch_type: Option<String>,
    pub(super) entry_status: String,
    pub(super) repo_id: Option<String>,
    pub(super) assigned_agent_id: Option<String>,
    /// #3594 (T3): whether the card's `scope_assessment_status` metadata is
    /// exactly `"pending"`. While a scope-assessment is in flight the activate
    /// loop must NOT create an implementation dispatch yet — the depth that
    /// gates the flow (direct / plan_only / full) is not known until the
    /// scope-assessment completes, and scope-assessment is a side-path that is
    /// deliberately excluded from `has_active_dispatch()` (so activate would
    /// otherwise fall through and create impl, bypassing the gate). This is the
    /// only "pending" guard: a card that never ran scope-assessment has NO
    /// `scope_assessment_status` key → `false` here → no behavior change (the
    /// core no-regression invariant — absent ≠ pending).
    pub(super) scope_assessment_pending: bool,
}

impl ActivateCardState {
    /// Whether the card has an active, ATTACHABLE implementation dispatch — i.e.
    /// one the auto-queue activate/restore paths may bind a pending entry to
    /// instead of creating a new dispatch.
    ///
    /// #3605 (codex R2) ROOT FIX — side-path hijacking: side-path dispatches
    /// (consultation, scope-assessment) deliberately become `latest_dispatch_id`
    /// (engine::transition::decide_dispatch_attached) but are inert — they record
    /// info about the card without ever advancing/completing it. They must NOT be
    /// treated as an attachable active dispatch: attaching a pending auto_queue
    /// entry to a side-path leaves the entry bound to a dispatch whose terminal
    /// completion is skipped (dispatch_status::should_skip_auto_queue_terminal_sync),
    /// so the entry sticks in `dispatched` and the real implementation dispatch is
    /// never created (stale recovery only reclaims cancelled/failed). Excluding
    /// side-paths here makes activate fall through to creating a proper
    /// implementation dispatch.
    ///
    /// consultation already avoided this only because its dedicated JS path
    /// (auto-queue-error-recovery `_createConsultationDispatch` →
    /// `record_consultation_dispatch_on_pg`) atomically marks the bound entry
    /// `dispatched` at creation, so the entry is never pending when activate runs.
    /// scope-assessment's JS path writes card metadata only and never touches the
    /// entry, exposing this latent gap; the guard now closes it for the whole
    /// side-path set.
    ///
    /// NB: this is the auto-queue `ActivateCardState` predicate only. It is
    /// independent of the FSM review gate `has_active_dispatch`
    /// (engine::transition::GateSnapshot / kanban::transition_core), which is a
    /// separate count-based query and is intentionally left unchanged.
    pub(super) fn has_active_dispatch(&self) -> bool {
        self.latest_dispatch_id.is_some()
            && matches!(
                self.latest_dispatch_status.as_deref(),
                Some("pending") | Some("dispatched")
            )
            && !crate::dispatch::dispatch_is_side_path(self.latest_dispatch_type.as_deref())
    }

    /// #3594 (T3): whether the activate loop must defer creating an
    /// implementation dispatch because a scope-assessment is still in flight for
    /// this card (`scope_assessment_status == "pending"`). Once the
    /// scope-assessment completes, the policy layer (kanban-rules onDispatch
    /// completed) flips the status to `"completed"` and creates the depth-gated
    /// next dispatch (impl directly for `direct`, or `plan`), so this returns
    /// `false` and activate resumes normally on the next tick. Strictly
    /// `"pending"`-only: a card with no scope-assessment never blocks here.
    pub(super) fn scope_assessment_pending(&self) -> bool {
        self.scope_assessment_pending
    }
}

#[derive(Debug, Clone)]
pub(super) struct RestoreEntryRecord {
    pub(super) entry_id: String,
    pub(super) card_id: String,
    pub(super) agent_id: String,
    pub(super) thread_group: i64,
}

#[derive(Debug, Default)]
pub(super) struct RestoreRunCounts {
    pub(super) restored_pending: usize,
    pub(super) restored_done: usize,
    pub(super) restored_dispatched: usize,
    pub(super) rebound_slots: usize,
    pub(super) created_dispatches: usize,
    pub(super) unbound_dispatches: usize,
}

pub(super) const RUN_STATUS_RESTORING: &str = "restoring";

#[derive(Debug, Clone)]
pub(super) enum RestoreEntryDecision {
    Pending,
    Done,
    ExistingDispatch { title: String },
    NewDispatch { title: String },
}

#[derive(Debug, Clone)]
pub(super) struct RestoreDispatchCandidate {
    pub(super) entry: RestoreEntryRecord,
    pub(super) title: String,
}

#[derive(Debug, Default)]
pub(super) struct RestoreDispatchAttemptResult {
    pub(super) dispatched: bool,
    pub(super) created_dispatch: bool,
    pub(super) rebound_slot: bool,
    pub(super) unbound_dispatch: bool,
}

pub(super) async fn load_activate_card_state_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
    entry_id: &str,
) -> Result<ActivateCardState, String> {
    let row = sqlx::query(
        "SELECT status, title, latest_dispatch_id, repo_id, assigned_agent_id,
                COALESCE(metadata->>'scope_assessment_status', '') = 'pending'
                    AS scope_assessment_pending
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres card {card_id}: {error}"))?
    .ok_or_else(|| format!("postgres card {card_id} not found"))?;

    let mut latest_dispatch_id: Option<String> = row
        .try_get("latest_dispatch_id")
        .map_err(|error| format!("decode latest_dispatch_id for {card_id}: {error}"))?;
    // #3605 (codex R2): also load the dispatch_type so has_active_dispatch() can
    // exclude inert side-paths (consultation, scope-assessment) from the
    // attachable-implementation-dispatch decision.
    let mut latest_dispatch_status: Option<String> = None;
    let mut latest_dispatch_type: Option<String> = None;
    if let Some(dispatch_id) = latest_dispatch_id.as_deref() {
        if let Some(dispatch_row) =
            sqlx::query("SELECT status, dispatch_type FROM task_dispatches WHERE id = $1")
                .bind(dispatch_id)
                .fetch_optional(pool)
                .await
                .map_err(|error| {
                    format!("load postgres dispatch status for {dispatch_id}: {error}")
                })?
        {
            latest_dispatch_status = dispatch_row
                .try_get("status")
                .map_err(|error| format!("decode dispatch status for {dispatch_id}: {error}"))?;
            latest_dispatch_type = dispatch_row
                .try_get("dispatch_type")
                .map_err(|error| format!("decode dispatch type for {dispatch_id}: {error}"))?;
        }
    }
    if !matches!(
        latest_dispatch_status.as_deref(),
        Some("pending") | Some("dispatched")
    ) {
        if let Some(row) = sqlx::query(
            "SELECT td.id, td.status, td.dispatch_type
             FROM sessions s
             JOIN task_dispatches td ON td.id = s.active_dispatch_id
             WHERE td.kanban_card_id = $1
               AND td.status IN ('pending', 'dispatched')
               AND COALESCE(s.status, '') NOT IN ('disconnected', 'completed', 'failed', 'cancelled')
             ORDER BY s.last_heartbeat DESC NULLS LAST, td.created_at DESC
             LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load postgres live session dispatch for {card_id}: {error}"))?
        {
            latest_dispatch_id = Some(row.try_get("id").map_err(|error| {
                format!("decode live session dispatch id for {card_id}: {error}")
            })?);
            latest_dispatch_status = Some(row.try_get("status").map_err(|error| {
                format!("decode live session dispatch status for {card_id}: {error}")
            })?);
            latest_dispatch_type = row.try_get("dispatch_type").map_err(|error| {
                format!("decode live session dispatch type for {card_id}: {error}")
            })?;
        }
    }
    let entry_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_entries WHERE id = $1")
            .bind(entry_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| {
                format!("load postgres auto-queue entry status for {entry_id}: {error}")
            })?
            .unwrap_or_else(|| "pending".to_string());

    Ok(ActivateCardState {
        status: row
            .try_get("status")
            .map_err(|error| format!("decode status for {card_id}: {error}"))?,
        title: row
            .try_get("title")
            .map_err(|error| format!("decode title for {card_id}: {error}"))?,
        latest_dispatch_id,
        latest_dispatch_status,
        latest_dispatch_type,
        entry_status,
        repo_id: row
            .try_get("repo_id")
            .map_err(|error| format!("decode repo_id for {card_id}: {error}"))?,
        assigned_agent_id: row
            .try_get("assigned_agent_id")
            .map_err(|error| format!("decode assigned_agent_id for {card_id}: {error}"))?,
        scope_assessment_pending: row
            .try_get("scope_assessment_pending")
            .map_err(|error| format!("decode scope_assessment_pending for {card_id}: {error}"))?,
    })
}

pub(super) async fn resolve_activate_pipeline_pg(
    pool: &sqlx::PgPool,
    repo_id: Option<&str>,
    agent_id: Option<&str>,
) -> Result<crate::pipeline::PipelineConfig, String> {
    crate::pipeline::ensure_loaded();

    let repo_override = if let Some(repo_id) = repo_id {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT pipeline_config::text AS pipeline_config FROM github_repos WHERE id = $1",
        )
        .bind(repo_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load repo pipeline override for {repo_id}: {error}"))?
        .flatten()
        .map(|json| crate::pipeline::parse_override(&json))
        .transpose()
        .map_err(|error| format!("parse repo pipeline override for {repo_id}: {error}"))?
        .flatten()
    } else {
        None
    };

    let agent_override = if let Some(agent_id) = agent_id {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT pipeline_config::text AS pipeline_config FROM agents WHERE id = $1",
        )
        .bind(agent_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load agent pipeline override for {agent_id}: {error}"))?
        .flatten()
        .map(|json| crate::pipeline::parse_override(&json))
        .transpose()
        .map_err(|error| format!("parse agent pipeline override for {agent_id}: {error}"))?
        .flatten()
    } else {
        None
    };

    Ok(crate::pipeline::resolve(
        repo_override.as_ref(),
        agent_override.as_ref(),
    ))
}
