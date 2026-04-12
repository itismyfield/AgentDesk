pub mod runtime;

use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

use crate::db::{
    Db,
    auto_queue::{
        self, AutoQueueRunRecord, GenerateCandidateRecord, GenerateCardFilter, StatusEntryRecord,
        StatusFilter,
    },
};
use crate::engine::PolicyEngine;
use crate::services::service_error::{ErrorCode, ServiceError, ServiceResult};

#[derive(Clone)]
pub struct AutoQueueService {
    db: Db,
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
}

#[derive(Debug, Serialize)]
pub struct AutoQueueRunView {
    pub id: String,
    pub repo: Option<String>,
    pub agent_id: Option<String>,
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
pub struct AutoQueueStatusEntryView {
    pub id: String,
    pub agent_id: String,
    pub card_id: String,
    pub priority_rank: i64,
    pub reason: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub dispatched_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub card_title: Option<String>,
    pub github_issue_number: Option<i64>,
    pub github_repo: Option<String>,
    pub thread_group: i64,
    pub slot_index: Option<i64>,
    pub batch_phase: i64,
    pub thread_links: Vec<ThreadLinkView>,
}

#[derive(Debug, Serialize, Default)]
pub struct AutoQueueStatusCounts {
    pub pending: i64,
    pub dispatched: i64,
    pub done: i64,
    pub skipped: i64,
}

#[derive(Debug, Serialize, Default)]
pub struct AutoQueueThreadGroupView {
    pub pending: i64,
    pub dispatched: i64,
    pub done: i64,
    pub skipped: i64,
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

impl AutoQueueService {
    pub fn new(db: Db, engine: PolicyEngine) -> Self {
        Self { db, engine }
    }

    pub fn prepare_generate_cards(
        &self,
        input: &PrepareGenerateInput,
    ) -> ServiceResult<Vec<GenerateCandidate>> {
        if let Some(issue_numbers) = input.issue_numbers.as_ref().filter(|nums| !nums.is_empty()) {
            let transition_plan = {
                let conn = self.db.read_conn().map_err(|error| {
                    ServiceError::internal(format!("{error}"))
                        .with_code(ErrorCode::Database)
                        .with_operation("prepare_generate_cards.read_conn.transition_plan")
                })?;
                crate::pipeline::ensure_loaded();
                let backlog_cards = auto_queue::list_backlog_cards(
                    &conn,
                    &GenerateCardFilter {
                        repo: input.repo.clone(),
                        agent_id: input.agent_id.clone(),
                        issue_numbers: Some(issue_numbers.clone()),
                    },
                )
                .map_err(|error| {
                    ServiceError::internal(format!("load backlog cards: {error}"))
                        .with_code(ErrorCode::Database)
                        .with_operation("prepare_generate_cards.list_backlog_cards")
                })?;

                let mut plan = Vec::with_capacity(backlog_cards.len());
                for card in backlog_cards {
                    let effective = crate::pipeline::resolve_for_card(
                        &conn,
                        card.repo_id.as_deref(),
                        card.assigned_agent_id.as_deref(),
                    );
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
                    plan.push((card.card_id, path));
                }
                plan
            };

            for (card_id, path) in transition_plan {
                for step in &path {
                    crate::kanban::transition_status_with_opts(
                        &self.db,
                        &self.engine,
                        &card_id,
                        step,
                        "auto-queue-generate",
                        false,
                    )
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

        let conn = self.db.read_conn().map_err(|error| {
            ServiceError::internal(format!("{error}"))
                .with_code(ErrorCode::Database)
                .with_operation("prepare_generate_cards.read_conn.generate_candidates")
        })?;
        crate::pipeline::ensure_loaded();
        let enqueueable_states = crate::pipeline::try_get()
            .map(enqueueable_states_for)
            .unwrap_or_else(|| vec!["ready".to_string(), "requested".to_string()]);
        let cards = auto_queue::list_generate_candidates(
            &conn,
            &GenerateCardFilter {
                repo: input.repo.clone(),
                agent_id: input.agent_id.clone(),
                issue_numbers: input.issue_numbers.clone(),
            },
            &enqueueable_states,
        )
        .map_err(|error| {
            ServiceError::internal(format!("load generate cards: {error}"))
                .with_code(ErrorCode::Database)
                .with_operation("prepare_generate_cards.list_generate_candidates")
        })?;

        Ok(cards.into_iter().map(GenerateCandidate::from).collect())
    }

    pub fn count_cards_by_status(
        &self,
        repo: Option<&str>,
        agent_id: Option<&str>,
        status: &str,
    ) -> ServiceResult<i64> {
        let conn = self.db.read_conn().map_err(|error| {
            ServiceError::internal(format!("{error}"))
                .with_code(ErrorCode::Database)
                .with_operation("count_cards_by_status.read_conn")
                .with_context("status", status)
        })?;
        auto_queue::count_cards_by_status(&conn, repo, agent_id, status).map_err(|error| {
            ServiceError::internal(format!("count cards: {error}"))
                .with_code(ErrorCode::Database)
                .with_operation("count_cards_by_status")
                .with_context("status", status)
        })
    }

    pub fn run_view(&self, run_id: &str) -> ServiceResult<Option<AutoQueueRunView>> {
        let conn = self.db.read_conn().map_err(|error| {
            ServiceError::internal(format!("{error}"))
                .with_code(ErrorCode::Database)
                .with_operation("run_view.read_conn")
                .with_context("run_id", run_id)
        })?;
        auto_queue::get_run(&conn, run_id)
            .map(|record| record.map(AutoQueueRunView::from))
            .map_err(|error| {
                ServiceError::internal(format!("load run: {error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("run_view.get_run")
                    .with_context("run_id", run_id)
            })
    }

    pub fn run_json(&self, run_id: &str) -> ServiceResult<Value> {
        Ok(self
            .run_view(run_id)?
            .map(|run| json!(run))
            .unwrap_or(Value::Null))
    }

    pub fn entry_view(
        &self,
        entry_id: &str,
        guild_id: Option<&str>,
    ) -> ServiceResult<Option<AutoQueueStatusEntryView>> {
        let conn = self.db.read_conn().map_err(|error| {
            ServiceError::internal(format!("{error}"))
                .with_code(ErrorCode::Database)
                .with_operation("entry_view.read_conn")
                .with_context("entry_id", entry_id)
        })?;
        let Some(record) = auto_queue::get_status_entry(&conn, entry_id).map_err(|error| {
            ServiceError::internal(format!("load status entry: {error}"))
                .with_code(ErrorCode::Database)
                .with_operation("entry_view.get_status_entry")
                .with_context("entry_id", entry_id)
        })?
        else {
            return Ok(None);
        };
        let mut agent_bindings_cache = HashMap::new();
        Ok(Some(build_entry_view(
            &conn,
            record,
            guild_id,
            &mut agent_bindings_cache,
        )?))
    }

    pub fn entry_json(&self, entry_id: &str, guild_id: Option<&str>) -> ServiceResult<Value> {
        Ok(self
            .entry_view(entry_id, guild_id)?
            .map(|entry| json!(entry))
            .unwrap_or(Value::Null))
    }

    pub fn status_for_run(
        &self,
        run_id: &str,
        input: StatusInput,
    ) -> ServiceResult<AutoQueueStatusResponse> {
        let conn = self.db.read_conn().map_err(|error| {
            ServiceError::internal(format!("{error}"))
                .with_code(ErrorCode::Database)
                .with_operation("status_for_run.read_conn")
                .with_context("run_id", run_id)
        })?;
        let Some(run) = auto_queue::get_run(&conn, run_id).map_err(|error| {
            ServiceError::internal(format!("load run: {error}"))
                .with_code(ErrorCode::Database)
                .with_operation("status_for_run.get_run")
                .with_context("run_id", run_id)
        })?
        else {
            return Ok(AutoQueueStatusResponse::default());
        };
        let records = auto_queue::list_status_entries(
            &conn,
            run_id,
            &StatusFilter {
                repo: input.repo.clone(),
                agent_id: input.agent_id.clone(),
            },
        )
        .map_err(|error| {
            ServiceError::internal(format!("load status entries: {error}"))
                .with_code(ErrorCode::Database)
                .with_operation("status_for_run.list_status_entries")
                .with_context("run_id", run_id)
        })?;

        build_status_response(&conn, run, records, input.guild_id.as_deref())
    }

    pub fn status_json_for_run(&self, run_id: &str, input: StatusInput) -> ServiceResult<Value> {
        Ok(json!(self.status_for_run(run_id, input)?))
    }

    pub fn status(&self, input: StatusInput) -> ServiceResult<AutoQueueStatusResponse> {
        let run_id = {
            let conn = self.db.read_conn().map_err(|error| {
                ServiceError::internal(format!("{error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("status.read_conn")
            })?;
            auto_queue::find_latest_run_id(
                &conn,
                &StatusFilter {
                    repo: input.repo.clone(),
                    agent_id: input.agent_id.clone(),
                },
            )
            .map_err(|error| {
                ServiceError::internal(format!("load latest run: {error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("status.find_latest_run_id")
            })?
        };
        let Some(run_id) = run_id else {
            return Ok(AutoQueueStatusResponse::default());
        };
        self.status_for_run(&run_id, input)
    }
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
    fn from_record(record: StatusEntryRecord, thread_links: Vec<ThreadLinkView>) -> Self {
        Self {
            id: record.id,
            agent_id: record.agent_id,
            card_id: record.card_id,
            priority_rank: record.priority_rank,
            reason: record.reason,
            status: record.status,
            created_at: record.created_at,
            dispatched_at: record.dispatched_at,
            completed_at: record.completed_at,
            card_title: record.card_title,
            github_issue_number: record.github_issue_number,
            github_repo: record.github_repo,
            thread_group: record.thread_group,
            slot_index: record.slot_index,
            batch_phase: record.batch_phase,
            thread_links,
        }
    }
}

fn build_status_response(
    conn: &rusqlite::Connection,
    run: AutoQueueRunRecord,
    records: Vec<StatusEntryRecord>,
    guild_id: Option<&str>,
) -> ServiceResult<AutoQueueStatusResponse> {
    let mut agent_bindings_cache: HashMap<String, Option<crate::db::agents::AgentChannelBindings>> =
        HashMap::new();
    let mut entries = Vec::with_capacity(records.len());
    for record in records {
        entries.push(build_entry_view(
            conn,
            record,
            guild_id,
            &mut agent_bindings_cache,
        )?);
    }

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
        } else {
            "done".to_string()
        };
    }

    Ok(AutoQueueStatusResponse {
        run: Some(AutoQueueRunView::from(run)),
        entries,
        agents,
        thread_groups,
    })
}

fn build_entry_view(
    conn: &rusqlite::Connection,
    record: StatusEntryRecord,
    guild_id: Option<&str>,
    agent_bindings_cache: &mut HashMap<String, Option<crate::db::agents::AgentChannelBindings>>,
) -> ServiceResult<AutoQueueStatusEntryView> {
    let bindings = if let Some(cached) = agent_bindings_cache.get(&record.agent_id) {
        cached.clone()
    } else {
        let loaded = crate::db::agents::load_agent_channel_bindings(conn, &record.agent_id)
            .map_err(|error| {
                ServiceError::internal(format!("load agent channel bindings: {error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("build_entry_view.load_agent_channel_bindings")
                    .with_context("agent_id", &record.agent_id)
            })?;
        agent_bindings_cache.insert(record.agent_id.clone(), loaded.clone());
        loaded
    };
    let thread_links = build_entry_thread_links(&record, bindings.as_ref(), guild_id);
    Ok(AutoQueueStatusEntryView::from_record(record, thread_links))
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
        _ => {}
    }
}

fn increment_thread_group_counts(group: &mut AutoQueueThreadGroupView, status: &str) {
    match status {
        "pending" => group.pending += 1,
        "dispatched" => group.dispatched += 1,
        "done" => group.done += 1,
        "skipped" => group.skipped += 1,
        _ => {}
    }
}

fn build_entry_thread_links(
    record: &StatusEntryRecord,
    bindings: Option<&crate::db::agents::AgentChannelBindings>,
    guild_id: Option<&str>,
) -> Vec<ThreadLinkView> {
    let thread_map = parse_card_thread_bindings(record.channel_thread_map.as_deref());
    let active_thread_id = normalized_optional(record.active_thread_id.as_deref());
    let candidates = build_thread_link_candidates(bindings);

    if !thread_map.is_empty() {
        let mut links = Vec::new();
        let mut used_channels = BTreeMap::<u64, ()>::new();

        for candidate in &candidates {
            let Some(thread_id) = thread_map.get(&candidate.channel_id) else {
                continue;
            };
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

    active_thread_id
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
    let guild_id = guild_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    ThreadLinkView {
        role: role.to_string(),
        label,
        channel_id: channel_id.map(|value| value.to_string()),
        url: guild_id
            .map(|guild_id| format!("https://discord.com/channels/{guild_id}/{thread_id}")),
        thread_id,
    }
}

fn normalized_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}
