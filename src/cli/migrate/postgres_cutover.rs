use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{NaiveDateTime, TimeZone, Utc};
use clap::Args;
use libsql_rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use sqlx::{PgPool, Postgres, QueryBuilder, Row, Transaction};

use crate::config::Config;
use crate::utils::format::expand_tilde_path;

#[derive(Clone, Debug, Args)]
pub struct PostgresCutoverArgs {
    /// Preview counts and blockers without writing files or importing into PostgreSQL
    #[arg(long)]
    pub dry_run: bool,
    /// Optional directory for JSONL archive snapshots
    #[arg(long = "archive-dir", value_name = "PATH")]
    pub archive_dir: Option<String>,
    /// Skip PostgreSQL import and only report/export the SQLite history.
    ///
    /// This path skips the `BEGIN IMMEDIATE` write barrier on SQLite, so it
    /// MUST run with dcserver stopped to guarantee a consistent snapshot. The
    /// CLI will refuse to proceed when a live dcserver is detected unless
    /// `--allow-runtime-active` is set.
    #[arg(long)]
    pub skip_pg_import: bool,
    /// Acknowledge and proceed even when SQLite still has unsent message_outbox
    /// rows. By default cutover refuses so Discord messages are not silently
    /// dropped — pass this only after confirming the pending rows are known
    /// stale and will not need to be re-delivered.
    #[arg(long = "allow-unsent-messages")]
    pub allow_unsent_messages: bool,
    /// Override the runtime-active safety check for archive-only cutover.
    ///
    /// Use only when you know the workload is frozen (e.g. dcserver paused at
    /// OS level, snapshot taken from an offline copy). Detection still runs
    /// and is reflected in the report; this flag downgrades it to a warning.
    #[arg(long = "allow-runtime-active")]
    pub allow_runtime_active: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
struct CutoverCounts {
    agents: i64,
    github_repos: i64,
    kanban_cards: i64,
    kanban_audit_logs: i64,
    auto_queue_runs: i64,
    auto_queue_entries: i64,
    auto_queue_entry_transitions: i64,
    auto_queue_entry_dispatch_history: i64,
    auto_queue_phase_gates: i64,
    auto_queue_slots: i64,
    task_dispatches: i64,
    dispatch_events: i64,
    dispatch_queue: i64,
    card_retrospectives: i64,
    card_review_state: i64,
    review_decisions: i64,
    review_tuning_outcomes: i64,
    messages: i64,
    message_outbox: i64,
    meetings: i64,
    meeting_transcripts: i64,
    pending_dm_replies: i64,
    pipeline_stages: i64,
    pr_tracking: i64,
    skills: i64,
    skill_usage: i64,
    runtime_decisions: i64,
    session_termination_events: i64,
    sessions: i64,
    session_transcripts: i64,
    turns: i64,
    departments: i64,
    offices: i64,
    office_agents: i64,
    kv_meta: i64,
    api_friction_events: i64,
    api_friction_issues: i64,
    memento_feedback_turn_stats: i64,
    rate_limit_cache: i64,
    deferred_hooks: i64,
    audit_logs: i64,
    active_dispatches: i64,
    working_sessions: i64,
    open_dispatch_outbox: i64,
    pending_message_outbox: i64,
}

type SqliteCutoverCounts = CutoverCounts;
type PgCutoverCounts = CutoverCounts;

impl CutoverCounts {
    fn has_live_state(&self) -> bool {
        self.active_dispatches > 0
            || self.working_sessions > 0
            || self.open_dispatch_outbox > 0
            || self.pending_message_outbox > 0
    }
}

#[derive(Debug, Default, Serialize)]
struct ArchiveOutput {
    directory: String,
    audit_logs_file: Option<String>,
    session_transcripts_file: Option<String>,
}

#[derive(Debug, Default, Serialize)]
struct ImportSummary {
    offices_upserted: i64,
    departments_upserted: i64,
    office_agents_upserted: i64,
    github_repos_upserted: i64,
    agents_upserted: i64,
    cards_upserted: i64,
    kanban_audit_logs_upserted: i64,
    card_retrospectives_upserted: i64,
    card_review_state_upserted: i64,
    auto_queue_runs_upserted: i64,
    auto_queue_entries_upserted: i64,
    auto_queue_entry_transitions_upserted: i64,
    auto_queue_entry_dispatch_history_upserted: i64,
    auto_queue_phase_gates_upserted: i64,
    auto_queue_slots_upserted: i64,
    task_dispatches_upserted: i64,
    dispatch_events_upserted: i64,
    dispatch_outbox_upserted: i64,
    dispatch_queue_upserted: i64,
    pr_tracking_upserted: i64,
    sessions_upserted: i64,
    session_termination_events_upserted: i64,
    session_transcripts_upserted: i64,
    turns_upserted: i64,
    meetings_upserted: i64,
    meeting_transcripts_upserted: i64,
    messages_upserted: i64,
    message_outbox_upserted: i64,
    pending_dm_replies_upserted: i64,
    review_decisions_upserted: i64,
    review_tuning_outcomes_upserted: i64,
    skills_upserted: i64,
    skill_usage_upserted: i64,
    pipeline_stages_upserted: i64,
    runtime_decisions_upserted: i64,
    kv_meta_upserted: i64,
    api_friction_events_upserted: i64,
    api_friction_issues_upserted: i64,
    memento_feedback_turn_stats_upserted: i64,
    rate_limit_cache_upserted: i64,
    deferred_hooks_upserted: i64,
    audit_logs_inserted: i64,
    auto_queue_entries_skipped_orphans: i64,
    auto_queue_entry_transitions_skipped_orphans: i64,
    auto_queue_entry_dispatch_history_skipped_orphans: i64,
    auto_queue_phase_gates_skipped_orphans: i64,
    auto_queue_slots_skipped_orphans: i64,
    dispatch_events_skipped_orphans: i64,
    card_retrospectives_skipped_orphans: i64,
    card_review_state_skipped_orphans: i64,
    pr_tracking_skipped_orphans: i64,
    session_termination_events_skipped_orphans: i64,
    meeting_transcripts_skipped_orphans: i64,
}

#[derive(Debug, Default, Serialize)]
struct PostgresCutoverReport {
    ok: bool,
    sqlite: SqliteCutoverCounts,
    postgres_before: Option<PgCutoverCounts>,
    postgres_after: Option<PgCutoverCounts>,
    archive: Option<ArchiveOutput>,
    imported: Option<ImportSummary>,
    runtime_active: Option<RuntimeActiveStatus>,
    blocker: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct RuntimeActiveStatus {
    active: bool,
    pid_file: Option<PidFileSignal>,
    tcp: Option<TcpSignal>,
    overridden: bool,
}

#[derive(Debug, Default, Clone)]
struct OrphanSkipSummary {
    auto_queue_entries: i64,
    auto_queue_entry_transitions: i64,
    auto_queue_entry_dispatch_history: i64,
    auto_queue_phase_gates: i64,
    auto_queue_slots: i64,
    dispatch_events: i64,
    card_retrospectives: i64,
    card_review_state: i64,
    pr_tracking: i64,
    session_termination_events: i64,
    meeting_transcripts: i64,
}

impl OrphanSkipSummary {
    fn apply_to_import_summary(&self, summary: &mut ImportSummary) {
        summary.auto_queue_entries_skipped_orphans = self.auto_queue_entries;
        summary.auto_queue_entry_transitions_skipped_orphans = self.auto_queue_entry_transitions;
        summary.auto_queue_entry_dispatch_history_skipped_orphans =
            self.auto_queue_entry_dispatch_history;
        summary.auto_queue_phase_gates_skipped_orphans = self.auto_queue_phase_gates;
        summary.auto_queue_slots_skipped_orphans = self.auto_queue_slots;
        summary.dispatch_events_skipped_orphans = self.dispatch_events;
        summary.card_retrospectives_skipped_orphans = self.card_retrospectives;
        summary.card_review_state_skipped_orphans = self.card_review_state;
        summary.pr_tracking_skipped_orphans = self.pr_tracking;
        summary.session_termination_events_skipped_orphans = self.session_termination_events;
        summary.meeting_transcripts_skipped_orphans = self.meeting_transcripts;
    }
}

#[derive(Debug, Clone, Default, Serialize)]
struct PidFileSignal {
    path: String,
    exists: bool,
    pid: Option<u32>,
    process_alive: bool,
    error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct TcpSignal {
    host: String,
    port: u16,
    listening: bool,
    error: Option<String>,
}

#[derive(Debug, Default)]
struct SqliteCutoverSnapshot {
    counts: SqliteCutoverCounts,
    orphan_skips: OrphanSkipSummary,
    offices: Vec<OfficeRow>,
    departments: Vec<DepartmentRow>,
    office_agents: Vec<OfficeAgentRow>,
    github_repos: Vec<GithubRepoRow>,
    agents: Vec<AgentRow>,
    kanban_cards: Vec<KanbanCardRow>,
    kanban_audit_logs: Vec<KanbanAuditLogRow>,
    auto_queue_runs: Vec<AutoQueueRunRow>,
    auto_queue_entries: Vec<AutoQueueEntryRow>,
    auto_queue_entry_transitions: Vec<AutoQueueEntryTransitionRow>,
    auto_queue_entry_dispatch_history: Vec<AutoQueueEntryDispatchHistoryRow>,
    auto_queue_phase_gates: Vec<AutoQueuePhaseGateRow>,
    auto_queue_slots: Vec<AutoQueueSlotRow>,
    card_retrospectives: Vec<CardRetrospectiveRow>,
    card_review_state: Vec<CardReviewStateRow>,
    audit_logs: Vec<AuditLogRow>,
    session_transcripts: Vec<SessionTranscriptRow>,
    task_dispatches: Vec<TaskDispatchRow>,
    dispatch_events: Vec<DispatchEventRow>,
    dispatch_queue: Vec<DispatchQueueRow>,
    review_decisions: Vec<ReviewDecisionRow>,
    review_tuning_outcomes: Vec<ReviewTuningOutcomeRow>,
    sessions: Vec<SessionRow>,
    dispatch_outbox: Vec<DispatchOutboxRow>,
    session_termination_events: Vec<SessionTerminationEventRow>,
    turns: Vec<TurnRow>,
    meetings: Vec<MeetingRow>,
    meeting_transcripts: Vec<MeetingTranscriptRow>,
    messages: Vec<MessageRow>,
    message_outbox: Vec<MessageOutboxRow>,
    pending_dm_replies: Vec<PendingDmReplyRow>,
    pipeline_stages: Vec<PipelineStageRow>,
    pr_tracking: Vec<PrTrackingRow>,
    skills: Vec<SkillRow>,
    skill_usage: Vec<SkillUsageRow>,
    runtime_decisions: Vec<RuntimeDecisionRow>,
    kv_meta: Vec<KvMetaRow>,
    api_friction_events: Vec<ApiFrictionEventRow>,
    api_friction_issues: Vec<ApiFrictionIssueRow>,
    memento_feedback_turn_stats: Vec<MementoFeedbackTurnStatsRow>,
    rate_limit_cache: Vec<RateLimitCacheRow>,
    deferred_hooks: Vec<DeferredHookRow>,
}

fn collect_string_ids(ids: impl Iterator<Item = String>) -> BTreeSet<String> {
    ids.collect()
}

fn optional_parent_missing(parent_id: &Option<String>, parent_ids: &BTreeSet<String>) -> bool {
    match parent_id.as_deref() {
        Some(id) => !parent_ids.contains(id),
        None => false,
    }
}

fn clear_missing_optional_parent(parent_id: &mut Option<String>, parent_ids: &BTreeSet<String>) {
    if optional_parent_missing(parent_id, parent_ids) {
        *parent_id = None;
    }
}

fn retain_rows<T>(rows: &mut Vec<T>, mut keep: impl FnMut(&T) -> bool) -> i64 {
    let before = rows.len();
    rows.retain(|row| keep(row));
    before as i64 - rows.len() as i64
}

fn prune_sqlite_cutover_orphans(snapshot: &mut SqliteCutoverSnapshot) -> OrphanSkipSummary {
    let office_ids = collect_string_ids(snapshot.offices.iter().map(|row| row.id.clone()));
    for row in &mut snapshot.departments {
        clear_missing_optional_parent(&mut row.office_id, &office_ids);
    }

    let agent_ids = collect_string_ids(snapshot.agents.iter().map(|row| row.id.clone()));
    for row in &mut snapshot.kanban_cards {
        clear_missing_optional_parent(&mut row.assigned_agent_id, &agent_ids);
    }
    for row in &mut snapshot.sessions {
        clear_missing_optional_parent(&mut row.agent_id, &agent_ids);
    }

    let card_ids = collect_string_ids(snapshot.kanban_cards.iter().map(|row| row.id.clone()));
    for row in &mut snapshot.task_dispatches {
        clear_missing_optional_parent(&mut row.kanban_card_id, &card_ids);
    }
    for row in &mut snapshot.dispatch_queue {
        clear_missing_optional_parent(&mut row.kanban_card_id, &card_ids);
    }
    for row in &mut snapshot.review_decisions {
        clear_missing_optional_parent(&mut row.kanban_card_id, &card_ids);
    }

    let dispatch_ids =
        collect_string_ids(snapshot.task_dispatches.iter().map(|row| row.id.clone()));
    let session_keys =
        collect_string_ids(snapshot.sessions.iter().map(|row| row.session_key.clone()));
    let meeting_ids = collect_string_ids(snapshot.meetings.iter().map(|row| row.id.clone()));
    let run_ids = collect_string_ids(snapshot.auto_queue_runs.iter().map(|row| row.id.clone()));

    let mut summary = OrphanSkipSummary::default();

    summary.dispatch_events = retain_rows(&mut snapshot.dispatch_events, |row| {
        dispatch_ids.contains(&row.dispatch_id)
    });
    for row in &mut snapshot.dispatch_events {
        clear_missing_optional_parent(&mut row.kanban_card_id, &card_ids);
    }

    summary.card_retrospectives = retain_rows(&mut snapshot.card_retrospectives, |row| {
        card_ids.contains(&row.card_id) && dispatch_ids.contains(&row.dispatch_id)
    });
    summary.card_review_state = retain_rows(&mut snapshot.card_review_state, |row| {
        card_ids.contains(&row.card_id)
    });
    summary.pr_tracking = retain_rows(&mut snapshot.pr_tracking, |row| {
        card_ids.contains(&row.card_id)
    });
    summary.session_termination_events =
        retain_rows(&mut snapshot.session_termination_events, |row| {
            session_keys.contains(&row.session_key)
        });
    summary.meeting_transcripts = retain_rows(&mut snapshot.meeting_transcripts, |row| {
        !optional_parent_missing(&row.meeting_id, &meeting_ids)
    });

    summary.auto_queue_entries = retain_rows(&mut snapshot.auto_queue_entries, |row| {
        !optional_parent_missing(&row.run_id, &run_ids)
    });
    for row in &mut snapshot.auto_queue_entries {
        clear_missing_optional_parent(&mut row.kanban_card_id, &card_ids);
    }

    let entry_ids =
        collect_string_ids(snapshot.auto_queue_entries.iter().map(|row| row.id.clone()));

    summary.auto_queue_entry_transitions =
        retain_rows(&mut snapshot.auto_queue_entry_transitions, |row| {
            entry_ids.contains(&row.entry_id)
        });
    summary.auto_queue_entry_dispatch_history =
        retain_rows(&mut snapshot.auto_queue_entry_dispatch_history, |row| {
            entry_ids.contains(&row.entry_id) && dispatch_ids.contains(&row.dispatch_id)
        });
    summary.auto_queue_phase_gates = retain_rows(&mut snapshot.auto_queue_phase_gates, |row| {
        run_ids.contains(&row.run_id)
    });
    for row in &mut snapshot.auto_queue_phase_gates {
        clear_missing_optional_parent(&mut row.dispatch_id, &dispatch_ids);
        clear_missing_optional_parent(&mut row.anchor_card_id, &card_ids);
    }
    summary.auto_queue_slots = retain_rows(&mut snapshot.auto_queue_slots, |row| {
        !optional_parent_missing(&row.assigned_run_id, &run_ids)
    });

    summary
}

fn orphan_skip_warnings(summary: &ImportSummary) -> Vec<String> {
    [
        (
            "auto_queue_entries",
            summary.auto_queue_entries_skipped_orphans,
        ),
        (
            "auto_queue_entry_transitions",
            summary.auto_queue_entry_transitions_skipped_orphans,
        ),
        (
            "auto_queue_entry_dispatch_history",
            summary.auto_queue_entry_dispatch_history_skipped_orphans,
        ),
        (
            "auto_queue_phase_gates",
            summary.auto_queue_phase_gates_skipped_orphans,
        ),
        ("auto_queue_slots", summary.auto_queue_slots_skipped_orphans),
        ("dispatch_events", summary.dispatch_events_skipped_orphans),
        (
            "card_retrospectives",
            summary.card_retrospectives_skipped_orphans,
        ),
        (
            "card_review_state",
            summary.card_review_state_skipped_orphans,
        ),
        ("pr_tracking", summary.pr_tracking_skipped_orphans),
        (
            "session_termination_events",
            summary.session_termination_events_skipped_orphans,
        ),
        (
            "meeting_transcripts",
            summary.meeting_transcripts_skipped_orphans,
        ),
    ]
    .into_iter()
    .filter(|(_, count)| *count > 0)
    .map(|(table, count)| format!("postgres cutover skipped {count} orphan rows from {table}"))
    .collect()
}

#[derive(Debug, Clone, Serialize)]
struct AuditLogRow {
    entity_type: Option<String>,
    entity_id: Option<String>,
    action: Option<String>,
    timestamp: Option<String>,
    actor: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SessionTranscriptRow {
    turn_id: String,
    session_key: Option<String>,
    channel_id: Option<String>,
    agent_id: Option<String>,
    provider: Option<String>,
    dispatch_id: Option<String>,
    user_message: String,
    assistant_message: String,
    events_json: String,
    duration_ms: Option<i64>,
    created_at: Option<String>,
}

#[derive(Debug, Clone)]
struct AgentRow {
    id: String,
    name: String,
    name_ko: Option<String>,
    department: Option<String>,
    provider: Option<String>,
    discord_channel_id: Option<String>,
    discord_channel_alt: Option<String>,
    discord_channel_cc: Option<String>,
    discord_channel_cdx: Option<String>,
    avatar_emoji: Option<String>,
    status: Option<String>,
    xp: Option<i64>,
    skills: Option<String>,
    sprite_number: Option<i64>,
    description: Option<String>,
    system_prompt: Option<String>,
    pipeline_config: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Debug, Clone)]
struct GithubRepoRow {
    id: String,
    display_name: Option<String>,
    sync_enabled: Option<bool>,
    last_synced_at: Option<String>,
    default_agent_id: Option<String>,
    pipeline_config: Option<String>,
}

#[derive(Debug, Clone)]
struct KanbanCardRow {
    id: String,
    repo_id: Option<String>,
    title: String,
    status: Option<String>,
    priority: Option<String>,
    assigned_agent_id: Option<String>,
    github_issue_url: Option<String>,
    github_issue_number: Option<i64>,
    latest_dispatch_id: Option<String>,
    review_round: Option<i64>,
    metadata: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    blocked_reason: Option<String>,
    pipeline_stage_id: Option<String>,
    review_notes: Option<String>,
    review_status: Option<String>,
    requested_at: Option<String>,
    owner_agent_id: Option<String>,
    requester_agent_id: Option<String>,
    parent_card_id: Option<String>,
    depth: Option<i64>,
    sort_order: Option<i64>,
    description: Option<String>,
    active_thread_id: Option<String>,
    channel_thread_map: Option<String>,
    suggestion_pending_at: Option<String>,
    review_entered_at: Option<String>,
    awaiting_dod_at: Option<String>,
    deferred_dod_json: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Debug, Clone)]
struct KanbanAuditLogRow {
    id: i64,
    card_id: Option<String>,
    from_status: Option<String>,
    to_status: Option<String>,
    source: Option<String>,
    result: Option<String>,
    created_at: Option<String>,
}

#[derive(Debug, Clone)]
struct TaskDispatchRow {
    id: String,
    kanban_card_id: Option<String>,
    from_agent_id: Option<String>,
    to_agent_id: Option<String>,
    dispatch_type: Option<String>,
    status: Option<String>,
    title: Option<String>,
    context: Option<String>,
    result: Option<String>,
    parent_dispatch_id: Option<String>,
    chain_depth: Option<i64>,
    thread_id: Option<String>,
    retry_count: Option<i64>,
    created_at: Option<String>,
    updated_at: Option<String>,
    completed_at: Option<String>,
}

#[derive(Debug, Clone)]
struct DispatchEventRow {
    id: i64,
    dispatch_id: String,
    kanban_card_id: Option<String>,
    dispatch_type: Option<String>,
    from_status: Option<String>,
    to_status: String,
    transition_source: String,
    payload_json: Option<String>,
    created_at: Option<String>,
}

#[derive(Debug, Clone)]
struct DispatchQueueRow {
    id: i64,
    kanban_card_id: Option<String>,
    priority_score: Option<f64>,
    queued_at: Option<String>,
}

#[derive(Debug, Clone)]
struct SessionRow {
    session_key: String,
    agent_id: Option<String>,
    provider: Option<String>,
    status: Option<String>,
    active_dispatch_id: Option<String>,
    model: Option<String>,
    session_info: Option<String>,
    tokens: Option<i64>,
    cwd: Option<String>,
    last_heartbeat: Option<String>,
    thread_channel_id: Option<String>,
    claude_session_id: Option<String>,
    raw_provider_session_id: Option<String>,
    created_at: Option<String>,
}

#[derive(Debug, Clone)]
struct DispatchOutboxRow {
    id: i64,
    dispatch_id: String,
    action: String,
    agent_id: Option<String>,
    card_id: Option<String>,
    title: Option<String>,
    status: String,
    retry_count: Option<i64>,
    next_attempt_at: Option<String>,
    created_at: Option<String>,
    processed_at: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct CardRetrospectiveRow {
    id: String,
    card_id: String,
    dispatch_id: String,
    terminal_status: String,
    repo_id: Option<String>,
    issue_number: Option<i64>,
    title: String,
    topic: String,
    content: String,
    review_round: Option<i64>,
    review_notes: Option<String>,
    duration_seconds: Option<i64>,
    success: Option<bool>,
    result_json: String,
    memory_payload: String,
    sync_backend: Option<String>,
    sync_status: Option<String>,
    sync_error: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Debug, Clone)]
struct CardReviewStateRow {
    card_id: String,
    review_round: Option<i64>,
    state: Option<String>,
    pending_dispatch_id: Option<String>,
    last_verdict: Option<String>,
    last_decision: Option<String>,
    decided_by: Option<String>,
    decided_at: Option<String>,
    approach_change_round: Option<i64>,
    session_reset_round: Option<i64>,
    review_entered_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Debug, Clone)]
struct AutoQueueRunRow {
    id: String,
    repo: Option<String>,
    agent_id: Option<String>,
    status: Option<String>,
    ai_model: Option<String>,
    ai_rationale: Option<String>,
    timeout_minutes: Option<i64>,
    unified_thread: Option<bool>,
    unified_thread_id: Option<String>,
    unified_thread_channel_id: Option<String>,
    max_concurrent_threads: Option<i64>,
    thread_group_count: Option<i64>,
    created_at: Option<String>,
    completed_at: Option<String>,
}

#[derive(Debug, Clone)]
struct AutoQueueEntryRow {
    id: String,
    run_id: Option<String>,
    kanban_card_id: Option<String>,
    agent_id: Option<String>,
    priority_rank: Option<i64>,
    reason: Option<String>,
    status: Option<String>,
    retry_count: Option<i64>,
    dispatch_id: Option<String>,
    slot_index: Option<i64>,
    thread_group: Option<i64>,
    batch_phase: Option<i64>,
    created_at: Option<String>,
    dispatched_at: Option<String>,
    completed_at: Option<String>,
}

#[derive(Debug, Clone)]
struct AutoQueueEntryTransitionRow {
    id: i64,
    entry_id: String,
    from_status: Option<String>,
    to_status: String,
    trigger_source: String,
    created_at: Option<String>,
}

#[derive(Debug, Clone)]
struct AutoQueueEntryDispatchHistoryRow {
    id: i64,
    entry_id: String,
    dispatch_id: String,
    trigger_source: Option<String>,
    created_at: Option<String>,
}

#[derive(Debug, Clone)]
struct AutoQueuePhaseGateRow {
    id: i64,
    run_id: String,
    phase: Option<i64>,
    status: Option<String>,
    verdict: Option<String>,
    dispatch_id: Option<String>,
    pass_verdict: Option<String>,
    next_phase: Option<i64>,
    final_phase: Option<bool>,
    anchor_card_id: Option<String>,
    failure_reason: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Debug, Clone)]
struct AutoQueueSlotRow {
    agent_id: String,
    slot_index: i64,
    assigned_run_id: Option<String>,
    assigned_thread_group: Option<i64>,
    thread_id_map: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Debug, Clone)]
struct ReviewDecisionRow {
    id: i64,
    kanban_card_id: Option<String>,
    dispatch_id: Option<String>,
    item_index: Option<i64>,
    decision: Option<String>,
    decided_at: Option<String>,
}

#[derive(Debug, Clone)]
struct ReviewTuningOutcomeRow {
    id: i64,
    card_id: Option<String>,
    dispatch_id: Option<String>,
    review_round: Option<i64>,
    verdict: String,
    decision: Option<String>,
    outcome: String,
    finding_categories: Option<String>,
    created_at: Option<String>,
}

#[derive(Debug, Clone)]
struct MessageRow {
    id: i64,
    sender_type: Option<String>,
    sender_id: Option<String>,
    receiver_type: Option<String>,
    receiver_id: Option<String>,
    content: Option<String>,
    message_type: Option<String>,
    created_at: Option<String>,
}

#[derive(Debug, Clone)]
struct MessageOutboxRow {
    id: i64,
    target: String,
    content: String,
    bot: Option<String>,
    source: Option<String>,
    reason_code: Option<String>,
    session_key: Option<String>,
    status: Option<String>,
    created_at: Option<String>,
    sent_at: Option<String>,
    error: Option<String>,
    claimed_at: Option<String>,
    claim_owner: Option<String>,
}

#[derive(Debug, Clone)]
struct MeetingRow {
    id: String,
    channel_id: Option<String>,
    title: Option<String>,
    status: Option<String>,
    effective_rounds: Option<i64>,
    started_at: Option<String>,
    completed_at: Option<String>,
    summary: Option<String>,
    thread_id: Option<String>,
    primary_provider: Option<String>,
    reviewer_provider: Option<String>,
    participant_names: Option<String>,
    selection_reason: Option<String>,
    created_at: Option<i64>,
}

#[derive(Debug, Clone)]
struct MeetingTranscriptRow {
    id: i64,
    meeting_id: Option<String>,
    seq: Option<i64>,
    round: Option<i64>,
    speaker_agent_id: Option<String>,
    speaker_name: Option<String>,
    content: Option<String>,
    is_summary: Option<bool>,
}

#[derive(Debug, Clone)]
struct PendingDmReplyRow {
    id: i64,
    source_agent: String,
    user_id: String,
    channel_id: Option<String>,
    context: String,
    status: Option<String>,
    created_at: Option<String>,
    consumed_at: Option<String>,
    expires_at: Option<String>,
}

#[derive(Debug, Clone)]
struct PipelineStageRow {
    id: i64,
    repo_id: Option<String>,
    stage_name: Option<String>,
    stage_order: Option<i64>,
    trigger_after: Option<String>,
    entry_skill: Option<String>,
    timeout_minutes: Option<i64>,
    on_failure: Option<String>,
    skip_condition: Option<String>,
    provider: Option<String>,
    agent_override_id: Option<String>,
    on_failure_target: Option<String>,
    max_retries: Option<i64>,
    parallel_with: Option<String>,
}

#[derive(Debug, Clone)]
struct PrTrackingRow {
    card_id: String,
    repo_id: Option<String>,
    worktree_path: Option<String>,
    branch: Option<String>,
    pr_number: Option<i64>,
    head_sha: Option<String>,
    state: Option<String>,
    last_error: Option<String>,
    dispatch_generation: Option<String>,
    review_round: Option<i64>,
    retry_count: Option<i64>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Debug, Clone)]
struct SkillRow {
    id: String,
    name: Option<String>,
    description: Option<String>,
    source_path: Option<String>,
    trigger_patterns: Option<String>,
    updated_at: Option<String>,
}

#[derive(Debug, Clone)]
struct SkillUsageRow {
    id: i64,
    skill_id: Option<String>,
    agent_id: Option<String>,
    session_key: Option<String>,
    used_at: Option<String>,
}

#[derive(Debug, Clone)]
struct RuntimeDecisionRow {
    id: i64,
    signal: String,
    evidence_json: String,
    chosen_action: String,
    actor: String,
    session_key: Option<String>,
    dispatch_id: Option<String>,
    created_at: Option<String>,
}

#[derive(Debug, Clone)]
struct SessionTerminationEventRow {
    id: i64,
    session_key: String,
    dispatch_id: Option<String>,
    killer_component: String,
    reason_code: String,
    reason_text: Option<String>,
    probe_snapshot: Option<String>,
    last_offset: Option<i64>,
    tmux_alive: Option<i64>,
    created_at: Option<String>,
}

#[derive(Debug, Clone)]
struct TurnRow {
    turn_id: String,
    session_key: Option<String>,
    thread_id: Option<String>,
    thread_title: Option<String>,
    channel_id: String,
    agent_id: Option<String>,
    provider: Option<String>,
    session_id: Option<String>,
    dispatch_id: Option<String>,
    started_at: String,
    finished_at: String,
    duration_ms: Option<i64>,
    input_tokens: Option<i64>,
    cache_create_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    output_tokens: Option<i64>,
    created_at: Option<String>,
}

#[derive(Debug, Clone)]
struct DepartmentRow {
    id: String,
    name: Option<String>,
    office_id: Option<String>,
    name_ko: Option<String>,
    icon: Option<String>,
    color: Option<String>,
    description: Option<String>,
    sort_order: Option<i64>,
    created_at: Option<String>,
}

#[derive(Debug, Clone)]
struct OfficeRow {
    id: String,
    name: Option<String>,
    layout: Option<String>,
    name_ko: Option<String>,
    icon: Option<String>,
    color: Option<String>,
    description: Option<String>,
    sort_order: Option<i64>,
    created_at: Option<String>,
}

#[derive(Debug, Clone)]
struct OfficeAgentRow {
    office_id: String,
    agent_id: String,
    department_id: Option<String>,
    joined_at: Option<String>,
}

#[derive(Debug, Clone)]
struct KvMetaRow {
    key: String,
    value: Option<String>,
    expires_at: Option<String>,
}

#[derive(Debug, Clone)]
struct ApiFrictionEventRow {
    id: String,
    fingerprint: String,
    endpoint: String,
    friction_type: String,
    summary: String,
    workaround: Option<String>,
    suggested_fix: Option<String>,
    docs_category: Option<String>,
    keywords_json: String,
    payload_json: String,
    session_key: Option<String>,
    channel_id: Option<String>,
    provider: Option<String>,
    dispatch_id: Option<String>,
    card_id: Option<String>,
    repo_id: Option<String>,
    github_issue_number: Option<i64>,
    task_summary: Option<String>,
    agent_id: Option<String>,
    memory_backend: Option<String>,
    memory_status: Option<String>,
    memory_error: Option<String>,
    created_at: Option<String>,
}

#[derive(Debug, Clone)]
struct ApiFrictionIssueRow {
    fingerprint: String,
    repo_id: String,
    endpoint: String,
    friction_type: String,
    title: String,
    body: String,
    issue_number: Option<i64>,
    issue_url: Option<String>,
    event_count: Option<i64>,
    first_event_at: Option<String>,
    last_event_at: Option<String>,
    last_error: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Debug, Clone)]
struct MementoFeedbackTurnStatsRow {
    turn_id: String,
    stat_date: String,
    agent_id: String,
    provider: String,
    recall_count: Option<i64>,
    manual_tool_feedback_count: Option<i64>,
    manual_covered_recall_count: Option<i64>,
    auto_tool_feedback_count: Option<i64>,
    covered_recall_count: Option<i64>,
    created_at: Option<String>,
}

#[derive(Debug, Clone)]
struct RateLimitCacheRow {
    provider: String,
    data: Option<String>,
    fetched_at: Option<i64>,
}

#[derive(Debug, Clone)]
struct DeferredHookRow {
    id: i64,
    hook_name: String,
    payload: String,
    status: Option<String>,
    created_at: Option<String>,
}

#[deprecated(
    note = "production cutover complete on 2026-04-19; reuse only for hypothetical re-cutover scenarios. See ARCHITECTURE.md or epic #834."
)]
pub async fn cmd_migrate_postgres_cutover(args: PostgresCutoverArgs) -> Result<(), String> {
    if !args.dry_run && args.skip_pg_import && args.archive_dir.is_none() {
        return Err(
            "postgres-cutover needs at least one action: omit --skip-pg-import or pass --archive-dir"
                .to_string(),
        );
    }

    let config = load_effective_config()?;
    let runtime_active = if args.skip_pg_import {
        Some(detect_runtime_active(
            crate::config::runtime_root().as_deref(),
            &config.server.host,
            config.server.port,
            args.allow_runtime_active,
        ))
    } else {
        None
    };
    // dry_run only prints counts/blockers — no rows need to be loaded into memory
    // for either history (archive) or full state (PG import). This keeps preflight
    // O(table-count SELECT COUNT(*)) instead of O(total-row-bytes), so large
    // installations don't OOM their preflight check.
    let need_history_rows = !args.dry_run && (args.archive_dir.is_some() || !args.skip_pg_import);
    let need_full_rows = !args.dry_run && !args.skip_pg_import;
    let pg_pool = if args.skip_pg_import {
        None
    } else {
        Some(connect_postgres_for_cutover(&config).await?)
    };
    let sqlite_path = config.data.dir.join(&config.data.db_name);
    let sqlite = if !args.dry_run && !args.skip_pg_import {
        crate::db::open_write_connection(&sqlite_path)
    } else {
        crate::db::open_read_only_connection(&sqlite_path)
    }
    .map_err(|e| {
        format!(
            "open sqlite cutover connection {}: {e}",
            sqlite_path.display()
        )
    })?;
    let barrier_active = if !args.dry_run && !args.skip_pg_import {
        sqlite
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| format!("acquire sqlite cutover write barrier: {e}"))?;
        true
    } else {
        false
    };
    let result: Result<PostgresCutoverReport, String> = async {
        let snapshot = load_sqlite_cutover_snapshot(&sqlite, need_history_rows, need_full_rows)?;

        let pg_before = if let Some(pool) = pg_pool.as_ref() {
            Some(load_pg_cutover_counts(pool).await?)
        } else {
            None
        };

        let mut report = PostgresCutoverReport {
            ok: false,
            sqlite: snapshot.counts.clone(),
            postgres_before: pg_before,
            postgres_after: None,
            archive: None,
            imported: None,
            runtime_active: runtime_active.clone(),
            blocker: None,
        };

        report.blocker = cutover_blocker(&args, &report.sqlite, runtime_active.as_ref());

        if args.dry_run || report.blocker.is_some() {
            report.ok = report.blocker.is_none();
            return Ok(report);
        }

        if let Some(dir) = args.archive_dir.as_deref() {
            report.archive = Some(write_archive_files(
                dir,
                &snapshot.audit_logs,
                &snapshot.session_transcripts,
            )?);
        }

        if let Some(pool) = pg_pool.as_ref() {
            report.imported = Some(import_full_state_into_pg(pool, &snapshot).await?);
            report.postgres_after = Some(load_pg_cutover_counts(pool).await?);
        }

        report.ok = report.blocker.is_none();
        Ok(report)
    }
    .await;

    if barrier_active {
        sqlite
            .execute_batch("ROLLBACK")
            .map_err(|e| format!("release sqlite cutover write barrier: {e}"))?;
    }

    let report = result?;
    print_report(&report)?;
    if let Some(imported) = report.imported.as_ref() {
        for warning in orphan_skip_warnings(imported) {
            eprintln!("WARN: {warning}");
        }
    }
    if let Some(blocker) = report.blocker {
        return Err(blocker);
    }
    Ok(())
}

fn cutover_blocker(
    args: &PostgresCutoverArgs,
    sqlite_counts: &SqliteCutoverCounts,
    runtime_active: Option<&RuntimeActiveStatus>,
) -> Option<String> {
    if args.skip_pg_import {
        if let Some(status) = runtime_active
            && status.active
            && !status.overridden
        {
            return Some(format!(
                "dcserver runtime appears active ({}); archive-only cutover skips the SQLite write \
                 barrier and would race against live audit_logs/session_transcripts writes. Stop \
                 dcserver first (e.g. `launchctl bootout gui/$(id -u)/com.agentdesk.release`) or \
                 pass `--allow-runtime-active` if the workload is provably frozen.",
                describe_runtime_active(status)
            ));
        }

        if sqlite_counts.has_live_state() {
            return Some(
                "sqlite still has in-flight dispatch/session/outbox/message state; archive-only cutover would lose it. Omit --skip-pg-import or drain runtime to idle first."
                    .to_string(),
            );
        }
    }

    if !args.skip_pg_import && sqlite_counts.open_dispatch_outbox > 0 {
        return Some(
            "sqlite still has open dispatch_outbox rows; drain outbox before PG cutover to avoid duplicate delivery."
                .to_string(),
        );
    }

    if !args.skip_pg_import
        && sqlite_counts.pending_message_outbox > 0
        && !args.allow_unsent_messages
    {
        return Some(format!(
            "sqlite still has {count} pending message_outbox row(s); these Discord messages would be lost on cutover. \
Drain by letting the message-outbox worker settle (restart dcserver if it is stalled) or pass --allow-unsent-messages \
after confirming the rows are stale and safe to drop.",
            count = sqlite_counts.pending_message_outbox,
        ));
    }

    None
}

fn describe_runtime_active(status: &RuntimeActiveStatus) -> String {
    let mut signals = Vec::new();
    if let Some(pid) = status.pid_file.as_ref() {
        if pid.process_alive {
            match pid.pid {
                Some(value) => signals.push(format!("pid {value} alive at {}", pid.path)),
                None => signals.push(format!("pid file alive at {}", pid.path)),
            }
        } else if let Some(error) = pid.error.as_ref() {
            signals.push(format!("pid probe error: {error}"));
        }
    }
    if let Some(tcp) = status.tcp.as_ref() {
        if tcp.listening {
            signals.push(format!(
                "TCP {host}:{port} accepting connections",
                host = tcp.host,
                port = tcp.port
            ));
        } else if let Some(error) = tcp.error.as_ref() {
            signals.push(format!(
                "TCP probe error ({}:{}): {error}",
                tcp.host, tcp.port
            ));
        }
    }
    if signals.is_empty() {
        "no specific signals captured".to_string()
    } else {
        signals.join("; ")
    }
}

const RUNTIME_TCP_PROBE_TIMEOUT: Duration = Duration::from_millis(400);

fn detect_runtime_active(
    runtime_root: Option<&Path>,
    host: &str,
    port: u16,
    allow_override: bool,
) -> RuntimeActiveStatus {
    let pid_signal = runtime_root.map(probe_pid_file);
    let tcp_signal = probe_server_tcp(host, port, RUNTIME_TCP_PROBE_TIMEOUT);
    let pid_alive = pid_signal.as_ref().is_some_and(|p| p.process_alive);
    let tcp_listening = tcp_signal.as_ref().is_some_and(|t| t.listening);
    let pid_uncertain = pid_signal.as_ref().is_some_and(|p| p.error.is_some());
    let tcp_uncertain = tcp_signal.as_ref().is_some_and(|t| t.error.is_some());
    RuntimeActiveStatus {
        active: pid_alive || tcp_listening || pid_uncertain || tcp_uncertain,
        pid_file: pid_signal,
        tcp: tcp_signal,
        overridden: allow_override,
    }
}

fn probe_pid_file(runtime_root: &Path) -> PidFileSignal {
    let path = runtime_root.join("runtime").join("dcserver.pid");
    let exists = path.exists();
    let display = path.display().to_string();
    if !exists {
        return PidFileSignal {
            path: display,
            exists: false,
            ..Default::default()
        };
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(value) => value,
        Err(error) => {
            return PidFileSignal {
                path: display,
                exists: true,
                error: Some(format!("read pid file: {error}")),
                ..Default::default()
            };
        }
    };
    let pid = match raw.trim().parse::<u32>() {
        Ok(value) => value,
        Err(error) => {
            return PidFileSignal {
                path: display,
                exists: true,
                error: Some(format!("parse pid '{}': {error}", raw.trim())),
                ..Default::default()
            };
        }
    };
    PidFileSignal {
        path: display,
        exists: true,
        pid: Some(pid),
        process_alive: process_is_alive(pid),
        error: None,
    }
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    unsafe {
        if libc::kill(pid as libc::pid_t, 0) == 0 {
            return true;
        }
        let err = std::io::Error::last_os_error();
        err.raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

fn probe_server_tcp(host: &str, port: u16, timeout: Duration) -> Option<TcpSignal> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized =
        if trimmed == "0.0.0.0" || trimmed == "::" || trimmed.eq_ignore_ascii_case("[::]") {
            "127.0.0.1".to_string()
        } else {
            trimmed.to_string()
        };
    let addrs: Vec<SocketAddr> = match (normalized.as_str(), port).to_socket_addrs() {
        Ok(iter) => iter.collect(),
        Err(error) => {
            return Some(TcpSignal {
                host: normalized,
                port,
                listening: false,
                error: Some(format!("resolve socket: {error}")),
            });
        }
    };
    if addrs.is_empty() {
        return Some(TcpSignal {
            host: normalized,
            port,
            listening: false,
            error: Some("no socket addresses resolved".to_string()),
        });
    }
    let mut last_uncertain_error = None;
    for addr in addrs {
        match std::net::TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => {
                let _ = stream.shutdown(std::net::Shutdown::Both);
                return Some(TcpSignal {
                    host: normalized,
                    port,
                    listening: true,
                    error: None,
                });
            }
            Err(error) => {
                if error.kind() == std::io::ErrorKind::ConnectionRefused {
                    continue;
                }
                last_uncertain_error = Some(format!("tcp connect {addr}: {error}"));
            }
        }
    }
    Some(TcpSignal {
        host: normalized,
        port,
        listening: false,
        error: last_uncertain_error,
    })
}

fn load_rows_if_needed<T>(
    should_load: bool,
    loader: impl FnOnce() -> Result<Vec<T>, String>,
) -> Result<Vec<T>, String> {
    if should_load {
        loader()
    } else {
        Ok(Vec::new())
    }
}

fn load_sqlite_cutover_snapshot(
    sqlite: &Connection,
    need_history_rows: bool,
    need_full_rows: bool,
) -> Result<SqliteCutoverSnapshot, String> {
    let counts = sqlite_cutover_counts(sqlite)?;
    let mut snapshot = SqliteCutoverSnapshot {
        counts: counts.clone(),
        orphan_skips: OrphanSkipSummary::default(),
        offices: load_rows_if_needed(need_full_rows && counts.offices > 0, || {
            load_all_offices(sqlite)
        })?,
        departments: load_rows_if_needed(need_full_rows && counts.departments > 0, || {
            load_all_departments(sqlite)
        })?,
        office_agents: load_rows_if_needed(need_full_rows && counts.office_agents > 0, || {
            load_all_office_agents(sqlite)
        })?,
        github_repos: load_rows_if_needed(need_full_rows && counts.github_repos > 0, || {
            load_all_github_repos(sqlite)
        })?,
        agents: load_rows_if_needed(need_full_rows && counts.agents > 0, || {
            load_all_agents(sqlite)
        })?,
        kanban_cards: load_rows_if_needed(need_full_rows && counts.kanban_cards > 0, || {
            load_all_kanban_cards(sqlite)
        })?,
        kanban_audit_logs: load_rows_if_needed(
            need_full_rows && counts.kanban_audit_logs > 0,
            || load_all_kanban_audit_logs(sqlite),
        )?,
        auto_queue_runs: load_rows_if_needed(need_full_rows && counts.auto_queue_runs > 0, || {
            load_all_auto_queue_runs(sqlite)
        })?,
        auto_queue_entries: load_rows_if_needed(
            need_full_rows && counts.auto_queue_entries > 0,
            || load_all_auto_queue_entries(sqlite),
        )?,
        auto_queue_entry_transitions: load_rows_if_needed(
            need_full_rows && counts.auto_queue_entry_transitions > 0,
            || load_all_auto_queue_entry_transitions(sqlite),
        )?,
        auto_queue_entry_dispatch_history: load_rows_if_needed(
            need_full_rows && counts.auto_queue_entry_dispatch_history > 0,
            || load_all_auto_queue_entry_dispatch_history(sqlite),
        )?,
        auto_queue_phase_gates: load_rows_if_needed(
            need_full_rows && counts.auto_queue_phase_gates > 0,
            || load_all_auto_queue_phase_gates(sqlite),
        )?,
        auto_queue_slots: load_rows_if_needed(
            need_full_rows && counts.auto_queue_slots > 0,
            || load_all_auto_queue_slots(sqlite),
        )?,
        card_retrospectives: load_rows_if_needed(
            need_full_rows && counts.card_retrospectives > 0,
            || load_all_card_retrospectives(sqlite),
        )?,
        card_review_state: load_rows_if_needed(
            need_full_rows && counts.card_review_state > 0,
            || load_all_card_review_state(sqlite),
        )?,
        audit_logs: load_rows_if_needed(need_history_rows && counts.audit_logs > 0, || {
            load_audit_logs(sqlite)
        })?,
        session_transcripts: load_rows_if_needed(
            need_history_rows && counts.session_transcripts > 0,
            || load_session_transcripts(sqlite),
        )?,
        task_dispatches: load_rows_if_needed(need_full_rows && counts.task_dispatches > 0, || {
            load_all_task_dispatches(sqlite)
        })?,
        dispatch_events: load_rows_if_needed(need_full_rows && counts.dispatch_events > 0, || {
            load_all_dispatch_events(sqlite)
        })?,
        dispatch_queue: load_rows_if_needed(need_full_rows && counts.dispatch_queue > 0, || {
            load_all_dispatch_queue(sqlite)
        })?,
        review_decisions: load_rows_if_needed(
            need_full_rows && counts.review_decisions > 0,
            || load_all_review_decisions(sqlite),
        )?,
        review_tuning_outcomes: load_rows_if_needed(
            need_full_rows && counts.review_tuning_outcomes > 0,
            || load_all_review_tuning_outcomes(sqlite),
        )?,
        sessions: load_rows_if_needed(need_full_rows && counts.sessions > 0, || {
            load_all_sessions(sqlite)
        })?,
        dispatch_outbox: load_rows_if_needed(need_full_rows, || load_all_dispatch_outbox(sqlite))?,
        session_termination_events: load_rows_if_needed(
            need_full_rows && counts.session_termination_events > 0,
            || load_all_session_termination_events(sqlite),
        )?,
        turns: load_rows_if_needed(need_full_rows && counts.turns > 0, || {
            load_all_turns(sqlite)
        })?,
        meetings: load_rows_if_needed(need_full_rows && counts.meetings > 0, || {
            load_all_meetings(sqlite)
        })?,
        meeting_transcripts: load_rows_if_needed(
            need_full_rows && counts.meeting_transcripts > 0,
            || load_all_meeting_transcripts(sqlite),
        )?,
        messages: load_rows_if_needed(need_full_rows && counts.messages > 0, || {
            load_all_messages(sqlite)
        })?,
        message_outbox: load_rows_if_needed(need_full_rows && counts.message_outbox > 0, || {
            load_all_message_outbox(sqlite)
        })?,
        pending_dm_replies: load_rows_if_needed(
            need_full_rows && counts.pending_dm_replies > 0,
            || load_all_pending_dm_replies(sqlite),
        )?,
        pipeline_stages: load_rows_if_needed(need_full_rows && counts.pipeline_stages > 0, || {
            load_all_pipeline_stages(sqlite)
        })?,
        pr_tracking: load_rows_if_needed(need_full_rows && counts.pr_tracking > 0, || {
            load_all_pr_tracking(sqlite)
        })?,
        skills: load_rows_if_needed(need_full_rows && counts.skills > 0, || {
            load_all_skills(sqlite)
        })?,
        skill_usage: load_rows_if_needed(need_full_rows && counts.skill_usage > 0, || {
            load_all_skill_usage(sqlite)
        })?,
        runtime_decisions: load_rows_if_needed(
            need_full_rows && counts.runtime_decisions > 0,
            || load_all_runtime_decisions(sqlite),
        )?,
        kv_meta: load_rows_if_needed(need_full_rows && counts.kv_meta > 0, || {
            load_all_kv_meta(sqlite)
        })?,
        api_friction_events: load_rows_if_needed(
            need_full_rows && counts.api_friction_events > 0,
            || load_all_api_friction_events(sqlite),
        )?,
        api_friction_issues: load_rows_if_needed(
            need_full_rows && counts.api_friction_issues > 0,
            || load_all_api_friction_issues(sqlite),
        )?,
        memento_feedback_turn_stats: load_rows_if_needed(
            need_full_rows && counts.memento_feedback_turn_stats > 0,
            || load_all_memento_feedback_turn_stats(sqlite),
        )?,
        rate_limit_cache: load_rows_if_needed(
            need_full_rows && counts.rate_limit_cache > 0,
            || load_all_rate_limit_cache(sqlite),
        )?,
        deferred_hooks: load_rows_if_needed(need_full_rows && counts.deferred_hooks > 0, || {
            load_all_deferred_hooks(sqlite)
        })?,
    };
    snapshot.orphan_skips = prune_sqlite_cutover_orphans(&mut snapshot);
    Ok(snapshot)
}

fn print_report(report: &PostgresCutoverReport) -> Result<(), String> {
    let rendered = serde_json::to_string_pretty(report)
        .map_err(|e| format!("serialize postgres cutover report: {e}"))?;
    println!("{rendered}");
    Ok(())
}

fn load_effective_config() -> Result<Config, String> {
    if let Some(root) = crate::config::runtime_root() {
        return crate::services::discord_config_audit::load_runtime_config(&root)
            .map(|loaded| loaded.config)
            .map_err(|e| format!("load runtime config: {e}"));
    }

    crate::config::load().map_err(|e| format!("load config: {e}"))
}

async fn connect_postgres_for_cutover(config: &Config) -> Result<PgPool, String> {
    crate::db::postgres::connect_and_migrate(config)
        .await
        .and_then(|pool| {
            pool.ok_or_else(|| {
                "postgres is disabled; enable config.database or set DATABASE_URL before cutover"
                    .to_string()
            })
        })
}

fn sqlite_cutover_counts(conn: &Connection) -> Result<SqliteCutoverCounts, String> {
    Ok(SqliteCutoverCounts {
        agents: query_count_if_table_exists(conn, "agents", "SELECT COUNT(*) FROM agents")?,
        github_repos: query_count_if_table_exists(
            conn,
            "github_repos",
            "SELECT COUNT(*) FROM github_repos",
        )?,
        kanban_cards: query_count_if_table_exists(
            conn,
            "kanban_cards",
            "SELECT COUNT(*) FROM kanban_cards",
        )?,
        kanban_audit_logs: query_count_if_table_exists(
            conn,
            "kanban_audit_logs",
            "SELECT COUNT(*) FROM kanban_audit_logs",
        )?,
        auto_queue_runs: query_count_if_table_exists(
            conn,
            "auto_queue_runs",
            "SELECT COUNT(*) FROM auto_queue_runs",
        )?,
        auto_queue_entries: query_count_if_table_exists(
            conn,
            "auto_queue_entries",
            "SELECT COUNT(*) FROM auto_queue_entries",
        )?,
        auto_queue_entry_transitions: query_count_if_table_exists(
            conn,
            "auto_queue_entry_transitions",
            "SELECT COUNT(*) FROM auto_queue_entry_transitions",
        )?,
        auto_queue_entry_dispatch_history: query_count_if_table_exists(
            conn,
            "auto_queue_entry_dispatch_history",
            "SELECT COUNT(*) FROM auto_queue_entry_dispatch_history",
        )?,
        auto_queue_phase_gates: query_count_if_table_exists(
            conn,
            "auto_queue_phase_gates",
            "SELECT COUNT(*) FROM auto_queue_phase_gates",
        )?,
        auto_queue_slots: query_count_if_table_exists(
            conn,
            "auto_queue_slots",
            "SELECT COUNT(*) FROM auto_queue_slots",
        )?,
        task_dispatches: query_count_if_table_exists(
            conn,
            "task_dispatches",
            "SELECT COUNT(*) FROM task_dispatches",
        )?,
        dispatch_events: query_count_if_table_exists(
            conn,
            "dispatch_events",
            "SELECT COUNT(*) FROM dispatch_events",
        )?,
        dispatch_queue: query_count_if_table_exists(
            conn,
            "dispatch_queue",
            "SELECT COUNT(*) FROM dispatch_queue",
        )?,
        card_retrospectives: query_count_if_table_exists(
            conn,
            "card_retrospectives",
            "SELECT COUNT(*) FROM card_retrospectives",
        )?,
        card_review_state: query_count_if_table_exists(
            conn,
            "card_review_state",
            "SELECT COUNT(*) FROM card_review_state",
        )?,
        review_decisions: query_count_if_table_exists(
            conn,
            "review_decisions",
            "SELECT COUNT(*) FROM review_decisions",
        )?,
        review_tuning_outcomes: query_count_if_table_exists(
            conn,
            "review_tuning_outcomes",
            "SELECT COUNT(*) FROM review_tuning_outcomes",
        )?,
        messages: query_count_if_table_exists(conn, "messages", "SELECT COUNT(*) FROM messages")?,
        message_outbox: query_count_if_table_exists(
            conn,
            "message_outbox",
            "SELECT COUNT(*) FROM message_outbox",
        )?,
        meetings: query_count_if_table_exists(conn, "meetings", "SELECT COUNT(*) FROM meetings")?,
        meeting_transcripts: query_count_if_table_exists(
            conn,
            "meeting_transcripts",
            "SELECT COUNT(*) FROM meeting_transcripts",
        )?,
        pending_dm_replies: query_count_if_table_exists(
            conn,
            "pending_dm_replies",
            "SELECT COUNT(*) FROM pending_dm_replies",
        )?,
        pipeline_stages: query_count_if_table_exists(
            conn,
            "pipeline_stages",
            "SELECT COUNT(*) FROM pipeline_stages",
        )?,
        pr_tracking: query_count_if_table_exists(
            conn,
            "pr_tracking",
            "SELECT COUNT(*) FROM pr_tracking",
        )?,
        skills: query_count_if_table_exists(conn, "skills", "SELECT COUNT(*) FROM skills")?,
        skill_usage: query_count_if_table_exists(
            conn,
            "skill_usage",
            "SELECT COUNT(*) FROM skill_usage",
        )?,
        runtime_decisions: query_count_if_table_exists(
            conn,
            "runtime_decisions",
            "SELECT COUNT(*) FROM runtime_decisions",
        )?,
        session_termination_events: query_count_if_table_exists(
            conn,
            "session_termination_events",
            "SELECT COUNT(*) FROM session_termination_events",
        )?,
        sessions: query_count_if_table_exists(conn, "sessions", "SELECT COUNT(*) FROM sessions")?,
        audit_logs: query_count_if_table_exists(
            conn,
            "audit_logs",
            "SELECT COUNT(*) FROM audit_logs",
        )?,
        session_transcripts: query_count_if_table_exists(
            conn,
            "session_transcripts",
            "SELECT COUNT(*) FROM session_transcripts",
        )?,
        turns: query_count_if_table_exists(conn, "turns", "SELECT COUNT(*) FROM turns")?,
        departments: query_count_if_table_exists(
            conn,
            "departments",
            "SELECT COUNT(*) FROM departments",
        )?,
        offices: query_count_if_table_exists(conn, "offices", "SELECT COUNT(*) FROM offices")?,
        office_agents: query_count_if_table_exists(
            conn,
            "office_agents",
            "SELECT COUNT(*) FROM office_agents",
        )?,
        kv_meta: query_count_if_table_exists(conn, "kv_meta", "SELECT COUNT(*) FROM kv_meta")?,
        api_friction_events: query_count_if_table_exists(
            conn,
            "api_friction_events",
            "SELECT COUNT(*) FROM api_friction_events",
        )?,
        api_friction_issues: query_count_if_table_exists(
            conn,
            "api_friction_issues",
            "SELECT COUNT(*) FROM api_friction_issues",
        )?,
        memento_feedback_turn_stats: query_count_if_table_exists(
            conn,
            "memento_feedback_turn_stats",
            "SELECT COUNT(*) FROM memento_feedback_turn_stats",
        )?,
        rate_limit_cache: query_count_if_table_exists(
            conn,
            "rate_limit_cache",
            "SELECT COUNT(*) FROM rate_limit_cache",
        )?,
        deferred_hooks: query_count_if_table_exists(
            conn,
            "deferred_hooks",
            "SELECT COUNT(*) FROM deferred_hooks",
        )?,
        active_dispatches: query_count_if_table_exists(
            conn,
            "task_dispatches",
            "SELECT COUNT(*) FROM task_dispatches WHERE status IN ('pending', 'dispatched')",
        )?,
        working_sessions: query_count_if_table_exists(
            conn,
            "sessions",
            "SELECT COUNT(*) FROM sessions WHERE status = 'working'",
        )?,
        open_dispatch_outbox: query_count_if_table_exists(
            conn,
            "dispatch_outbox",
            "SELECT COUNT(*) FROM dispatch_outbox WHERE status NOT IN ('done', 'failed')",
        )?,
        pending_message_outbox: query_count_if_table_exists(
            conn,
            "message_outbox",
            "SELECT COUNT(*) FROM message_outbox WHERE status = 'pending'",
        )?,
    })
}

fn query_count(conn: &Connection, sql: &str) -> Result<i64, String> {
    conn.query_row(sql, [], |row| row.get(0))
        .map_err(|e| format!("sqlite count query failed: {e}"))
}

fn query_count_if_table_exists(conn: &Connection, table: &str, sql: &str) -> Result<i64, String> {
    if sqlite_table_exists(conn, table)? {
        query_count(conn, sql)
    } else {
        Ok(0)
    }
}

fn sqlite_table_exists(conn: &Connection, table: &str) -> Result<bool, String> {
    conn.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM sqlite_master
             WHERE type = 'table'
               AND name = ?1
         )",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .map(|value| value != 0)
    .map_err(|e| format!("check sqlite table {table}: {e}"))
}

fn sqlite_column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    conn.prepare(&format!("SELECT {column} FROM {table} LIMIT 0"))
        .is_ok()
}

fn normalize_required_json(value: Option<String>, default: &str) -> String {
    normalize_optional_json(value).unwrap_or_else(|| default.to_string())
}

fn sqlite_optional_bool(value: Option<i64>) -> Option<bool> {
    value.map(|raw| raw != 0)
}

const INSERT_BATCH_SIZE: usize = 500;

fn load_audit_logs(conn: &Connection) -> Result<Vec<AuditLogRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT entity_type, entity_id, action, timestamp, actor
             FROM audit_logs
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare audit_logs export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(AuditLogRow {
                entity_type: row.get(0)?,
                entity_id: row.get(1)?,
                action: row.get(2)?,
                timestamp: row.get(3)?,
                actor: row.get(4)?,
            })
        })
        .map_err(|e| format!("query audit_logs export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect audit_logs export: {e}"))
}

fn load_session_transcripts(conn: &Connection) -> Result<Vec<SessionTranscriptRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT turn_id,
                    session_key,
                    channel_id,
                    agent_id,
                    provider,
                    dispatch_id,
                    user_message,
                    assistant_message,
                    COALESCE(events_json, '[]') AS events_json,
                    duration_ms,
                    created_at
             FROM session_transcripts
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare session_transcripts export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(SessionTranscriptRow {
                turn_id: row.get(0)?,
                session_key: row.get(1)?,
                channel_id: row.get(2)?,
                agent_id: row.get(3)?,
                provider: row.get(4)?,
                dispatch_id: row.get(5)?,
                user_message: sanitize_pg_text(
                    &row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                ),
                assistant_message: sanitize_pg_text(
                    &row.get::<_, Option<String>>(7)?.unwrap_or_default(),
                ),
                events_json: normalize_required_json(row.get::<_, Option<String>>(8)?, "[]"),
                duration_ms: row.get(9)?,
                created_at: row.get(10)?,
            })
        })
        .map_err(|e| format!("query session_transcripts export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect session_transcripts export: {e}"))
}

fn load_all_offices(conn: &Connection) -> Result<Vec<OfficeRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, layout, name_ko, icon, color, description, sort_order, created_at
             FROM offices
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare offices export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(OfficeRow {
                id: row.get(0)?,
                name: row.get(1)?,
                layout: row.get(2)?,
                name_ko: row.get(3)?,
                icon: row.get(4)?,
                color: row.get(5)?,
                description: row.get(6)?,
                sort_order: row.get(7)?,
                created_at: row.get(8)?,
            })
        })
        .map_err(|e| format!("query offices export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect offices export: {e}"))
}

fn load_all_departments(conn: &Connection) -> Result<Vec<DepartmentRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, office_id, name_ko, icon, color, description, sort_order, created_at
             FROM departments
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare departments export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(DepartmentRow {
                id: row.get(0)?,
                name: row.get(1)?,
                office_id: row.get(2)?,
                name_ko: row.get(3)?,
                icon: row.get(4)?,
                color: row.get(5)?,
                description: row.get(6)?,
                sort_order: row.get(7)?,
                created_at: row.get(8)?,
            })
        })
        .map_err(|e| format!("query departments export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect departments export: {e}"))
}

fn load_all_office_agents(conn: &Connection) -> Result<Vec<OfficeAgentRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT office_id, agent_id, department_id, joined_at
             FROM office_agents
             ORDER BY office_id ASC, agent_id ASC",
        )
        .map_err(|e| format!("prepare office_agents export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(OfficeAgentRow {
                office_id: row.get(0)?,
                agent_id: row.get(1)?,
                department_id: row.get(2)?,
                joined_at: row.get(3)?,
            })
        })
        .map_err(|e| format!("query office_agents export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect office_agents export: {e}"))
}

fn load_all_github_repos(conn: &Connection) -> Result<Vec<GithubRepoRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, display_name, sync_enabled, last_synced_at, default_agent_id, pipeline_config
             FROM github_repos
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare github_repos export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(GithubRepoRow {
                id: row.get(0)?,
                display_name: row.get(1)?,
                sync_enabled: sqlite_optional_bool(row.get(2)?),
                last_synced_at: row.get(3)?,
                default_agent_id: row.get(4)?,
                pipeline_config: normalize_optional_json(row.get(5)?),
            })
        })
        .map_err(|e| format!("query github_repos export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect github_repos export: {e}"))
}

fn load_all_agents(conn: &Connection) -> Result<Vec<AgentRow>, String> {
    let sprite_number_sql = if sqlite_column_exists(conn, "agents", "sprite_number") {
        "sprite_number".to_string()
    } else {
        "NULL AS sprite_number".to_string()
    };
    let description_sql = if sqlite_column_exists(conn, "agents", "description") {
        "description".to_string()
    } else {
        "NULL AS description".to_string()
    };
    let system_prompt_sql = if sqlite_column_exists(conn, "agents", "system_prompt") {
        "system_prompt".to_string()
    } else {
        "NULL AS system_prompt".to_string()
    };
    let pipeline_config_sql = if sqlite_column_exists(conn, "agents", "pipeline_config") {
        "pipeline_config".to_string()
    } else {
        "NULL AS pipeline_config".to_string()
    };
    let sql = format!(
        "SELECT id,
                name,
                name_ko,
                department,
                provider,
                discord_channel_id,
                discord_channel_alt,
                discord_channel_cc,
                discord_channel_cdx,
                avatar_emoji,
                status,
                xp,
                skills,
                {sprite_number_sql},
                {description_sql},
                {system_prompt_sql},
                {pipeline_config_sql},
                created_at,
                updated_at
         FROM agents
         ORDER BY id ASC"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("prepare agents export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(AgentRow {
                id: row.get(0)?,
                name: row.get(1)?,
                name_ko: row.get(2)?,
                department: row.get(3)?,
                provider: row.get(4)?,
                discord_channel_id: row.get(5)?,
                discord_channel_alt: row.get(6)?,
                discord_channel_cc: row.get(7)?,
                discord_channel_cdx: row.get(8)?,
                avatar_emoji: row.get(9)?,
                status: row.get(10)?,
                xp: row.get(11)?,
                skills: row.get(12)?,
                sprite_number: row.get(13)?,
                description: row.get(14)?,
                system_prompt: row.get(15)?,
                pipeline_config: normalize_optional_json(row.get(16)?),
                created_at: row.get(17)?,
                updated_at: row.get(18)?,
            })
        })
        .map_err(|e| format!("query agents export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect agents export: {e}"))
}

fn load_all_kanban_cards(conn: &Connection) -> Result<Vec<KanbanCardRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    repo_id,
                    title,
                    status,
                    priority,
                    assigned_agent_id,
                    github_issue_url,
                    github_issue_number,
                    latest_dispatch_id,
                    review_round,
                    metadata,
                    started_at,
                    completed_at,
                    blocked_reason,
                    pipeline_stage_id,
                    review_notes,
                    review_status,
                    requested_at,
                    owner_agent_id,
                    requester_agent_id,
                    parent_card_id,
                    depth,
                    sort_order,
                    description,
                    active_thread_id,
                    channel_thread_map,
                    suggestion_pending_at,
                    review_entered_at,
                    awaiting_dod_at,
                    deferred_dod_json,
                    created_at,
                    updated_at
             FROM kanban_cards
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare kanban_cards export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(KanbanCardRow {
                id: row.get(0)?,
                repo_id: row.get(1)?,
                title: row.get(2)?,
                status: row.get(3)?,
                priority: row.get(4)?,
                assigned_agent_id: row.get(5)?,
                github_issue_url: row.get(6)?,
                github_issue_number: row.get(7)?,
                latest_dispatch_id: row.get(8)?,
                review_round: row.get(9)?,
                metadata: normalize_optional_json(row.get(10)?),
                started_at: row.get(11)?,
                completed_at: row.get(12)?,
                blocked_reason: row.get(13)?,
                pipeline_stage_id: row.get(14)?,
                review_notes: row.get(15)?,
                review_status: row.get(16)?,
                requested_at: row.get(17)?,
                owner_agent_id: row.get(18)?,
                requester_agent_id: row.get(19)?,
                parent_card_id: row.get(20)?,
                depth: row.get(21)?,
                sort_order: row.get(22)?,
                description: row.get(23)?,
                active_thread_id: row.get(24)?,
                channel_thread_map: normalize_optional_json(row.get(25)?),
                suggestion_pending_at: row.get(26)?,
                review_entered_at: row.get(27)?,
                awaiting_dod_at: row.get(28)?,
                deferred_dod_json: normalize_optional_json(row.get(29)?),
                created_at: row.get(30)?,
                updated_at: row.get(31)?,
            })
        })
        .map_err(|e| format!("query kanban_cards export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect kanban_cards export: {e}"))
}

fn load_all_kanban_audit_logs(conn: &Connection) -> Result<Vec<KanbanAuditLogRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, card_id, from_status, to_status, source, result, created_at
             FROM kanban_audit_logs
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare kanban_audit_logs export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(KanbanAuditLogRow {
                id: row.get(0)?,
                card_id: row.get(1)?,
                from_status: row.get(2)?,
                to_status: row.get(3)?,
                source: row.get(4)?,
                result: row.get(5)?,
                created_at: row.get(6)?,
            })
        })
        .map_err(|e| format!("query kanban_audit_logs export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect kanban_audit_logs export: {e}"))
}

fn load_all_card_retrospectives(conn: &Connection) -> Result<Vec<CardRetrospectiveRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    card_id,
                    dispatch_id,
                    terminal_status,
                    repo_id,
                    issue_number,
                    title,
                    topic,
                    content,
                    review_round,
                    review_notes,
                    duration_seconds,
                    success,
                    result_json,
                    memory_payload,
                    sync_backend,
                    sync_status,
                    sync_error,
                    created_at,
                    updated_at
             FROM card_retrospectives
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare card_retrospectives export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(CardRetrospectiveRow {
                id: row.get(0)?,
                card_id: row.get(1)?,
                dispatch_id: row.get(2)?,
                terminal_status: row.get(3)?,
                repo_id: row.get(4)?,
                issue_number: row.get(5)?,
                title: row.get(6)?,
                topic: row.get(7)?,
                content: row.get(8)?,
                review_round: row.get(9)?,
                review_notes: row.get(10)?,
                duration_seconds: row.get(11)?,
                success: sqlite_optional_bool(row.get(12)?),
                result_json: normalize_required_json(row.get(13)?, "{}"),
                memory_payload: normalize_required_json(row.get(14)?, "{}"),
                sync_backend: row.get(15)?,
                sync_status: row.get(16)?,
                sync_error: row.get(17)?,
                created_at: row.get(18)?,
                updated_at: row.get(19)?,
            })
        })
        .map_err(|e| format!("query card_retrospectives export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect card_retrospectives export: {e}"))
}

fn load_all_card_review_state(conn: &Connection) -> Result<Vec<CardReviewStateRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT card_id,
                    review_round,
                    state,
                    pending_dispatch_id,
                    last_verdict,
                    last_decision,
                    decided_by,
                    decided_at,
                    approach_change_round,
                    session_reset_round,
                    review_entered_at,
                    updated_at
             FROM card_review_state
             ORDER BY card_id ASC",
        )
        .map_err(|e| format!("prepare card_review_state export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(CardReviewStateRow {
                card_id: row.get(0)?,
                review_round: row.get(1)?,
                state: row.get(2)?,
                pending_dispatch_id: row.get(3)?,
                last_verdict: row.get(4)?,
                last_decision: row.get(5)?,
                decided_by: row.get(6)?,
                decided_at: row.get(7)?,
                approach_change_round: row.get(8)?,
                session_reset_round: row.get(9)?,
                review_entered_at: row.get(10)?,
                updated_at: row.get(11)?,
            })
        })
        .map_err(|e| format!("query card_review_state export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect card_review_state export: {e}"))
}

fn load_all_auto_queue_runs(conn: &Connection) -> Result<Vec<AutoQueueRunRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    repo,
                    agent_id,
                    status,
                    ai_model,
                    ai_rationale,
                    timeout_minutes,
                    unified_thread,
                    unified_thread_id,
                    unified_thread_channel_id,
                    max_concurrent_threads,
                    thread_group_count,
                    created_at,
                    completed_at
             FROM auto_queue_runs
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare auto_queue_runs export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(AutoQueueRunRow {
                id: row.get(0)?,
                repo: row.get(1)?,
                agent_id: row.get(2)?,
                status: row.get(3)?,
                ai_model: row.get(4)?,
                ai_rationale: row.get(5)?,
                timeout_minutes: row.get(6)?,
                unified_thread: sqlite_optional_bool(row.get(7)?),
                unified_thread_id: row.get(8)?,
                unified_thread_channel_id: row.get(9)?,
                max_concurrent_threads: row.get(10)?,
                thread_group_count: row.get(11)?,
                created_at: row.get(12)?,
                completed_at: row.get(13)?,
            })
        })
        .map_err(|e| format!("query auto_queue_runs export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect auto_queue_runs export: {e}"))
}

fn load_all_auto_queue_entries(conn: &Connection) -> Result<Vec<AutoQueueEntryRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    run_id,
                    kanban_card_id,
                    agent_id,
                    priority_rank,
                    reason,
                    status,
                    retry_count,
                    dispatch_id,
                    slot_index,
                    thread_group,
                    batch_phase,
                    created_at,
                    dispatched_at,
                    completed_at
             FROM auto_queue_entries
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare auto_queue_entries export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(AutoQueueEntryRow {
                id: row.get(0)?,
                run_id: row.get(1)?,
                kanban_card_id: row.get(2)?,
                agent_id: row.get(3)?,
                priority_rank: row.get(4)?,
                reason: row.get(5)?,
                status: row.get(6)?,
                retry_count: row.get(7)?,
                dispatch_id: row.get(8)?,
                slot_index: row.get(9)?,
                thread_group: row.get(10)?,
                batch_phase: row.get(11)?,
                created_at: row.get(12)?,
                dispatched_at: row.get(13)?,
                completed_at: row.get(14)?,
            })
        })
        .map_err(|e| format!("query auto_queue_entries export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect auto_queue_entries export: {e}"))
}

fn load_all_auto_queue_entry_transitions(
    conn: &Connection,
) -> Result<Vec<AutoQueueEntryTransitionRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, entry_id, from_status, to_status, trigger_source, created_at
             FROM auto_queue_entry_transitions
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare auto_queue_entry_transitions export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(AutoQueueEntryTransitionRow {
                id: row.get(0)?,
                entry_id: row.get(1)?,
                from_status: row.get(2)?,
                to_status: row.get(3)?,
                trigger_source: row.get(4)?,
                created_at: row.get(5)?,
            })
        })
        .map_err(|e| format!("query auto_queue_entry_transitions export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect auto_queue_entry_transitions export: {e}"))
}

fn load_all_auto_queue_entry_dispatch_history(
    conn: &Connection,
) -> Result<Vec<AutoQueueEntryDispatchHistoryRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, entry_id, dispatch_id, trigger_source, created_at
             FROM auto_queue_entry_dispatch_history
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare auto_queue_entry_dispatch_history export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(AutoQueueEntryDispatchHistoryRow {
                id: row.get(0)?,
                entry_id: row.get(1)?,
                dispatch_id: row.get(2)?,
                trigger_source: row.get(3)?,
                created_at: row.get(4)?,
            })
        })
        .map_err(|e| format!("query auto_queue_entry_dispatch_history export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect auto_queue_entry_dispatch_history export: {e}"))
}

fn load_all_auto_queue_phase_gates(
    conn: &Connection,
) -> Result<Vec<AutoQueuePhaseGateRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    run_id,
                    phase,
                    status,
                    verdict,
                    dispatch_id,
                    pass_verdict,
                    next_phase,
                    final_phase,
                    anchor_card_id,
                    failure_reason,
                    created_at,
                    updated_at
             FROM auto_queue_phase_gates
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare auto_queue_phase_gates export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(AutoQueuePhaseGateRow {
                id: row.get(0)?,
                run_id: row.get(1)?,
                phase: row.get(2)?,
                status: row.get(3)?,
                verdict: row.get(4)?,
                dispatch_id: row.get(5)?,
                pass_verdict: row.get(6)?,
                next_phase: row.get(7)?,
                final_phase: sqlite_optional_bool(row.get(8)?),
                anchor_card_id: row.get(9)?,
                failure_reason: row.get(10)?,
                created_at: row.get(11)?,
                updated_at: row.get(12)?,
            })
        })
        .map_err(|e| format!("query auto_queue_phase_gates export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect auto_queue_phase_gates export: {e}"))
}

fn load_all_auto_queue_slots(conn: &Connection) -> Result<Vec<AutoQueueSlotRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map, created_at, updated_at
             FROM auto_queue_slots
             ORDER BY agent_id ASC, slot_index ASC",
        )
        .map_err(|e| format!("prepare auto_queue_slots export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(AutoQueueSlotRow {
                agent_id: row.get(0)?,
                slot_index: row.get(1)?,
                assigned_run_id: row.get(2)?,
                assigned_thread_group: row.get(3)?,
                thread_id_map: normalize_optional_json(row.get(4)?),
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })
        .map_err(|e| format!("query auto_queue_slots export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect auto_queue_slots export: {e}"))
}

fn load_all_task_dispatches(conn: &Connection) -> Result<Vec<TaskDispatchRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    kanban_card_id,
                    from_agent_id,
                    to_agent_id,
                    dispatch_type,
                    status,
                    title,
                    context,
                    result,
                    parent_dispatch_id,
                    chain_depth,
                    thread_id,
                    retry_count,
                    created_at,
                    updated_at,
                    completed_at
             FROM task_dispatches
             ORDER BY created_at ASC, id ASC",
        )
        .map_err(|e| format!("prepare task_dispatches export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(TaskDispatchRow {
                id: row.get(0)?,
                kanban_card_id: row.get(1)?,
                from_agent_id: row.get(2)?,
                to_agent_id: row.get(3)?,
                dispatch_type: row.get(4)?,
                status: row.get(5)?,
                title: row.get(6)?,
                context: row.get(7)?,
                result: row.get(8)?,
                parent_dispatch_id: row.get(9)?,
                chain_depth: row.get(10)?,
                thread_id: row.get(11)?,
                retry_count: row.get(12)?,
                created_at: row.get(13)?,
                updated_at: row.get(14)?,
                completed_at: row.get(15)?,
            })
        })
        .map_err(|e| format!("query task_dispatches export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect task_dispatches export: {e}"))
}

fn load_all_dispatch_events(conn: &Connection) -> Result<Vec<DispatchEventRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    dispatch_id,
                    kanban_card_id,
                    dispatch_type,
                    from_status,
                    to_status,
                    transition_source,
                    payload_json,
                    created_at
             FROM dispatch_events
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare dispatch_events export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(DispatchEventRow {
                id: row.get(0)?,
                dispatch_id: row.get(1)?,
                kanban_card_id: row.get(2)?,
                dispatch_type: row.get(3)?,
                from_status: row.get(4)?,
                to_status: row.get(5)?,
                transition_source: row.get(6)?,
                payload_json: normalize_optional_json(row.get(7)?),
                created_at: row.get(8)?,
            })
        })
        .map_err(|e| format!("query dispatch_events export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect dispatch_events export: {e}"))
}

fn load_all_dispatch_queue(conn: &Connection) -> Result<Vec<DispatchQueueRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, kanban_card_id, priority_score, queued_at
             FROM dispatch_queue
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare dispatch_queue export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(DispatchQueueRow {
                id: row.get(0)?,
                kanban_card_id: row.get(1)?,
                priority_score: row.get(2)?,
                queued_at: row.get(3)?,
            })
        })
        .map_err(|e| format!("query dispatch_queue export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect dispatch_queue export: {e}"))
}

fn load_all_review_decisions(conn: &Connection) -> Result<Vec<ReviewDecisionRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, kanban_card_id, dispatch_id, item_index, decision, decided_at
             FROM review_decisions
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare review_decisions export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(ReviewDecisionRow {
                id: row.get(0)?,
                kanban_card_id: row.get(1)?,
                dispatch_id: row.get(2)?,
                item_index: row.get(3)?,
                decision: row.get(4)?,
                decided_at: row.get(5)?,
            })
        })
        .map_err(|e| format!("query review_decisions export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect review_decisions export: {e}"))
}

fn load_all_review_tuning_outcomes(
    conn: &Connection,
) -> Result<Vec<ReviewTuningOutcomeRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, card_id, dispatch_id, review_round, verdict, decision, outcome, finding_categories, created_at
             FROM review_tuning_outcomes
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare review_tuning_outcomes export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(ReviewTuningOutcomeRow {
                id: row.get(0)?,
                card_id: row.get(1)?,
                dispatch_id: row.get(2)?,
                review_round: row.get(3)?,
                verdict: row.get(4)?,
                decision: row.get(5)?,
                outcome: row.get(6)?,
                finding_categories: row.get(7)?,
                created_at: row.get(8)?,
            })
        })
        .map_err(|e| format!("query review_tuning_outcomes export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect review_tuning_outcomes export: {e}"))
}

fn load_all_sessions(conn: &Connection) -> Result<Vec<SessionRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT session_key,
                    agent_id,
                    provider,
                    status,
                    active_dispatch_id,
                    model,
                    session_info,
                    tokens,
                    cwd,
                    last_heartbeat,
                    thread_channel_id,
                    claude_session_id,
                    raw_provider_session_id,
                    created_at
             FROM sessions
             ORDER BY created_at ASC, session_key ASC",
        )
        .map_err(|e| format!("prepare sessions export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(SessionRow {
                session_key: row.get(0)?,
                agent_id: row.get(1)?,
                provider: row.get(2)?,
                status: row.get(3)?,
                active_dispatch_id: row.get(4)?,
                model: row.get(5)?,
                session_info: row.get(6)?,
                tokens: row.get(7)?,
                cwd: row.get(8)?,
                last_heartbeat: row.get(9)?,
                thread_channel_id: row.get(10)?,
                claude_session_id: row.get(11)?,
                raw_provider_session_id: row.get(12)?,
                created_at: row.get(13)?,
            })
        })
        .map_err(|e| format!("query sessions export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect sessions export: {e}"))
}

fn load_all_dispatch_outbox(conn: &Connection) -> Result<Vec<DispatchOutboxRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    dispatch_id,
                    action,
                    agent_id,
                    card_id,
                    title,
                    status,
                    retry_count,
                    next_attempt_at,
                    created_at,
                    processed_at,
                    error
             FROM dispatch_outbox
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare dispatch_outbox export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(DispatchOutboxRow {
                id: row.get(0)?,
                dispatch_id: row.get(1)?,
                action: row.get(2)?,
                agent_id: row.get(3)?,
                card_id: row.get(4)?,
                title: row.get(5)?,
                status: row
                    .get::<_, Option<String>>(6)?
                    .unwrap_or_else(|| "pending".to_string()),
                retry_count: row.get(7)?,
                next_attempt_at: row.get(8)?,
                created_at: row.get(9)?,
                processed_at: row.get(10)?,
                error: row.get(11)?,
            })
        })
        .map_err(|e| format!("query dispatch_outbox export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect dispatch_outbox export: {e}"))
}

fn load_all_session_termination_events(
    conn: &Connection,
) -> Result<Vec<SessionTerminationEventRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    session_key,
                    dispatch_id,
                    killer_component,
                    reason_code,
                    reason_text,
                    probe_snapshot,
                    last_offset,
                    tmux_alive,
                    created_at
             FROM session_termination_events
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare session_termination_events export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(SessionTerminationEventRow {
                id: row.get(0)?,
                session_key: row.get(1)?,
                dispatch_id: row.get(2)?,
                killer_component: row.get(3)?,
                reason_code: row.get(4)?,
                reason_text: row.get(5)?,
                probe_snapshot: row.get(6)?,
                last_offset: row.get(7)?,
                tmux_alive: row.get(8)?,
                created_at: row.get(9)?,
            })
        })
        .map_err(|e| format!("query session_termination_events export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect session_termination_events export: {e}"))
}

fn load_all_turns(conn: &Connection) -> Result<Vec<TurnRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT turn_id,
                    session_key,
                    thread_id,
                    thread_title,
                    channel_id,
                    agent_id,
                    provider,
                    session_id,
                    dispatch_id,
                    started_at,
                    finished_at,
                    duration_ms,
                    input_tokens,
                    cache_create_tokens,
                    cache_read_tokens,
                    output_tokens,
                    created_at
             FROM turns
             ORDER BY started_at ASC, turn_id ASC",
        )
        .map_err(|e| format!("prepare turns export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(TurnRow {
                turn_id: row.get(0)?,
                session_key: row.get(1)?,
                thread_id: row.get(2)?,
                thread_title: row.get(3)?,
                channel_id: row.get(4)?,
                agent_id: row.get(5)?,
                provider: row.get(6)?,
                session_id: row.get(7)?,
                dispatch_id: row.get(8)?,
                started_at: row.get(9)?,
                finished_at: row.get(10)?,
                duration_ms: row.get(11)?,
                input_tokens: row.get(12)?,
                cache_create_tokens: row.get(13)?,
                cache_read_tokens: row.get(14)?,
                output_tokens: row.get(15)?,
                created_at: row.get(16)?,
            })
        })
        .map_err(|e| format!("query turns export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect turns export: {e}"))
}

fn load_all_meetings(conn: &Connection) -> Result<Vec<MeetingRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    channel_id,
                    title,
                    status,
                    effective_rounds,
                    started_at,
                    completed_at,
                    summary,
                    thread_id,
                    primary_provider,
                    reviewer_provider,
                    participant_names,
                    selection_reason,
                    created_at
             FROM meetings
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare meetings export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(MeetingRow {
                id: row.get(0)?,
                channel_id: row.get(1)?,
                title: row.get(2)?,
                status: row.get(3)?,
                effective_rounds: row.get(4)?,
                started_at: sqlite_meeting_timestamp_to_pg_text(row, 5),
                completed_at: sqlite_meeting_timestamp_to_pg_text(row, 6),
                summary: row.get(7)?,
                thread_id: row.get(8)?,
                primary_provider: row.get(9)?,
                reviewer_provider: row.get(10)?,
                participant_names: row.get(11)?,
                selection_reason: row.get(12)?,
                created_at: row.get(13)?,
            })
        })
        .map_err(|e| format!("query meetings export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect meetings export: {e}"))
}

fn sqlite_meeting_timestamp_to_pg_text(
    row: &libsql_rusqlite::Row<'_>,
    idx: usize,
) -> Option<String> {
    use libsql_rusqlite::types::ValueRef;

    match row.get_ref(idx).ok()? {
        ValueRef::Null => None,
        ValueRef::Integer(value) => unix_millis_to_rfc3339(value),
        ValueRef::Real(value) => unix_millis_to_rfc3339(value as i64),
        ValueRef::Text(bytes) => {
            let text = std::str::from_utf8(bytes).ok()?.trim();
            if text.is_empty() {
                None
            } else if let Ok(value) = text.parse::<i64>() {
                unix_millis_to_rfc3339(value)
            } else if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(text) {
                Some(dt.with_timezone(&Utc).to_rfc3339())
            } else if let Ok(dt) = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f") {
                Some(dt.and_utc().to_rfc3339())
            } else {
                Some(text.to_string())
            }
        }
        ValueRef::Blob(_) => None,
    }
}

fn unix_millis_to_rfc3339(value: i64) -> Option<String> {
    Utc.timestamp_millis_opt(value)
        .single()
        .map(|dt| dt.to_rfc3339())
}

fn load_all_meeting_transcripts(conn: &Connection) -> Result<Vec<MeetingTranscriptRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, meeting_id, seq, round, speaker_agent_id, speaker_name, content, is_summary
             FROM meeting_transcripts
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare meeting_transcripts export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(MeetingTranscriptRow {
                id: row.get(0)?,
                meeting_id: row.get(1)?,
                seq: row.get(2)?,
                round: row.get(3)?,
                speaker_agent_id: row.get(4)?,
                speaker_name: row.get(5)?,
                content: row.get(6)?,
                is_summary: sqlite_optional_bool(row.get(7)?),
            })
        })
        .map_err(|e| format!("query meeting_transcripts export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect meeting_transcripts export: {e}"))
}

fn load_all_messages(conn: &Connection) -> Result<Vec<MessageRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, sender_type, sender_id, receiver_type, receiver_id, content, message_type, created_at
             FROM messages
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare messages export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(MessageRow {
                id: row.get(0)?,
                sender_type: row.get(1)?,
                sender_id: row.get(2)?,
                receiver_type: row.get(3)?,
                receiver_id: row.get(4)?,
                content: row.get(5)?,
                message_type: row.get(6)?,
                created_at: row.get(7)?,
            })
        })
        .map_err(|e| format!("query messages export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect messages export: {e}"))
}

fn load_all_message_outbox(conn: &Connection) -> Result<Vec<MessageOutboxRow>, String> {
    let reason_code_sql = if sqlite_column_exists(conn, "message_outbox", "reason_code") {
        "reason_code".to_string()
    } else {
        "NULL AS reason_code".to_string()
    };
    let session_key_sql = if sqlite_column_exists(conn, "message_outbox", "session_key") {
        "session_key".to_string()
    } else {
        "NULL AS session_key".to_string()
    };
    let claimed_at_sql = if sqlite_column_exists(conn, "message_outbox", "claimed_at") {
        "claimed_at".to_string()
    } else {
        "NULL AS claimed_at".to_string()
    };
    let claim_owner_sql = if sqlite_column_exists(conn, "message_outbox", "claim_owner") {
        "claim_owner".to_string()
    } else {
        "NULL AS claim_owner".to_string()
    };
    let sql = format!(
        "SELECT id,
                target,
                content,
                bot,
                source,
                {reason_code_sql},
                {session_key_sql},
                status,
                created_at,
                sent_at,
                error,
                {claimed_at_sql},
                {claim_owner_sql}
         FROM message_outbox
         ORDER BY id ASC"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("prepare message_outbox export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(MessageOutboxRow {
                id: row.get(0)?,
                target: row.get(1)?,
                content: row.get(2)?,
                bot: row.get(3)?,
                source: row.get(4)?,
                reason_code: row.get(5)?,
                session_key: row.get(6)?,
                status: row.get(7)?,
                created_at: row.get(8)?,
                sent_at: row.get(9)?,
                error: row.get(10)?,
                claimed_at: row.get(11)?,
                claim_owner: row.get(12)?,
            })
        })
        .map_err(|e| format!("query message_outbox export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect message_outbox export: {e}"))
}

fn load_all_pending_dm_replies(conn: &Connection) -> Result<Vec<PendingDmReplyRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, source_agent, user_id, channel_id, context, status, created_at, consumed_at, expires_at
             FROM pending_dm_replies
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare pending_dm_replies export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(PendingDmReplyRow {
                id: row.get(0)?,
                source_agent: row.get(1)?,
                user_id: row.get(2)?,
                channel_id: row.get(3)?,
                context: normalize_required_json(row.get(4)?, "{}"),
                status: row.get(5)?,
                created_at: row.get(6)?,
                consumed_at: row.get(7)?,
                expires_at: row.get(8)?,
            })
        })
        .map_err(|e| format!("query pending_dm_replies export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect pending_dm_replies export: {e}"))
}

fn load_all_pipeline_stages(conn: &Connection) -> Result<Vec<PipelineStageRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    repo_id,
                    stage_name,
                    stage_order,
                    trigger_after,
                    entry_skill,
                    timeout_minutes,
                    on_failure,
                    skip_condition,
                    provider,
                    agent_override_id,
                    on_failure_target,
                    max_retries,
                    parallel_with
             FROM pipeline_stages
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare pipeline_stages export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(PipelineStageRow {
                id: row.get(0)?,
                repo_id: row.get(1)?,
                stage_name: row.get(2)?,
                stage_order: row.get(3)?,
                trigger_after: row.get(4)?,
                entry_skill: row.get(5)?,
                timeout_minutes: row.get(6)?,
                on_failure: row.get(7)?,
                skip_condition: row.get(8)?,
                provider: row.get(9)?,
                agent_override_id: row.get(10)?,
                on_failure_target: row.get(11)?,
                max_retries: row.get(12)?,
                parallel_with: row.get(13)?,
            })
        })
        .map_err(|e| format!("query pipeline_stages export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect pipeline_stages export: {e}"))
}

fn load_all_pr_tracking(conn: &Connection) -> Result<Vec<PrTrackingRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT card_id,
                    repo_id,
                    worktree_path,
                    branch,
                    pr_number,
                    head_sha,
                    state,
                    last_error,
                    dispatch_generation,
                    review_round,
                    retry_count,
                    created_at,
                    updated_at
             FROM pr_tracking
             ORDER BY card_id ASC",
        )
        .map_err(|e| format!("prepare pr_tracking export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(PrTrackingRow {
                card_id: row.get(0)?,
                repo_id: row.get(1)?,
                worktree_path: row.get(2)?,
                branch: row.get(3)?,
                pr_number: row.get(4)?,
                head_sha: row.get(5)?,
                state: row.get(6)?,
                last_error: row.get(7)?,
                dispatch_generation: row.get(8)?,
                review_round: row.get(9)?,
                retry_count: row.get(10)?,
                created_at: row.get(11)?,
                updated_at: row.get(12)?,
            })
        })
        .map_err(|e| format!("query pr_tracking export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect pr_tracking export: {e}"))
}

fn load_all_skills(conn: &Connection) -> Result<Vec<SkillRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, description, source_path, trigger_patterns, updated_at
             FROM skills
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare skills export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(SkillRow {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                source_path: row.get(3)?,
                trigger_patterns: row.get(4)?,
                updated_at: row.get(5)?,
            })
        })
        .map_err(|e| format!("query skills export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect skills export: {e}"))
}

fn load_all_skill_usage(conn: &Connection) -> Result<Vec<SkillUsageRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, skill_id, agent_id, session_key, used_at
             FROM skill_usage
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare skill_usage export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(SkillUsageRow {
                id: row.get(0)?,
                skill_id: row.get(1)?,
                agent_id: row.get(2)?,
                session_key: row.get(3)?,
                used_at: row.get(4)?,
            })
        })
        .map_err(|e| format!("query skill_usage export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect skill_usage export: {e}"))
}

fn load_all_runtime_decisions(conn: &Connection) -> Result<Vec<RuntimeDecisionRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, signal, evidence_json, chosen_action, actor, session_key, dispatch_id, created_at
             FROM runtime_decisions
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare runtime_decisions export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(RuntimeDecisionRow {
                id: row.get(0)?,
                signal: row.get(1)?,
                evidence_json: normalize_required_json(row.get(2)?, "{}"),
                chosen_action: row.get(3)?,
                actor: row.get(4)?,
                session_key: row.get(5)?,
                dispatch_id: row.get(6)?,
                created_at: row.get(7)?,
            })
        })
        .map_err(|e| format!("query runtime_decisions export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect runtime_decisions export: {e}"))
}

fn load_all_kv_meta(conn: &Connection) -> Result<Vec<KvMetaRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT key, value, expires_at
             FROM kv_meta
             ORDER BY key ASC",
        )
        .map_err(|e| format!("prepare kv_meta export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(KvMetaRow {
                key: row.get(0)?,
                value: row.get(1)?,
                expires_at: row.get(2)?,
            })
        })
        .map_err(|e| format!("query kv_meta export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect kv_meta export: {e}"))
}

fn load_all_api_friction_events(conn: &Connection) -> Result<Vec<ApiFrictionEventRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    fingerprint,
                    endpoint,
                    friction_type,
                    summary,
                    workaround,
                    suggested_fix,
                    docs_category,
                    keywords_json,
                    payload_json,
                    session_key,
                    channel_id,
                    provider,
                    dispatch_id,
                    card_id,
                    repo_id,
                    github_issue_number,
                    task_summary,
                    agent_id,
                    memory_backend,
                    memory_status,
                    memory_error,
                    created_at
             FROM api_friction_events
             ORDER BY created_at ASC, id ASC",
        )
        .map_err(|e| format!("prepare api_friction_events export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(ApiFrictionEventRow {
                id: row.get(0)?,
                fingerprint: row.get(1)?,
                endpoint: row.get(2)?,
                friction_type: row.get(3)?,
                summary: row.get(4)?,
                workaround: row.get(5)?,
                suggested_fix: row.get(6)?,
                docs_category: row.get(7)?,
                keywords_json: normalize_required_json(row.get(8)?, "[]"),
                payload_json: normalize_required_json(row.get(9)?, "{}"),
                session_key: row.get(10)?,
                channel_id: row.get(11)?,
                provider: row.get(12)?,
                dispatch_id: row.get(13)?,
                card_id: row.get(14)?,
                repo_id: row.get(15)?,
                github_issue_number: row.get(16)?,
                task_summary: row.get(17)?,
                agent_id: row.get(18)?,
                memory_backend: row.get(19)?,
                memory_status: row.get(20)?,
                memory_error: row.get(21)?,
                created_at: row.get(22)?,
            })
        })
        .map_err(|e| format!("query api_friction_events export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect api_friction_events export: {e}"))
}

fn load_all_api_friction_issues(conn: &Connection) -> Result<Vec<ApiFrictionIssueRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT fingerprint,
                    repo_id,
                    endpoint,
                    friction_type,
                    title,
                    body,
                    issue_number,
                    issue_url,
                    event_count,
                    first_event_at,
                    last_event_at,
                    last_error,
                    created_at,
                    updated_at
             FROM api_friction_issues
             ORDER BY fingerprint ASC",
        )
        .map_err(|e| format!("prepare api_friction_issues export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(ApiFrictionIssueRow {
                fingerprint: row.get(0)?,
                repo_id: row.get(1)?,
                endpoint: row.get(2)?,
                friction_type: row.get(3)?,
                title: row.get(4)?,
                body: row.get(5)?,
                issue_number: row.get(6)?,
                issue_url: row.get(7)?,
                event_count: row.get(8)?,
                first_event_at: row.get(9)?,
                last_event_at: row.get(10)?,
                last_error: row.get(11)?,
                created_at: row.get(12)?,
                updated_at: row.get(13)?,
            })
        })
        .map_err(|e| format!("query api_friction_issues export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect api_friction_issues export: {e}"))
}

fn load_all_memento_feedback_turn_stats(
    conn: &Connection,
) -> Result<Vec<MementoFeedbackTurnStatsRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT turn_id,
                    stat_date,
                    agent_id,
                    provider,
                    recall_count,
                    manual_tool_feedback_count,
                    manual_covered_recall_count,
                    auto_tool_feedback_count,
                    covered_recall_count,
                    created_at
             FROM memento_feedback_turn_stats
             ORDER BY turn_id ASC",
        )
        .map_err(|e| format!("prepare memento_feedback_turn_stats export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(MementoFeedbackTurnStatsRow {
                turn_id: row.get(0)?,
                stat_date: row.get(1)?,
                agent_id: row.get(2)?,
                provider: row.get(3)?,
                recall_count: row.get(4)?,
                manual_tool_feedback_count: row.get(5)?,
                manual_covered_recall_count: row.get(6)?,
                auto_tool_feedback_count: row.get(7)?,
                covered_recall_count: row.get(8)?,
                created_at: row.get(9)?,
            })
        })
        .map_err(|e| format!("query memento_feedback_turn_stats export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect memento_feedback_turn_stats export: {e}"))
}

fn load_all_rate_limit_cache(conn: &Connection) -> Result<Vec<RateLimitCacheRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT provider, data, fetched_at
             FROM rate_limit_cache
             ORDER BY provider ASC",
        )
        .map_err(|e| format!("prepare rate_limit_cache export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(RateLimitCacheRow {
                provider: row.get(0)?,
                data: row.get(1)?,
                fetched_at: row.get(2)?,
            })
        })
        .map_err(|e| format!("query rate_limit_cache export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect rate_limit_cache export: {e}"))
}

fn load_all_deferred_hooks(conn: &Connection) -> Result<Vec<DeferredHookRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, hook_name, payload, status, created_at
             FROM deferred_hooks
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare deferred_hooks export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(DeferredHookRow {
                id: row.get(0)?,
                hook_name: row.get(1)?,
                payload: normalize_required_json(row.get(2)?, "{}"),
                status: row.get(3)?,
                created_at: row.get(4)?,
            })
        })
        .map_err(|e| format!("query deferred_hooks export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect deferred_hooks export: {e}"))
}

fn load_active_task_dispatches(conn: &Connection) -> Result<Vec<TaskDispatchRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    kanban_card_id,
                    from_agent_id,
                    to_agent_id,
                    dispatch_type,
                    status,
                    title,
                    context,
                    result,
                    parent_dispatch_id,
                    chain_depth,
                    thread_id,
                    retry_count,
                    created_at,
                    updated_at,
                    completed_at
             FROM task_dispatches
             WHERE status IN ('pending', 'dispatched')
             ORDER BY created_at ASC, id ASC",
        )
        .map_err(|e| format!("prepare task_dispatches export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(TaskDispatchRow {
                id: row.get(0)?,
                kanban_card_id: row.get(1)?,
                from_agent_id: row.get(2)?,
                to_agent_id: row.get(3)?,
                dispatch_type: row.get(4)?,
                status: row.get(5)?,
                title: row.get(6)?,
                context: row.get(7)?,
                result: row.get(8)?,
                parent_dispatch_id: row.get(9)?,
                chain_depth: row.get(10)?,
                thread_id: row.get(11)?,
                retry_count: row.get(12)?,
                created_at: row.get(13)?,
                updated_at: row.get(14)?,
                completed_at: row.get(15)?,
            })
        })
        .map_err(|e| format!("query task_dispatches export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect task_dispatches export: {e}"))
}

fn load_live_sessions(conn: &Connection) -> Result<Vec<SessionRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT session_key,
                    agent_id,
                    provider,
                    status,
                    active_dispatch_id,
                    model,
                    session_info,
                    tokens,
                    cwd,
                    last_heartbeat,
                    thread_channel_id,
                    claude_session_id,
                    raw_provider_session_id,
                    created_at
             FROM sessions
             WHERE status = 'working' OR active_dispatch_id IS NOT NULL
             ORDER BY created_at ASC, session_key ASC",
        )
        .map_err(|e| format!("prepare sessions export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(SessionRow {
                session_key: row.get(0)?,
                agent_id: row.get(1)?,
                provider: row.get(2)?,
                status: row.get(3)?,
                active_dispatch_id: row.get(4)?,
                model: row.get(5)?,
                session_info: row.get(6)?,
                tokens: row.get(7)?,
                cwd: row.get(8)?,
                last_heartbeat: row.get(9)?,
                thread_channel_id: row.get(10)?,
                claude_session_id: row.get(11)?,
                raw_provider_session_id: row.get(12)?,
                created_at: row.get(13)?,
            })
        })
        .map_err(|e| format!("query sessions export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect sessions export: {e}"))
}

fn load_open_dispatch_outbox(conn: &Connection) -> Result<Vec<DispatchOutboxRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    dispatch_id,
                    action,
                    agent_id,
                    card_id,
                    title,
                    status,
                    retry_count,
                    next_attempt_at,
                    created_at,
                    processed_at,
                    error
             FROM dispatch_outbox
             WHERE status <> 'done' OR processed_at IS NULL
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare dispatch_outbox export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(DispatchOutboxRow {
                id: row.get(0)?,
                dispatch_id: row.get(1)?,
                action: row.get(2)?,
                agent_id: row.get(3)?,
                card_id: row.get(4)?,
                title: row.get(5)?,
                status: row
                    .get::<_, Option<String>>(6)?
                    .unwrap_or_else(|| "pending".to_string()),
                retry_count: row.get(7)?,
                next_attempt_at: row.get(8)?,
                created_at: row.get(9)?,
                processed_at: row.get(10)?,
                error: row.get(11)?,
            })
        })
        .map_err(|e| format!("query dispatch_outbox export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect dispatch_outbox export: {e}"))
}

fn referenced_card_ids(
    task_dispatches: &[TaskDispatchRow],
    dispatch_outbox: &[DispatchOutboxRow],
) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for row in task_dispatches {
        if let Some(card_id) = row.kanban_card_id.as_deref().map(str::trim) {
            if !card_id.is_empty() {
                ids.insert(card_id.to_string());
            }
        }
    }
    for row in dispatch_outbox {
        if let Some(card_id) = row.card_id.as_deref().map(str::trim) {
            if !card_id.is_empty() {
                ids.insert(card_id.to_string());
            }
        }
    }
    ids.into_iter().collect()
}

fn load_referenced_kanban_cards(
    conn: &Connection,
    card_ids: &[String],
) -> Result<Vec<KanbanCardRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    repo_id,
                    title,
                    status,
                    priority,
                    assigned_agent_id,
                    github_issue_url,
                    github_issue_number,
                    latest_dispatch_id,
                    review_round,
                    metadata,
                    started_at,
                    completed_at,
                    blocked_reason,
                    pipeline_stage_id,
                    review_notes,
                    review_status,
                    requested_at,
                    owner_agent_id,
                    requester_agent_id,
                    parent_card_id,
                    depth,
                    sort_order,
                    description,
                    active_thread_id,
                    channel_thread_map,
                    suggestion_pending_at,
                    review_entered_at,
                    awaiting_dod_at,
                    deferred_dod_json,
                    created_at,
                    updated_at
             FROM kanban_cards
             WHERE id = ?1",
        )
        .map_err(|e| format!("prepare kanban_cards export: {e}"))?;

    let mut rows = Vec::with_capacity(card_ids.len());
    for card_id in card_ids {
        if let Some(row) = stmt
            .query_row([card_id], |row| {
                Ok(KanbanCardRow {
                    id: row.get(0)?,
                    repo_id: row.get(1)?,
                    title: row.get(2)?,
                    status: row.get(3)?,
                    priority: row.get(4)?,
                    assigned_agent_id: row.get(5)?,
                    github_issue_url: row.get(6)?,
                    github_issue_number: row.get(7)?,
                    latest_dispatch_id: row.get(8)?,
                    review_round: row.get(9)?,
                    metadata: normalize_optional_json(row.get(10)?),
                    started_at: row.get(11)?,
                    completed_at: row.get(12)?,
                    blocked_reason: row.get(13)?,
                    pipeline_stage_id: row.get(14)?,
                    review_notes: row.get(15)?,
                    review_status: row.get(16)?,
                    requested_at: row.get(17)?,
                    owner_agent_id: row.get(18)?,
                    requester_agent_id: row.get(19)?,
                    parent_card_id: row.get(20)?,
                    depth: row.get(21)?,
                    sort_order: row.get(22)?,
                    description: row.get(23)?,
                    active_thread_id: row.get(24)?,
                    channel_thread_map: normalize_optional_json(row.get(25)?),
                    suggestion_pending_at: row.get(26)?,
                    review_entered_at: row.get(27)?,
                    awaiting_dod_at: row.get(28)?,
                    deferred_dod_json: normalize_optional_json(row.get(29)?),
                    created_at: row.get(30)?,
                    updated_at: row.get(31)?,
                })
            })
            .optional()
            .map_err(|e| format!("load kanban card {card_id}: {e}"))?
        {
            rows.push(row);
        }
    }
    Ok(rows)
}

fn referenced_agent_ids(
    task_dispatches: &[TaskDispatchRow],
    sessions: &[SessionRow],
    dispatch_outbox: &[DispatchOutboxRow],
    cards: &[KanbanCardRow],
) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for row in task_dispatches {
        for agent_id in [&row.from_agent_id, &row.to_agent_id] {
            if let Some(agent_id) = agent_id.as_deref().map(str::trim) {
                if !agent_id.is_empty() {
                    ids.insert(agent_id.to_string());
                }
            }
        }
    }
    for row in sessions {
        if let Some(agent_id) = row.agent_id.as_deref().map(str::trim) {
            if !agent_id.is_empty() {
                ids.insert(agent_id.to_string());
            }
        }
    }
    for row in dispatch_outbox {
        if let Some(agent_id) = row.agent_id.as_deref().map(str::trim) {
            if !agent_id.is_empty() {
                ids.insert(agent_id.to_string());
            }
        }
    }
    for row in cards {
        if let Some(agent_id) = row.assigned_agent_id.as_deref().map(str::trim) {
            if !agent_id.is_empty() {
                ids.insert(agent_id.to_string());
            }
        }
    }
    ids.into_iter().collect()
}

fn load_referenced_agents(
    conn: &Connection,
    agent_ids: &[String],
) -> Result<Vec<AgentRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    name,
                    name_ko,
                    department,
                    provider,
                    discord_channel_id,
                    discord_channel_alt,
                    discord_channel_cc,
                    discord_channel_cdx,
                    avatar_emoji,
                    status,
                    xp,
                    skills,
                    created_at,
                    updated_at
             FROM agents
             WHERE id = ?1",
        )
        .map_err(|e| format!("prepare agents export: {e}"))?;

    let mut rows = Vec::with_capacity(agent_ids.len());
    for agent_id in agent_ids {
        if let Some(row) = stmt
            .query_row([agent_id], |row| {
                Ok(AgentRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    name_ko: row.get(2)?,
                    department: row.get(3)?,
                    provider: row.get(4)?,
                    discord_channel_id: row.get(5)?,
                    discord_channel_alt: row.get(6)?,
                    discord_channel_cc: row.get(7)?,
                    discord_channel_cdx: row.get(8)?,
                    avatar_emoji: row.get(9)?,
                    status: row.get(10)?,
                    xp: row.get(11)?,
                    skills: row.get(12)?,
                    sprite_number: None,
                    description: None,
                    system_prompt: None,
                    pipeline_config: None,
                    created_at: row.get(13)?,
                    updated_at: row.get(14)?,
                })
            })
            .optional()
            .map_err(|e| format!("load agent {agent_id}: {e}"))?
        {
            rows.push(row);
        }
    }
    Ok(rows)
}

fn normalize_optional_json(value: Option<String>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(sanitize_json_text_for_pg(trimmed))
        }
    })
}

fn sanitize_json_text_for_pg(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(mut value) => {
            sanitize_json_value_for_pg(&mut value);
            serde_json::to_string(&value).unwrap_or_else(|_| sanitize_pg_text(raw))
        }
        Err(_) => sanitize_pg_text(raw).replace("\\u0000", "\\uFFFD"),
    }
}

fn sanitize_json_value_for_pg(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => {
            if text.contains('\0') {
                *text = text.replace('\0', "\u{FFFD}");
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                sanitize_json_value_for_pg(item);
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values_mut() {
                sanitize_json_value_for_pg(value);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn sanitize_pg_text(raw: &str) -> String {
    raw.replace('\0', "\u{FFFD}")
}

fn write_archive_files(
    archive_dir: &str,
    audit_logs: &[AuditLogRow],
    session_transcripts: &[SessionTranscriptRow],
) -> Result<ArchiveOutput, String> {
    let dir = normalize_archive_dir(archive_dir)?;
    fs::create_dir_all(&dir).map_err(|e| format!("create archive dir {}: {e}", dir.display()))?;

    let audit_path = dir.join("audit_logs.jsonl");
    let transcript_path = dir.join("session_transcripts.jsonl");
    write_jsonl(&audit_path, audit_logs)?;
    write_jsonl(&transcript_path, session_transcripts)?;

    Ok(ArchiveOutput {
        directory: dir.display().to_string(),
        audit_logs_file: Some(audit_path.display().to_string()),
        session_transcripts_file: Some(transcript_path.display().to_string()),
    })
}

fn normalize_archive_dir(raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("archive dir cannot be empty".to_string());
    }
    let expanded = expand_tilde_path(trimmed);
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(expanded))
            .map_err(|e| format!("resolve archive dir: {e}"))
    }
}

fn write_jsonl<T: Serialize>(path: &Path, rows: &[T]) -> Result<(), String> {
    let file = File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    let mut writer = BufWriter::new(file);
    for row in rows {
        let line =
            serde_json::to_string(row).map_err(|e| format!("serialize {}: {e}", path.display()))?;
        writer
            .write_all(line.as_bytes())
            .and_then(|_| writer.write_all(b"\n"))
            .map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    writer
        .flush()
        .map_err(|e| format!("flush {}: {e}", path.display()))
}

async fn load_pg_cutover_counts(pool: &PgPool) -> Result<PgCutoverCounts, String> {
    let active_dispatches = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM task_dispatches WHERE status IN ('pending', 'dispatched')",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| format!("count postgres active task_dispatches: {e}"))?;
    let working_sessions = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM sessions WHERE status = 'working' OR active_dispatch_id IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| format!("count postgres live sessions: {e}"))?;
    let open_dispatch_outbox = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM dispatch_outbox WHERE status NOT IN ('done', 'failed')",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| format!("count postgres open dispatch_outbox: {e}"))?;
    let pending_message_outbox = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM message_outbox WHERE status NOT IN ('sent', 'failed')",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| format!("count postgres pending message_outbox: {e}"))?;
    Ok(PgCutoverCounts {
        agents: pg_table_count(pool, "agents").await?,
        github_repos: pg_table_count(pool, "github_repos").await?,
        kanban_cards: pg_table_count(pool, "kanban_cards").await?,
        kanban_audit_logs: pg_table_count(pool, "kanban_audit_logs").await?,
        auto_queue_runs: pg_table_count(pool, "auto_queue_runs").await?,
        auto_queue_entries: pg_table_count(pool, "auto_queue_entries").await?,
        auto_queue_entry_transitions: pg_table_count(pool, "auto_queue_entry_transitions").await?,
        auto_queue_entry_dispatch_history: pg_table_count(
            pool,
            "auto_queue_entry_dispatch_history",
        )
        .await?,
        auto_queue_phase_gates: pg_table_count(pool, "auto_queue_phase_gates").await?,
        auto_queue_slots: pg_table_count(pool, "auto_queue_slots").await?,
        task_dispatches: pg_table_count(pool, "task_dispatches").await?,
        dispatch_events: pg_table_count(pool, "dispatch_events").await?,
        dispatch_queue: pg_table_count(pool, "dispatch_queue").await?,
        card_retrospectives: pg_table_count(pool, "card_retrospectives").await?,
        card_review_state: pg_table_count(pool, "card_review_state").await?,
        review_decisions: pg_table_count(pool, "review_decisions").await?,
        review_tuning_outcomes: pg_table_count(pool, "review_tuning_outcomes").await?,
        messages: pg_table_count(pool, "messages").await?,
        message_outbox: pg_table_count(pool, "message_outbox").await?,
        meetings: pg_table_count(pool, "meetings").await?,
        meeting_transcripts: pg_table_count(pool, "meeting_transcripts").await?,
        pending_dm_replies: pg_table_count(pool, "pending_dm_replies").await?,
        pipeline_stages: pg_table_count(pool, "pipeline_stages").await?,
        pr_tracking: pg_table_count(pool, "pr_tracking").await?,
        skills: pg_table_count(pool, "skills").await?,
        skill_usage: pg_table_count(pool, "skill_usage").await?,
        runtime_decisions: pg_table_count(pool, "runtime_decisions").await?,
        session_termination_events: pg_table_count(pool, "session_termination_events").await?,
        sessions: pg_table_count(pool, "sessions").await?,
        session_transcripts: pg_table_count(pool, "session_transcripts").await?,
        turns: pg_table_count(pool, "turns").await?,
        departments: pg_table_count(pool, "departments").await?,
        offices: pg_table_count(pool, "offices").await?,
        office_agents: pg_table_count(pool, "office_agents").await?,
        kv_meta: pg_table_count(pool, "kv_meta").await?,
        api_friction_events: pg_table_count(pool, "api_friction_events").await?,
        api_friction_issues: pg_table_count(pool, "api_friction_issues").await?,
        memento_feedback_turn_stats: pg_table_count(pool, "memento_feedback_turn_stats").await?,
        rate_limit_cache: pg_table_count(pool, "rate_limit_cache").await?,
        deferred_hooks: pg_table_count(pool, "deferred_hooks").await?,
        audit_logs: pg_table_count(pool, "audit_logs").await?,
        active_dispatches,
        working_sessions,
        open_dispatch_outbox,
        pending_message_outbox,
    })
}

async fn pg_table_count(pool: &PgPool, table: &str) -> Result<i64, String> {
    let sql = format!("SELECT COUNT(*) FROM {}", quote_ident(table));
    sqlx::query_scalar::<_, i64>(&sql)
        .fetch_one(pool)
        .await
        .map_err(|e| format!("count postgres {table}: {e}"))
}

async fn import_live_state_into_pg(
    pool: &PgPool,
    agents: &[AgentRow],
    cards: &[KanbanCardRow],
    task_dispatches: &[TaskDispatchRow],
    sessions: &[SessionRow],
    dispatch_outbox: &[DispatchOutboxRow],
) -> Result<ImportSummary, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| format!("begin postgres live-state transaction: {e}"))?;

    let mut inserted_agents = 0i64;
    for row in agents {
        let result = sqlx::query(
            "INSERT INTO agents (
                id,
                name,
                name_ko,
                department,
                provider,
                discord_channel_id,
                discord_channel_alt,
                discord_channel_cc,
                discord_channel_cdx,
                avatar_emoji,
                status,
                xp,
                skills,
                sprite_number,
                description,
                system_prompt,
                pipeline_config,
                created_at,
                updated_at
             )
             VALUES (
                $1,
                $2,
                $3,
                $4,
                COALESCE($5, 'claude'),
                $6,
                $7,
                $8,
                $9,
                $10,
                COALESCE($11, 'idle'),
                COALESCE($12, 0),
                $13,
                $14,
                $15,
                $16,
                CAST($17 AS jsonb),
                COALESCE(CAST($18 AS timestamptz), NOW()),
                COALESCE(CAST($19 AS timestamptz), NOW())
             )
             ON CONFLICT (id) DO UPDATE
             SET name = EXCLUDED.name,
                 name_ko = EXCLUDED.name_ko,
                 department = EXCLUDED.department,
                 provider = EXCLUDED.provider,
                 discord_channel_id = EXCLUDED.discord_channel_id,
                 discord_channel_alt = EXCLUDED.discord_channel_alt,
                 discord_channel_cc = EXCLUDED.discord_channel_cc,
                 discord_channel_cdx = EXCLUDED.discord_channel_cdx,
                 avatar_emoji = EXCLUDED.avatar_emoji,
                 status = EXCLUDED.status,
                 xp = EXCLUDED.xp,
                 skills = EXCLUDED.skills,
                 sprite_number = EXCLUDED.sprite_number,
                 description = EXCLUDED.description,
                 system_prompt = EXCLUDED.system_prompt,
                 pipeline_config = EXCLUDED.pipeline_config,
                 created_at = EXCLUDED.created_at,
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(&row.id)
        .bind(&row.name)
        .bind(&row.name_ko)
        .bind(&row.department)
        .bind(&row.provider)
        .bind(&row.discord_channel_id)
        .bind(&row.discord_channel_alt)
        .bind(&row.discord_channel_cc)
        .bind(&row.discord_channel_cdx)
        .bind(&row.avatar_emoji)
        .bind(&row.status)
        .bind(row.xp)
        .bind(&row.skills)
        .bind(row.sprite_number)
        .bind(&row.description)
        .bind(&row.system_prompt)
        .bind(&row.pipeline_config)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres agent {}: {e}", row.id))?;
        inserted_agents += result.rows_affected() as i64;
    }

    let mut upserted_cards = 0i64;
    for row in cards {
        let result = sqlx::query(
            "INSERT INTO kanban_cards (
                id,
                repo_id,
                title,
                status,
                priority,
                assigned_agent_id,
                github_issue_url,
                github_issue_number,
                latest_dispatch_id,
                review_round,
                metadata,
                started_at,
                completed_at,
                blocked_reason,
                pipeline_stage_id,
                review_notes,
                review_status,
                requested_at,
                owner_agent_id,
                requester_agent_id,
                parent_card_id,
                depth,
                sort_order,
                description,
                active_thread_id,
                channel_thread_map,
                suggestion_pending_at,
                review_entered_at,
                awaiting_dod_at,
                deferred_dod_json,
                created_at,
                updated_at
             )
             VALUES (
                $1,
                $2,
                $3,
                COALESCE($4, 'backlog'),
                COALESCE($5, 'medium'),
                $6,
                $7,
                $8,
                $9,
                COALESCE($10, 0),
                CAST($11 AS jsonb),
                CAST($12 AS timestamptz),
                CAST($13 AS timestamptz),
                $14,
                $15,
                $16,
                $17,
                CAST($18 AS timestamptz),
                $19,
                $20,
                $21,
                COALESCE($22, 0),
                COALESCE($23, 0),
                $24,
                $25,
                CAST($26 AS jsonb),
                CAST($27 AS timestamptz),
                CAST($28 AS timestamptz),
                CAST($29 AS timestamptz),
                CAST($30 AS jsonb),
                COALESCE(CAST($31 AS timestamptz), NOW()),
                COALESCE(CAST($32 AS timestamptz), NOW())
             )
             ON CONFLICT (id) DO UPDATE
             SET repo_id = EXCLUDED.repo_id,
                 title = EXCLUDED.title,
                 status = EXCLUDED.status,
                 priority = EXCLUDED.priority,
                 assigned_agent_id = EXCLUDED.assigned_agent_id,
                 github_issue_url = EXCLUDED.github_issue_url,
                 github_issue_number = EXCLUDED.github_issue_number,
                 latest_dispatch_id = EXCLUDED.latest_dispatch_id,
                 review_round = EXCLUDED.review_round,
                 metadata = EXCLUDED.metadata,
                 started_at = EXCLUDED.started_at,
                 completed_at = EXCLUDED.completed_at,
                 blocked_reason = EXCLUDED.blocked_reason,
                 pipeline_stage_id = EXCLUDED.pipeline_stage_id,
                 review_notes = EXCLUDED.review_notes,
                 review_status = EXCLUDED.review_status,
                 requested_at = EXCLUDED.requested_at,
                 owner_agent_id = EXCLUDED.owner_agent_id,
                 requester_agent_id = EXCLUDED.requester_agent_id,
                 parent_card_id = EXCLUDED.parent_card_id,
                 depth = EXCLUDED.depth,
                 sort_order = EXCLUDED.sort_order,
                 description = EXCLUDED.description,
                 active_thread_id = EXCLUDED.active_thread_id,
                 channel_thread_map = EXCLUDED.channel_thread_map,
                 suggestion_pending_at = EXCLUDED.suggestion_pending_at,
                 review_entered_at = EXCLUDED.review_entered_at,
                 awaiting_dod_at = EXCLUDED.awaiting_dod_at,
                 deferred_dod_json = EXCLUDED.deferred_dod_json,
                 created_at = EXCLUDED.created_at,
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(&row.id)
        .bind(&row.repo_id)
        .bind(&row.title)
        .bind(&row.status)
        .bind(&row.priority)
        .bind(&row.assigned_agent_id)
        .bind(&row.github_issue_url)
        .bind(row.github_issue_number)
        .bind(&row.latest_dispatch_id)
        .bind(row.review_round)
        .bind(&row.metadata)
        .bind(&row.started_at)
        .bind(&row.completed_at)
        .bind(&row.blocked_reason)
        .bind(&row.pipeline_stage_id)
        .bind(&row.review_notes)
        .bind(&row.review_status)
        .bind(&row.requested_at)
        .bind(&row.owner_agent_id)
        .bind(&row.requester_agent_id)
        .bind(&row.parent_card_id)
        .bind(row.depth)
        .bind(row.sort_order)
        .bind(&row.description)
        .bind(&row.active_thread_id)
        .bind(&row.channel_thread_map)
        .bind(&row.suggestion_pending_at)
        .bind(&row.review_entered_at)
        .bind(&row.awaiting_dod_at)
        .bind(&row.deferred_dod_json)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres kanban_card {}: {e}", row.id))?;
        upserted_cards += result.rows_affected() as i64;
    }

    let mut upserted_dispatches = 0i64;
    for row in task_dispatches {
        let result = sqlx::query(
            "INSERT INTO task_dispatches (
                id,
                kanban_card_id,
                from_agent_id,
                to_agent_id,
                dispatch_type,
                status,
                title,
                context,
                result,
                parent_dispatch_id,
                chain_depth,
                thread_id,
                retry_count,
                created_at,
                updated_at,
                completed_at
             )
             VALUES (
                $1,
                $2,
                $3,
                $4,
                $5,
                $6,
                $7,
                $8,
                $9,
                $10,
                COALESCE($11, 0),
                $12,
                COALESCE($13, 0),
                COALESCE(CAST($14 AS timestamptz), NOW()),
                COALESCE(CAST($15 AS timestamptz), NOW()),
                CAST($16 AS timestamptz)
             )
             ON CONFLICT (id) DO UPDATE
             SET kanban_card_id = EXCLUDED.kanban_card_id,
                 from_agent_id = EXCLUDED.from_agent_id,
                 to_agent_id = EXCLUDED.to_agent_id,
                 dispatch_type = EXCLUDED.dispatch_type,
                 status = EXCLUDED.status,
                 title = EXCLUDED.title,
                 context = EXCLUDED.context,
                 result = EXCLUDED.result,
                 parent_dispatch_id = EXCLUDED.parent_dispatch_id,
                 chain_depth = EXCLUDED.chain_depth,
                 thread_id = EXCLUDED.thread_id,
                 retry_count = EXCLUDED.retry_count,
                 created_at = EXCLUDED.created_at,
                 updated_at = EXCLUDED.updated_at,
                 completed_at = EXCLUDED.completed_at",
        )
        .bind(&row.id)
        .bind(&row.kanban_card_id)
        .bind(&row.from_agent_id)
        .bind(&row.to_agent_id)
        .bind(&row.dispatch_type)
        .bind(&row.status)
        .bind(&row.title)
        .bind(&row.context)
        .bind(&row.result)
        .bind(&row.parent_dispatch_id)
        .bind(row.chain_depth)
        .bind(&row.thread_id)
        .bind(row.retry_count)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .bind(&row.completed_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres task_dispatches {}: {e}", row.id))?;
        upserted_dispatches += result.rows_affected() as i64;
    }

    let mut upserted_sessions = 0i64;
    for row in sessions {
        let result = sqlx::query(
            "INSERT INTO sessions (
                session_key,
                agent_id,
                provider,
                status,
                active_dispatch_id,
                model,
                session_info,
                tokens,
                cwd,
                last_heartbeat,
                thread_channel_id,
                claude_session_id,
                raw_provider_session_id,
                created_at
             )
             VALUES (
                $1,
                $2,
                COALESCE($3, 'claude'),
                COALESCE($4, 'disconnected'),
                $5,
                $6,
                $7,
                COALESCE($8, 0),
                $9,
                CAST($10 AS timestamptz),
                $11,
                $12,
                $13,
                COALESCE(CAST($14 AS timestamptz), NOW())
             )
             ON CONFLICT (session_key) DO UPDATE
             SET agent_id = EXCLUDED.agent_id,
                 provider = EXCLUDED.provider,
                 status = EXCLUDED.status,
                 active_dispatch_id = EXCLUDED.active_dispatch_id,
                 model = EXCLUDED.model,
                 session_info = EXCLUDED.session_info,
                 tokens = EXCLUDED.tokens,
                 cwd = EXCLUDED.cwd,
                 last_heartbeat = EXCLUDED.last_heartbeat,
                 thread_channel_id = EXCLUDED.thread_channel_id,
                 claude_session_id = EXCLUDED.claude_session_id,
                 raw_provider_session_id = EXCLUDED.raw_provider_session_id",
        )
        .bind(&row.session_key)
        .bind(&row.agent_id)
        .bind(&row.provider)
        .bind(&row.status)
        .bind(&row.active_dispatch_id)
        .bind(&row.model)
        .bind(&row.session_info)
        .bind(row.tokens)
        .bind(&row.cwd)
        .bind(&row.last_heartbeat)
        .bind(&row.thread_channel_id)
        .bind(&row.claude_session_id)
        .bind(&row.raw_provider_session_id)
        .bind(&row.created_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres sessions {}: {e}", row.session_key))?;
        upserted_sessions += result.rows_affected() as i64;
    }

    let mut upserted_outbox = 0i64;
    for row in dispatch_outbox {
        let result = sqlx::query(
            "INSERT INTO dispatch_outbox (
                id,
                dispatch_id,
                action,
                agent_id,
                card_id,
                title,
                status,
                retry_count,
                next_attempt_at,
                created_at,
                processed_at,
                error
             )
             VALUES (
                $1,
                $2,
                $3,
                $4,
                $5,
                $6,
                $7,
                COALESCE($8, 0),
                CAST($9 AS timestamptz),
                COALESCE(CAST($10 AS timestamptz), NOW()),
                CAST($11 AS timestamptz),
                $12
             )
             ON CONFLICT (id) DO UPDATE
             SET dispatch_id = EXCLUDED.dispatch_id,
                 action = EXCLUDED.action,
                 agent_id = EXCLUDED.agent_id,
                 card_id = EXCLUDED.card_id,
                 title = EXCLUDED.title,
                 status = EXCLUDED.status,
                 retry_count = EXCLUDED.retry_count,
                 next_attempt_at = EXCLUDED.next_attempt_at,
                 created_at = EXCLUDED.created_at,
                 processed_at = EXCLUDED.processed_at,
                 error = EXCLUDED.error",
        )
        .bind(row.id)
        .bind(&row.dispatch_id)
        .bind(&row.action)
        .bind(&row.agent_id)
        .bind(&row.card_id)
        .bind(&row.title)
        .bind(&row.status)
        .bind(row.retry_count)
        .bind(&row.next_attempt_at)
        .bind(&row.created_at)
        .bind(&row.processed_at)
        .bind(&row.error)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres dispatch_outbox {}: {e}", row.id))?;
        upserted_outbox += result.rows_affected() as i64;
    }

    advance_pg_serial_sequences(&mut tx).await?;

    tx.commit()
        .await
        .map_err(|e| format!("commit postgres live-state transaction: {e}"))?;

    Ok(ImportSummary {
        agents_upserted: inserted_agents,
        cards_upserted: upserted_cards,
        task_dispatches_upserted: upserted_dispatches,
        sessions_upserted: upserted_sessions,
        dispatch_outbox_upserted: upserted_outbox,
        ..Default::default()
    })
}

async fn advance_pg_serial_sequences(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), String> {
    let serial_columns = sqlx::query(
        "SELECT table_name, column_name
         FROM information_schema.columns
         WHERE table_schema = 'public'
           AND data_type IN ('bigint', 'integer')
           AND (
                column_default LIKE 'nextval(%'
                OR is_identity = 'YES'
           )
         ORDER BY table_name, ordinal_position",
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| format!("list postgres serial columns: {e}"))?;

    for column in serial_columns {
        let table_name = column
            .try_get::<String, _>("table_name")
            .map_err(|e| format!("decode postgres serial table name: {e}"))?;
        let column_name = column
            .try_get::<String, _>("column_name")
            .map_err(|e| format!("decode postgres serial column name: {e}"))?;

        let sequence_name =
            sqlx::query_scalar::<_, Option<String>>("SELECT pg_get_serial_sequence($1, $2)")
                .bind(format!("public.{table_name}"))
                .bind(&column_name)
                .fetch_one(&mut **tx)
                .await
                .map_err(|e| {
                    format!("resolve postgres serial sequence for {table_name}.{column_name}: {e}")
                })?;

        let Some(sequence_name) = sequence_name else {
            continue;
        };

        let quoted_table = quote_ident(&table_name);
        let quoted_column = quote_ident(&column_name);
        let max_query = format!(
            "SELECT COALESCE(MAX({quoted_column}), 0)::BIGINT AS max_id FROM public.{quoted_table}"
        );
        let max_id = sqlx::query_scalar::<_, i64>(&max_query)
            .fetch_one(&mut **tx)
            .await
            .map_err(|e| format!("load max id for {table_name}.{column_name}: {e}"))?;

        sqlx::query("SELECT setval($1, $2, $3)")
            .bind(&sequence_name)
            .bind(if max_id > 0 { max_id } else { 1 })
            .bind(max_id > 0)
            .execute(&mut **tx)
            .await
            .map_err(|e| {
                format!(
                    "advance postgres serial sequence {sequence_name} for {table_name}.{column_name}: {e}"
                )
            })?;
    }

    Ok(())
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

async fn import_history_into_pg(
    pool: &PgPool,
    audit_logs: &[AuditLogRow],
    session_transcripts: &[SessionTranscriptRow],
) -> Result<ImportSummary, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| format!("begin postgres cutover transaction: {e}"))?;

    let mut inserted_audit_logs = 0i64;
    for row in audit_logs {
        let result = sqlx::query(
            "INSERT INTO audit_logs (entity_type, entity_id, action, timestamp, actor)
             SELECT $1, $2, $3, COALESCE(CAST($4 AS timestamptz), NOW()), $5
             WHERE NOT EXISTS (
                 SELECT 1
                   FROM audit_logs
                  WHERE entity_type IS NOT DISTINCT FROM $1
                    AND entity_id IS NOT DISTINCT FROM $2
                    AND action IS NOT DISTINCT FROM $3
                    AND actor IS NOT DISTINCT FROM $5
                    AND timestamp = COALESCE(CAST($4 AS timestamptz), NOW())
             )",
        )
        .bind(&row.entity_type)
        .bind(&row.entity_id)
        .bind(&row.action)
        .bind(&row.timestamp)
        .bind(&row.actor)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres audit_logs: {e}"))?;
        inserted_audit_logs += result.rows_affected() as i64;
    }

    let mut upserted_session_transcripts = 0i64;
    for row in session_transcripts {
        let user_message = sanitize_pg_text(&row.user_message);
        let assistant_message = sanitize_pg_text(&row.assistant_message);
        let events_json = sanitize_json_text_for_pg(&row.events_json);
        let result = sqlx::query(
            "INSERT INTO session_transcripts (
                turn_id,
                session_key,
                channel_id,
                agent_id,
                provider,
                dispatch_id,
                user_message,
                assistant_message,
                events_json,
                duration_ms,
                created_at
             )
             VALUES (
                $1,
                $2,
                $3,
                $4,
                $5,
                $6,
                $7,
                $8,
                CAST($9 AS jsonb),
                $10,
                COALESCE(CAST($11 AS timestamptz), NOW())
             )
             ON CONFLICT (turn_id) DO UPDATE
             SET session_key = EXCLUDED.session_key,
                 channel_id = EXCLUDED.channel_id,
                 agent_id = COALESCE(EXCLUDED.agent_id, session_transcripts.agent_id),
                 provider = EXCLUDED.provider,
                 dispatch_id = EXCLUDED.dispatch_id,
                 user_message = EXCLUDED.user_message,
                 assistant_message = EXCLUDED.assistant_message,
                 events_json = EXCLUDED.events_json,
                 duration_ms = EXCLUDED.duration_ms",
        )
        .bind(&row.turn_id)
        .bind(&row.session_key)
        .bind(&row.channel_id)
        .bind(&row.agent_id)
        .bind(&row.provider)
        .bind(&row.dispatch_id)
        .bind(&user_message)
        .bind(&assistant_message)
        .bind(&events_json)
        .bind(row.duration_ms)
        .bind(&row.created_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres session_transcripts {}: {e}", row.turn_id))?;
        upserted_session_transcripts += result.rows_affected() as i64;
    }

    tx.commit()
        .await
        .map_err(|e| format!("commit postgres cutover transaction: {e}"))?;

    Ok(ImportSummary {
        audit_logs_inserted: inserted_audit_logs,
        session_transcripts_upserted: upserted_session_transcripts,
        ..Default::default()
    })
}

fn merge_import_summaries(base: &mut ImportSummary, next: ImportSummary) {
    base.offices_upserted += next.offices_upserted;
    base.departments_upserted += next.departments_upserted;
    base.office_agents_upserted += next.office_agents_upserted;
    base.github_repos_upserted += next.github_repos_upserted;
    base.agents_upserted += next.agents_upserted;
    base.cards_upserted += next.cards_upserted;
    base.kanban_audit_logs_upserted += next.kanban_audit_logs_upserted;
    base.card_retrospectives_upserted += next.card_retrospectives_upserted;
    base.card_review_state_upserted += next.card_review_state_upserted;
    base.auto_queue_runs_upserted += next.auto_queue_runs_upserted;
    base.auto_queue_entries_upserted += next.auto_queue_entries_upserted;
    base.auto_queue_entry_transitions_upserted += next.auto_queue_entry_transitions_upserted;
    base.auto_queue_entry_dispatch_history_upserted +=
        next.auto_queue_entry_dispatch_history_upserted;
    base.auto_queue_phase_gates_upserted += next.auto_queue_phase_gates_upserted;
    base.auto_queue_slots_upserted += next.auto_queue_slots_upserted;
    base.task_dispatches_upserted += next.task_dispatches_upserted;
    base.dispatch_events_upserted += next.dispatch_events_upserted;
    base.dispatch_outbox_upserted += next.dispatch_outbox_upserted;
    base.dispatch_queue_upserted += next.dispatch_queue_upserted;
    base.pr_tracking_upserted += next.pr_tracking_upserted;
    base.sessions_upserted += next.sessions_upserted;
    base.session_termination_events_upserted += next.session_termination_events_upserted;
    base.session_transcripts_upserted += next.session_transcripts_upserted;
    base.turns_upserted += next.turns_upserted;
    base.meetings_upserted += next.meetings_upserted;
    base.meeting_transcripts_upserted += next.meeting_transcripts_upserted;
    base.messages_upserted += next.messages_upserted;
    base.message_outbox_upserted += next.message_outbox_upserted;
    base.pending_dm_replies_upserted += next.pending_dm_replies_upserted;
    base.review_decisions_upserted += next.review_decisions_upserted;
    base.review_tuning_outcomes_upserted += next.review_tuning_outcomes_upserted;
    base.skills_upserted += next.skills_upserted;
    base.skill_usage_upserted += next.skill_usage_upserted;
    base.pipeline_stages_upserted += next.pipeline_stages_upserted;
    base.runtime_decisions_upserted += next.runtime_decisions_upserted;
    base.kv_meta_upserted += next.kv_meta_upserted;
    base.api_friction_events_upserted += next.api_friction_events_upserted;
    base.api_friction_issues_upserted += next.api_friction_issues_upserted;
    base.memento_feedback_turn_stats_upserted += next.memento_feedback_turn_stats_upserted;
    base.rate_limit_cache_upserted += next.rate_limit_cache_upserted;
    base.deferred_hooks_upserted += next.deferred_hooks_upserted;
    base.audit_logs_inserted += next.audit_logs_inserted;
    base.auto_queue_entries_skipped_orphans += next.auto_queue_entries_skipped_orphans;
    base.auto_queue_entry_transitions_skipped_orphans +=
        next.auto_queue_entry_transitions_skipped_orphans;
    base.auto_queue_entry_dispatch_history_skipped_orphans +=
        next.auto_queue_entry_dispatch_history_skipped_orphans;
    base.auto_queue_phase_gates_skipped_orphans += next.auto_queue_phase_gates_skipped_orphans;
    base.auto_queue_slots_skipped_orphans += next.auto_queue_slots_skipped_orphans;
    base.dispatch_events_skipped_orphans += next.dispatch_events_skipped_orphans;
    base.card_retrospectives_skipped_orphans += next.card_retrospectives_skipped_orphans;
    base.card_review_state_skipped_orphans += next.card_review_state_skipped_orphans;
    base.pr_tracking_skipped_orphans += next.pr_tracking_skipped_orphans;
    base.session_termination_events_skipped_orphans +=
        next.session_termination_events_skipped_orphans;
    base.meeting_transcripts_skipped_orphans += next.meeting_transcripts_skipped_orphans;
}

async fn upsert_kanban_audit_logs_into_pg(
    tx: &mut Transaction<'_, Postgres>,
    rows: &[KanbanAuditLogRow],
) -> Result<i64, String> {
    for chunk in rows.chunks(INSERT_BATCH_SIZE) {
        let mut builder = QueryBuilder::<Postgres>::new(
            "INSERT INTO kanban_audit_logs (id, card_id, from_status, to_status, source, result, created_at) ",
        );
        builder.push_values(chunk, |mut b, row| {
            b.push_bind(row.id);
            b.push_bind(&row.card_id);
            b.push_bind(&row.from_status);
            b.push_bind(&row.to_status);
            b.push_bind(&row.source);
            b.push_bind(&row.result);
            b.push_unseparated(", CAST(")
                .push_bind_unseparated(&row.created_at)
                .push_unseparated(" AS timestamptz)");
        });
        builder.push(
            " ON CONFLICT (id) DO UPDATE
              SET card_id = EXCLUDED.card_id,
                  from_status = EXCLUDED.from_status,
                  to_status = EXCLUDED.to_status,
                  source = EXCLUDED.source,
                  result = EXCLUDED.result,
                  created_at = EXCLUDED.created_at",
        );
        builder
            .build()
            .execute(&mut **tx)
            .await
            .map_err(|e| format!("import postgres kanban_audit_logs chunk: {e}"))?;
    }
    Ok(rows.len() as i64)
}

async fn upsert_dispatch_events_into_pg(
    tx: &mut Transaction<'_, Postgres>,
    rows: &[DispatchEventRow],
) -> Result<i64, String> {
    for chunk in rows.chunks(INSERT_BATCH_SIZE) {
        let mut builder = QueryBuilder::<Postgres>::new(
            "INSERT INTO dispatch_events (
                id, dispatch_id, kanban_card_id, dispatch_type, from_status, to_status, transition_source, payload_json, created_at
             ) ",
        );
        builder.push_values(chunk, |mut b, row| {
            b.push_bind(row.id);
            b.push_bind(&row.dispatch_id);
            b.push_bind(&row.kanban_card_id);
            b.push_bind(&row.dispatch_type);
            b.push_bind(&row.from_status);
            b.push_bind(&row.to_status);
            b.push_bind(&row.transition_source);
            b.push_unseparated(", CAST(")
                .push_bind_unseparated(&row.payload_json)
                .push_unseparated(" AS jsonb)");
            b.push_unseparated(", CAST(")
                .push_bind_unseparated(&row.created_at)
                .push_unseparated(" AS timestamptz)");
        });
        builder.push(
            " ON CONFLICT (id) DO UPDATE
              SET dispatch_id = EXCLUDED.dispatch_id,
                  kanban_card_id = EXCLUDED.kanban_card_id,
                  dispatch_type = EXCLUDED.dispatch_type,
                  from_status = EXCLUDED.from_status,
                  to_status = EXCLUDED.to_status,
                  transition_source = EXCLUDED.transition_source,
                  payload_json = EXCLUDED.payload_json,
                  created_at = EXCLUDED.created_at",
        );
        builder
            .build()
            .execute(&mut **tx)
            .await
            .map_err(|e| format!("import postgres dispatch_events chunk: {e}"))?;
    }
    Ok(rows.len() as i64)
}

async fn upsert_message_outbox_into_pg(
    tx: &mut Transaction<'_, Postgres>,
    rows: &[MessageOutboxRow],
) -> Result<i64, String> {
    for chunk in rows.chunks(INSERT_BATCH_SIZE) {
        let mut builder = QueryBuilder::<Postgres>::new(
            "INSERT INTO message_outbox (
                id, target, content, bot, source, reason_code, session_key, status, created_at, sent_at, error, claimed_at, claim_owner
             ) ",
        );
        builder.push_values(chunk, |mut b, row| {
            b.push_bind(row.id);
            b.push_bind(&row.target);
            b.push_bind(&row.content);
            b.push_bind(&row.bot);
            b.push_bind(&row.source);
            b.push_bind(&row.reason_code);
            b.push_bind(&row.session_key);
            b.push_bind(&row.status);
            b.push_unseparated(", CAST(")
                .push_bind_unseparated(&row.created_at)
                .push_unseparated(" AS timestamptz)");
            b.push_unseparated(", CAST(")
                .push_bind_unseparated(&row.sent_at)
                .push_unseparated(" AS timestamptz)");
            b.push_bind(&row.error);
            b.push_unseparated(", CAST(")
                .push_bind_unseparated(&row.claimed_at)
                .push_unseparated(" AS timestamptz)");
            b.push_bind(&row.claim_owner);
        });
        builder.push(
            " ON CONFLICT (id) DO UPDATE
              SET target = EXCLUDED.target,
                  content = EXCLUDED.content,
                  bot = EXCLUDED.bot,
                  source = EXCLUDED.source,
                  reason_code = EXCLUDED.reason_code,
                  session_key = EXCLUDED.session_key,
                  status = EXCLUDED.status,
                  created_at = EXCLUDED.created_at,
                  sent_at = EXCLUDED.sent_at,
                  error = EXCLUDED.error,
                  claimed_at = EXCLUDED.claimed_at,
                  claim_owner = EXCLUDED.claim_owner",
        );
        builder
            .build()
            .execute(&mut **tx)
            .await
            .map_err(|e| format!("import postgres message_outbox chunk: {e}"))?;
    }
    Ok(rows.len() as i64)
}

async fn upsert_turns_into_pg(
    tx: &mut Transaction<'_, Postgres>,
    rows: &[TurnRow],
) -> Result<i64, String> {
    for chunk in rows.chunks(INSERT_BATCH_SIZE) {
        let mut builder = QueryBuilder::<Postgres>::new(
            "INSERT INTO turns (
                turn_id, session_key, thread_id, thread_title, channel_id, agent_id, provider, session_id, dispatch_id,
                started_at, finished_at, duration_ms, input_tokens, cache_create_tokens, cache_read_tokens, output_tokens, created_at
             ) ",
        );
        builder.push_values(chunk, |mut b, row| {
            b.push_bind(&row.turn_id);
            b.push_bind(&row.session_key);
            b.push_bind(&row.thread_id);
            b.push_bind(&row.thread_title);
            b.push_bind(&row.channel_id);
            b.push_bind(&row.agent_id);
            b.push_bind(&row.provider);
            b.push_bind(&row.session_id);
            b.push_bind(&row.dispatch_id);
            b.push_unseparated(", CAST(")
                .push_bind_unseparated(&row.started_at)
                .push_unseparated(" AS timestamptz)");
            b.push_unseparated(", CAST(")
                .push_bind_unseparated(&row.finished_at)
                .push_unseparated(" AS timestamptz)");
            b.push_bind(row.duration_ms);
            b.push_bind(row.input_tokens);
            b.push_bind(row.cache_create_tokens);
            b.push_bind(row.cache_read_tokens);
            b.push_bind(row.output_tokens);
            b.push_unseparated(", CAST(")
                .push_bind_unseparated(&row.created_at)
                .push_unseparated(" AS timestamptz)");
        });
        builder.push(
            " ON CONFLICT (turn_id) DO UPDATE
              SET session_key = EXCLUDED.session_key,
                  thread_id = EXCLUDED.thread_id,
                  thread_title = EXCLUDED.thread_title,
                  channel_id = EXCLUDED.channel_id,
                  agent_id = EXCLUDED.agent_id,
                  provider = EXCLUDED.provider,
                  session_id = EXCLUDED.session_id,
                  dispatch_id = EXCLUDED.dispatch_id,
                  started_at = EXCLUDED.started_at,
                  finished_at = EXCLUDED.finished_at,
                  duration_ms = EXCLUDED.duration_ms,
                  input_tokens = EXCLUDED.input_tokens,
                  cache_create_tokens = EXCLUDED.cache_create_tokens,
                  cache_read_tokens = EXCLUDED.cache_read_tokens,
                  output_tokens = EXCLUDED.output_tokens,
                  created_at = EXCLUDED.created_at",
        );
        builder
            .build()
            .execute(&mut **tx)
            .await
            .map_err(|e| format!("import postgres turns chunk: {e}"))?;
    }
    Ok(rows.len() as i64)
}

async fn import_supporting_tables_into_pg(
    pool: &PgPool,
    snapshot: &SqliteCutoverSnapshot,
) -> Result<ImportSummary, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| format!("begin postgres supporting-table transaction: {e}"))?;
    let mut summary = ImportSummary::default();

    for row in &snapshot.offices {
        sqlx::query(
            "INSERT INTO offices (id, name, layout, name_ko, icon, color, description, sort_order, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, COALESCE($8, 0), CAST($9 AS timestamptz))
             ON CONFLICT (id) DO UPDATE
             SET name = EXCLUDED.name,
                 layout = EXCLUDED.layout,
                 name_ko = EXCLUDED.name_ko,
                 icon = EXCLUDED.icon,
                 color = EXCLUDED.color,
                 description = EXCLUDED.description,
                 sort_order = EXCLUDED.sort_order,
                 created_at = EXCLUDED.created_at",
        )
        .bind(&row.id)
        .bind(&row.name)
        .bind(&row.layout)
        .bind(&row.name_ko)
        .bind(&row.icon)
        .bind(&row.color)
        .bind(&row.description)
        .bind(row.sort_order)
        .bind(&row.created_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres office {}: {e}", row.id))?;
    }
    summary.offices_upserted = snapshot.offices.len() as i64;

    for row in &snapshot.departments {
        sqlx::query(
            "INSERT INTO departments (id, name, office_id, name_ko, icon, color, description, sort_order, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, COALESCE($8, 0), CAST($9 AS timestamptz))
             ON CONFLICT (id) DO UPDATE
             SET name = EXCLUDED.name,
                 office_id = EXCLUDED.office_id,
                 name_ko = EXCLUDED.name_ko,
                 icon = EXCLUDED.icon,
                 color = EXCLUDED.color,
                 description = EXCLUDED.description,
                 sort_order = EXCLUDED.sort_order,
                 created_at = EXCLUDED.created_at",
        )
        .bind(&row.id)
        .bind(&row.name)
        .bind(&row.office_id)
        .bind(&row.name_ko)
        .bind(&row.icon)
        .bind(&row.color)
        .bind(&row.description)
        .bind(row.sort_order)
        .bind(&row.created_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres department {}: {e}", row.id))?;
    }
    summary.departments_upserted = snapshot.departments.len() as i64;

    for row in &snapshot.github_repos {
        sqlx::query(
            "INSERT INTO github_repos (id, display_name, sync_enabled, last_synced_at, default_agent_id, pipeline_config)
             VALUES ($1, $2, COALESCE($3, TRUE), CAST($4 AS timestamptz), $5, CAST($6 AS jsonb))
             ON CONFLICT (id) DO UPDATE
             SET display_name = EXCLUDED.display_name,
                 sync_enabled = EXCLUDED.sync_enabled,
                 last_synced_at = EXCLUDED.last_synced_at,
                 default_agent_id = EXCLUDED.default_agent_id,
                 pipeline_config = EXCLUDED.pipeline_config",
        )
        .bind(&row.id)
        .bind(&row.display_name)
        .bind(row.sync_enabled)
        .bind(&row.last_synced_at)
        .bind(&row.default_agent_id)
        .bind(&row.pipeline_config)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres github_repo {}: {e}", row.id))?;
    }
    summary.github_repos_upserted = snapshot.github_repos.len() as i64;

    for row in &snapshot.office_agents {
        sqlx::query(
            "INSERT INTO office_agents (office_id, agent_id, department_id, joined_at)
             VALUES ($1, $2, $3, CAST($4 AS timestamptz))
             ON CONFLICT (office_id, agent_id) DO UPDATE
             SET department_id = EXCLUDED.department_id,
                 joined_at = EXCLUDED.joined_at",
        )
        .bind(&row.office_id)
        .bind(&row.agent_id)
        .bind(&row.department_id)
        .bind(&row.joined_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            format!(
                "import postgres office_agent {}/{}: {e}",
                row.office_id, row.agent_id
            )
        })?;
    }
    summary.office_agents_upserted = snapshot.office_agents.len() as i64;

    summary.kanban_audit_logs_upserted =
        upsert_kanban_audit_logs_into_pg(&mut tx, &snapshot.kanban_audit_logs).await?;

    for row in &snapshot.card_retrospectives {
        sqlx::query(
            "INSERT INTO card_retrospectives (
                id, card_id, dispatch_id, terminal_status, repo_id, issue_number, title, topic, content,
                review_round, review_notes, duration_seconds, success, result_json, memory_payload,
                sync_backend, sync_status, sync_error, created_at, updated_at
             )
             VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9,
                COALESCE($10, 0), $11, $12, COALESCE($13, FALSE), CAST($14 AS jsonb), CAST($15 AS jsonb),
                $16, COALESCE($17, 'skipped'), $18, CAST($19 AS timestamptz), CAST($20 AS timestamptz)
             )
             ON CONFLICT (id) DO UPDATE
             SET card_id = EXCLUDED.card_id,
                 dispatch_id = EXCLUDED.dispatch_id,
                 terminal_status = EXCLUDED.terminal_status,
                 repo_id = EXCLUDED.repo_id,
                 issue_number = EXCLUDED.issue_number,
                 title = EXCLUDED.title,
                 topic = EXCLUDED.topic,
                 content = EXCLUDED.content,
                 review_round = EXCLUDED.review_round,
                 review_notes = EXCLUDED.review_notes,
                 duration_seconds = EXCLUDED.duration_seconds,
                 success = EXCLUDED.success,
                 result_json = EXCLUDED.result_json,
                 memory_payload = EXCLUDED.memory_payload,
                 sync_backend = EXCLUDED.sync_backend,
                 sync_status = EXCLUDED.sync_status,
                 sync_error = EXCLUDED.sync_error,
                 created_at = EXCLUDED.created_at,
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(&row.id)
        .bind(&row.card_id)
        .bind(&row.dispatch_id)
        .bind(&row.terminal_status)
        .bind(&row.repo_id)
        .bind(row.issue_number)
        .bind(&row.title)
        .bind(&row.topic)
        .bind(&row.content)
        .bind(row.review_round)
        .bind(&row.review_notes)
        .bind(row.duration_seconds)
        .bind(row.success)
        .bind(&row.result_json)
        .bind(&row.memory_payload)
        .bind(&row.sync_backend)
        .bind(&row.sync_status)
        .bind(&row.sync_error)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres card_retrospective {}: {e}", row.id))?;
    }
    summary.card_retrospectives_upserted = snapshot.card_retrospectives.len() as i64;

    for row in &snapshot.card_review_state {
        sqlx::query(
            "INSERT INTO card_review_state (
                card_id, review_round, state, pending_dispatch_id, last_verdict, last_decision, decided_by,
                decided_at, approach_change_round, session_reset_round, review_entered_at, updated_at
             )
             VALUES (
                $1, COALESCE($2, 0), COALESCE($3, 'idle'), $4, $5, $6, $7,
                CAST($8 AS timestamptz), $9, $10, CAST($11 AS timestamptz), CAST($12 AS timestamptz)
             )
             ON CONFLICT (card_id) DO UPDATE
             SET review_round = EXCLUDED.review_round,
                 state = EXCLUDED.state,
                 pending_dispatch_id = EXCLUDED.pending_dispatch_id,
                 last_verdict = EXCLUDED.last_verdict,
                 last_decision = EXCLUDED.last_decision,
                 decided_by = EXCLUDED.decided_by,
                 decided_at = EXCLUDED.decided_at,
                 approach_change_round = EXCLUDED.approach_change_round,
                 session_reset_round = EXCLUDED.session_reset_round,
                 review_entered_at = EXCLUDED.review_entered_at,
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(&row.card_id)
        .bind(row.review_round)
        .bind(&row.state)
        .bind(&row.pending_dispatch_id)
        .bind(&row.last_verdict)
        .bind(&row.last_decision)
        .bind(&row.decided_by)
        .bind(&row.decided_at)
        .bind(row.approach_change_round)
        .bind(row.session_reset_round)
        .bind(&row.review_entered_at)
        .bind(&row.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres card_review_state {}: {e}", row.card_id))?;
    }
    summary.card_review_state_upserted = snapshot.card_review_state.len() as i64;

    for row in &snapshot.auto_queue_runs {
        sqlx::query(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, ai_model, ai_rationale, timeout_minutes, unified_thread,
                unified_thread_id, unified_thread_channel_id, max_concurrent_threads, thread_group_count,
                created_at, completed_at
             )
             VALUES (
                $1, $2, $3, COALESCE($4, 'active'), $5, $6, COALESCE($7, 120), COALESCE($8, FALSE),
                $9, $10, COALESCE($11, 1), COALESCE($12, 1), CAST($13 AS timestamptz), CAST($14 AS timestamptz)
             )
             ON CONFLICT (id) DO UPDATE
             SET repo = EXCLUDED.repo,
                 agent_id = EXCLUDED.agent_id,
                 status = EXCLUDED.status,
                 ai_model = EXCLUDED.ai_model,
                 ai_rationale = EXCLUDED.ai_rationale,
                 timeout_minutes = EXCLUDED.timeout_minutes,
                 unified_thread = EXCLUDED.unified_thread,
                 unified_thread_id = EXCLUDED.unified_thread_id,
                 unified_thread_channel_id = EXCLUDED.unified_thread_channel_id,
                 max_concurrent_threads = EXCLUDED.max_concurrent_threads,
                 thread_group_count = EXCLUDED.thread_group_count,
                 created_at = EXCLUDED.created_at,
                 completed_at = EXCLUDED.completed_at",
        )
        .bind(&row.id)
        .bind(&row.repo)
        .bind(&row.agent_id)
        .bind(&row.status)
        .bind(&row.ai_model)
        .bind(&row.ai_rationale)
        .bind(row.timeout_minutes)
        .bind(row.unified_thread)
        .bind(&row.unified_thread_id)
        .bind(&row.unified_thread_channel_id)
        .bind(row.max_concurrent_threads)
        .bind(row.thread_group_count)
        .bind(&row.created_at)
        .bind(&row.completed_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres auto_queue_run {}: {e}", row.id))?;
    }
    summary.auto_queue_runs_upserted = snapshot.auto_queue_runs.len() as i64;

    for row in &snapshot.auto_queue_entries {
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, priority_rank, reason, status, retry_count,
                dispatch_id, slot_index, thread_group, batch_phase, created_at, dispatched_at, completed_at
             )
             VALUES (
                $1, $2, $3, $4, COALESCE($5, 0), $6, COALESCE($7, 'pending'), COALESCE($8, 0),
                $9, $10, COALESCE($11, 0), COALESCE($12, 0), CAST($13 AS timestamptz), CAST($14 AS timestamptz), CAST($15 AS timestamptz)
             )
             ON CONFLICT (id) DO UPDATE
             SET run_id = EXCLUDED.run_id,
                 kanban_card_id = EXCLUDED.kanban_card_id,
                 agent_id = EXCLUDED.agent_id,
                 priority_rank = EXCLUDED.priority_rank,
                 reason = EXCLUDED.reason,
                 status = EXCLUDED.status,
                 retry_count = EXCLUDED.retry_count,
                 dispatch_id = EXCLUDED.dispatch_id,
                 slot_index = EXCLUDED.slot_index,
                 thread_group = EXCLUDED.thread_group,
                 batch_phase = EXCLUDED.batch_phase,
                 created_at = EXCLUDED.created_at,
                 dispatched_at = EXCLUDED.dispatched_at,
                 completed_at = EXCLUDED.completed_at",
        )
        .bind(&row.id)
        .bind(&row.run_id)
        .bind(&row.kanban_card_id)
        .bind(&row.agent_id)
        .bind(row.priority_rank)
        .bind(&row.reason)
        .bind(&row.status)
        .bind(row.retry_count)
        .bind(&row.dispatch_id)
        .bind(row.slot_index)
        .bind(row.thread_group)
        .bind(row.batch_phase)
        .bind(&row.created_at)
        .bind(&row.dispatched_at)
        .bind(&row.completed_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres auto_queue_entry {}: {e}", row.id))?;
    }
    summary.auto_queue_entries_upserted = snapshot.auto_queue_entries.len() as i64;

    for row in &snapshot.auto_queue_entry_transitions {
        sqlx::query(
            "INSERT INTO auto_queue_entry_transitions (id, entry_id, from_status, to_status, trigger_source, created_at)
             VALUES ($1, $2, $3, $4, $5, CAST($6 AS timestamptz))
             ON CONFLICT (id) DO UPDATE
             SET entry_id = EXCLUDED.entry_id,
                 from_status = EXCLUDED.from_status,
                 to_status = EXCLUDED.to_status,
                 trigger_source = EXCLUDED.trigger_source,
                 created_at = EXCLUDED.created_at",
        )
        .bind(row.id)
        .bind(&row.entry_id)
        .bind(&row.from_status)
        .bind(&row.to_status)
        .bind(&row.trigger_source)
        .bind(&row.created_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres auto_queue_entry_transition {}: {e}", row.id))?;
    }
    summary.auto_queue_entry_transitions_upserted =
        snapshot.auto_queue_entry_transitions.len() as i64;

    for row in &snapshot.auto_queue_slots {
        sqlx::query(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map, created_at, updated_at
             )
             VALUES ($1, $2, $3, $4, CAST($5 AS jsonb), CAST($6 AS timestamptz), CAST($7 AS timestamptz))
             ON CONFLICT (agent_id, slot_index) DO UPDATE
             SET assigned_run_id = EXCLUDED.assigned_run_id,
                 assigned_thread_group = EXCLUDED.assigned_thread_group,
                 thread_id_map = EXCLUDED.thread_id_map,
                 created_at = EXCLUDED.created_at,
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(&row.agent_id)
        .bind(row.slot_index)
        .bind(&row.assigned_run_id)
        .bind(row.assigned_thread_group)
        .bind(&row.thread_id_map)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            format!(
                "import postgres auto_queue_slot {}/{}: {e}",
                row.agent_id, row.slot_index
            )
        })?;
    }
    summary.auto_queue_slots_upserted = snapshot.auto_queue_slots.len() as i64;

    for row in &snapshot.auto_queue_entry_dispatch_history {
        sqlx::query(
            "INSERT INTO auto_queue_entry_dispatch_history (id, entry_id, dispatch_id, trigger_source, created_at)
             VALUES ($1, $2, $3, $4, CAST($5 AS timestamptz))
             ON CONFLICT (id) DO UPDATE
             SET entry_id = EXCLUDED.entry_id,
                 dispatch_id = EXCLUDED.dispatch_id,
                 trigger_source = EXCLUDED.trigger_source,
                 created_at = EXCLUDED.created_at",
        )
        .bind(row.id)
        .bind(&row.entry_id)
        .bind(&row.dispatch_id)
        .bind(&row.trigger_source)
        .bind(&row.created_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            format!(
                "import postgres auto_queue_entry_dispatch_history {}: {e}",
                row.id
            )
        })?;
    }
    summary.auto_queue_entry_dispatch_history_upserted =
        snapshot.auto_queue_entry_dispatch_history.len() as i64;

    for row in &snapshot.auto_queue_phase_gates {
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates (
                id, run_id, phase, status, verdict, dispatch_id, pass_verdict, next_phase, final_phase,
                anchor_card_id, failure_reason, created_at, updated_at
             )
             VALUES (
                $1, $2, COALESCE($3, 0), COALESCE($4, 'pending'), $5, $6, COALESCE($7, 'phase_gate_passed'),
                $8, COALESCE($9, FALSE), $10, $11, CAST($12 AS timestamptz), CAST($13 AS timestamptz)
             )
             ON CONFLICT (id) DO UPDATE
             SET run_id = EXCLUDED.run_id,
                 phase = EXCLUDED.phase,
                 status = EXCLUDED.status,
                 verdict = EXCLUDED.verdict,
                 dispatch_id = EXCLUDED.dispatch_id,
                 pass_verdict = EXCLUDED.pass_verdict,
                 next_phase = EXCLUDED.next_phase,
                 final_phase = EXCLUDED.final_phase,
                 anchor_card_id = EXCLUDED.anchor_card_id,
                 failure_reason = EXCLUDED.failure_reason,
                 created_at = EXCLUDED.created_at,
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(row.id)
        .bind(&row.run_id)
        .bind(row.phase)
        .bind(&row.status)
        .bind(&row.verdict)
        .bind(&row.dispatch_id)
        .bind(&row.pass_verdict)
        .bind(row.next_phase)
        .bind(row.final_phase)
        .bind(&row.anchor_card_id)
        .bind(&row.failure_reason)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres auto_queue_phase_gate {}: {e}", row.id))?;
    }
    summary.auto_queue_phase_gates_upserted = snapshot.auto_queue_phase_gates.len() as i64;

    summary.dispatch_events_upserted =
        upsert_dispatch_events_into_pg(&mut tx, &snapshot.dispatch_events).await?;

    for row in &snapshot.dispatch_queue {
        sqlx::query(
            "INSERT INTO dispatch_queue (id, kanban_card_id, priority_score, queued_at)
             VALUES ($1, $2, $3, CAST($4 AS timestamptz))
             ON CONFLICT (id) DO UPDATE
             SET kanban_card_id = EXCLUDED.kanban_card_id,
                 priority_score = EXCLUDED.priority_score,
                 queued_at = EXCLUDED.queued_at",
        )
        .bind(row.id)
        .bind(&row.kanban_card_id)
        .bind(row.priority_score)
        .bind(&row.queued_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres dispatch_queue {}: {e}", row.id))?;
    }
    summary.dispatch_queue_upserted = snapshot.dispatch_queue.len() as i64;

    for row in &snapshot.pr_tracking {
        sqlx::query(
            "INSERT INTO pr_tracking (
                card_id, repo_id, worktree_path, branch, pr_number, head_sha, state, last_error,
                dispatch_generation, review_round, retry_count, created_at, updated_at
             )
             VALUES (
                $1, $2, $3, $4, $5, $6, COALESCE($7, 'create-pr'), $8,
                COALESCE($9, ''), COALESCE($10, 0), COALESCE($11, 0), CAST($12 AS timestamptz), CAST($13 AS timestamptz)
             )
             ON CONFLICT (card_id) DO UPDATE
             SET repo_id = EXCLUDED.repo_id,
                 worktree_path = EXCLUDED.worktree_path,
                 branch = EXCLUDED.branch,
                 pr_number = EXCLUDED.pr_number,
                 head_sha = EXCLUDED.head_sha,
                 state = EXCLUDED.state,
                 last_error = EXCLUDED.last_error,
                 dispatch_generation = EXCLUDED.dispatch_generation,
                 review_round = EXCLUDED.review_round,
                 retry_count = EXCLUDED.retry_count,
                 created_at = EXCLUDED.created_at,
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(&row.card_id)
        .bind(&row.repo_id)
        .bind(&row.worktree_path)
        .bind(&row.branch)
        .bind(row.pr_number)
        .bind(&row.head_sha)
        .bind(&row.state)
        .bind(&row.last_error)
        .bind(&row.dispatch_generation)
        .bind(row.review_round)
        .bind(row.retry_count)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres pr_tracking {}: {e}", row.card_id))?;
    }
    summary.pr_tracking_upserted = snapshot.pr_tracking.len() as i64;

    for row in &snapshot.session_termination_events {
        sqlx::query(
            "INSERT INTO session_termination_events (
                id, session_key, dispatch_id, killer_component, reason_code, reason_text, probe_snapshot,
                last_offset, tmux_alive, created_at
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, CAST($10 AS timestamptz))
             ON CONFLICT (id) DO UPDATE
             SET session_key = EXCLUDED.session_key,
                 dispatch_id = EXCLUDED.dispatch_id,
                 killer_component = EXCLUDED.killer_component,
                 reason_code = EXCLUDED.reason_code,
                 reason_text = EXCLUDED.reason_text,
                 probe_snapshot = EXCLUDED.probe_snapshot,
                 last_offset = EXCLUDED.last_offset,
                 tmux_alive = EXCLUDED.tmux_alive,
                 created_at = EXCLUDED.created_at",
        )
        .bind(row.id)
        .bind(&row.session_key)
        .bind(&row.dispatch_id)
        .bind(&row.killer_component)
        .bind(&row.reason_code)
        .bind(&row.reason_text)
        .bind(&row.probe_snapshot)
        .bind(row.last_offset)
        .bind(row.tmux_alive)
        .bind(&row.created_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres session_termination_event {}: {e}", row.id))?;
    }
    summary.session_termination_events_upserted = snapshot.session_termination_events.len() as i64;

    summary.turns_upserted = upsert_turns_into_pg(&mut tx, &snapshot.turns).await?;

    for row in &snapshot.meetings {
        sqlx::query(
            "INSERT INTO meetings (
                id, channel_id, title, status, effective_rounds, started_at, completed_at, summary,
                thread_id, primary_provider, reviewer_provider, participant_names, selection_reason, created_at
             )
             VALUES (
                $1, $2, $3, $4, $5, CAST($6 AS timestamptz), CAST($7 AS timestamptz), $8,
                $9, $10, $11, $12, $13, $14
             )
             ON CONFLICT (id) DO UPDATE
             SET channel_id = EXCLUDED.channel_id,
                 title = EXCLUDED.title,
                 status = EXCLUDED.status,
                 effective_rounds = EXCLUDED.effective_rounds,
                 started_at = EXCLUDED.started_at,
                 completed_at = EXCLUDED.completed_at,
                 summary = EXCLUDED.summary,
                 thread_id = EXCLUDED.thread_id,
                 primary_provider = EXCLUDED.primary_provider,
                 reviewer_provider = EXCLUDED.reviewer_provider,
                 participant_names = EXCLUDED.participant_names,
                 selection_reason = EXCLUDED.selection_reason,
                 created_at = EXCLUDED.created_at",
        )
        .bind(&row.id)
        .bind(&row.channel_id)
        .bind(&row.title)
        .bind(&row.status)
        .bind(row.effective_rounds)
        .bind(&row.started_at)
        .bind(&row.completed_at)
        .bind(&row.summary)
        .bind(&row.thread_id)
        .bind(&row.primary_provider)
        .bind(&row.reviewer_provider)
        .bind(&row.participant_names)
        .bind(&row.selection_reason)
        .bind(row.created_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres meeting {}: {e}", row.id))?;
    }
    summary.meetings_upserted = snapshot.meetings.len() as i64;

    for row in &snapshot.meeting_transcripts {
        sqlx::query(
            "INSERT INTO meeting_transcripts (id, meeting_id, seq, round, speaker_agent_id, speaker_name, content, is_summary)
             VALUES ($1, $2, $3, $4, $5, $6, $7, COALESCE($8, FALSE))
             ON CONFLICT (id) DO UPDATE
             SET meeting_id = EXCLUDED.meeting_id,
                 seq = EXCLUDED.seq,
                 round = EXCLUDED.round,
                 speaker_agent_id = EXCLUDED.speaker_agent_id,
                 speaker_name = EXCLUDED.speaker_name,
                 content = EXCLUDED.content,
                 is_summary = EXCLUDED.is_summary",
        )
        .bind(row.id)
        .bind(&row.meeting_id)
        .bind(row.seq)
        .bind(row.round)
        .bind(&row.speaker_agent_id)
        .bind(&row.speaker_name)
        .bind(&row.content)
        .bind(row.is_summary)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres meeting_transcript {}: {e}", row.id))?;
    }
    summary.meeting_transcripts_upserted = snapshot.meeting_transcripts.len() as i64;

    for row in &snapshot.messages {
        sqlx::query(
            "INSERT INTO messages (id, sender_type, sender_id, receiver_type, receiver_id, content, message_type, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, COALESCE($7, 'chat'), CAST($8 AS timestamptz))
             ON CONFLICT (id) DO UPDATE
             SET sender_type = EXCLUDED.sender_type,
                 sender_id = EXCLUDED.sender_id,
                 receiver_type = EXCLUDED.receiver_type,
                 receiver_id = EXCLUDED.receiver_id,
                 content = EXCLUDED.content,
                 message_type = EXCLUDED.message_type,
                 created_at = EXCLUDED.created_at",
        )
        .bind(row.id)
        .bind(&row.sender_type)
        .bind(&row.sender_id)
        .bind(&row.receiver_type)
        .bind(&row.receiver_id)
        .bind(&row.content)
        .bind(&row.message_type)
        .bind(&row.created_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres message {}: {e}", row.id))?;
    }
    summary.messages_upserted = snapshot.messages.len() as i64;

    summary.message_outbox_upserted =
        upsert_message_outbox_into_pg(&mut tx, &snapshot.message_outbox).await?;

    for row in &snapshot.pending_dm_replies {
        sqlx::query(
            "INSERT INTO pending_dm_replies (
                id, source_agent, user_id, channel_id, context, status, created_at, consumed_at, expires_at
             )
             VALUES (
                $1, $2, $3, $4, CAST($5 AS jsonb), COALESCE($6, 'pending'),
                CAST($7 AS timestamptz), CAST($8 AS timestamptz), CAST($9 AS timestamptz)
             )
             ON CONFLICT (id) DO UPDATE
             SET source_agent = EXCLUDED.source_agent,
                 user_id = EXCLUDED.user_id,
                 channel_id = EXCLUDED.channel_id,
                 context = EXCLUDED.context,
                 status = EXCLUDED.status,
                 created_at = EXCLUDED.created_at,
                 consumed_at = EXCLUDED.consumed_at,
                 expires_at = EXCLUDED.expires_at",
        )
        .bind(row.id)
        .bind(&row.source_agent)
        .bind(&row.user_id)
        .bind(&row.channel_id)
        .bind(&row.context)
        .bind(&row.status)
        .bind(&row.created_at)
        .bind(&row.consumed_at)
        .bind(&row.expires_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres pending_dm_reply {}: {e}", row.id))?;
    }
    summary.pending_dm_replies_upserted = snapshot.pending_dm_replies.len() as i64;

    for row in &snapshot.review_decisions {
        sqlx::query(
            "INSERT INTO review_decisions (id, kanban_card_id, dispatch_id, item_index, decision, decided_at)
             VALUES ($1, $2, $3, $4, $5, CAST($6 AS timestamptz))
             ON CONFLICT (id) DO UPDATE
             SET kanban_card_id = EXCLUDED.kanban_card_id,
                 dispatch_id = EXCLUDED.dispatch_id,
                 item_index = EXCLUDED.item_index,
                 decision = EXCLUDED.decision,
                 decided_at = EXCLUDED.decided_at",
        )
        .bind(row.id)
        .bind(&row.kanban_card_id)
        .bind(&row.dispatch_id)
        .bind(row.item_index)
        .bind(&row.decision)
        .bind(&row.decided_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres review_decision {}: {e}", row.id))?;
    }
    summary.review_decisions_upserted = snapshot.review_decisions.len() as i64;

    for row in &snapshot.review_tuning_outcomes {
        sqlx::query(
            "INSERT INTO review_tuning_outcomes (
                id, card_id, dispatch_id, review_round, verdict, decision, outcome, finding_categories, created_at
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, CAST($9 AS timestamptz))
             ON CONFLICT (id) DO UPDATE
             SET card_id = EXCLUDED.card_id,
                 dispatch_id = EXCLUDED.dispatch_id,
                 review_round = EXCLUDED.review_round,
                 verdict = EXCLUDED.verdict,
                 decision = EXCLUDED.decision,
                 outcome = EXCLUDED.outcome,
                 finding_categories = EXCLUDED.finding_categories,
                 created_at = EXCLUDED.created_at",
        )
        .bind(row.id)
        .bind(&row.card_id)
        .bind(&row.dispatch_id)
        .bind(row.review_round)
        .bind(&row.verdict)
        .bind(&row.decision)
        .bind(&row.outcome)
        .bind(&row.finding_categories)
        .bind(&row.created_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres review_tuning_outcome {}: {e}", row.id))?;
    }
    summary.review_tuning_outcomes_upserted = snapshot.review_tuning_outcomes.len() as i64;

    for row in &snapshot.skills {
        sqlx::query(
            "INSERT INTO skills (id, name, description, source_path, trigger_patterns, updated_at)
             VALUES ($1, $2, $3, $4, $5, CAST($6 AS timestamptz))
             ON CONFLICT (id) DO UPDATE
             SET name = EXCLUDED.name,
                 description = EXCLUDED.description,
                 source_path = EXCLUDED.source_path,
                 trigger_patterns = EXCLUDED.trigger_patterns,
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(&row.id)
        .bind(&row.name)
        .bind(&row.description)
        .bind(&row.source_path)
        .bind(&row.trigger_patterns)
        .bind(&row.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres skill {}: {e}", row.id))?;
    }
    summary.skills_upserted = snapshot.skills.len() as i64;

    for row in &snapshot.skill_usage {
        sqlx::query(
            "INSERT INTO skill_usage (id, skill_id, agent_id, session_key, used_at)
             VALUES ($1, $2, $3, $4, CAST($5 AS timestamptz))
             ON CONFLICT (id) DO UPDATE
             SET skill_id = EXCLUDED.skill_id,
                 agent_id = EXCLUDED.agent_id,
                 session_key = EXCLUDED.session_key,
                 used_at = EXCLUDED.used_at",
        )
        .bind(row.id)
        .bind(&row.skill_id)
        .bind(&row.agent_id)
        .bind(&row.session_key)
        .bind(&row.used_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres skill_usage {}: {e}", row.id))?;
    }
    summary.skill_usage_upserted = snapshot.skill_usage.len() as i64;

    for row in &snapshot.pipeline_stages {
        sqlx::query(
            "INSERT INTO pipeline_stages (
                id, repo_id, stage_name, stage_order, trigger_after, entry_skill, timeout_minutes,
                on_failure, skip_condition, provider, agent_override_id, on_failure_target, max_retries, parallel_with
             )
             VALUES (
                $1, $2, $3, $4, $5, $6, COALESCE($7, 60),
                COALESCE($8, 'fail'), $9, $10, $11, $12, COALESCE($13, 0), $14
             )
             ON CONFLICT (id) DO UPDATE
             SET repo_id = EXCLUDED.repo_id,
                 stage_name = EXCLUDED.stage_name,
                 stage_order = EXCLUDED.stage_order,
                 trigger_after = EXCLUDED.trigger_after,
                 entry_skill = EXCLUDED.entry_skill,
                 timeout_minutes = EXCLUDED.timeout_minutes,
                 on_failure = EXCLUDED.on_failure,
                 skip_condition = EXCLUDED.skip_condition,
                 provider = EXCLUDED.provider,
                 agent_override_id = EXCLUDED.agent_override_id,
                 on_failure_target = EXCLUDED.on_failure_target,
                 max_retries = EXCLUDED.max_retries,
                 parallel_with = EXCLUDED.parallel_with",
        )
        .bind(row.id)
        .bind(&row.repo_id)
        .bind(&row.stage_name)
        .bind(row.stage_order)
        .bind(&row.trigger_after)
        .bind(&row.entry_skill)
        .bind(row.timeout_minutes)
        .bind(&row.on_failure)
        .bind(&row.skip_condition)
        .bind(&row.provider)
        .bind(&row.agent_override_id)
        .bind(&row.on_failure_target)
        .bind(row.max_retries)
        .bind(&row.parallel_with)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres pipeline_stage {}: {e}", row.id))?;
    }
    summary.pipeline_stages_upserted = snapshot.pipeline_stages.len() as i64;

    for row in &snapshot.runtime_decisions {
        sqlx::query(
            "INSERT INTO runtime_decisions (
                id, signal, evidence_json, chosen_action, actor, session_key, dispatch_id, created_at
             )
             VALUES ($1, $2, CAST($3 AS jsonb), $4, $5, $6, $7, CAST($8 AS timestamptz))
             ON CONFLICT (id) DO UPDATE
             SET signal = EXCLUDED.signal,
                 evidence_json = EXCLUDED.evidence_json,
                 chosen_action = EXCLUDED.chosen_action,
                 actor = EXCLUDED.actor,
                 session_key = EXCLUDED.session_key,
                 dispatch_id = EXCLUDED.dispatch_id,
                 created_at = EXCLUDED.created_at",
        )
        .bind(row.id)
        .bind(&row.signal)
        .bind(&row.evidence_json)
        .bind(&row.chosen_action)
        .bind(&row.actor)
        .bind(&row.session_key)
        .bind(&row.dispatch_id)
        .bind(&row.created_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres runtime_decision {}: {e}", row.id))?;
    }
    summary.runtime_decisions_upserted = snapshot.runtime_decisions.len() as i64;

    for row in &snapshot.kv_meta {
        sqlx::query(
            "INSERT INTO kv_meta (key, value, expires_at)
             VALUES ($1, $2, CAST($3 AS timestamptz))
             ON CONFLICT (key) DO UPDATE
             SET value = EXCLUDED.value,
                 expires_at = EXCLUDED.expires_at",
        )
        .bind(&row.key)
        .bind(&row.value)
        .bind(&row.expires_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres kv_meta {}: {e}", row.key))?;
    }
    summary.kv_meta_upserted = snapshot.kv_meta.len() as i64;

    for row in &snapshot.api_friction_events {
        sqlx::query(
            "INSERT INTO api_friction_events (
                id, fingerprint, endpoint, friction_type, summary, workaround, suggested_fix, docs_category,
                keywords_json, payload_json, session_key, channel_id, provider, dispatch_id, card_id, repo_id,
                github_issue_number, task_summary, agent_id, memory_backend, memory_status, memory_error, created_at
             )
             VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8,
                CAST($9 AS jsonb), CAST($10 AS jsonb), $11, $12, $13, $14, $15, $16,
                $17, $18, $19, $20, COALESCE($21, 'pending'), $22, CAST($23 AS timestamptz)
             )
             ON CONFLICT (id) DO UPDATE
             SET fingerprint = EXCLUDED.fingerprint,
                 endpoint = EXCLUDED.endpoint,
                 friction_type = EXCLUDED.friction_type,
                 summary = EXCLUDED.summary,
                 workaround = EXCLUDED.workaround,
                 suggested_fix = EXCLUDED.suggested_fix,
                 docs_category = EXCLUDED.docs_category,
                 keywords_json = EXCLUDED.keywords_json,
                 payload_json = EXCLUDED.payload_json,
                 session_key = EXCLUDED.session_key,
                 channel_id = EXCLUDED.channel_id,
                 provider = EXCLUDED.provider,
                 dispatch_id = EXCLUDED.dispatch_id,
                 card_id = EXCLUDED.card_id,
                 repo_id = EXCLUDED.repo_id,
                 github_issue_number = EXCLUDED.github_issue_number,
                 task_summary = EXCLUDED.task_summary,
                 agent_id = EXCLUDED.agent_id,
                 memory_backend = EXCLUDED.memory_backend,
                 memory_status = EXCLUDED.memory_status,
                 memory_error = EXCLUDED.memory_error,
                 created_at = EXCLUDED.created_at",
        )
        .bind(&row.id)
        .bind(&row.fingerprint)
        .bind(&row.endpoint)
        .bind(&row.friction_type)
        .bind(&row.summary)
        .bind(&row.workaround)
        .bind(&row.suggested_fix)
        .bind(&row.docs_category)
        .bind(&row.keywords_json)
        .bind(&row.payload_json)
        .bind(&row.session_key)
        .bind(&row.channel_id)
        .bind(&row.provider)
        .bind(&row.dispatch_id)
        .bind(&row.card_id)
        .bind(&row.repo_id)
        .bind(row.github_issue_number)
        .bind(&row.task_summary)
        .bind(&row.agent_id)
        .bind(&row.memory_backend)
        .bind(&row.memory_status)
        .bind(&row.memory_error)
        .bind(&row.created_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres api_friction_event {}: {e}", row.id))?;
    }
    summary.api_friction_events_upserted = snapshot.api_friction_events.len() as i64;

    for row in &snapshot.api_friction_issues {
        sqlx::query(
            "INSERT INTO api_friction_issues (
                fingerprint, repo_id, endpoint, friction_type, title, body, issue_number, issue_url,
                event_count, first_event_at, last_event_at, last_error, created_at, updated_at
             )
             VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8,
                COALESCE($9, 0), CAST($10 AS timestamptz), CAST($11 AS timestamptz), $12, CAST($13 AS timestamptz), CAST($14 AS timestamptz)
             )
             ON CONFLICT (fingerprint) DO UPDATE
             SET repo_id = EXCLUDED.repo_id,
                 endpoint = EXCLUDED.endpoint,
                 friction_type = EXCLUDED.friction_type,
                 title = EXCLUDED.title,
                 body = EXCLUDED.body,
                 issue_number = EXCLUDED.issue_number,
                 issue_url = EXCLUDED.issue_url,
                 event_count = EXCLUDED.event_count,
                 first_event_at = EXCLUDED.first_event_at,
                 last_event_at = EXCLUDED.last_event_at,
                 last_error = EXCLUDED.last_error,
                 created_at = EXCLUDED.created_at,
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(&row.fingerprint)
        .bind(&row.repo_id)
        .bind(&row.endpoint)
        .bind(&row.friction_type)
        .bind(&row.title)
        .bind(&row.body)
        .bind(row.issue_number)
        .bind(&row.issue_url)
        .bind(row.event_count)
        .bind(&row.first_event_at)
        .bind(&row.last_event_at)
        .bind(&row.last_error)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres api_friction_issue {}: {e}", row.fingerprint))?;
    }
    summary.api_friction_issues_upserted = snapshot.api_friction_issues.len() as i64;

    for row in &snapshot.memento_feedback_turn_stats {
        sqlx::query(
            "INSERT INTO memento_feedback_turn_stats (
                turn_id, stat_date, agent_id, provider, recall_count, manual_tool_feedback_count,
                manual_covered_recall_count, auto_tool_feedback_count, covered_recall_count, created_at
             )
             VALUES (
                $1, $2, $3, $4, COALESCE($5, 0), COALESCE($6, 0),
                COALESCE($7, 0), COALESCE($8, 0), COALESCE($9, 0), CAST($10 AS timestamptz)
             )
             ON CONFLICT (turn_id) DO UPDATE
             SET stat_date = EXCLUDED.stat_date,
                 agent_id = EXCLUDED.agent_id,
                 provider = EXCLUDED.provider,
                 recall_count = EXCLUDED.recall_count,
                 manual_tool_feedback_count = EXCLUDED.manual_tool_feedback_count,
                 manual_covered_recall_count = EXCLUDED.manual_covered_recall_count,
                 auto_tool_feedback_count = EXCLUDED.auto_tool_feedback_count,
                 covered_recall_count = EXCLUDED.covered_recall_count,
                 created_at = EXCLUDED.created_at",
        )
        .bind(&row.turn_id)
        .bind(&row.stat_date)
        .bind(&row.agent_id)
        .bind(&row.provider)
        .bind(row.recall_count)
        .bind(row.manual_tool_feedback_count)
        .bind(row.manual_covered_recall_count)
        .bind(row.auto_tool_feedback_count)
        .bind(row.covered_recall_count)
        .bind(&row.created_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            format!(
                "import postgres memento_feedback_turn_stats {}: {e}",
                row.turn_id
            )
        })?;
    }
    summary.memento_feedback_turn_stats_upserted =
        snapshot.memento_feedback_turn_stats.len() as i64;

    for row in &snapshot.rate_limit_cache {
        sqlx::query(
            "INSERT INTO rate_limit_cache (provider, data, fetched_at)
             VALUES ($1, $2, $3)
             ON CONFLICT (provider) DO UPDATE
             SET data = EXCLUDED.data,
                 fetched_at = EXCLUDED.fetched_at",
        )
        .bind(&row.provider)
        .bind(&row.data)
        .bind(row.fetched_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres rate_limit_cache {}: {e}", row.provider))?;
    }
    summary.rate_limit_cache_upserted = snapshot.rate_limit_cache.len() as i64;

    for row in &snapshot.deferred_hooks {
        sqlx::query(
            "INSERT INTO deferred_hooks (id, hook_name, payload, status, created_at)
             VALUES ($1, $2, $3, COALESCE($4, 'pending'), CAST($5 AS timestamptz))
             ON CONFLICT (id) DO UPDATE
             SET hook_name = EXCLUDED.hook_name,
                 payload = EXCLUDED.payload,
                 status = EXCLUDED.status,
                 created_at = EXCLUDED.created_at",
        )
        .bind(row.id)
        .bind(&row.hook_name)
        .bind(&row.payload)
        .bind(&row.status)
        .bind(&row.created_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres deferred_hook {}: {e}", row.id))?;
    }
    summary.deferred_hooks_upserted = snapshot.deferred_hooks.len() as i64;

    advance_pg_serial_sequences(&mut tx).await?;
    tx.commit()
        .await
        .map_err(|e| format!("commit postgres supporting-table transaction: {e}"))?;

    Ok(summary)
}

async fn import_full_state_into_pg(
    pool: &PgPool,
    snapshot: &SqliteCutoverSnapshot,
) -> Result<ImportSummary, String> {
    let mut summary = ImportSummary::default();
    snapshot.orphan_skips.apply_to_import_summary(&mut summary);
    merge_import_summaries(
        &mut summary,
        import_live_state_into_pg(
            pool,
            &snapshot.agents,
            &snapshot.kanban_cards,
            &snapshot.task_dispatches,
            &snapshot.sessions,
            &snapshot.dispatch_outbox,
        )
        .await?,
    );
    merge_import_summaries(
        &mut summary,
        import_supporting_tables_into_pg(pool, snapshot).await?,
    );
    merge_import_summaries(
        &mut summary,
        import_history_into_pg(pool, &snapshot.audit_logs, &snapshot.session_transcripts).await?,
    );
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::{
        AgentRow, AuditLogRow, DispatchOutboxRow, KanbanCardRow, PidFileSignal,
        PostgresCutoverArgs, RuntimeActiveStatus, SessionRow, SessionTranscriptRow,
        SqliteCutoverCounts, TaskDispatchRow, TcpSignal, advance_pg_serial_sequences,
        cutover_blocker, detect_runtime_active, import_full_state_into_pg, import_history_into_pg,
        import_live_state_into_pg, load_pg_cutover_counts, load_session_transcripts,
        load_sqlite_cutover_snapshot, orphan_skip_warnings, sqlite_cutover_counts,
        write_archive_files,
    };
    use libsql_rusqlite::Connection;
    use sqlx::{PgPool, Row};
    use std::path::Path;
    use tempfile::TempDir;

    fn idle_runtime_status() -> RuntimeActiveStatus {
        RuntimeActiveStatus {
            active: false,
            pid_file: Some(PidFileSignal {
                path: "/nonexistent/runtime/dcserver.pid".to_string(),
                exists: false,
                ..Default::default()
            }),
            tcp: Some(TcpSignal {
                host: "127.0.0.1".to_string(),
                port: 0,
                listening: false,
                error: None,
            }),
            overridden: false,
        }
    }

    fn active_runtime_status() -> RuntimeActiveStatus {
        RuntimeActiveStatus {
            active: true,
            pid_file: Some(PidFileSignal {
                path: "/tmp/runtime/dcserver.pid".to_string(),
                exists: true,
                pid: Some(std::process::id()),
                process_alive: true,
                error: None,
            }),
            tcp: Some(TcpSignal {
                host: "127.0.0.1".to_string(),
                port: 65535,
                listening: false,
                error: None,
            }),
            overridden: false,
        }
    }

    struct TestPostgresDb {
        admin_url: String,
        database_name: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let admin_url = admin_database_url();
            let database_name = format!("agentdesk_cutover_{}", uuid::Uuid::new_v4().simple());
            let admin_pool = PgPool::connect(&admin_url)
                .await
                .expect("connect postgres admin db");
            sqlx::query(&format!("CREATE DATABASE \"{database_name}\""))
                .execute(&admin_pool)
                .await
                .expect("create postgres test db");
            admin_pool.close().await;
            Self {
                admin_url,
                database_name,
            }
        }

        async fn connect_and_migrate(&self) -> PgPool {
            let pool = PgPool::connect(&database_url(&self.database_name))
                .await
                .expect("connect postgres test db");
            crate::db::postgres::migrate(&pool)
                .await
                .expect("migrate postgres test db");
            pool
        }

        async fn drop(self) {
            let admin_pool = PgPool::connect(&self.admin_url)
                .await
                .expect("reconnect postgres admin db");
            sqlx::query(
                "SELECT pg_terminate_backend(pid)
                 FROM pg_stat_activity
                 WHERE datname = $1
                   AND pid <> pg_backend_pid()",
            )
            .bind(&self.database_name)
            .execute(&admin_pool)
            .await
            .expect("terminate postgres test db sessions");
            sqlx::query(&format!(
                "DROP DATABASE IF EXISTS \"{}\"",
                self.database_name
            ))
            .execute(&admin_pool)
            .await
            .expect("drop postgres test db");
            admin_pool.close().await;
        }
    }

    fn database_url(db_name: &str) -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return format!("{}/{}", trimmed.trim_end_matches('/'), db_name);
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| {
                if Path::new("/private/tmp/.s.PGSQL.5432").exists() {
                    "/private/tmp".to_string()
                } else {
                    "localhost".to_string()
                }
            });
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        if host.starts_with('/') {
            let encoded_host = host.replace('/', "%2F");
            match password {
                Some(password) => format!(
                    "postgresql://{user}:{password}@localhost:{port}/{db_name}?host={encoded_host}"
                ),
                None => {
                    format!("postgresql://{user}@localhost:{port}/{db_name}?host={encoded_host}")
                }
            }
        } else {
            match password {
                Some(password) => format!("postgresql://{user}:{password}@{host}:{port}/{db_name}"),
                None => format!("postgresql://{user}@{host}:{port}/{db_name}"),
            }
        }
    }

    fn admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        database_url(&admin_db)
    }

    fn seed_full_cutover_fixture(conn: &Connection) {
        conn.execute_batch(
            "
            INSERT INTO offices (id, name, layout, name_ko, icon, color, description, sort_order, created_at)
            VALUES ('office-1', 'HQ', 'grid', '본부', ':office:', '#111111', 'Main office', 1, '2026-04-18 09:00:00');
            INSERT INTO departments (id, name, office_id, name_ko, icon, color, description, sort_order, created_at)
            VALUES ('dept-1', 'Platform', 'office-1', '플랫폼', ':gear:', '#222222', 'Platform dept', 1, '2026-04-18 09:00:00');
            INSERT INTO github_repos (id, display_name, sync_enabled, last_synced_at, default_agent_id, pipeline_config)
            VALUES ('repo-1', 'AgentDesk', 1, '2026-04-18 09:00:00', 'agent-1', '{\"pipeline\":\"full\"}');
            INSERT INTO agents (
                id, name, name_ko, department, provider, discord_channel_id, discord_channel_alt,
                discord_channel_cc, discord_channel_cdx, avatar_emoji, status, xp, skills,
                sprite_number, description, system_prompt, pipeline_config, created_at, updated_at
            )
            VALUES (
                'agent-1', 'AgentDesk', '에이전트데스크', 'platform', 'codex', 'chan-1', 'chan-2',
                'chan-1', 'chan-2', ':robot:', 'idle', 10, '[\"cutover\"]',
                7, 'Primary agent', 'System prompt', '{\"mode\":\"full\"}', '2026-04-18 09:00:00', '2026-04-18 09:00:01'
            );
            INSERT INTO office_agents (office_id, agent_id, department_id, joined_at)
            VALUES ('office-1', 'agent-1', 'dept-1', '2026-04-18 09:00:00');
            INSERT INTO kanban_cards (
                id, repo_id, title, status, priority, assigned_agent_id, github_issue_url, github_issue_number,
                latest_dispatch_id, review_round, metadata, started_at, completed_at, blocked_reason,
                pipeline_stage_id, review_notes, review_status, requested_at, owner_agent_id, requester_agent_id,
                parent_card_id, depth, sort_order, description, active_thread_id, channel_thread_map,
                suggestion_pending_at, review_entered_at, awaiting_dod_at, deferred_dod_json, created_at, updated_at
            )
            VALUES (
                'card-1', 'repo-1', 'Full cutover card', 'done', 'high', 'agent-1',
                'https://github.com/example/repo/issues/1', 1, 'dispatch-1', 2, '{\"k\":\"v\"}',
                '2026-04-18 09:10:00', '2026-04-18 09:20:00', NULL, 'stage-plan', 'Looks good', 'approved',
                '2026-04-18 09:05:00', 'agent-1', 'agent-1', NULL, 0, 1, 'Card description',
                'thread-1', '{\"primary\":\"thread-1\"}', NULL, '2026-04-18 09:12:00', NULL,
                '{\"dod\":\"queued\"}', '2026-04-18 09:00:00', '2026-04-18 09:20:00'
            );
            INSERT INTO task_dispatches (
                id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type, status, title, context, result,
                parent_dispatch_id, chain_depth, thread_id, retry_count, created_at, updated_at, completed_at
            )
            VALUES (
                'dispatch-1', 'card-1', 'agent-1', 'agent-1', 'implementation', 'completed',
                'Implement full cutover', '{\"cutover\":true}', '{\"ok\":true}', NULL, 0, 'thread-1', 1,
                '2026-04-18 09:10:00', '2026-04-18 09:20:00', '2026-04-18 09:20:00'
            );
            INSERT INTO dispatch_events (
                id, dispatch_id, kanban_card_id, dispatch_type, from_status, to_status, transition_source, payload_json, created_at
            )
            VALUES (
                1, 'dispatch-1', 'card-1', 'implementation', 'pending', 'completed', 'test', '{\"status\":\"ok\"}', '2026-04-18 09:20:00'
            );
            INSERT INTO dispatch_outbox (
                id, dispatch_id, action, agent_id, card_id, title, status, retry_count, next_attempt_at, created_at, processed_at, error
            )
            VALUES (
                1, 'dispatch-1', 'notify', 'agent-1', 'card-1', 'Notify', 'done', 0, NULL, '2026-04-18 09:10:00', '2026-04-18 09:20:00', NULL
            );
            INSERT INTO dispatch_queue (id, kanban_card_id, priority_score, queued_at)
            VALUES (1, 'card-1', 10.5, '2026-04-18 09:08:00');
            INSERT INTO kanban_audit_logs (id, card_id, from_status, to_status, source, result, created_at)
            VALUES (1, 'card-1', 'in_progress', 'done', 'test', 'ok', '2026-04-18 09:20:00');
            INSERT INTO card_retrospectives (
                id, card_id, dispatch_id, terminal_status, repo_id, issue_number, title, topic, content, review_round,
                review_notes, duration_seconds, success, result_json, memory_payload, sync_backend, sync_status, sync_error, created_at, updated_at
            )
            VALUES (
                'retro-1', 'card-1', 'dispatch-1', 'done', 'repo-1', 1, 'Retro title', 'retro', 'Retro content', 2,
                'Retro notes', 600, 1, '{\"result\":\"ok\"}', '{\"memory\":\"ok\"}', 'memento', 'synced', NULL,
                '2026-04-18 09:21:00', '2026-04-18 09:21:01'
            );
            INSERT INTO card_review_state (
                card_id, review_round, state, pending_dispatch_id, last_verdict, last_decision, decided_by, decided_at,
                approach_change_round, session_reset_round, review_entered_at, updated_at
            )
            VALUES (
                'card-1', 2, 'idle', 'dispatch-1', 'approve', 'ship', 'agent-1', '2026-04-18 09:20:00',
                1, 0, '2026-04-18 09:12:00', '2026-04-18 09:20:00'
            );
            INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, ai_model, ai_rationale, timeout_minutes, unified_thread,
                unified_thread_id, unified_thread_channel_id, max_concurrent_threads, thread_group_count, created_at, completed_at
            )
            VALUES (
                'run-1', 'repo-1', 'agent-1', 'completed', 'gpt-5', 'test', 120, 1,
                'thread-group-1', 'chan-1', 1, 1, '2026-04-18 09:00:00', '2026-04-18 09:20:00'
            );
            INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, priority_rank, reason, status, dispatch_id, retry_count,
                slot_index, thread_group, batch_phase, created_at, dispatched_at, completed_at
            )
            VALUES (
                'entry-1', 'run-1', 'card-1', 'agent-1', 1, 'test', 'completed', 'dispatch-1', 0,
                0, 0, 0, '2026-04-18 09:00:00', '2026-04-18 09:10:00', '2026-04-18 09:20:00'
            );
            INSERT INTO auto_queue_entry_transitions (id, entry_id, from_status, to_status, trigger_source, created_at)
            VALUES (1, 'entry-1', 'pending', 'completed', 'test', '2026-04-18 09:20:00');
            INSERT INTO auto_queue_entry_dispatch_history (id, entry_id, dispatch_id, trigger_source, created_at)
            VALUES (1, 'entry-1', 'dispatch-1', 'test', '2026-04-18 09:10:00');
            INSERT INTO auto_queue_phase_gates (
                id, run_id, phase, status, verdict, dispatch_id, pass_verdict, next_phase, final_phase,
                anchor_card_id, failure_reason, created_at, updated_at
            )
            VALUES (
                1, 'run-1', 1, 'passed', 'approve', 'dispatch-1', 'phase_gate_passed', 2, 1,
                'card-1', NULL, '2026-04-18 09:15:00', '2026-04-18 09:20:00'
            );
            INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map, created_at, updated_at
            )
            VALUES (
                'agent-1', 0, 'run-1', 1, '{\"0\":\"thread-1\"}', '2026-04-18 09:00:00', '2026-04-18 09:20:00'
            );
            INSERT INTO sessions (
                session_key, agent_id, provider, status, active_dispatch_id, model, session_info, tokens, cwd,
                last_heartbeat, thread_channel_id, claude_session_id, raw_provider_session_id, created_at
            )
            VALUES (
                'session-1', 'agent-1', 'codex', 'disconnected', 'dispatch-1', 'gpt-5-codex', '{\"session\":\"full\"}', 123,
                '/tmp/agentdesk', '2026-04-18 09:20:00', 'chan-1', NULL, 'provider-1', '2026-04-18 09:00:00'
            );
            INSERT INTO session_termination_events (
                id, session_key, dispatch_id, killer_component, reason_code, reason_text, probe_snapshot, last_offset, tmux_alive, created_at
            )
            VALUES (
                1, 'session-1', 'dispatch-1', 'test', 'finished', 'done', '{}', 10, 1, '2026-04-18 09:20:01'
            );
            INSERT INTO session_transcripts (
                turn_id, session_key, channel_id, agent_id, provider, dispatch_id, user_message, assistant_message, events_json, duration_ms, created_at
            )
            VALUES (
                'turn-1', 'session-1', 'chan-1', 'agent-1', 'codex', 'dispatch-1', 'hello', 'world', '[]', 1000, '2026-04-18 09:19:00'
            );
            INSERT INTO turns (
                turn_id, session_key, thread_id, thread_title, channel_id, agent_id, provider, session_id, dispatch_id,
                started_at, finished_at, duration_ms, input_tokens, cache_create_tokens, cache_read_tokens, output_tokens, created_at
            )
            VALUES (
                'turn-1', 'session-1', 'thread-1', 'Main thread', 'chan-1', 'agent-1', 'codex', 'provider-1', 'dispatch-1',
                '2026-04-18 09:18:00', '2026-04-18 09:19:00', 60000, 10, 0, 0, 20, '2026-04-18 09:19:00'
            );
            INSERT INTO meetings (
                id, channel_id, title, status, effective_rounds, started_at, completed_at, summary,
                thread_id, primary_provider, reviewer_provider, participant_names, selection_reason, created_at
            )
            VALUES (
                'meeting-1', 'chan-1', 'Daily sync', 'completed', 1, '2026-04-18 08:00:00', '2026-04-18 08:30:00', 'Summary',
                'thread-meeting-1', 'codex', 'claude', 'AgentDesk', 'test', 1713427200
            );
            INSERT INTO meeting_transcripts (id, meeting_id, seq, round, speaker_agent_id, speaker_name, content, is_summary)
            VALUES (1, 'meeting-1', 1, 1, 'agent-1', 'AgentDesk', 'Meeting content', 0);
            INSERT INTO messages (id, sender_type, sender_id, receiver_type, receiver_id, content, message_type, created_at)
            VALUES (1, 'agent', 'agent-1', 'user', 'user-1', 'Hello', 'chat', '2026-04-18 09:00:00');
            INSERT INTO message_outbox (
                id, target, content, bot, source, reason_code, session_key, status, created_at, sent_at, error, claimed_at, claim_owner
            )
            VALUES (
                1, 'chan-1', 'Outbound', 'announce', 'system', 'test', 'session-1', 'sent',
                '2026-04-18 09:00:00', '2026-04-18 09:01:00', NULL, '2026-04-18 09:00:30', 'worker-1'
            );
            INSERT INTO pending_dm_replies (id, source_agent, user_id, channel_id, context, status, created_at, consumed_at, expires_at)
            VALUES (1, 'agent-1', 'user-1', 'chan-1', '{\"pending\":true}', 'pending', '2026-04-18 09:00:00', NULL, '2026-04-19 09:00:00');
            INSERT INTO review_decisions (id, kanban_card_id, dispatch_id, item_index, decision, decided_at)
            VALUES (1, 'card-1', 'dispatch-1', 0, 'ship', '2026-04-18 09:20:00');
            INSERT INTO review_tuning_outcomes (id, card_id, dispatch_id, review_round, verdict, decision, outcome, finding_categories, created_at)
            VALUES (1, 'card-1', 'dispatch-1', 2, 'approve', 'ship', 'correct', '[]', '2026-04-18 09:20:00');
            INSERT INTO skills (id, name, description, source_path, trigger_patterns, updated_at)
            VALUES ('skill-1', 'cutover', 'Cutover skill', '/tmp/skill', 'cutover', '2026-04-18 09:00:00');
            INSERT INTO skill_usage (id, skill_id, agent_id, session_key, used_at)
            VALUES (1, 'skill-1', 'agent-1', 'session-1', '2026-04-18 09:19:00');
            INSERT INTO pipeline_stages (
                id, repo_id, stage_name, stage_order, trigger_after, entry_skill, timeout_minutes, on_failure,
                skip_condition, provider, agent_override_id, on_failure_target, max_retries, parallel_with
            )
            VALUES (
                1, 'repo-1', 'stage-plan', 1, NULL, 'skill-1', 60, 'fail',
                NULL, 'codex', 'agent-1', NULL, 0, NULL
            );
            INSERT INTO runtime_decisions (id, signal, evidence_json, chosen_action, actor, session_key, dispatch_id, created_at)
            VALUES (1, 'queue-full', '{\"load\":1}', 'pause', 'agent-1', 'session-1', 'dispatch-1', '2026-04-18 09:19:30');
            INSERT INTO api_friction_events (
                id, fingerprint, endpoint, friction_type, summary, workaround, suggested_fix, docs_category, keywords_json,
                payload_json, session_key, channel_id, provider, dispatch_id, card_id, repo_id, github_issue_number,
                task_summary, agent_id, memory_backend, memory_status, memory_error, created_at
            )
            VALUES (
                'afe-1', 'fp-1', '/api/test', 'timeout', 'Summary', 'Retry', 'Fix docs', 'api', '[]',
                '{\"payload\":true}', 'session-1', 'chan-1', 'codex', 'dispatch-1', 'card-1', 'repo-1', 1,
                'Task summary', 'agent-1', 'memento', 'done', NULL, '2026-04-18 09:19:45'
            );
            INSERT INTO api_friction_issues (
                fingerprint, repo_id, endpoint, friction_type, title, body, issue_number, issue_url,
                event_count, first_event_at, last_event_at, last_error, created_at, updated_at
            )
            VALUES (
                'fp-1', 'repo-1', '/api/test', 'timeout', 'Issue title', 'Issue body', 10, 'https://example.com/issues/10',
                1, '2026-04-18 09:19:45', '2026-04-18 09:19:45', NULL, '2026-04-18 09:19:45', '2026-04-18 09:19:45'
            );
            INSERT INTO memento_feedback_turn_stats (
                turn_id, stat_date, agent_id, provider, recall_count, manual_tool_feedback_count,
                manual_covered_recall_count, auto_tool_feedback_count, covered_recall_count, created_at
            )
            VALUES (
                'turn-1', '2026-04-18', 'agent-1', 'codex', 1, 1, 1, 0, 1, '2026-04-18 09:19:00'
            );
            INSERT INTO rate_limit_cache (provider, data, fetched_at)
            VALUES ('codex', '{\"limit\":100}', 1713428340);
            INSERT INTO deferred_hooks (id, hook_name, payload, status, created_at)
            VALUES (1, 'hook.test', '{\"hook\":true}', 'pending', '2026-04-18 09:00:00');
            INSERT INTO audit_logs (entity_type, entity_id, action, timestamp, actor)
            VALUES ('card', 'card-1', 'completed', '2026-04-18 09:20:00', 'agent-1');
            ",
        )
        .expect("seed full cutover fixture");
    }

    fn sqlite_table_count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("sqlite table count")
    }

    async fn pg_table_count_test(pool: &PgPool, table: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(pool)
            .await
            .expect("pg table count")
    }

    #[test]
    fn sqlite_cutover_counts_detects_live_state() {
        let conn = Connection::open_in_memory().expect("sqlite in memory");
        crate::db::schema::migrate(&conn).expect("sqlite migrate");
        conn.execute(
            "INSERT INTO task_dispatches (id, status) VALUES ('dispatch-cutover', 'pending')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (session_key, status) VALUES ('session-cutover', 'working')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO dispatch_outbox (dispatch_id, action, status) VALUES ('dispatch-cutover', 'notify', 'pending')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_outbox (target, content, bot, source, status) VALUES ('thread-cutover', 'hello', 'announce', 'system', 'pending')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_outbox (target, content, bot, source, status) VALUES ('thread-cutover', 'sent already', 'announce', 'system', 'sent')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_outbox (target, content, bot, source, status) VALUES ('thread-cutover', 'failed perm', 'announce', 'system', 'failed')",
            [],
        )
        .unwrap();

        let counts = sqlite_cutover_counts(&conn).expect("count sqlite cutover state");
        assert_eq!(counts.active_dispatches, 1);
        assert_eq!(counts.working_sessions, 1);
        assert_eq!(counts.open_dispatch_outbox, 1);
        assert_eq!(counts.pending_message_outbox, 1);
        assert!(counts.has_live_state());
    }

    #[test]
    fn sqlite_cutover_counts_treats_terminal_failed_dispatch_outbox_as_closed() {
        let conn = Connection::open_in_memory().expect("sqlite in memory");
        crate::db::schema::migrate(&conn).expect("sqlite migrate");
        conn.execute(
            "INSERT INTO dispatch_outbox (dispatch_id, action, status, processed_at, error)
             VALUES ('dispatch-terminal-failed', 'notify', 'failed', datetime('now'), 'permanent failure')",
            [],
        )
        .unwrap();

        let counts = sqlite_cutover_counts(&conn).expect("count sqlite cutover state");
        assert_eq!(counts.open_dispatch_outbox, 0);
        assert!(!counts.has_live_state());

        conn.execute(
            "INSERT INTO dispatch_outbox (dispatch_id, action, status)
             VALUES ('dispatch-replayable-pending', 'notify', 'pending')",
            [],
        )
        .unwrap();

        let counts = sqlite_cutover_counts(&conn).expect("recount sqlite cutover state");
        assert_eq!(counts.open_dispatch_outbox, 1);
        assert!(counts.has_live_state());
    }

    #[test]
    fn archive_only_cutover_blocks_when_live_state_exists() {
        let args = PostgresCutoverArgs {
            dry_run: false,
            archive_dir: Some("/tmp/cutover-archive".to_string()),
            skip_pg_import: true,
            allow_unsent_messages: false,
            allow_runtime_active: false,
        };
        let counts = SqliteCutoverCounts {
            active_dispatches: 1,
            ..Default::default()
        };

        let blocker = cutover_blocker(&args, &counts, Some(&idle_runtime_status()))
            .expect("live state blocker");
        assert!(blocker.contains("archive-only cutover would lose it"));
    }

    #[test]
    fn archive_only_cutover_allows_idle_sqlite() {
        let args = PostgresCutoverArgs {
            dry_run: false,
            archive_dir: Some("/tmp/cutover-archive".to_string()),
            skip_pg_import: true,
            allow_unsent_messages: false,
            allow_runtime_active: false,
        };

        assert!(
            cutover_blocker(
                &args,
                &SqliteCutoverCounts::default(),
                Some(&idle_runtime_status())
            )
            .is_none()
        );
    }

    #[test]
    fn archive_only_cutover_blocks_when_only_pending_messages_remain() {
        let args = PostgresCutoverArgs {
            dry_run: false,
            archive_dir: Some("/tmp/cutover-archive".to_string()),
            skip_pg_import: true,
            allow_unsent_messages: false,
            allow_runtime_active: false,
        };
        let counts = SqliteCutoverCounts {
            pending_message_outbox: 3,
            ..Default::default()
        };

        let blocker = cutover_blocker(&args, &counts, Some(&idle_runtime_status()))
            .expect("message_outbox blocker");
        assert!(blocker.contains("archive-only cutover would lose it"));
    }

    #[test]
    fn archive_only_cutover_still_blocks_pending_messages_even_with_override() {
        let args = PostgresCutoverArgs {
            dry_run: false,
            archive_dir: Some("/tmp/cutover-archive".to_string()),
            skip_pg_import: true,
            allow_unsent_messages: true,
            allow_runtime_active: false,
        };
        let counts = SqliteCutoverCounts {
            pending_message_outbox: 1,
            ..Default::default()
        };

        let blocker = cutover_blocker(&args, &counts, Some(&idle_runtime_status()))
            .expect("archive-only ignores unsent-message override");
        assert!(blocker.contains("archive-only cutover would lose it"));
    }

    #[test]
    fn archive_only_cutover_blocks_when_runtime_active_without_override() {
        let args = PostgresCutoverArgs {
            dry_run: false,
            archive_dir: Some("/tmp/cutover-archive".to_string()),
            skip_pg_import: true,
            allow_unsent_messages: false,
            allow_runtime_active: false,
        };

        let blocker = cutover_blocker(
            &args,
            &SqliteCutoverCounts::default(),
            Some(&active_runtime_status()),
        )
        .expect("runtime-active blocker");
        assert!(blocker.contains("dcserver runtime appears active"));
        assert!(blocker.contains("--allow-runtime-active"));
    }

    #[test]
    fn archive_only_cutover_allows_runtime_active_when_override_set() {
        let args = PostgresCutoverArgs {
            dry_run: false,
            archive_dir: Some("/tmp/cutover-archive".to_string()),
            skip_pg_import: true,
            allow_unsent_messages: false,
            allow_runtime_active: true,
        };
        let mut runtime = active_runtime_status();
        runtime.overridden = true;

        assert!(cutover_blocker(&args, &SqliteCutoverCounts::default(), Some(&runtime)).is_none());
    }

    #[test]
    fn pg_cutover_blocks_when_dispatch_outbox_is_not_drained() {
        let args = PostgresCutoverArgs {
            dry_run: false,
            archive_dir: None,
            skip_pg_import: false,
            allow_unsent_messages: false,
            allow_runtime_active: false,
        };
        let counts = SqliteCutoverCounts {
            open_dispatch_outbox: 1,
            ..Default::default()
        };

        let blocker = cutover_blocker(&args, &counts, None).expect("dispatch_outbox blocker");
        assert!(blocker.contains("drain outbox"));
    }

    #[tokio::test]
    async fn pg_cutover_counts_treat_terminal_failed_dispatch_outbox_as_closed() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO dispatch_outbox (dispatch_id, action, status, processed_at, error)
             VALUES ('dispatch-terminal-failed', 'notify', 'failed', NOW(), 'permanent failure')",
        )
        .execute(&pool)
        .await
        .expect("seed terminal failed dispatch_outbox");

        let counts = load_pg_cutover_counts(&pool)
            .await
            .expect("count postgres terminal failed outbox");
        assert_eq!(counts.open_dispatch_outbox, 0);

        sqlx::query(
            "INSERT INTO dispatch_outbox (dispatch_id, action, status)
             VALUES ('dispatch-replayable-pending', 'notify', 'pending')",
        )
        .execute(&pool)
        .await
        .expect("seed pending dispatch_outbox");

        let counts = load_pg_cutover_counts(&pool)
            .await
            .expect("count postgres pending outbox");
        assert_eq!(counts.open_dispatch_outbox, 1);

        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn pg_cutover_blocks_when_message_outbox_has_pending_rows() {
        let args = PostgresCutoverArgs {
            dry_run: false,
            archive_dir: None,
            skip_pg_import: false,
            allow_unsent_messages: false,
            allow_runtime_active: false,
        };
        let counts = SqliteCutoverCounts {
            pending_message_outbox: 4,
            ..Default::default()
        };

        let blocker = cutover_blocker(&args, &counts, None).expect("message_outbox blocker");
        assert!(blocker.contains("4 pending message_outbox row(s)"));
        assert!(blocker.contains("--allow-unsent-messages"));
    }

    #[test]
    fn pg_cutover_proceeds_when_operator_acknowledges_unsent_messages() {
        let args = PostgresCutoverArgs {
            dry_run: false,
            archive_dir: None,
            skip_pg_import: false,
            allow_unsent_messages: true,
            allow_runtime_active: false,
        };
        let counts = SqliteCutoverCounts {
            pending_message_outbox: 7,
            ..Default::default()
        };

        assert!(cutover_blocker(&args, &counts, None).is_none());
    }

    #[test]
    fn full_pg_cutover_ignores_runtime_active_signal() {
        let args = PostgresCutoverArgs {
            dry_run: false,
            archive_dir: None,
            skip_pg_import: false,
            allow_unsent_messages: false,
            allow_runtime_active: false,
        };

        assert!(
            cutover_blocker(
                &args,
                &SqliteCutoverCounts::default(),
                Some(&active_runtime_status())
            )
            .is_none()
        );
    }

    #[test]
    fn detect_runtime_active_flags_alive_pid_file() {
        let temp = TempDir::new().expect("tempdir");
        let runtime_dir = temp.path().join("runtime");
        std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
        std::fs::write(
            runtime_dir.join("dcserver.pid"),
            std::process::id().to_string(),
        )
        .expect("write pid file");

        let status = detect_runtime_active(Some(temp.path()), "127.0.0.1", 1, false);
        assert!(status.active);
        let pid = status.pid_file.as_ref().expect("pid signal");
        assert!(pid.exists);
        assert!(pid.process_alive);
        assert_eq!(pid.pid, Some(std::process::id()));
    }

    #[test]
    fn load_sqlite_cutover_snapshot_loads_full_state_rows() {
        let conn = Connection::open_in_memory().expect("sqlite in memory");
        crate::db::schema::migrate(&conn).expect("sqlite migrate");
        conn.execute(
            "INSERT INTO agents (id, name, provider, status) VALUES ('project-agentdesk', 'AgentDesk', 'codex', 'idle')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id) VALUES ('card-cutover', 'Cutover card', 'in_progress', 'project-agentdesk')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, from_agent_id, to_agent_id, status) VALUES ('dispatch-cutover', 'card-cutover', 'project-agentdesk', 'project-agentdesk', 'pending')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (session_key, agent_id, status, active_dispatch_id) VALUES ('session-cutover', 'project-agentdesk', 'working', 'dispatch-cutover')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO dispatch_outbox (dispatch_id, action, status, agent_id, card_id) VALUES ('dispatch-cutover', 'notify', 'pending', 'project-agentdesk', 'card-cutover')",
            [],
        )
        .unwrap();

        let snapshot = load_sqlite_cutover_snapshot(&conn, false, true).expect("sqlite snapshot");
        assert_eq!(snapshot.counts.active_dispatches, 1);
        assert_eq!(snapshot.counts.working_sessions, 1);
        assert_eq!(snapshot.counts.open_dispatch_outbox, 1);
        assert_eq!(snapshot.task_dispatches.len(), 1);
        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.dispatch_outbox.len(), 1);
        assert_eq!(snapshot.kanban_cards.len(), 1);
        assert_eq!(snapshot.agents.len(), 1);
        assert_eq!(snapshot.kanban_cards[0].id, "card-cutover");
        assert_eq!(snapshot.agents[0].id, "project-agentdesk");
    }

    #[test]
    fn load_sqlite_cutover_snapshot_normalizes_meeting_integer_timestamps() {
        let conn = Connection::open_in_memory().expect("sqlite in memory");
        crate::db::schema::migrate(&conn).expect("sqlite migrate");
        let started_at_ms = 1_760_000_000_123_i64;
        let completed_at_ms = 1_760_000_005_456_i64;
        conn.execute(
            "INSERT INTO meetings (
                id, channel_id, title, status, effective_rounds, started_at, completed_at, created_at
             ) VALUES (
                'meeting-cutover', 'channel-1', 'PG cutover meeting', 'completed', 2, ?1, ?2, ?1
             )",
            [started_at_ms, completed_at_ms],
        )
        .unwrap();

        let snapshot = load_sqlite_cutover_snapshot(&conn, false, true).expect("sqlite snapshot");

        assert_eq!(snapshot.meetings.len(), 1);
        assert_eq!(
            snapshot.meetings[0].started_at.as_deref(),
            super::unix_millis_to_rfc3339(started_at_ms).as_deref()
        );
        assert_eq!(
            snapshot.meetings[0].completed_at.as_deref(),
            super::unix_millis_to_rfc3339(completed_at_ms).as_deref()
        );
    }

    #[test]
    fn load_sqlite_cutover_snapshot_tolerates_missing_recent_tables() {
        let conn = Connection::open_in_memory().expect("sqlite in memory");
        crate::db::schema::migrate(&conn).expect("sqlite migrate");
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             DROP TABLE auto_queue_phase_gates;
             DROP TABLE auto_queue_entry_dispatch_history;
             DROP TABLE auto_queue_entry_transitions;
             DROP TABLE auto_queue_entries;
             DROP TABLE auto_queue_slots;
             DROP TABLE auto_queue_runs;
             PRAGMA foreign_keys = ON;",
        )
        .expect("drop recent auto_queue tables");

        let counts = sqlite_cutover_counts(&conn).expect("count older sqlite snapshot");
        assert_eq!(counts.auto_queue_runs, 0);
        assert_eq!(counts.auto_queue_entries, 0);
        assert_eq!(counts.auto_queue_entry_transitions, 0);
        assert_eq!(counts.auto_queue_entry_dispatch_history, 0);
        assert_eq!(counts.auto_queue_phase_gates, 0);
        assert_eq!(counts.auto_queue_slots, 0);

        let snapshot =
            load_sqlite_cutover_snapshot(&conn, true, true).expect("load older sqlite snapshot");
        assert!(snapshot.auto_queue_runs.is_empty());
        assert!(snapshot.auto_queue_entries.is_empty());
        assert!(snapshot.auto_queue_entry_transitions.is_empty());
        assert!(snapshot.auto_queue_entry_dispatch_history.is_empty());
        assert!(snapshot.auto_queue_phase_gates.is_empty());
        assert!(snapshot.auto_queue_slots.is_empty());
    }

    #[test]
    fn load_sqlite_cutover_snapshot_skips_orphan_rows_and_tracks_counts() {
        let conn = Connection::open_in_memory().expect("sqlite in memory");
        crate::db::schema::migrate(&conn).expect("sqlite migrate");
        seed_full_cutover_fixture(&conn);
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status)
             VALUES ('entry-orphan-run', 'run-missing', 'card-1', 'agent-1', 'pending');
             INSERT INTO auto_queue_entry_transitions (id, entry_id, from_status, to_status, trigger_source, created_at)
             VALUES (999, 'entry-missing', 'pending', 'failed', 'test', '2026-04-18 09:30:00');
             INSERT INTO auto_queue_entry_dispatch_history (id, entry_id, dispatch_id, trigger_source, created_at)
             VALUES (999, 'entry-missing', 'dispatch-1', 'test', '2026-04-18 09:30:01');
             INSERT INTO auto_queue_phase_gates (id, run_id, phase, status, dispatch_id, pass_verdict, final_phase, created_at, updated_at)
             VALUES (999, 'run-missing', 1, 'pending', 'dispatch-missing-phase', 'phase_gate_passed', 0, '2026-04-18 09:30:02', '2026-04-18 09:30:02');
             INSERT INTO auto_queue_slots (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map, created_at, updated_at)
             VALUES ('agent-1', 99, 'run-missing', 1, '{\"99\":\"thread-orphan\"}', '2026-04-18 09:30:03', '2026-04-18 09:30:03');
             INSERT INTO dispatch_events (id, dispatch_id, kanban_card_id, to_status, transition_source, created_at)
             VALUES (999, 'dispatch-missing', 'card-1', 'queued', 'test', '2026-04-18 09:30:04');
             INSERT INTO card_retrospectives (
                 id, card_id, dispatch_id, terminal_status, title, topic, content, result_json, memory_payload, created_at, updated_at
             )
             VALUES (
                 'retro-missing', 'card-1', 'dispatch-missing', 'done', 'Retro missing', 'retro', 'Missing dispatch', '{}', '{}',
                 '2026-04-18 09:30:05', '2026-04-18 09:30:05'
             );
             INSERT INTO card_review_state (card_id, state, updated_at)
             VALUES ('card-missing', 'idle', '2026-04-18 09:30:06');
             INSERT INTO pr_tracking (card_id, state, dispatch_generation, review_round, retry_count, created_at, updated_at)
             VALUES ('card-missing', 'create-pr', '', 0, 0, '2026-04-18 09:30:07', '2026-04-18 09:30:07');
             INSERT INTO session_termination_events (
                 id, session_key, killer_component, reason_code, created_at
             )
             VALUES (999, 'session-missing', 'test', 'missing_parent', '2026-04-18 09:30:08');
             INSERT INTO meeting_transcripts (id, meeting_id, seq, round, speaker_name, content, is_summary)
             VALUES (999, 'meeting-missing', 1, 1, 'AgentDesk', 'Orphan transcript', 0);
             PRAGMA foreign_keys = ON;",
        )
        .expect("insert orphan sqlite rows");

        let snapshot =
            load_sqlite_cutover_snapshot(&conn, true, true).expect("sqlite snapshot with orphans");

        assert_eq!(snapshot.orphan_skips.auto_queue_entries, 1);
        assert_eq!(snapshot.orphan_skips.auto_queue_entry_transitions, 1);
        assert_eq!(snapshot.orphan_skips.auto_queue_entry_dispatch_history, 1);
        assert_eq!(snapshot.orphan_skips.auto_queue_phase_gates, 1);
        assert_eq!(snapshot.orphan_skips.auto_queue_slots, 1);
        assert_eq!(snapshot.orphan_skips.dispatch_events, 1);
        assert_eq!(snapshot.orphan_skips.card_retrospectives, 1);
        assert_eq!(snapshot.orphan_skips.card_review_state, 1);
        assert_eq!(snapshot.orphan_skips.pr_tracking, 1);
        assert_eq!(snapshot.orphan_skips.session_termination_events, 1);
        assert_eq!(snapshot.orphan_skips.meeting_transcripts, 1);
        assert!(
            !snapshot
                .auto_queue_entries
                .iter()
                .any(|row| row.id == "entry-orphan-run")
        );
        assert!(
            !snapshot
                .auto_queue_entry_transitions
                .iter()
                .any(|row| row.id == 999)
        );
        assert!(
            !snapshot
                .auto_queue_entry_dispatch_history
                .iter()
                .any(|row| row.id == 999)
        );
        assert!(
            !snapshot
                .auto_queue_phase_gates
                .iter()
                .any(|row| row.id == 999)
        );
        assert!(
            !snapshot
                .auto_queue_slots
                .iter()
                .any(|row| row.slot_index == 99)
        );
        assert!(!snapshot.dispatch_events.iter().any(|row| row.id == 999));
        assert!(
            !snapshot
                .card_retrospectives
                .iter()
                .any(|row| row.id == "retro-missing")
        );
        assert!(
            !snapshot
                .card_review_state
                .iter()
                .any(|row| row.card_id == "card-missing")
        );
        assert!(
            !snapshot
                .pr_tracking
                .iter()
                .any(|row| row.card_id == "card-missing")
        );
        assert!(
            !snapshot
                .session_termination_events
                .iter()
                .any(|row| row.id == 999)
        );
        assert!(!snapshot.meeting_transcripts.iter().any(|row| row.id == 999));
    }

    #[tokio::test]
    async fn import_live_state_into_pg_copies_active_dispatches_sessions_and_outbox() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let agents = vec![AgentRow {
            id: "project-agentdesk".to_string(),
            name: "AgentDesk".to_string(),
            name_ko: Some("에이전트데스크".to_string()),
            department: Some("platform".to_string()),
            provider: Some("codex".to_string()),
            discord_channel_id: Some("123456789".to_string()),
            discord_channel_alt: Some("987654321".to_string()),
            discord_channel_cc: Some("123456789".to_string()),
            discord_channel_cdx: Some("987654321".to_string()),
            avatar_emoji: Some(":robot:".to_string()),
            status: Some("idle".to_string()),
            xp: Some(42),
            skills: Some("[\"postgres-cutover\"]".to_string()),
            sprite_number: Some(7),
            description: Some("AgentDesk coordinator".to_string()),
            system_prompt: Some("System prompt".to_string()),
            pipeline_config: Some("{\"mode\":\"full\"}".to_string()),
            created_at: Some("2026-04-18 09:59:59".to_string()),
            updated_at: Some("2026-04-18 10:00:00".to_string()),
        }];
        let cards = vec![KanbanCardRow {
            id: "card-cutover-live".to_string(),
            repo_id: Some("itismyfield/AgentDesk".to_string()),
            title: "Carry in-flight cutover card".to_string(),
            status: Some("in_progress".to_string()),
            priority: Some("high".to_string()),
            assigned_agent_id: Some("project-agentdesk".to_string()),
            github_issue_url: Some(
                "https://github.com/itismyfield/AgentDesk/issues/479".to_string(),
            ),
            github_issue_number: Some(479),
            latest_dispatch_id: Some("dispatch-cutover-live".to_string()),
            review_round: Some(0),
            metadata: Some("{\"cutover\":true}".to_string()),
            started_at: Some("2026-04-18 10:00:00".to_string()),
            completed_at: None,
            blocked_reason: None,
            pipeline_stage_id: Some("pg-cutover".to_string()),
            review_notes: None,
            review_status: None,
            requested_at: Some("2026-04-18 10:00:00".to_string()),
            owner_agent_id: Some("project-agentdesk".to_string()),
            requester_agent_id: Some("project-agentdesk".to_string()),
            parent_card_id: None,
            depth: Some(0),
            sort_order: Some(0),
            description: Some("Preserve the live card during PG cutover".to_string()),
            active_thread_id: Some("thread-123".to_string()),
            channel_thread_map: Some("{\"primary\":\"thread-123\"}".to_string()),
            suggestion_pending_at: None,
            review_entered_at: None,
            awaiting_dod_at: None,
            deferred_dod_json: None,
            created_at: Some("2026-04-18 10:00:00".to_string()),
            updated_at: Some("2026-04-18 10:00:01".to_string()),
        }];
        let dispatches = vec![TaskDispatchRow {
            id: "dispatch-cutover-live".to_string(),
            kanban_card_id: Some("card-cutover-live".to_string()),
            from_agent_id: Some("project-agentdesk".to_string()),
            to_agent_id: Some("project-agentdesk".to_string()),
            dispatch_type: Some("implementation".to_string()),
            status: Some("dispatched".to_string()),
            title: Some("Carry in-flight dispatch".to_string()),
            context: Some("{\"cutover\":true}".to_string()),
            result: None,
            parent_dispatch_id: None,
            chain_depth: Some(0),
            thread_id: Some("thread-123".to_string()),
            retry_count: Some(1),
            created_at: Some("2026-04-18 10:00:00".to_string()),
            updated_at: Some("2026-04-18 10:00:01".to_string()),
            completed_at: None,
        }];
        let sessions = vec![SessionRow {
            session_key: "codex/live-cutover".to_string(),
            agent_id: Some("project-agentdesk".to_string()),
            provider: Some("codex".to_string()),
            status: Some("working".to_string()),
            active_dispatch_id: Some("dispatch-cutover-live".to_string()),
            model: Some("gpt-5-codex".to_string()),
            session_info: Some("{\"source\":\"cutover\"}".to_string()),
            tokens: Some(321),
            cwd: Some("/tmp/agentdesk".to_string()),
            last_heartbeat: Some("2026-04-18 10:00:02".to_string()),
            thread_channel_id: Some("123456789".to_string()),
            claude_session_id: None,
            raw_provider_session_id: Some("provider-session-1".to_string()),
            created_at: Some("2026-04-18 10:00:00".to_string()),
        }];
        let outbox = vec![DispatchOutboxRow {
            id: 42,
            dispatch_id: "dispatch-cutover-live".to_string(),
            action: "notify".to_string(),
            agent_id: Some("project-agentdesk".to_string()),
            card_id: Some("card-cutover-live".to_string()),
            title: Some("Carry in-flight outbox".to_string()),
            status: "pending".to_string(),
            retry_count: Some(2),
            next_attempt_at: Some("2026-04-18 10:00:03".to_string()),
            created_at: Some("2026-04-18 10:00:00".to_string()),
            processed_at: None,
            error: None,
        }];

        let summary =
            import_live_state_into_pg(&pool, &agents, &cards, &dispatches, &sessions, &outbox)
                .await
                .expect("import live state");
        assert_eq!(summary.agents_upserted, 1);
        assert_eq!(summary.cards_upserted, 1);
        assert_eq!(summary.task_dispatches_upserted, 1);
        assert_eq!(summary.sessions_upserted, 1);
        assert_eq!(summary.dispatch_outbox_upserted, 1);

        let counts = load_pg_cutover_counts(&pool)
            .await
            .expect("pg cutover counts");
        assert_eq!(counts.active_dispatches, 1);
        assert_eq!(counts.working_sessions, 1);
        assert_eq!(counts.open_dispatch_outbox, 1);

        let session = sqlx::query(
            "SELECT status, active_dispatch_id, raw_provider_session_id
             FROM sessions
             WHERE session_key = $1",
        )
        .bind("codex/live-cutover")
        .fetch_one(&pool)
        .await
        .expect("load imported session");
        assert_eq!(session.get::<String, _>("status"), "working");
        assert_eq!(
            session
                .get::<Option<String>, _>("active_dispatch_id")
                .as_deref(),
            Some("dispatch-cutover-live")
        );
        assert_eq!(
            session
                .get::<Option<String>, _>("raw_provider_session_id")
                .as_deref(),
            Some("provider-session-1")
        );

        let card = sqlx::query(
            "SELECT status, assigned_agent_id, latest_dispatch_id
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind("card-cutover-live")
        .fetch_one(&pool)
        .await
        .expect("load imported card");
        assert_eq!(card.get::<String, _>("status"), "in_progress");
        assert_eq!(
            card.get::<Option<String>, _>("assigned_agent_id")
                .as_deref(),
            Some("project-agentdesk")
        );
        assert_eq!(
            card.get::<Option<String>, _>("latest_dispatch_id")
                .as_deref(),
            Some("dispatch-cutover-live")
        );

        let second =
            import_live_state_into_pg(&pool, &agents, &cards, &dispatches, &sessions, &outbox)
                .await
                .expect("re-import live state");
        assert_eq!(second.agents_upserted, 1);

        let outbox_row = sqlx::query(
            "SELECT status, retry_count
             FROM dispatch_outbox
             WHERE id = 42",
        )
        .fetch_one(&pool)
        .await
        .expect("load imported outbox");
        assert_eq!(outbox_row.get::<String, _>("status"), "pending");
        assert_eq!(outbox_row.get::<i64, _>("retry_count"), 2);

        let next_outbox_id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO dispatch_outbox (dispatch_id, action, status)
             VALUES ('dispatch-cutover-next', 'notify', 'pending')
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("insert next outbox row after sequence advance");
        assert_eq!(next_outbox_id, 43);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn advance_pg_serial_sequences_updates_all_bigserial_tables() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO message_outbox (id, target, content, bot, source, status)
             VALUES (41, 'thread-1', 'hello', 'announce', 'test', 'pending')",
        )
        .execute(&pool)
        .await
        .expect("seed message_outbox");

        let mut tx = pool.begin().await.expect("begin sequence advance tx");
        advance_pg_serial_sequences(&mut tx)
            .await
            .expect("advance all serial sequences");
        tx.commit().await.expect("commit sequence advance tx");

        let next_message_id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO message_outbox (target, content, bot, source)
             VALUES ('thread-2', 'world', 'announce', 'test')
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("insert next message_outbox row");
        assert_eq!(next_message_id, 42);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn postgres_cutover_schema_includes_pr_tracking_create_pr_support() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let columns = sqlx::query_scalar::<_, String>(
            "SELECT column_name
             FROM information_schema.columns
             WHERE table_schema = 'public'
               AND table_name = 'pr_tracking'
               AND column_name IN ('dispatch_generation', 'review_round', 'retry_count')
             ORDER BY column_name",
        )
        .fetch_all(&pool)
        .await
        .expect("load pr_tracking columns");
        assert_eq!(
            columns,
            vec![
                "dispatch_generation".to_string(),
                "retry_count".to_string(),
                "review_round".to_string(),
            ]
        );

        let has_index = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                SELECT 1
                FROM pg_indexes
                WHERE schemaname = 'public'
                  AND tablename = 'task_dispatches'
                  AND indexname = 'idx_single_active_create_pr'
             )",
        )
        .fetch_one(&pool)
        .await
        .expect("check create-pr partial index");
        assert!(has_index);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn import_history_into_pg_is_idempotent() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let audit_logs = vec![AuditLogRow {
            entity_type: Some("card".to_string()),
            entity_id: Some("card-1".to_string()),
            action: Some("advance".to_string()),
            timestamp: Some("2026-04-18 10:00:00".to_string()),
            actor: Some("project-agentdesk".to_string()),
        }];
        let session_transcripts = vec![SessionTranscriptRow {
            turn_id: "discord:cutover:1".to_string(),
            session_key: Some("session-cutover".to_string()),
            channel_id: Some("123456".to_string()),
            agent_id: Some("project-agentdesk".to_string()),
            provider: Some("codex".to_string()),
            dispatch_id: None,
            user_message: "hello".to_string(),
            assistant_message: "world".to_string(),
            events_json: "[]".to_string(),
            duration_ms: Some(1234),
            created_at: Some("2026-04-18 10:01:02".to_string()),
        }];

        let first = import_history_into_pg(&pool, &audit_logs, &session_transcripts)
            .await
            .expect("first import");
        let second = import_history_into_pg(&pool, &audit_logs, &session_transcripts)
            .await
            .expect("second import");

        assert_eq!(first.audit_logs_inserted, 1);
        assert_eq!(first.session_transcripts_upserted, 1);
        assert_eq!(second.audit_logs_inserted, 0);
        assert_eq!(second.session_transcripts_upserted, 1);

        let counts = load_pg_cutover_counts(&pool).await.expect("pg counts");
        assert_eq!(counts.audit_logs, 1);
        assert_eq!(counts.session_transcripts, 1);

        let transcript = sqlx::query(
            "SELECT user_message, assistant_message, duration_ms
             FROM session_transcripts
             WHERE turn_id = $1",
        )
        .bind("discord:cutover:1")
        .fetch_one(&pool)
        .await
        .expect("load imported transcript");
        assert_eq!(transcript.get::<String, _>("user_message"), "hello");
        assert_eq!(transcript.get::<String, _>("assistant_message"), "world");
        assert_eq!(transcript.get::<Option<i32>, _>("duration_ms"), Some(1234));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn import_full_state_into_pg_copies_all_tables() {
        let conn = Connection::open_in_memory().expect("sqlite in memory");
        crate::db::schema::migrate(&conn).expect("sqlite migrate");
        seed_full_cutover_fixture(&conn);
        let snapshot =
            load_sqlite_cutover_snapshot(&conn, true, true).expect("full sqlite snapshot");

        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let summary = import_full_state_into_pg(&pool, &snapshot)
            .await
            .expect("import full state");

        let tables = [
            "offices",
            "departments",
            "office_agents",
            "github_repos",
            "agents",
            "kanban_cards",
            "kanban_audit_logs",
            "auto_queue_runs",
            "auto_queue_entries",
            "auto_queue_entry_transitions",
            "auto_queue_entry_dispatch_history",
            "auto_queue_phase_gates",
            "auto_queue_slots",
            "task_dispatches",
            "dispatch_events",
            "dispatch_queue",
            "card_retrospectives",
            "card_review_state",
            "review_decisions",
            "review_tuning_outcomes",
            "messages",
            "message_outbox",
            "meetings",
            "meeting_transcripts",
            "pending_dm_replies",
            "pipeline_stages",
            "pr_tracking",
            "skills",
            "skill_usage",
            "runtime_decisions",
            "session_termination_events",
            "sessions",
            "session_transcripts",
            "turns",
            "kv_meta",
            "api_friction_events",
            "api_friction_issues",
            "memento_feedback_turn_stats",
            "rate_limit_cache",
            "deferred_hooks",
            "audit_logs",
            "dispatch_outbox",
        ];

        for table in tables {
            assert_eq!(
                pg_table_count_test(&pool, table).await,
                sqlite_table_count(&conn, table),
                "table count mismatch for {table}"
            );
        }

        assert_eq!(
            summary.cards_upserted,
            sqlite_table_count(&conn, "kanban_cards")
        );
        assert_eq!(
            summary.kanban_audit_logs_upserted,
            sqlite_table_count(&conn, "kanban_audit_logs")
        );
        assert_eq!(
            summary.dispatch_events_upserted,
            sqlite_table_count(&conn, "dispatch_events")
        );
        assert_eq!(
            summary.message_outbox_upserted,
            sqlite_table_count(&conn, "message_outbox")
        );
        assert_eq!(summary.turns_upserted, sqlite_table_count(&conn, "turns"));
        assert_eq!(
            summary.audit_logs_inserted,
            sqlite_table_count(&conn, "audit_logs")
        );
        assert_eq!(
            summary.session_transcripts_upserted,
            sqlite_table_count(&conn, "session_transcripts")
        );
        assert_eq!(summary.auto_queue_entries_skipped_orphans, 0);
        assert_eq!(summary.auto_queue_entry_transitions_skipped_orphans, 0);
        assert_eq!(summary.auto_queue_entry_dispatch_history_skipped_orphans, 0);
        assert_eq!(summary.auto_queue_phase_gates_skipped_orphans, 0);
        assert_eq!(summary.auto_queue_slots_skipped_orphans, 0);
        assert_eq!(summary.dispatch_events_skipped_orphans, 0);
        assert_eq!(summary.card_retrospectives_skipped_orphans, 0);
        assert_eq!(summary.card_review_state_skipped_orphans, 0);
        assert_eq!(summary.pr_tracking_skipped_orphans, 0);
        assert_eq!(summary.session_termination_events_skipped_orphans, 0);
        assert_eq!(summary.meeting_transcripts_skipped_orphans, 0);

        let message_outbox = sqlx::query(
            "SELECT reason_code, session_key
             FROM message_outbox
             WHERE id = 1",
        )
        .fetch_one(&pool)
        .await
        .expect("load imported message_outbox row");
        assert_eq!(
            message_outbox
                .get::<Option<String>, _>("reason_code")
                .as_deref(),
            Some("test")
        );
        assert_eq!(
            message_outbox
                .get::<Option<String>, _>("session_key")
                .as_deref(),
            Some("session-1")
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn import_full_state_into_pg_reports_skipped_orphans() {
        let conn = Connection::open_in_memory().expect("sqlite in memory");
        crate::db::schema::migrate(&conn).expect("sqlite migrate");
        seed_full_cutover_fixture(&conn);
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             INSERT INTO auto_queue_entry_transitions (id, entry_id, from_status, to_status, trigger_source, created_at)
             VALUES (999, 'entry-missing', 'pending', 'failed', 'test', '2026-04-18 09:30:00');
             INSERT INTO auto_queue_entry_dispatch_history (id, entry_id, dispatch_id, trigger_source, created_at)
             VALUES (999, 'entry-missing', 'dispatch-1', 'test', '2026-04-18 09:30:01');
             PRAGMA foreign_keys = ON;",
        )
        .expect("insert orphan auto_queue rows");
        let snapshot =
            load_sqlite_cutover_snapshot(&conn, true, true).expect("full sqlite snapshot");

        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let summary = import_full_state_into_pg(&pool, &snapshot)
            .await
            .expect("import full state with orphan skips");
        assert_eq!(summary.auto_queue_entry_transitions_skipped_orphans, 1);
        assert_eq!(summary.auto_queue_entry_dispatch_history_skipped_orphans, 1);
        assert_eq!(
            pg_table_count_test(&pool, "auto_queue_entry_transitions").await,
            sqlite_table_count(&conn, "auto_queue_entry_transitions") - 1
        );
        assert_eq!(
            pg_table_count_test(&pool, "auto_queue_entry_dispatch_history").await,
            sqlite_table_count(&conn, "auto_queue_entry_dispatch_history") - 1
        );

        let warnings = orphan_skip_warnings(&summary);
        assert!(
            warnings
                .iter()
                .any(|line| line.contains("auto_queue_entry_transitions"))
        );
        assert!(
            warnings
                .iter()
                .any(|line| line.contains("auto_queue_entry_dispatch_history"))
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn import_full_state_into_pg_is_idempotent() {
        let conn = Connection::open_in_memory().expect("sqlite in memory");
        crate::db::schema::migrate(&conn).expect("sqlite migrate");
        seed_full_cutover_fixture(&conn);
        let snapshot =
            load_sqlite_cutover_snapshot(&conn, true, true).expect("full sqlite snapshot");

        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        import_full_state_into_pg(&pool, &snapshot)
            .await
            .expect("first full import");
        let tracked_tables = [
            "offices",
            "departments",
            "office_agents",
            "github_repos",
            "agents",
            "kanban_cards",
            "kanban_audit_logs",
            "auto_queue_runs",
            "auto_queue_entries",
            "auto_queue_entry_transitions",
            "auto_queue_entry_dispatch_history",
            "auto_queue_phase_gates",
            "auto_queue_slots",
            "task_dispatches",
            "dispatch_events",
            "dispatch_queue",
            "card_retrospectives",
            "card_review_state",
            "review_decisions",
            "review_tuning_outcomes",
            "messages",
            "message_outbox",
            "meetings",
            "meeting_transcripts",
            "pending_dm_replies",
            "pipeline_stages",
            "pr_tracking",
            "skills",
            "skill_usage",
            "runtime_decisions",
            "session_termination_events",
            "sessions",
            "session_transcripts",
            "turns",
            "kv_meta",
            "api_friction_events",
            "api_friction_issues",
            "memento_feedback_turn_stats",
            "rate_limit_cache",
            "deferred_hooks",
            "audit_logs",
            "dispatch_outbox",
        ];
        let mut first_counts = Vec::new();
        for table in tracked_tables {
            first_counts.push((table, pg_table_count_test(&pool, table).await));
        }

        import_full_state_into_pg(&pool, &snapshot)
            .await
            .expect("second full import");

        for (table, first_count) in first_counts {
            let pg_count = pg_table_count_test(&pool, table).await;
            assert_eq!(
                pg_count, first_count,
                "pg count changed after rerun for {table}"
            );
            assert_eq!(
                pg_count,
                sqlite_table_count(&conn, table),
                "sqlite/pg mismatch after rerun for {table}"
            );
        }

        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn write_archive_files_emits_jsonl_pairs() {
        let temp_dir = TempDir::new().expect("tempdir");
        let output = write_archive_files(
            temp_dir.path().to_str().unwrap(),
            &[AuditLogRow {
                entity_type: Some("card".to_string()),
                entity_id: Some("card-1".to_string()),
                action: Some("advance".to_string()),
                timestamp: Some("2026-04-18 10:00:00".to_string()),
                actor: Some("tester".to_string()),
            }],
            &[SessionTranscriptRow {
                turn_id: "discord:test:1".to_string(),
                session_key: None,
                channel_id: None,
                agent_id: None,
                provider: Some("codex".to_string()),
                dispatch_id: None,
                user_message: "hello".to_string(),
                assistant_message: "world".to_string(),
                events_json: "[]".to_string(),
                duration_ms: None,
                created_at: Some("2026-04-18 10:01:02".to_string()),
            }],
        )
        .expect("write archive files");

        assert!(Path::new(output.audit_logs_file.as_deref().unwrap()).exists());
        assert!(Path::new(output.session_transcripts_file.as_deref().unwrap()).exists());
    }

    #[test]
    fn postgres_cutover_args_default_to_pg_import() {
        let args = PostgresCutoverArgs {
            dry_run: true,
            archive_dir: None,
            skip_pg_import: false,
            allow_unsent_messages: false,
            allow_runtime_active: false,
        };
        assert!(args.dry_run);
        assert!(!args.skip_pg_import);
    }

    #[test]
    fn load_session_transcripts_handles_null_messages() {
        let conn = Connection::open_in_memory().expect("sqlite in memory");
        conn.execute_batch(
            "CREATE TABLE session_transcripts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                turn_id TEXT NOT NULL,
                session_key TEXT,
                channel_id TEXT,
                agent_id TEXT,
                provider TEXT,
                dispatch_id TEXT,
                user_message TEXT,
                assistant_message TEXT,
                events_json TEXT,
                duration_ms INTEGER,
                created_at TEXT
            );",
        )
        .expect("create legacy-compatible session_transcripts table");
        conn.execute(
            "INSERT INTO session_transcripts (
                turn_id, session_key, channel_id, provider, user_message, assistant_message, events_json
             ) VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5)",
            libsql_rusqlite::params!["discord:null:1", "session-null", "123", "codex", "[]"],
        )
        .unwrap();

        let rows = load_session_transcripts(&conn).expect("load session transcripts");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].user_message, "");
        assert_eq!(rows[0].assistant_message, "");
    }
}
