pub(crate) mod cancel_run;
pub(crate) mod route;
pub mod runtime;

use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row as SqlxRow};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use tracing::field::{Empty, display};

use crate::db::auto_queue::{
    self, AutoQueueRunRecord, GenerateCandidateRecord, GenerateCardFilter, StatusEntryRecord,
    StatusFilter,
};
use crate::engine::PolicyEngine;
use crate::services::service_error::{ErrorCode, ServiceError, ServiceResult};

#[derive(Debug, Clone, Default)]
pub struct AutoQueueLogContext<'a> {
    pub run_id: Option<&'a str>,
    pub entry_id: Option<&'a str>,
    pub card_id: Option<&'a str>,
    pub dispatch_id: Option<&'a str>,
    pub thread_group: Option<i64>,
    pub batch_phase: Option<i64>,
    pub slot_index: Option<i64>,
    pub agent_id: Option<&'a str>,
}

impl<'a> AutoQueueLogContext<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn run(mut self, run_id: &'a str) -> Self {
        self.run_id = Some(run_id);
        self
    }

    pub fn entry(mut self, entry_id: &'a str) -> Self {
        self.entry_id = Some(entry_id);
        self
    }

    pub fn card(mut self, card_id: &'a str) -> Self {
        self.card_id = Some(card_id);
        self
    }

    pub fn dispatch(mut self, dispatch_id: &'a str) -> Self {
        self.dispatch_id = Some(dispatch_id);
        self
    }

    pub fn maybe_dispatch(mut self, dispatch_id: Option<&'a str>) -> Self {
        self.dispatch_id = normalize_auto_queue_log_id(dispatch_id);
        self
    }

    pub fn agent(mut self, agent_id: &'a str) -> Self {
        self.agent_id = Some(agent_id);
        self
    }

    pub fn thread_group(mut self, thread_group: i64) -> Self {
        self.thread_group = Some(thread_group);
        self
    }

    pub fn batch_phase(mut self, batch_phase: i64) -> Self {
        self.batch_phase = Some(batch_phase);
        self
    }

    pub fn slot_index(mut self, slot_index: i64) -> Self {
        self.slot_index = Some(slot_index);
        self
    }

    pub fn maybe_slot_index(mut self, slot_index: Option<i64>) -> Self {
        self.slot_index = slot_index;
        self
    }
}

fn normalize_auto_queue_log_id(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

pub fn auto_queue_trace_span(action: &'static str, ctx: &AutoQueueLogContext<'_>) -> tracing::Span {
    let span = tracing::info_span!(
        "auto_queue",
        action = action,
        run_id = Empty,
        entry_id = Empty,
        card_id = Empty,
        dispatch_id = Empty,
        thread_group = Empty,
        batch_phase = Empty,
        slot_index = Empty,
        agent_id = Empty,
    );

    if let Some(run_id) = ctx.run_id {
        span.record("run_id", display(run_id));
    }
    if let Some(entry_id) = ctx.entry_id {
        span.record("entry_id", display(entry_id));
    }
    if let Some(card_id) = ctx.card_id {
        span.record("card_id", display(card_id));
    }
    if let Some(dispatch_id) = ctx.dispatch_id {
        span.record("dispatch_id", display(dispatch_id));
    }
    if let Some(thread_group) = ctx.thread_group {
        span.record("thread_group", thread_group);
    }
    if let Some(batch_phase) = ctx.batch_phase {
        span.record("batch_phase", batch_phase);
    }
    if let Some(slot_index) = ctx.slot_index {
        span.record("slot_index", slot_index);
    }
    if let Some(agent_id) = ctx.agent_id {
        span.record("agent_id", display(agent_id));
    }

    span
}

#[macro_export]
macro_rules! auto_queue_log {
    ($level:ident, $action:expr, $ctx:expr, $($arg:tt)+) => {{
        let __ctx = &$ctx;
        let __span = $crate::services::auto_queue::auto_queue_trace_span($action, __ctx);
        let __guard = __span.enter();
        tracing::$level!($($arg)+);
    }};
}

#[derive(Clone)]
pub struct AutoQueueService {
    engine: PolicyEngine,
}

#[derive(Debug, Clone, Default)]
pub struct PrepareGenerateInput {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub issue_numbers: Option<Vec<i64>>,
}

#[derive(Debug, Clone, Default)]
pub struct StatusInput {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub guild_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GenerateCandidate {
    pub card_id: String,
    pub agent_id: String,
    pub priority: String,
    pub description: Option<String>,
    pub metadata: Option<String>,
    pub github_issue_number: Option<i64>,
}

#[derive(Debug, Serialize, Default)]
pub struct AutoQueueStatusResponse {
    pub run: Option<AutoQueueRunView>,
    pub entries: Vec<AutoQueueStatusEntryView>,
    pub agents: BTreeMap<String, AutoQueueStatusCounts>,
    pub thread_groups: BTreeMap<String, AutoQueueThreadGroupView>,
    pub phase_gates: Vec<PhaseGateView>,
    #[serde(default, skip_serializing_if = "AutoQueueStatusDiagnostics::is_empty")]
    pub diagnostics: AutoQueueStatusDiagnostics,
}

#[derive(Debug, Serialize, Default)]
pub struct AutoQueueStatusDiagnostics {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slot_invariant_violations: Vec<AutoQueueSlotInvariantViolation>,
}

impl AutoQueueStatusDiagnostics {
    fn is_empty(&self) -> bool {
        self.slot_invariant_violations.is_empty()
    }
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct AutoQueueSlotInvariantViolation {
    pub invariant: String,
    pub run_id: String,
    pub agent_id: String,
    pub slot_index: i64,
    pub entry_ids: Vec<String>,
    pub dispatch_ids: Vec<String>,
    pub entries: Vec<AutoQueueSlotInvariantEntryDiagnostic>,
    pub recovery: AutoQueueSlotInvariantRecovery,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct AutoQueueSlotInvariantEntryDiagnostic {
    pub entry_id: String,
    pub card_id: String,
    pub dispatch_ids: Vec<String>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct AutoQueueSlotInvariantRecovery {
    pub summary: String,
    pub status_endpoint: String,
    pub rebind_slot_endpoint: String,
    pub reset_slot_thread_endpoint: String,
    pub skip_entry_endpoint_template: String,
}

#[derive(Debug, Serialize)]
pub struct AutoQueueRunView {
    pub id: String,
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub review_mode: String,
    pub status: String,
    pub timeout_minutes: i64,
    pub ai_model: Option<String>,
    pub ai_rationale: Option<String>,
    pub created_at: i64,
    pub completed_at: Option<i64>,
    pub unified_thread: bool,
    pub unified_thread_id: Option<String>,
    pub max_concurrent_threads: i64,
    pub thread_group_count: i64,
}

#[derive(Debug, Serialize)]
pub struct PhaseGateView {
    pub id: i64,
    pub phase: i64,
    pub status: String,
    pub dispatch_id: Option<String>,
    pub failure_reason: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AutoQueueStatusEntryView {
    pub id: String,
    pub agent_id: String,
    pub card_id: String,
    #[serde(skip)]
    pub dispatch_id: Option<String>,
    pub priority_rank: i64,
    pub reason: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub dispatched_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub card_title: Option<String>,
    pub github_issue_number: Option<i64>,
    pub github_repo: Option<String>,
    pub retry_count: i64,
    pub thread_group: i64,
    pub slot_index: Option<i64>,
    pub batch_phase: i64,
    pub dispatch_history: Vec<String>,
    pub thread_links: Vec<ThreadLinkView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card_status: Option<String>,
    pub review_round: i64,
}

#[derive(Debug, Serialize, Default)]
pub struct AutoQueueStatusCounts {
    pub pending: i64,
    pub dispatched: i64,
    pub done: i64,
    pub skipped: i64,
    pub failed: i64,
}

#[derive(Debug, Serialize, Default)]
pub struct AutoQueueThreadGroupView {
    pub pending: i64,
    pub dispatched: i64,
    pub done: i64,
    pub skipped: i64,
    pub failed: i64,
    pub entries: Vec<AutoQueueThreadGroupEntryView>,
    pub reason: Option<String>,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct AutoQueueThreadGroupEntryView {
    pub id: String,
    pub card_id: String,
    pub status: String,
    pub github_issue_number: Option<i64>,
    pub batch_phase: i64,
}

#[derive(Debug, Serialize)]
pub struct ThreadLinkView {
    pub role: String,
    pub label: String,
    pub channel_id: Option<String>,
    pub thread_id: String,
    pub url: Option<String>,
}

#[derive(Debug, Clone)]
struct ThreadLinkCandidate {
    label: String,
    channel_id: u64,
}

#[derive(Debug, Clone)]
struct DispatchThreadFact {
    dispatch_type: Option<String>,
    status: String,
    thread_id: String,
}

impl AutoQueueService {
    pub fn new(engine: PolicyEngine) -> Self {
        Self { engine }
    }

    pub async fn status_with_pg(
        &self,
        pool: &PgPool,
        input: StatusInput,
    ) -> ServiceResult<AutoQueueStatusResponse> {
        validate_status_filter_pg(pool, &input).await?;

        let run_id = auto_queue::find_latest_run_id_pg(
            pool,
            &StatusFilter {
                repo: input.repo.clone(),
                agent_id: input.agent_id.clone(),
            },
        )
        .await
        .map_err(|error| {
            ServiceError::internal(format!("load latest run: {error}"))
                .with_code(ErrorCode::Database)
                .with_operation("status.find_latest_run_id_pg")
        })?;

        let Some(run_id) = run_id else {
            return Ok(AutoQueueStatusResponse::default());
        };

        self.status_for_run_pg(pool, &run_id, input).await
    }

    pub fn prepare_generate_cards(
        &self,
        _input: &PrepareGenerateInput,
    ) -> ServiceResult<Vec<GenerateCandidate>> {
        Err(ServiceError::internal("postgres backend is unavailable")
            .with_code(ErrorCode::Database)
            .with_operation("prepare_generate_cards"))
    }

    pub async fn prepare_generate_cards_with_pg(
        &self,
        pool: &PgPool,
        input: &PrepareGenerateInput,
    ) -> ServiceResult<Vec<GenerateCandidate>> {
        if let Some(issue_numbers) = input.issue_numbers.as_ref().filter(|nums| !nums.is_empty()) {
            crate::pipeline::ensure_loaded();
            let backlog_cards = auto_queue::list_backlog_cards_pg(
                pool,
                &GenerateCardFilter {
                    repo: input.repo.clone(),
                    agent_id: input.agent_id.clone(),
                    issue_numbers: Some(issue_numbers.clone()),
                },
            )
            .await
            .map_err(|error| {
                ServiceError::internal(format!("load backlog cards: {error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("prepare_generate_cards_with_pg.list_backlog_cards_pg")
            })?;

            let mut transition_plan = Vec::with_capacity(backlog_cards.len());
            for card in backlog_cards {
                let effective = crate::pipeline::resolve_for_card_pg(
                    pool,
                    card.repo_id.as_deref(),
                    card.assigned_agent_id.as_deref(),
                )
                .await;
                let prep_path = if effective.is_valid_state("ready") {
                    effective
                        .free_path_to_state("backlog", "ready")
                        .or_else(|| effective.free_path_to_dispatchable("backlog"))
                } else {
                    effective.free_path_to_dispatchable("backlog")
                };
                let Some(path) = prep_path else {
                    return Err(ServiceError::bad_request(format!(
                        "card {} has no free path from backlog to ready/dispatchable state",
                        card.card_id
                    ))
                    .with_code(ErrorCode::AutoQueue)
                    .with_context("card_id", &card.card_id));
                };
                transition_plan.push((card.card_id, path));
            }

            for (card_id, path) in transition_plan {
                for step in &path {
                    crate::kanban::transition_status_with_opts_pg(
                        None,
                        pool,
                        &self.engine,
                        &card_id,
                        step,
                        "auto-queue-generate",
                        crate::engine::transition::ForceIntent::None,
                    )
                    .await
                    .map_err(|error| {
                        ServiceError::bad_request(format!(
                            "failed to auto-transition card {card_id} to {step}: {error}"
                        ))
                        .with_code(ErrorCode::AutoQueue)
                        .with_context("card_id", card_id.as_str())
                        .with_context("target_state", step)
                    })?;
                }
            }
        }

        crate::pipeline::ensure_loaded();
        let enqueueable_states = crate::pipeline::try_get()
            .map(enqueueable_states_for)
            .unwrap_or_else(|| vec!["ready".to_string(), "requested".to_string()]);
        let cards = auto_queue::list_generate_candidates_pg(
            pool,
            &GenerateCardFilter {
                repo: input.repo.clone(),
                agent_id: input.agent_id.clone(),
                issue_numbers: input.issue_numbers.clone(),
            },
            &enqueueable_states,
        )
        .await
        .map_err(|error| {
            ServiceError::internal(format!("load generate cards: {error}"))
                .with_code(ErrorCode::Database)
                .with_operation("prepare_generate_cards_with_pg.list_generate_candidates_pg")
        })?;

        Ok(cards.into_iter().map(GenerateCandidate::from).collect())
    }

    pub fn count_cards_by_status(
        &self,
        _repo: Option<&str>,
        _agent_id: Option<&str>,
        status: &str,
    ) -> ServiceResult<i64> {
        Err(ServiceError::internal("postgres backend is unavailable")
            .with_code(ErrorCode::Database)
            .with_operation("count_cards_by_status")
            .with_context("status", status))
    }

    pub async fn count_cards_by_status_with_pg(
        &self,
        pool: &PgPool,
        repo: Option<&str>,
        agent_id: Option<&str>,
        status: &str,
    ) -> ServiceResult<i64> {
        auto_queue::count_cards_by_status_pg(pool, repo, agent_id, status)
            .await
            .map_err(|error| {
                ServiceError::internal(format!("count cards: {error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("count_cards_by_status_with_pg")
                    .with_context("status", status)
            })
    }

    pub fn run_view(&self, run_id: &str) -> ServiceResult<Option<AutoQueueRunView>> {
        Err(ServiceError::internal("postgres backend is unavailable")
            .with_code(ErrorCode::Database)
            .with_operation("run_view")
            .with_context("run_id", run_id))
    }

    pub async fn run_view_with_pg(
        &self,
        pool: &PgPool,
        run_id: &str,
    ) -> ServiceResult<Option<AutoQueueRunView>> {
        auto_queue::get_run_pg(pool, run_id)
            .await
            .map(|record| record.map(AutoQueueRunView::from))
            .map_err(|error| {
                ServiceError::internal(format!("load run: {error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("run_view_with_pg.get_run_pg")
                    .with_context("run_id", run_id)
            })
    }

    pub fn run_json(&self, run_id: &str) -> ServiceResult<Value> {
        Ok(self
            .run_view(run_id)?
            .map(|run| json!(run))
            .unwrap_or(Value::Null))
    }

    pub async fn run_json_with_pg(&self, pool: &PgPool, run_id: &str) -> ServiceResult<Value> {
        Ok(self
            .run_view_with_pg(pool, run_id)
            .await?
            .map(|run| json!(run))
            .unwrap_or(Value::Null))
    }

    pub fn entry_view(
        &self,
        entry_id: &str,
        _guild_id: Option<&str>,
    ) -> ServiceResult<Option<AutoQueueStatusEntryView>> {
        Err(ServiceError::internal("postgres backend is unavailable")
            .with_code(ErrorCode::Database)
            .with_operation("entry_view")
            .with_context("entry_id", entry_id))
    }

    pub fn entry_json(&self, entry_id: &str, guild_id: Option<&str>) -> ServiceResult<Value> {
        Ok(self
            .entry_view(entry_id, guild_id)?
            .map(|entry| json!(entry))
            .unwrap_or(Value::Null))
    }

    pub async fn entry_json_with_pg(
        &self,
        pool: &PgPool,
        entry_id: &str,
        guild_id: Option<&str>,
    ) -> ServiceResult<Value> {
        let Some(record) = auto_queue::get_status_entry_pg(pool, entry_id)
            .await
            .map_err(|error| {
                ServiceError::internal(format!("load status entry: {error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("entry_json_with_pg.get_status_entry_pg")
                    .with_context("entry_id", entry_id)
            })?
        else {
            return Ok(Value::Null);
        };
        let mut agent_bindings_cache = HashMap::new();
        let entry = build_entry_view_pg(pool, record, guild_id, &mut agent_bindings_cache).await?;
        Ok(json!(entry))
    }

    pub fn status_for_run(
        &self,
        run_id: &str,
        _input: StatusInput,
    ) -> ServiceResult<AutoQueueStatusResponse> {
        Err(ServiceError::internal("postgres backend is unavailable")
            .with_code(ErrorCode::Database)
            .with_operation("status_for_run")
            .with_context("run_id", run_id))
    }

    pub fn status_json_for_run(&self, run_id: &str, input: StatusInput) -> ServiceResult<Value> {
        Ok(json!(self.status_for_run(run_id, input)?))
    }

    pub async fn status_json_for_run_with_pg(
        &self,
        pool: &PgPool,
        run_id: &str,
        input: StatusInput,
    ) -> ServiceResult<Value> {
        Ok(json!(self.status_for_run_pg(pool, run_id, input).await?))
    }

    pub fn status(&self, _input: StatusInput) -> ServiceResult<AutoQueueStatusResponse> {
        Err(ServiceError::internal("postgres backend is unavailable")
            .with_code(ErrorCode::Database)
            .with_operation("status"))
    }

    async fn status_for_run_pg(
        &self,
        pool: &PgPool,
        run_id: &str,
        input: StatusInput,
    ) -> ServiceResult<AutoQueueStatusResponse> {
        let Some(run) = auto_queue::get_run_pg(pool, run_id)
            .await
            .map_err(|error| {
                ServiceError::internal(format!("load run: {error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("status_for_run.get_run_pg")
                    .with_context("run_id", run_id)
            })?
        else {
            return Ok(AutoQueueStatusResponse::default());
        };

        let records = auto_queue::list_status_entries_pg(
            pool,
            run_id,
            &StatusFilter {
                repo: input.repo.clone(),
                agent_id: input.agent_id.clone(),
            },
        )
        .await
        .map_err(|error| {
            ServiceError::internal(format!("load status entries: {error}"))
                .with_code(ErrorCode::Database)
                .with_operation("status_for_run.list_status_entries_pg")
                .with_context("run_id", run_id)
        })?;

        build_status_response_pg(pool, run, records, input.guild_id.as_deref()).await
    }
}

fn normalized_status_filter(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

async fn validate_status_filter_pg(pool: &PgPool, input: &StatusInput) -> ServiceResult<()> {
    let Some(repo) = normalized_status_filter(input.repo.as_deref()) else {
        return Ok(());
    };

    let (repo_exists, agent_exists): (bool, bool) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM github_repos WHERE id = $1) AS repo_exists,
                EXISTS(SELECT 1 FROM agents WHERE id = $1) AS agent_exists",
    )
    .bind(repo)
    .fetch_one(pool)
    .await
    .map_err(|error| {
        ServiceError::internal(format!("validate queue status repo filter: {error}"))
            .with_code(ErrorCode::Database)
            .with_operation("status.validate_status_filter_pg")
            .with_context("repo", repo)
    })?;

    if !repo_exists && agent_exists {
        return Err(ServiceError::bad_request(format!(
            "repo filter '{repo}' matches an agent id, not a registered repo; use agent_id={repo} to filter by agent"
        ))
        .with_code(ErrorCode::AutoQueue)
        .with_context("repo", repo)
        .with_context("agent_id", repo)
        .with_context("hint", format!("GET /api/queue/status?agent_id={repo}")));
    }

    Ok(())
}

impl From<GenerateCandidateRecord> for GenerateCandidate {
    fn from(record: GenerateCandidateRecord) -> Self {
        Self {
            card_id: record.card_id,
            agent_id: record.agent_id,
            priority: record.priority,
            description: record.description,
            metadata: record.metadata,
            github_issue_number: record.github_issue_number,
        }
    }
}

impl From<AutoQueueRunRecord> for AutoQueueRunView {
    fn from(record: AutoQueueRunRecord) -> Self {
        Self {
            id: record.id,
            repo: record.repo,
            agent_id: record.agent_id,
            review_mode: record.review_mode,
            status: record.status,
            timeout_minutes: record.timeout_minutes,
            ai_model: record.ai_model,
            ai_rationale: record.ai_rationale,
            created_at: record.created_at,
            completed_at: record.completed_at,
            unified_thread: false,
            unified_thread_id: None,
            max_concurrent_threads: record.max_concurrent_threads,
            thread_group_count: record.thread_group_count,
        }
    }
}

impl AutoQueueStatusEntryView {
    fn from_record(
        record: StatusEntryRecord,
        dispatch_history: Vec<String>,
        thread_links: Vec<ThreadLinkView>,
    ) -> Self {
        Self {
            id: record.id,
            agent_id: record.agent_id,
            card_id: record.card_id,
            dispatch_id: record.dispatch_id,
            priority_rank: record.priority_rank,
            reason: record.reason,
            status: record.status,
            created_at: record.created_at,
            dispatched_at: record.dispatched_at,
            completed_at: record.completed_at,
            card_title: record.card_title,
            github_issue_number: record.github_issue_number,
            github_repo: record.github_repo,
            retry_count: record.retry_count,
            thread_group: record.thread_group,
            slot_index: record.slot_index,
            batch_phase: record.batch_phase,
            dispatch_history,
            thread_links,
            card_status: record.card_status,
            review_round: record.review_round,
        }
    }
}

async fn build_status_response_pg(
    pool: &PgPool,
    run: AutoQueueRunRecord,
    records: Vec<StatusEntryRecord>,
    guild_id: Option<&str>,
) -> ServiceResult<AutoQueueStatusResponse> {
    let mut agent_bindings_cache: HashMap<String, Option<crate::db::agents::AgentChannelBindings>> =
        HashMap::new();
    let mut entries = Vec::with_capacity(records.len());
    for record in records {
        entries.push(build_entry_view_pg(pool, record, guild_id, &mut agent_bindings_cache).await?);
    }

    let phase_gates = query_phase_gates_pg(pool, &run.id).await?;
    Ok(assemble_status_response(run, entries, phase_gates))
}

async fn query_phase_gates_pg(pool: &PgPool, run_id: &str) -> ServiceResult<Vec<PhaseGateView>> {
    let rows = sqlx::query(
        "SELECT id::BIGINT AS id,
                phase::BIGINT AS phase,
                status,
                dispatch_id,
                failure_reason,
                created_at::text AS created_at,
                updated_at::text AS updated_at
         FROM auto_queue_phase_gates
         WHERE run_id = $1
         ORDER BY phase ASC",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await
    .map_err(|error| {
        ServiceError::internal(format!("load phase gates: {error}"))
            .with_code(ErrorCode::Database)
            .with_operation("query_phase_gates_pg")
            .with_context("run_id", run_id)
    })?;

    rows.into_iter()
        .map(|row| {
            Ok(PhaseGateView {
                id: row.try_get("id").map_err(map_phase_gate_row_error)?,
                phase: row.try_get("phase").map_err(map_phase_gate_row_error)?,
                status: row.try_get("status").map_err(map_phase_gate_row_error)?,
                dispatch_id: row
                    .try_get("dispatch_id")
                    .map_err(map_phase_gate_row_error)?,
                failure_reason: row
                    .try_get("failure_reason")
                    .map_err(map_phase_gate_row_error)?,
                created_at: row
                    .try_get("created_at")
                    .map_err(map_phase_gate_row_error)?,
                updated_at: row
                    .try_get("updated_at")
                    .map_err(map_phase_gate_row_error)?,
            })
        })
        .collect()
}

async fn build_entry_view_pg(
    pool: &PgPool,
    record: StatusEntryRecord,
    guild_id: Option<&str>,
    agent_bindings_cache: &mut HashMap<String, Option<crate::db::agents::AgentChannelBindings>>,
) -> ServiceResult<AutoQueueStatusEntryView> {
    let bindings = if let Some(cached) = agent_bindings_cache.get(&record.agent_id) {
        cached.clone()
    } else {
        let loaded = crate::db::agents::load_agent_channel_bindings_pg(pool, &record.agent_id)
            .await
            .map_err(|error| {
                ServiceError::internal(format!("load agent channel bindings: {error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("build_entry_view.load_agent_channel_bindings_pg")
                    .with_context("agent_id", &record.agent_id)
            })?;
        agent_bindings_cache.insert(record.agent_id.clone(), loaded.clone());
        loaded
    };
    let dispatch_history = auto_queue::list_entry_dispatch_history_pg(pool, &record.id)
        .await
        .map_err(|error| {
            ServiceError::internal(format!("load entry dispatch history: {error}"))
                .with_code(ErrorCode::Database)
                .with_operation("build_entry_view.list_entry_dispatch_history_pg")
                .with_context("entry_id", &record.id)
        })?;
    let dispatch_thread_facts = list_card_dispatch_thread_facts_pg(pool, &record.card_id).await?;
    let thread_links =
        build_entry_thread_links(&record, bindings.as_ref(), guild_id, &dispatch_thread_facts);
    Ok(AutoQueueStatusEntryView::from_record(
        record,
        dispatch_history,
        thread_links,
    ))
}

/// Invariant: `auto_queue_slot_single_active_entry`.
///
/// Within a single auto_queue_run, each `(agent_id, slot_index)` pair must own
/// at most one entry in the `dispatched` state. Violations indicate a tick bug
/// (failed slot release, double-pick) and are observed via
/// `record_invariant_check` without panicking — release builds must not trip
/// on transient tick races. See `docs/invariants.md`.
const AUTO_QUEUE_SLOT_SINGLE_ACTIVE_ENTRY_INVARIANT: &str = "auto_queue_slot_single_active_entry";

fn check_auto_queue_slot_single_active_entry(
    run_id: &str,
    entries: &[AutoQueueStatusEntryView],
) -> Vec<AutoQueueSlotInvariantViolation> {
    let violations = collect_auto_queue_slot_single_active_entry_violations(run_id, entries);
    for violation in &violations {
        crate::services::observability::record_invariant_check(
            false,
            crate::services::observability::InvariantViolation {
                provider: None,
                channel_id: None,
                dispatch_id: violation.dispatch_ids.first().map(String::as_str),
                session_key: None,
                turn_id: None,
                invariant: AUTO_QUEUE_SLOT_SINGLE_ACTIVE_ENTRY_INVARIANT,
                code_location: "src/services/auto_queue.rs:check_auto_queue_slot_single_active_entry",
                message: "auto_queue run has multiple dispatched entries on the same slot",
                details: json!({
                    "run_id": &violation.run_id,
                    "agent_id": &violation.agent_id,
                    "slot_index": violation.slot_index,
                    "entry_ids": &violation.entry_ids,
                    "dispatch_ids": &violation.dispatch_ids,
                    "entries": &violation.entries,
                    "recovery": &violation.recovery,
                }),
            },
        );
    }
    violations
}

fn collect_auto_queue_slot_single_active_entry_violations(
    run_id: &str,
    entries: &[AutoQueueStatusEntryView],
) -> Vec<AutoQueueSlotInvariantViolation> {
    let mut dispatched_per_slot: BTreeMap<(String, i64), Vec<&AutoQueueStatusEntryView>> =
        BTreeMap::new();
    for entry in entries {
        if entry.status != "dispatched" {
            continue;
        }
        let Some(slot_index) = entry.slot_index else {
            continue;
        };
        dispatched_per_slot
            .entry((entry.agent_id.clone(), slot_index))
            .or_default()
            .push(entry);
    }

    dispatched_per_slot
        .into_iter()
        .filter_map(|((agent_id, slot_index), slot_entries)| {
            (slot_entries.len() > 1).then(|| {
                let mut entry_ids = Vec::with_capacity(slot_entries.len());
                let mut dispatch_ids = Vec::new();
                let mut entry_diagnostics = Vec::with_capacity(slot_entries.len());

                for entry in slot_entries {
                    entry_ids.push(entry.id.clone());
                    let entry_dispatch_ids = related_dispatch_ids(entry);
                    for dispatch_id in &entry_dispatch_ids {
                        push_unique_nonempty(&mut dispatch_ids, dispatch_id);
                    }
                    entry_diagnostics.push(AutoQueueSlotInvariantEntryDiagnostic {
                        entry_id: entry.id.clone(),
                        card_id: entry.card_id.clone(),
                        dispatch_ids: entry_dispatch_ids,
                    });
                }

                AutoQueueSlotInvariantViolation {
                    invariant: AUTO_QUEUE_SLOT_SINGLE_ACTIVE_ENTRY_INVARIANT.to_string(),
                    run_id: run_id.to_string(),
                    agent_id: agent_id.clone(),
                    slot_index,
                    entry_ids,
                    dispatch_ids,
                    entries: entry_diagnostics,
                    recovery: slot_invariant_recovery(&agent_id, slot_index),
                }
            })
        })
        .collect()
}

fn related_dispatch_ids(entry: &AutoQueueStatusEntryView) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(dispatch_id) = entry.dispatch_id.as_deref() {
        push_unique_nonempty(&mut ids, dispatch_id);
    }
    for dispatch_id in &entry.dispatch_history {
        push_unique_nonempty(&mut ids, dispatch_id);
    }
    ids
}

fn push_unique_nonempty(ids: &mut Vec<String>, id: &str) {
    let trimmed = id.trim();
    if !trimmed.is_empty() && !ids.iter().any(|existing| existing == trimmed) {
        ids.push(trimmed.to_string());
    }
}

fn slot_invariant_recovery(agent_id: &str, slot_index: i64) -> AutoQueueSlotInvariantRecovery {
    AutoQueueSlotInvariantRecovery {
        summary: "Choose the entry that should retain the active slot; complete/cancel/skip the stale entry, then reset or rebind the slot if the binding points at the wrong thread group.".to_string(),
        status_endpoint: format!("/api/queue/status?agent_id={agent_id}"),
        rebind_slot_endpoint: format!("/api/queue/slots/{agent_id}/{slot_index}/rebind"),
        reset_slot_thread_endpoint: format!("/api/queue/slots/{agent_id}/{slot_index}/reset-thread"),
        skip_entry_endpoint_template: "/api/queue/entries/{entry_id}/skip".to_string(),
    }
}

fn assemble_status_response(
    run: AutoQueueRunRecord,
    entries: Vec<AutoQueueStatusEntryView>,
    phase_gates: Vec<PhaseGateView>,
) -> AutoQueueStatusResponse {
    let slot_invariant_violations = check_auto_queue_slot_single_active_entry(&run.id, &entries);
    let mut agents = BTreeMap::<String, AutoQueueStatusCounts>::new();
    let mut thread_groups = BTreeMap::<String, AutoQueueThreadGroupView>::new();
    for entry in &entries {
        increment_status_counts(
            agents.entry(entry.agent_id.clone()).or_default(),
            entry.status.as_str(),
        );

        let group = thread_groups
            .entry(entry.thread_group.to_string())
            .or_default();
        increment_thread_group_counts(group, entry.status.as_str());
        if group
            .reason
            .as_deref()
            .map(|value| value.is_empty())
            .unwrap_or(true)
        {
            group.reason = entry.reason.clone();
        }
        group.entries.push(AutoQueueThreadGroupEntryView {
            id: entry.id.clone(),
            card_id: entry.card_id.clone(),
            status: entry.status.clone(),
            github_issue_number: entry.github_issue_number,
            batch_phase: entry.batch_phase,
        });
    }

    for group in thread_groups.values_mut() {
        group.status = if group.dispatched > 0 {
            "active".to_string()
        } else if group.pending > 0 {
            "pending".to_string()
        } else if group.failed > 0 {
            "failed".to_string()
        } else {
            "done".to_string()
        };
    }

    AutoQueueStatusResponse {
        run: Some(AutoQueueRunView::from(run)),
        entries,
        agents,
        thread_groups,
        phase_gates,
        diagnostics: AutoQueueStatusDiagnostics {
            slot_invariant_violations,
        },
    }
}

fn map_phase_gate_row_error(error: sqlx::Error) -> ServiceError {
    ServiceError::internal(format!("decode phase gate row: {error}"))
        .with_code(ErrorCode::Database)
        .with_operation("query_phase_gates_pg.decode_row")
}

fn enqueueable_states_for(pipeline: &crate::pipeline::PipelineConfig) -> Vec<String> {
    let mut states: Vec<String> = pipeline
        .dispatchable_states()
        .iter()
        .map(|state| state.to_string())
        .collect();

    if pipeline.is_valid_state("requested") && !states.iter().any(|state| state == "requested") {
        states.push("requested".to_string());
    }
    if pipeline.is_valid_state("ready") && !states.iter().any(|state| state == "ready") {
        states.push("ready".to_string());
    }

    states
}

fn increment_status_counts(counts: &mut AutoQueueStatusCounts, status: &str) {
    match status {
        "pending" => counts.pending += 1,
        "dispatched" => counts.dispatched += 1,
        "done" => counts.done += 1,
        "skipped" => counts.skipped += 1,
        "failed" => counts.failed += 1,
        _ => {}
    }
}

fn increment_thread_group_counts(group: &mut AutoQueueThreadGroupView, status: &str) {
    match status {
        "pending" => group.pending += 1,
        "dispatched" => group.dispatched += 1,
        "done" => group.done += 1,
        "skipped" => group.skipped += 1,
        "failed" => group.failed += 1,
        _ => {}
    }
}

fn build_entry_thread_links(
    record: &StatusEntryRecord,
    bindings: Option<&crate::db::agents::AgentChannelBindings>,
    guild_id: Option<&str>,
    dispatch_thread_facts: &[DispatchThreadFact],
) -> Vec<ThreadLinkView> {
    let thread_map = parse_card_thread_bindings(record.channel_thread_map.as_deref());
    let active_thread_id = normalized_optional(record.active_thread_id.as_deref());
    let candidates = build_thread_link_candidates(bindings);
    let canonical_threads = canonical_dispatch_threads_by_channel(dispatch_thread_facts, bindings);
    let suppressed_thread_ids = latest_invalid_dispatch_thread_ids(dispatch_thread_facts);

    if !thread_map.is_empty() {
        let mut links = Vec::new();
        let mut used_channels = BTreeMap::<u64, ()>::new();

        for candidate in &candidates {
            if let Some(canonical_thread_id) = canonical_threads.get(&candidate.channel_id) {
                used_channels.insert(candidate.channel_id, ());
                if let Some(thread_id) = canonical_thread_id {
                    links.push(thread_link_view(
                        candidate.label.as_str(),
                        candidate.label.clone(),
                        Some(candidate.channel_id),
                        thread_id,
                        guild_id,
                    ));
                }
                continue;
            }
            let Some(thread_id) = thread_map.get(&candidate.channel_id) else {
                continue;
            };
            if suppressed_thread_ids.contains(thread_id) {
                continue;
            }
            used_channels.insert(candidate.channel_id, ());
            links.push(thread_link_view(
                candidate.label.as_str(),
                candidate.label.clone(),
                Some(candidate.channel_id),
                thread_id,
                guild_id,
            ));
        }

        for (channel_id, thread_id) in &thread_map {
            if used_channels.insert(*channel_id, ()).is_none() {
                if suppressed_thread_ids.contains(thread_id) {
                    continue;
                }
                links.push(thread_link_view(
                    "channel",
                    format!("channel:{channel_id}"),
                    Some(*channel_id),
                    thread_id,
                    guild_id,
                ));
            }
        }

        return links;
    }

    if !canonical_threads.is_empty() {
        let mut links = Vec::new();
        for candidate in &candidates {
            let Some(Some(thread_id)) = canonical_threads.get(&candidate.channel_id) else {
                continue;
            };
            links.push(thread_link_view(
                candidate.label.as_str(),
                candidate.label.clone(),
                Some(candidate.channel_id),
                thread_id,
                guild_id,
            ));
        }
        if !links.is_empty() {
            return links;
        }
    }

    active_thread_id
        .filter(|thread_id| !suppressed_thread_ids.contains(thread_id))
        .map(|thread_id| {
            vec![thread_link_view(
                "active",
                "active".to_string(),
                None,
                &thread_id,
                guild_id,
            )]
        })
        .unwrap_or_default()
}

async fn list_card_dispatch_thread_facts_pg(
    pool: &PgPool,
    card_id: &str,
) -> ServiceResult<Vec<DispatchThreadFact>> {
    if card_id.trim().is_empty() {
        return Ok(Vec::new());
    }

    let rows = sqlx::query(
        "SELECT dispatch_type, status, thread_id
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND NULLIF(BTRIM(thread_id), '') IS NOT NULL
         ORDER BY updated_at DESC NULLS LAST,
                  created_at DESC NULLS LAST,
                  id DESC",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| {
        ServiceError::internal(format!("load dispatch thread facts: {error}"))
            .with_code(ErrorCode::Database)
            .with_operation("list_card_dispatch_thread_facts_pg")
            .with_context("card_id", card_id)
    })?;

    rows.into_iter()
        .map(|row| {
            Ok(DispatchThreadFact {
                dispatch_type: row
                    .try_get("dispatch_type")
                    .map_err(map_thread_fact_error)?,
                status: row.try_get("status").map_err(map_thread_fact_error)?,
                thread_id: row.try_get("thread_id").map_err(map_thread_fact_error)?,
            })
        })
        .collect()
}

fn map_thread_fact_error(error: sqlx::Error) -> ServiceError {
    ServiceError::internal(format!("decode dispatch thread fact: {error}"))
        .with_code(ErrorCode::Database)
        .with_operation("list_card_dispatch_thread_facts_pg.decode")
}

fn canonical_dispatch_threads_by_channel(
    facts: &[DispatchThreadFact],
    bindings: Option<&crate::db::agents::AgentChannelBindings>,
) -> BTreeMap<u64, Option<String>> {
    let mut channels = BTreeMap::new();
    for fact in facts {
        let Some(channel_id) = dispatch_thread_channel_id(fact, bindings) else {
            continue;
        };
        channels.entry(channel_id).or_insert_with(|| {
            if dispatch_thread_status_suppresses_link(&fact.status) {
                None
            } else {
                Some(fact.thread_id.clone())
            }
        });
    }
    channels
}

fn latest_invalid_dispatch_thread_ids(facts: &[DispatchThreadFact]) -> BTreeSet<String> {
    let mut statuses = BTreeMap::<String, bool>::new();
    for fact in facts {
        statuses
            .entry(fact.thread_id.clone())
            .or_insert_with(|| dispatch_thread_status_suppresses_link(&fact.status));
    }
    statuses
        .into_iter()
        .filter_map(|(thread_id, suppress)| suppress.then_some(thread_id))
        .collect()
}

fn dispatch_thread_channel_id(
    fact: &DispatchThreadFact,
    bindings: Option<&crate::db::agents::AgentChannelBindings>,
) -> Option<u64> {
    let bindings = bindings?;
    let channel = if crate::server::routes::dispatches::use_counter_model_channel(
        fact.dispatch_type.as_deref(),
    ) {
        bindings.counter_model_channel()
    } else {
        bindings.primary_channel()
    };
    channel
        .as_deref()
        .and_then(crate::server::routes::dispatches::parse_channel_id)
}

fn dispatch_thread_status_suppresses_link(status: &str) -> bool {
    matches!(status, "cancelled" | "failed" | "expired")
}

fn build_thread_link_candidates(
    bindings: Option<&crate::db::agents::AgentChannelBindings>,
) -> Vec<ThreadLinkCandidate> {
    let Some(bindings) = bindings else {
        return Vec::new();
    };

    let work_channel = bindings
        .primary_channel()
        .as_deref()
        .and_then(crate::server::routes::dispatches::parse_channel_id);
    let review_channel = bindings
        .counter_model_channel()
        .as_deref()
        .and_then(crate::server::routes::dispatches::parse_channel_id);

    match (work_channel, review_channel) {
        (Some(work_channel), Some(review_channel)) if work_channel == review_channel => {
            vec![ThreadLinkCandidate {
                label: "shared".to_string(),
                channel_id: work_channel,
            }]
        }
        (Some(work_channel), Some(review_channel)) => vec![
            ThreadLinkCandidate {
                label: "work".to_string(),
                channel_id: work_channel,
            },
            ThreadLinkCandidate {
                label: "review".to_string(),
                channel_id: review_channel,
            },
        ],
        (Some(work_channel), None) => vec![ThreadLinkCandidate {
            label: "work".to_string(),
            channel_id: work_channel,
        }],
        (None, Some(review_channel)) => vec![ThreadLinkCandidate {
            label: "review".to_string(),
            channel_id: review_channel,
        }],
        (None, None) => Vec::new(),
    }
}

fn parse_card_thread_bindings(raw: Option<&str>) -> BTreeMap<u64, String> {
    raw.and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| value.as_object().cloned())
        .map(|map| {
            map.into_iter()
                .filter_map(|(channel_id_raw, thread_value)| {
                    let channel_id = channel_id_raw.parse::<u64>().ok()?;
                    let thread_id = thread_value
                        .as_str()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string)
                        .or_else(|| thread_value.as_u64().map(|value| value.to_string()))?;
                    Some((channel_id, thread_id))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn thread_link_view(
    role: &str,
    label: String,
    channel_id: Option<u64>,
    thread_id: &str,
    guild_id: Option<&str>,
) -> ThreadLinkView {
    let thread_id = thread_id.trim().to_string();
    let valid_guild_id = crate::utils::discord::normalize_discord_snowflake(guild_id);
    let valid_thread_id = crate::utils::discord::normalize_discord_snowflake(Some(&thread_id));

    ThreadLinkView {
        role: role.to_string(),
        label,
        channel_id: channel_id.map(|value| value.to_string()),
        url: valid_guild_id
            .zip(valid_thread_id)
            .map(|(guild_id, thread_id)| {
                format!("https://discord.com/channels/{guild_id}/{thread_id}")
            }),
        thread_id,
    }
}

fn normalized_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_record(id: &str) -> AutoQueueRunRecord {
        AutoQueueRunRecord {
            id: id.to_string(),
            repo: Some("test-repo".to_string()),
            agent_id: Some("agent-slot".to_string()),
            review_mode: "enabled".to_string(),
            status: "active".to_string(),
            timeout_minutes: 30,
            ai_model: None,
            ai_rationale: None,
            created_at: 1_000,
            completed_at: None,
            max_concurrent_threads: 2,
            thread_group_count: 2,
        }
    }

    fn status_entry(
        id: &str,
        card_id: &str,
        status: &str,
        slot_index: Option<i64>,
        dispatch_id: Option<&str>,
        dispatch_history: Vec<&str>,
    ) -> AutoQueueStatusEntryView {
        AutoQueueStatusEntryView {
            id: id.to_string(),
            agent_id: "agent-slot".to_string(),
            card_id: card_id.to_string(),
            dispatch_id: dispatch_id.map(str::to_string),
            priority_rank: 0,
            reason: None,
            status: status.to_string(),
            created_at: 1_000,
            dispatched_at: None,
            completed_at: None,
            card_title: None,
            github_issue_number: None,
            github_repo: None,
            retry_count: 0,
            thread_group: 0,
            slot_index,
            batch_phase: 0,
            dispatch_history: dispatch_history.into_iter().map(str::to_string).collect(),
            thread_links: Vec::new(),
            card_status: None,
            review_round: 0,
        }
    }

    #[test]
    fn auto_queue_status_omits_diagnostics_without_slot_invariant_violation() {
        let response = assemble_status_response(
            run_record("run-clean"),
            vec![status_entry(
                "entry-clean",
                "card-clean",
                "dispatched",
                Some(0),
                Some("dispatch-clean"),
                vec!["dispatch-clean"],
            )],
            Vec::new(),
        );
        let value = serde_json::to_value(response).unwrap();

        assert!(
            value.get("diagnostics").is_none(),
            "diagnostics must not change the normal status surface"
        );
        assert!(
            value["entries"][0].get("dispatch_id").is_none(),
            "current dispatch id stays internal unless a diagnostic needs it"
        );
    }

    #[test]
    fn auto_queue_status_reports_actionable_slot_invariant_diagnostics() {
        let response = assemble_status_response(
            run_record("run-conflict"),
            vec![
                status_entry(
                    "entry-a",
                    "card-a",
                    "dispatched",
                    Some(1),
                    Some("dispatch-a"),
                    vec!["dispatch-a"],
                ),
                status_entry(
                    "entry-b",
                    "card-b",
                    "dispatched",
                    Some(1),
                    Some("dispatch-b"),
                    vec!["dispatch-b", "dispatch-b-retry"],
                ),
            ],
            Vec::new(),
        );
        let value = serde_json::to_value(response).unwrap();
        let violation = &value["diagnostics"]["slot_invariant_violations"][0];

        assert_eq!(
            violation["invariant"],
            AUTO_QUEUE_SLOT_SINGLE_ACTIVE_ENTRY_INVARIANT
        );
        assert_eq!(violation["run_id"], "run-conflict");
        assert_eq!(violation["agent_id"], "agent-slot");
        assert_eq!(violation["slot_index"], 1);
        assert_eq!(violation["entry_ids"], json!(["entry-a", "entry-b"]));
        assert_eq!(
            violation["dispatch_ids"],
            json!(["dispatch-a", "dispatch-b", "dispatch-b-retry"])
        );
        assert_eq!(violation["entries"][0]["entry_id"], "entry-a");
        assert_eq!(violation["entries"][1]["card_id"], "card-b");
        assert_eq!(
            violation["recovery"]["rebind_slot_endpoint"],
            "/api/queue/slots/agent-slot/1/rebind"
        );
        assert_eq!(
            violation["recovery"]["reset_slot_thread_endpoint"],
            "/api/queue/slots/agent-slot/1/reset-thread"
        );
    }

    #[test]
    fn thread_link_view_only_builds_url_for_discord_snowflakes() {
        let valid = thread_link_view(
            "work",
            "work".to_string(),
            Some(1485506232256168011),
            "1501968633650483271",
            Some("1490141479707086938"),
        );
        assert_eq!(
            valid.url.as_deref(),
            Some("https://discord.com/channels/1490141479707086938/1501968633650483271")
        );

        let bad_guild = thread_link_view(
            "work",
            "work".to_string(),
            Some(1485506232256168011),
            "1501968633650483271",
            Some("123"),
        );
        assert!(bad_guild.url.is_none());

        let bad_thread = thread_link_view(
            "work",
            "work".to_string(),
            Some(1485506232256168011),
            "thread-work-completed",
            Some("1490141479707086938"),
        );
        assert!(bad_thread.url.is_none());
        assert_eq!(bad_thread.thread_id, "thread-work-completed");
    }
}
