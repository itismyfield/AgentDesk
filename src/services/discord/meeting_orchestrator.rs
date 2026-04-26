use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{
    AutoArchiveDuration, ChannelId, ChannelType,
    builder::{CreateThread, EditThread},
};

use crate::services::memory::{RecallRequest, RecallResponse, build_resolved_memory_backend};
use crate::services::provider::ProviderKind;
use crate::services::provider_exec;

use super::agentdesk_config;
use super::formatting::send_long_message_raw;
use super::meeting_artifact_store::{MeetingArtifactKind, MeetingArtifactRepo, StoreOutcome};
use super::meeting_state_machine::{self as msm, MeetingEvent, MeetingState};
use super::org_schema;
use super::outbound::{
    DeliveryResult, DiscordOutboundClient, DiscordOutboundMessage, DiscordOutboundPolicy,
    OutboundDeduper, deliver_outbound, outbound_fingerprint,
};
use super::role_map::load_meeting_config as load_meeting_config_from_role_map;
use super::settings::{ResolvedMemorySettings, RoleBinding, load_role_prompt};
use super::{DispatchProfile, SharedData, rate_limit_wait};
use crate::server::routes::dispatches::discord_delivery::{
    DispatchMessagePostError, DispatchMessagePostErrorKind,
};

// ─── Data Structures ─────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub(super) struct MeetingParticipant {
    pub role_id: String,
    pub prompt_file: String,
    pub display_name: String,
    pub provider: Option<ProviderKind>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub workspace: Option<String>,
    pub peer_agents_enabled: bool,
    pub memory: ResolvedMemorySettings,
}

#[derive(Clone, Debug)]
pub(super) struct MeetingUtterance {
    pub role_id: String,
    pub display_name: String,
    pub round: u32,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum MeetingStatus {
    SelectingParticipants,
    InProgress,
    Concluding,
    Completed,
    Cancelled,
}

impl MeetingStatus {
    /// Mapping from the legacy `MeetingStatus` to the new state-machine state
    /// (#1008). Kept additive so the orchestrator's existing `.status` field
    /// remains the source of truth while call sites migrate to the reducer.
    pub(super) fn to_state(&self) -> MeetingState {
        match self {
            MeetingStatus::SelectingParticipants => MeetingState::Starting,
            MeetingStatus::InProgress => MeetingState::Running,
            MeetingStatus::Concluding => MeetingState::Summarizing,
            MeetingStatus::Completed => MeetingState::Completed,
            MeetingStatus::Cancelled => MeetingState::Cancelled,
        }
    }
}

/// Process-wide idempotent artifact repository for meetings (#1008).
///
/// Shared across `/meeting` Discord commands and `/api/meetings/*` HTTP
/// routes so that retries from either surface collapse onto the same
/// idempotency-key store.
pub(super) fn meeting_artifact_repo() -> &'static MeetingArtifactRepo {
    static REPO: std::sync::OnceLock<MeetingArtifactRepo> = std::sync::OnceLock::new();
    REPO.get_or_init(MeetingArtifactRepo::new)
}

/// Record a state-machine transition for a meeting and (best-effort) log any
/// rejected invalid transitions. This is the additive seam where the existing
/// orchestrator hands off to the reducer without yet rewriting the
/// `MeetingStatus` field.
pub(super) fn record_meeting_transition(
    meeting_id: &str,
    from: MeetingState,
    event: MeetingEvent,
) -> Option<MeetingState> {
    match msm::transition_idempotent_terminal(from, event) {
        Ok(next) => {
            tracing::debug!(
                meeting_id = %meeting_id,
                from = %from,
                event = ?event,
                to = %next,
                "[meeting] state transition"
            );
            Some(next)
        }
        Err(err) => {
            tracing::warn!(
                meeting_id = %meeting_id,
                error = %err,
                "[meeting] invalid state transition rejected"
            );
            None
        }
    }
}

/// Record a cancellation artifact keyed idempotently by meeting id so two
/// concurrent cancels produce only one artifact row.
pub(super) fn record_cancel_artifact(meeting_id: &str, reason: &str) -> StoreOutcome {
    meeting_artifact_repo().store_with_key(
        meeting_id,
        MeetingArtifactKind::Other("cancel_marker".to_string()),
        "cancel",
        reason,
    )
}

pub(super) struct Meeting {
    pub id: String,
    pub channel_id: u64,
    pub agenda: String,
    pub primary_provider: ProviderKind,
    pub reviewer_provider: ProviderKind,
    pub selection_reason: Option<String>,
    pub participants: Vec<MeetingParticipant>,
    pub transcript: Vec<MeetingUtterance>,
    pub current_round: u32,
    pub max_rounds: u32,
    pub status: MeetingStatus,
    /// Final summary produced by the summary agent
    pub summary: Option<String>,
    /// Meeting start timestamp (RFC 3339)
    pub started_at: String,
    /// Discord thread ID for meeting context isolation
    pub thread_id: Option<u64>,
    /// Channel to send meeting messages (thread_id if available, else parent channel)
    pub msg_channel: Option<u64>,
}

/// Rule for dynamic summary agent selection based on agenda keywords.
#[derive(Clone, Debug)]
pub(super) struct SummaryAgentRule {
    pub keywords: Vec<String>,
    pub agent: String,
}

/// Summary agent config: either a static agent or rule-based dynamic selection.
#[derive(Clone, Debug)]
pub(super) enum SummaryAgentConfig {
    Static(String),
    Dynamic {
        rules: Vec<SummaryAgentRule>,
        default: String,
    },
}

impl SummaryAgentConfig {
    /// Resolve which agent should write the summary based on the agenda.
    pub fn resolve(&self, agenda: &str) -> String {
        match self {
            Self::Static(agent) => agent.clone(),
            Self::Dynamic { rules, default } => {
                let agenda_lower = agenda.to_lowercase();
                for rule in rules {
                    if rule
                        .keywords
                        .iter()
                        .any(|kw| agenda_lower.contains(&kw.to_lowercase()))
                    {
                        return rule.agent.clone();
                    }
                }
                default.clone()
            }
        }
    }
}

/// Meeting configuration from role_map.json "meeting" section
#[derive(Clone, Debug)]
pub(super) struct MeetingConfig {
    pub channel_name: String,
    pub max_rounds: u32,
    pub max_participants: usize,
    pub summary_agent: SummaryAgentConfig,
    pub available_agents: Vec<MeetingAgentConfig>,
}

#[derive(Clone, Debug)]
pub(super) struct MeetingAgentConfig {
    pub role_id: String,
    pub display_name: String,
    pub keywords: Vec<String>,
    pub prompt_file: String,
    pub domain_summary: Option<String>,
    pub strengths: Vec<String>,
    pub task_types: Vec<String>,
    pub anti_signals: Vec<String>,
    pub provider_hint: Option<String>,
    pub provider: Option<ProviderKind>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub workspace: Option<String>,
    pub peer_agents_enabled: bool,
    pub memory: ResolvedMemorySettings,
}

impl MeetingAgentConfig {
    fn to_participant(&self) -> MeetingParticipant {
        MeetingParticipant {
            role_id: self.role_id.clone(),
            prompt_file: self.prompt_file.clone(),
            display_name: self.display_name.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            workspace: self.workspace.clone(),
            peer_agents_enabled: self.peer_agents_enabled,
            memory: self.memory.clone(),
        }
    }
}

impl MeetingParticipant {
    fn role_binding(&self) -> RoleBinding {
        RoleBinding {
            role_id: self.role_id.clone(),
            prompt_file: self.prompt_file.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            peer_agents_enabled: self.peer_agents_enabled,
            quality_feedback_injection_enabled: true,
            memory: self.memory.clone(),
        }
    }
}

const DEFAULT_MAX_PARTICIPANTS: usize = 5;
const MIN_MEETING_PARTICIPANTS: usize = 2;
const DEFAULT_MEETING_SELECTION_STAGE_TIMEOUT_SECS: u64 = 90;
const MIN_MEETING_STAGE_TIMEOUT_SECS: u64 = 30;
const MAX_MEETING_STAGE_TIMEOUT_SECS: u64 = 300;
const MEETING_TURN_STAGE_TIMEOUT_SECS: u64 = 90;
const MEETING_SUMMARY_STAGE_TIMEOUT_SECS: u64 = 120;

type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MeetingStartRequest {
    pub primary_provider: ProviderKind,
    pub agenda: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActiveMeetingSlot {
    Active,
    Cancelled,
    MissingOrReplaced,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParticipantSelectionDecision {
    selected_role_ids: Vec<String>,
    selection_reason: Option<String>,
}

/// Generate a unique meeting ID (timestamp + random hex)
fn generate_meeting_id() -> String {
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let random: u32 = rand::Rng::r#gen(&mut rand::thread_rng());
    format!("mtg-{}-{:08x}", ts, random)
}

fn short_query_hash(input: &str) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(input.as_bytes());
    hex::encode(&digest[..6])
}

fn meeting_query_hash(meeting_id: &str) -> String {
    format!(
        "#meeting-{}",
        short_query_hash(&format!("meeting:{meeting_id}"))
    )
}

fn thread_query_hash(thread_id: &str) -> String {
    format!(
        "#thread-{}",
        short_query_hash(&format!("thread:{thread_id}"))
    )
}

fn display_query_hash(hash: &str) -> String {
    hash.strip_prefix("#meeting-")
        .or_else(|| hash.strip_prefix("#thread-"))
        .map(|value| format!("#{value}"))
        .unwrap_or_else(|| hash.to_string())
}

fn compact_selection_reason(reason: &str) -> Option<String> {
    let compact = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut trimmed = compact.trim();

    for prefix in [
        "선정 사유:",
        "selection_reason:",
        "selection reason:",
        "reason:",
        "rationale:",
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            trimmed = rest.trim();
            break;
        }
    }

    trimmed = trimmed.trim_matches(|ch: char| matches!(ch, '"' | '\'' | '`' | '“' | '”'));
    trimmed = trimmed
        .trim_start_matches(|ch: char| matches!(ch, '-' | '*' | '•' | '1'..='9' | '.' | ')'));
    trimmed = trimmed.trim();

    if trimmed.is_empty() {
        return None;
    }

    Some(trimmed.to_string())
}

fn normalize_selection_reason(reason: &str) -> Option<String> {
    compact_selection_reason(reason)
}

fn build_meeting_start_status_message(
    agenda: &str,
    meeting_hash_display: &str,
    thread_hash_display: Option<&str>,
    primary_provider: &ProviderKind,
    reviewer_provider: &ProviderKind,
    selection_reason: Option<&str>,
) -> String {
    let thread_hash_line = thread_hash_display
        .map(|hash| format!("\n스레드 해시: {hash}"))
        .unwrap_or_default();
    let selection_reason_line = selection_reason
        .and_then(normalize_selection_reason)
        .map(|reason| format!("\n선정 사유: {reason}"))
        .unwrap_or_default();

    format!(
        "📋 **라운드 테이블 회의 시작**\n안건: {}\n회의 해시: {}{}\n진행 프로바이더: {} / 리뷰 프로바이더: {}\n참여자 선정 중...{}",
        agenda,
        meeting_hash_display,
        thread_hash_line,
        primary_provider.display_name(),
        reviewer_provider.display_name(),
        selection_reason_line
    )
}

fn clamp_max_participants(max_participants: usize) -> usize {
    max_participants.clamp(MIN_MEETING_PARTICIPANTS, DEFAULT_MAX_PARTICIPANTS)
}

fn csv_or_missing(values: &[String]) -> String {
    if values.is_empty() {
        "metadata_missing".to_string()
    } else {
        values.join(", ")
    }
}

fn agent_metadata_card(agent: &MeetingAgentConfig) -> String {
    let mut missing = Vec::new();
    if agent.keywords.is_empty() {
        missing.push("keywords");
    }
    if agent
        .domain_summary
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        missing.push("domain_summary");
    }
    if agent.strengths.is_empty() {
        missing.push("strengths");
    }
    if agent.task_types.is_empty() {
        missing.push("task_types");
    }
    if agent.anti_signals.is_empty() {
        missing.push("anti_signals");
    }
    if agent
        .provider_hint
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        missing.push("provider_hint");
    }

    let provider = agent
        .provider
        .as_ref()
        .map(ProviderKind::display_name)
        .or_else(|| {
            agent.provider_hint.as_deref().map(|value| {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    "metadata_missing"
                } else {
                    trimmed
                }
            })
        })
        .unwrap_or("metadata_missing");
    let model = agent
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("metadata_missing");
    let reasoning_effort = agent
        .reasoning_effort
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("metadata_missing");
    let selection_profile = format!(
        "{} | provider={} | strengths={} | task_types={}",
        agent.display_name,
        provider,
        csv_or_missing(&agent.strengths),
        csv_or_missing(&agent.task_types),
    );

    format!(
        r#"- role_id: {role_id}
  display_name: {display_name}
  selection_profile: {selection_profile}
  keywords: {keywords}
  domain_summary: {domain_summary}
  strengths: {strengths}
  task_types: {task_types}
  anti_signals: {anti_signals}
  provider: {provider}
  provider_hint: {provider_hint}
  model: {model}
  reasoning_effort: {reasoning_effort}
  metadata_missing: {metadata_missing}"#,
        role_id = agent.role_id,
        display_name = agent.display_name,
        selection_profile = selection_profile,
        keywords = csv_or_missing(&agent.keywords),
        domain_summary = agent
            .domain_summary
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("metadata_missing"),
        strengths = csv_or_missing(&agent.strengths),
        task_types = csv_or_missing(&agent.task_types),
        anti_signals = csv_or_missing(&agent.anti_signals),
        provider = provider,
        provider_hint = agent
            .provider_hint
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("metadata_missing"),
        model = model,
        reasoning_effort = reasoning_effort,
        metadata_missing = if missing.is_empty() {
            "[]".to_string()
        } else {
            format!("[{}]", missing.join(", "))
        },
    )
}

fn summary_agent_context(config: &MeetingConfig, resolved_summary_agent: &str) -> String {
    let Some(agent) = config
        .available_agents
        .iter()
        .find(|a| a.role_id == resolved_summary_agent)
    else {
        return format!(
            "summary_agent `{}` is not in the meeting candidate pool. Keep this summary persona as a fallback and do not replace it with a participant persona.",
            resolved_summary_agent
        );
    };

    if agent.prompt_file.trim().is_empty() {
        return format!(
            "summary_agent `{}` has no prompt file. Keep the `{}` summary persona and produce a neutral meeting record.",
            resolved_summary_agent, agent.display_name
        );
    }

    load_role_prompt(&RoleBinding {
        role_id: resolved_summary_agent.to_string(),
        prompt_file: agent.prompt_file.clone(),
        provider: agent.provider.clone(),
        model: agent.model.clone(),
        reasoning_effort: agent.reasoning_effort.clone(),
        peer_agents_enabled: agent.peer_agents_enabled,
        quality_feedback_injection_enabled: true,
        memory: agent.memory.clone(),
    })
    .unwrap_or_else(|| {
        format!(
            "summary_agent `{}` prompt could not be loaded. Keep the summary persona and produce a neutral meeting record.",
            resolved_summary_agent
        )
    })
}

pub(crate) fn list_available_agent_options() -> Vec<serde_json::Value> {
    load_meeting_config()
        .map(|config| {
            config
                .available_agents
                .iter()
                .map(|agent| {
                    serde_json::json!({
                        "role_id": agent.role_id.clone(),
                        "display_name": agent.display_name.clone(),
                        "keywords": agent.keywords.clone(),
                        "domain_summary": agent.domain_summary.clone(),
                        "strengths": agent.strengths.clone(),
                        "task_types": agent.task_types.clone(),
                        "anti_signals": agent.anti_signals.clone(),
                        "provider": agent.provider.as_ref().map(ProviderKind::display_name),
                        "provider_hint": agent.provider_hint.clone(),
                        "model": agent.model.clone(),
                        "reasoning_effort": agent.reasoning_effort.clone(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn execute_provider_stage(
    provider: ProviderKind,
    stage_label: &str,
    prompt: String,
    timeout_secs: u64,
) -> Result<String, String> {
    provider_exec::execute_simple_with_timeout(
        provider,
        prompt,
        std::time::Duration::from_secs(timeout_secs),
        stage_label.to_string(),
    )
    .await
    .map(|text| text.trim().to_string())
}

fn resolve_meeting_stage_timeout_secs(raw: Option<&str>, default_secs: u64) -> u64 {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .map(|value| {
            value.clamp(
                MIN_MEETING_STAGE_TIMEOUT_SECS,
                MAX_MEETING_STAGE_TIMEOUT_SECS,
            )
        })
        .unwrap_or(default_secs)
}

fn meeting_selection_stage_timeout_secs() -> u64 {
    resolve_meeting_stage_timeout_secs(
        std::env::var("AGENTDESK_MEETING_SELECTION_TIMEOUT_SECS")
            .ok()
            .as_deref(),
        DEFAULT_MEETING_SELECTION_STAGE_TIMEOUT_SECS,
    )
}

/// Create a Discord thread (without a parent message) for a meeting.
/// Returns the thread's ChannelId on success, or None on failure.
async fn create_meeting_thread(
    http: &serenity::Http,
    parent_channel_id: ChannelId,
    thread_name: &str,
) -> Option<ChannelId> {
    match parent_channel_id
        .create_thread(
            http,
            CreateThread::new(thread_name)
                .kind(ChannelType::PublicThread)
                .auto_archive_duration(AutoArchiveDuration::OneDay),
        )
        .await
    {
        Ok(thread) => Some(thread.id),
        Err(error) => {
            tracing::warn!("[meeting] Thread creation failed: {error}");
            None
        }
    }
}

/// Archive a meeting thread (set archived=true via Discord REST API).
async fn archive_meeting_thread(http: &serenity::Http, thread_channel_id: ChannelId) {
    match thread_channel_id
        .edit_thread(http, EditThread::new().archived(true))
        .await
    {
        Ok(_) => tracing::info!("[meeting] Archived thread {thread_channel_id}"),
        Err(error) => {
            tracing::warn!("[meeting] Failed to archive thread {thread_channel_id}: {error}")
        }
    }
}

struct MeetingOutboundClient<'a> {
    http: &'a serenity::Http,
    shared: &'a Arc<SharedData>,
}

fn meeting_deduper() -> &'static OutboundDeduper {
    static DEDUPER: std::sync::OnceLock<OutboundDeduper> = std::sync::OnceLock::new();
    DEDUPER.get_or_init(OutboundDeduper::new)
}

impl DiscordOutboundClient for MeetingOutboundClient<'_> {
    async fn post_message(
        &self,
        target_channel: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError> {
        let channel_id = parse_meeting_channel_id(target_channel)?;
        rate_limit_wait(self.shared, channel_id).await;
        channel_id
            .send_message(self.http, serenity::CreateMessage::new().content(content))
            .await
            .map(|message| message.id.get().to_string())
            .map_err(meeting_post_error)
    }

    async fn edit_message(
        &self,
        target_channel: &str,
        message_id: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError> {
        let channel_id = parse_meeting_channel_id(target_channel)?;
        let message_id = message_id
            .parse::<u64>()
            .map(serenity::MessageId::new)
            .map_err(|error| {
                DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    format!("invalid meeting message id {message_id}: {error}"),
                )
            })?;
        rate_limit_wait(self.shared, channel_id).await;
        channel_id
            .edit_message(
                self.http,
                message_id,
                serenity::EditMessage::new().content(content),
            )
            .await
            .map(|message| message.id.get().to_string())
            .map_err(meeting_post_error)
    }
}

fn parse_meeting_channel_id(raw: &str) -> Result<ChannelId, DispatchMessagePostError> {
    raw.parse::<u64>().map(ChannelId::new).map_err(|error| {
        DispatchMessagePostError::new(
            DispatchMessagePostErrorKind::Other,
            format!("invalid meeting channel id {raw}: {error}"),
        )
    })
}

fn meeting_post_error(error: serenity::Error) -> DispatchMessagePostError {
    let detail = error.to_string();
    let lowered = detail.to_ascii_lowercase();
    let kind = if detail.contains("BASE_TYPE_MAX_LENGTH")
        || lowered.contains("2000 or fewer in length")
        || lowered.contains("length")
    {
        DispatchMessagePostErrorKind::MessageTooLong
    } else {
        DispatchMessagePostErrorKind::Other
    };
    DispatchMessagePostError::new(kind, detail)
}

pub(in crate::services::discord) async fn send_meeting_message(
    http: &serenity::Http,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    content: impl Into<String>,
) -> Result<Option<serenity::MessageId>, String> {
    let content = content.into();
    let event_key = format!("send:{}", uuid::Uuid::new_v4());
    let message = meeting_outbound_message(channel_id, content, &event_key);
    deliver_meeting_message(http, shared, message).await
}

async fn send_meeting_message_with_event(
    http: &serenity::Http,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    event_key: impl AsRef<str>,
    content: impl Into<String>,
) -> Result<Option<serenity::MessageId>, String> {
    let message = meeting_outbound_message(channel_id, content.into(), event_key.as_ref());
    deliver_meeting_message(http, shared, message).await
}

async fn deliver_meeting_message(
    http: &serenity::Http,
    shared: &Arc<SharedData>,
    message: DiscordOutboundMessage,
) -> Result<Option<serenity::MessageId>, String> {
    meeting_delivery_result(
        deliver_outbound(
            &MeetingOutboundClient { http, shared },
            meeting_deduper(),
            message,
            DiscordOutboundPolicy::preserve_inline_content(),
        )
        .await,
    )
}

fn meeting_outbound_message(
    channel_id: ChannelId,
    content: String,
    event_key: &str,
) -> DiscordOutboundMessage {
    let content_hash = outbound_fingerprint(&[&content]);
    DiscordOutboundMessage::new(channel_id.get().to_string(), content).with_correlation(
        format!("meeting:{}", channel_id.get()),
        format!(
            "meeting:{}:{}:{content_hash}",
            channel_id.get(),
            normalize_meeting_event_key(event_key)
        ),
    )
}

fn normalize_meeting_event_key(value: &str) -> String {
    let normalized: String = value
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, ':' | '_' | '-' | '.') {
                ch
            } else {
                '_'
            }
        })
        .take(160)
        .collect();
    if normalized.is_empty() {
        "event".to_string()
    } else {
        normalized
    }
}

async fn edit_meeting_message(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: serenity::MessageId,
    shared: &Arc<SharedData>,
    content: impl Into<String>,
) -> Result<(), String> {
    let content = content.into();
    let message =
        meeting_outbound_message(channel_id, content, &format!("edit:{}", message_id.get()))
            .with_edit_message_id(message_id.get().to_string());
    meeting_delivery_result(
        deliver_outbound(
            &MeetingOutboundClient { http, shared },
            meeting_deduper(),
            message,
            DiscordOutboundPolicy::preserve_inline_content(),
        )
        .await,
    )
    .map(|_| ())
}

fn meeting_delivery_result(result: DeliveryResult) -> Result<Option<serenity::MessageId>, String> {
    match result {
        DeliveryResult::Success { message_id } | DeliveryResult::Fallback { message_id, .. } => {
            parse_meeting_message_id(&message_id).map(Some)
        }
        DeliveryResult::Duplicate { message_id } => message_id
            .as_deref()
            .map(parse_meeting_message_id)
            .transpose(),
        DeliveryResult::Skipped { reason } => {
            tracing::info!(?reason, "[meeting] outbound delivery skipped");
            Ok(None)
        }
        DeliveryResult::PermanentFailure { detail } => Err(detail),
    }
}

fn parse_meeting_message_id(message_id: &str) -> Result<serenity::MessageId, String> {
    message_id
        .parse::<u64>()
        .map(serenity::MessageId::new)
        .map_err(|error| format!("invalid meeting delivery message id {message_id}: {error}"))
}

fn parse_json_array_fragment(text: &str) -> Result<Vec<String>, String> {
    let trimmed = text.trim();
    let json_str = if let Some(start) = trimmed.find('[') {
        if let Some(end) = trimmed.rfind(']') {
            &trimmed[start..=end]
        } else {
            return Err("Invalid JSON array response".to_string());
        }
    } else {
        return Err("No JSON array found".to_string());
    };

    serde_json::from_str(json_str).map_err(|e| format!("Failed to parse JSON array: {}", e))
}

fn parse_json_object_fragment(text: &str) -> Result<serde_json::Value, String> {
    let trimmed = text.trim();
    let json_str = if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            &trimmed[start..=end]
        } else {
            return Err("Invalid JSON object response".to_string());
        }
    } else {
        return Err("No JSON object found".to_string());
    };

    serde_json::from_str(json_str).map_err(|e| format!("Failed to parse JSON object: {}", e))
}

fn parse_string_array_field(value: &serde_json::Value, key: &str) -> Option<Vec<String>> {
    value.get(key).and_then(|field| {
        field.as_array().map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
    })
}

fn parse_participant_selection_response(
    text: &str,
) -> Result<ParticipantSelectionDecision, String> {
    if let Ok(value) = parse_json_object_fragment(text) {
        let selected_role_ids = [
            "selected_role_ids",
            "role_ids",
            "selected_roles",
            "selected_participants",
            "participants",
        ]
        .iter()
        .find_map(|key| parse_string_array_field(&value, key));

        if let Some(selected_role_ids) = selected_role_ids {
            let selection_reason = ["selection_reason", "reason", "rationale"]
                .iter()
                .find_map(|key| value.get(key).and_then(|field| field.as_str()))
                .and_then(compact_selection_reason);

            return Ok(ParticipantSelectionDecision {
                selected_role_ids,
                selection_reason,
            });
        }
    }

    Ok(ParticipantSelectionDecision {
        selected_role_ids: parse_json_array_fragment(text)?,
        selection_reason: None,
    })
}

fn compact_selection_signal(agent: &MeetingAgentConfig) -> Option<String> {
    let first_non_empty = |values: &[String]| {
        values
            .iter()
            .map(|value| value.trim())
            .find(|value| !value.is_empty())
            .map(str::to_string)
    };

    let truncate = |value: String| {
        let mut chars = value.chars();
        let compact: String = chars.by_ref().take(24).collect();
        if chars.next().is_some() {
            format!("{compact}…")
        } else {
            compact
        }
    };

    first_non_empty(&agent.task_types)
        .or_else(|| first_non_empty(&agent.strengths))
        .or_else(|| first_non_empty(&agent.keywords))
        .or_else(|| {
            agent.domain_summary.as_deref().and_then(|summary| {
                summary
                    .split(|ch| ['.', '\n', ',', ';'].contains(&ch))
                    .map(str::trim)
                    .find(|segment| !segment.is_empty())
                    .map(str::to_string)
            })
        })
        .map(truncate)
}

fn tokenize_selection_reason_text(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            if current.chars().count() >= 2 {
                tokens.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
    }

    if current.chars().count() >= 2 {
        tokens.push(current);
    }

    tokens
}

fn compact_reason_fragment(value: &str) -> String {
    let trimmed = value.trim();
    let mut chars = trimmed.chars();
    let compact: String = chars.by_ref().take(24).collect();
    if chars.next().is_some() {
        format!("{compact}…")
    } else {
        compact
    }
}

fn selection_signal_candidates(agent: &MeetingAgentConfig) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();

    let mut push_value = |value: &str| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return;
        }
        let key = trimmed.to_lowercase();
        if seen.insert(key) {
            candidates.push(trimmed.to_string());
        }
    };

    for value in &agent.task_types {
        push_value(value);
    }
    for value in &agent.strengths {
        push_value(value);
    }
    for value in &agent.keywords {
        push_value(value);
    }
    if let Some(summary) = agent.domain_summary.as_deref() {
        for fragment in summary.split(|ch| ['.', '\n', ',', ';', '·', '/', '|'].contains(&ch)) {
            push_value(fragment);
        }
    }

    candidates
}

fn score_signal_against_agenda(
    signal: &str,
    agenda_lower: &str,
    agenda_tokens: &HashSet<String>,
) -> usize {
    if agenda_lower.is_empty() {
        return 0;
    }

    let signal_lower = signal.trim().to_lowercase();
    if signal_lower.is_empty() {
        return 0;
    }

    let mut score = 0;
    if agenda_lower.contains(&signal_lower) || signal_lower.contains(agenda_lower) {
        score += 6;
    }

    let matched_tokens = tokenize_selection_reason_text(&signal_lower)
        .into_iter()
        .filter(|token| agenda_tokens.contains(token))
        .count();

    score + matched_tokens * 3
}

fn build_participant_reason_clause(
    agent: &MeetingAgentConfig,
    agenda_lower: &str,
    agenda_tokens: &HashSet<String>,
) -> (usize, String, Option<String>) {
    let mut scored_signals: Vec<(usize, String)> = selection_signal_candidates(agent)
        .into_iter()
        .map(|signal| {
            (
                score_signal_against_agenda(&signal, agenda_lower, agenda_tokens),
                signal,
            )
        })
        .collect();

    scored_signals.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.chars().count().cmp(&b.1.chars().count()))
    });

    let mut detail_parts = Vec::new();
    let mut detail_seen = HashSet::new();
    let mut best_score = 0;

    for (score, signal) in scored_signals {
        if score == 0 {
            continue;
        }
        if best_score == 0 {
            best_score = score;
        }

        let compact = compact_reason_fragment(&signal);
        let key = compact.to_lowercase();
        if detail_seen.insert(key) {
            detail_parts.push(compact);
        }
        if detail_parts.len() >= 2 {
            break;
        }
    }

    if detail_parts.is_empty() {
        if let Some(fallback) = compact_selection_signal(agent) {
            detail_parts.push(fallback);
        }
    }

    let focus = detail_parts.first().cloned();
    let clause = if detail_parts.is_empty() {
        agent.display_name.clone()
    } else {
        format!("{}({})", agent.display_name, detail_parts.join("·"))
    };

    (best_score, clause, focus)
}

fn build_selection_reason_line(
    config: &MeetingConfig,
    agenda: &str,
    participants: &[MeetingParticipant],
    fixed_role_ids: &[String],
) -> String {
    let agents_by_id: HashMap<&str, &MeetingAgentConfig> = config
        .available_agents
        .iter()
        .map(|agent| (agent.role_id.as_str(), agent))
        .collect();
    let agenda_lower = agenda.trim().to_lowercase();
    let agenda_tokens: HashSet<String> =
        tokenize_selection_reason_text(agenda).into_iter().collect();
    let fixed_role_ids: HashSet<String> = normalize_role_ids(fixed_role_ids).into_iter().collect();
    let fixed_count = participants
        .iter()
        .filter(|participant| fixed_role_ids.contains(&participant.role_id))
        .count();
    let auto_count = participants.len().saturating_sub(fixed_count);

    let mut focus_labels = Vec::new();
    let mut seen_labels = HashSet::new();
    let mut participant_clauses = Vec::new();
    for participant in participants {
        let Some(agent) = agents_by_id.get(participant.role_id.as_str()) else {
            participant_clauses.push((
                fixed_role_ids.contains(&participant.role_id),
                0usize,
                participant.display_name.clone(),
            ));
            continue;
        };
        let (score, clause, focus) =
            build_participant_reason_clause(agent, &agenda_lower, &agenda_tokens);
        if let Some(label) = focus {
            let dedupe_key = label.to_lowercase();
            if seen_labels.insert(dedupe_key) {
                focus_labels.push(label);
            }
        }
        participant_clauses.push((fixed_role_ids.contains(&participant.role_id), score, clause));
    }

    participant_clauses.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));

    let mut roster_labels: Vec<String> = participant_clauses
        .iter()
        .take(2)
        .map(|(_, _, clause)| clause.clone())
        .collect();
    if participant_clauses.len() > roster_labels.len() {
        roster_labels.push(format!(
            "외 {}명",
            participant_clauses.len() - roster_labels.len()
        ));
    }
    let roster = if roster_labels.is_empty() {
        "선정된 전문가들".to_string()
    } else {
        roster_labels.join(", ")
    };

    let focus = if !focus_labels.is_empty() {
        focus_labels
            .into_iter()
            .take(2)
            .collect::<Vec<_>>()
            .join(" · ")
    } else if !agenda_lower.is_empty() {
        compact_reason_fragment(agenda)
    } else {
        "핵심 전문성".to_string()
    };

    match (fixed_count, auto_count) {
        (0, _) => {
            format!(
                "안건의 {focus} 축에 맞춰 {roster}를 중심으로 자동 {}명 구성했어.",
                participants.len()
            )
        }
        (_, 0) => {
            format!(
                "안건의 {focus} 축이 고정 전문가와 맞아 {roster} 중심으로 고정 {fixed_count}명만 유지했어."
            )
        }
        _ => format!(
            "안건의 {focus} 축에 맞춰 {roster}를 우선했고, 고정 {fixed_count}명은 유지한 뒤 자동 {auto_count}명으로 보강했어."
        ),
    }
}

fn normalize_role_ids(role_ids: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    role_ids
        .iter()
        .map(|role_id| role_id.trim())
        .filter(|role_id| !role_id.is_empty())
        .filter_map(|role_id| {
            let normalized = role_id.to_string();
            seen.insert(normalized.clone()).then_some(normalized)
        })
        .collect()
}

fn fixed_participant_prompt_lines(fixed_role_ids: &[String]) -> String {
    if fixed_role_ids.is_empty() {
        "고정 전문 에이전트: 없음".to_string()
    } else {
        format!(
            "고정 전문 에이전트: {}\n- 이 role_id들은 최종 참가자에 반드시 포함한다.\n- 진행자는 남은 슬롯만 자동 선정한다.",
            fixed_role_ids.join(", ")
        )
    }
}

fn merge_selected_participants(
    config: &MeetingConfig,
    selected_role_ids: &[String],
    fixed_role_ids: &[String],
    max_participants: usize,
) -> Result<Vec<MeetingParticipant>, String> {
    let agents_by_id: HashMap<&str, &MeetingAgentConfig> = config
        .available_agents
        .iter()
        .map(|agent| (agent.role_id.as_str(), agent))
        .collect();
    let fixed_role_ids = normalize_role_ids(fixed_role_ids);
    if fixed_role_ids.len() > max_participants {
        return Err(format!(
            "Too many fixed participants: {} (max {})",
            fixed_role_ids.len(),
            max_participants
        ));
    }

    let mut participants = Vec::new();
    let mut seen = HashSet::new();
    for role_id in &fixed_role_ids {
        let agent = agents_by_id
            .get(role_id.as_str())
            .ok_or_else(|| format!("Unknown fixed meeting participant role_id: {role_id}"))?;
        participants.push(agent.to_participant());
        seen.insert(role_id.clone());
    }

    for role_id in normalize_role_ids(selected_role_ids) {
        if participants.len() >= max_participants {
            break;
        }
        if seen.contains(&role_id) {
            continue;
        }
        if let Some(agent) = agents_by_id.get(role_id.as_str()) {
            participants.push(agent.to_participant());
            seen.insert(role_id);
        }
    }

    if participants.len() < MIN_MEETING_PARTICIPANTS || participants.len() > max_participants {
        return Err(format!(
            "Invalid participant count after cross-check: {} (expected {}..={})",
            participants.len(),
            MIN_MEETING_PARTICIPANTS,
            max_participants
        ));
    }

    Ok(participants)
}

fn validate_fixed_participants(
    config: &MeetingConfig,
    fixed_role_ids: &[String],
    max_participants: usize,
) -> Result<(), String> {
    let fixed_role_ids = normalize_role_ids(fixed_role_ids);
    if fixed_role_ids.len() > max_participants {
        return Err(format!(
            "Too many fixed participants: {} (max {})",
            fixed_role_ids.len(),
            max_participants
        ));
    }

    let known_role_ids: HashSet<&str> = config
        .available_agents
        .iter()
        .map(|agent| agent.role_id.as_str())
        .collect();
    for role_id in fixed_role_ids {
        if !known_role_ids.contains(role_id.as_str()) {
            return Err(format!(
                "Unknown fixed meeting participant role_id: {role_id}"
            ));
        }
    }

    Ok(())
}

fn truncate_for_meeting(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    trimmed.chars().take(max_chars).collect::<String>() + "..."
}

fn extract_consensus_line(content: &str) -> Option<String> {
    content.lines().find_map(|line| {
        line.trim()
            .strip_prefix("CONSENSUS:")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| truncate_for_meeting(value, 220))
    })
}

fn compact_meeting_note(content: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("CONSENSUS:") || trimmed.starts_with("이견:")
        {
            return None;
        }
        Some(truncate_for_meeting(trimmed, 180))
    })
}

fn build_fallback_meeting_summary(
    agenda: &str,
    participants_list: &str,
    transcript: &[MeetingUtterance],
) -> String {
    let discussion_points = transcript
        .iter()
        .filter_map(|utterance| {
            compact_meeting_note(&utterance.content)
                .map(|note| format!("- {}: {}", utterance.display_name, note))
        })
        .take(4)
        .collect::<Vec<_>>();

    let mut seen_consensus = HashSet::new();
    let consensus_points = transcript
        .iter()
        .filter_map(|utterance| extract_consensus_line(&utterance.content))
        .filter(|point| seen_consensus.insert(point.clone()))
        .take(3)
        .collect::<Vec<_>>();

    let discussion_block = if discussion_points.is_empty() {
        "- 발언 기록을 바탕으로 자동 fallback 회의록을 생성했다.".to_string()
    } else {
        discussion_points.join("\n")
    };

    let conclusion = if consensus_points.is_empty() {
        "요약 에이전트 응답이 없어 참석자 발언의 핵심 판단을 fallback으로 정리했다.".to_string()
    } else {
        consensus_points.join(" ")
    };

    format!(
        "### 📋 회의록: {agenda}\n**참여자**: {participants}\n\n#### 주요 논의\n{discussion}\n\n#### 결론\n{conclusion}\n\n#### Action Items\n- [ ] [대복이 | Main] — fallback 회의록을 검토하고 필요한 정식 요약/후속 액션을 확정한다.",
        agenda = truncate_for_meeting(agenda, 120),
        participants = if participants_list.trim().is_empty() {
            "(참여자 정보 없음)"
        } else {
            participants_list
        },
        discussion = discussion_block,
        conclusion = conclusion,
    )
}

fn parse_primary_provider_arg(
    raw: Option<&str>,
    default_provider: ProviderKind,
) -> Result<ProviderKind, String> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => match ProviderKind::from_str(value) {
            Some(provider) if provider.is_supported() => Ok(provider),
            _ => Err(format!(
                "지원하지 않는 provider야: `{}` (`claude`, `codex`, `gemini`, `qwen` 중 하나여야 함)",
                value
            )),
        },
        None => Ok(default_provider),
    }
}

pub(super) fn parse_meeting_start_text(
    text: &str,
    default_provider: ProviderKind,
) -> Result<Option<MeetingStartRequest>, String> {
    let Some(rest) = text.trim().strip_prefix("/meeting start ") else {
        return Ok(None);
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return Err(
            "사용법: `/meeting start [--primary claude|codex|gemini|qwen] <안건>`".to_string(),
        );
    }

    let mut primary_provider = default_provider.clone();
    let mut agenda = rest;

    if let Some(after_flag) = rest.strip_prefix("--primary=") {
        let after_flag = after_flag.trim_start();
        let split_at = after_flag
            .find(char::is_whitespace)
            .unwrap_or(after_flag.len());
        let provider_raw = after_flag[..split_at].trim();
        let remainder = after_flag[split_at..].trim();
        primary_provider =
            parse_primary_provider_arg(Some(provider_raw), default_provider.clone())?;
        agenda = remainder;
    } else if let Some(after_flag) = rest.strip_prefix("--primary ") {
        let after_flag = after_flag.trim_start();
        let split_at = after_flag
            .find(char::is_whitespace)
            .unwrap_or(after_flag.len());
        let provider_raw = after_flag[..split_at].trim();
        let remainder = after_flag[split_at..].trim();
        primary_provider = parse_primary_provider_arg(Some(provider_raw), default_provider)?;
        agenda = remainder;
    }

    if agenda.trim().is_empty() {
        return Err(
            "사용법: `/meeting start [--primary claude|codex|gemini|qwen] <안건>`".to_string(),
        );
    }

    Ok(Some(MeetingStartRequest {
        primary_provider,
        agenda: agenda.trim().to_string(),
    }))
}

fn meeting_matches(meeting: &Meeting, expected_id: Option<&str>) -> bool {
    expected_id.map(|id| meeting.id == id).unwrap_or(true)
}

fn effective_round_count(meeting: &Meeting) -> u32 {
    let transcript_max_round = meeting
        .transcript
        .iter()
        .map(|u| u.round)
        .max()
        .unwrap_or(0);
    meeting.current_round.max(transcript_max_round)
}

fn meeting_slot_state(meeting: Option<&Meeting>, expected_id: &str) -> ActiveMeetingSlot {
    match meeting {
        Some(m) if m.id == expected_id && m.status != MeetingStatus::Cancelled => {
            ActiveMeetingSlot::Active
        }
        Some(m) if m.id == expected_id => ActiveMeetingSlot::Cancelled,
        _ => ActiveMeetingSlot::MissingOrReplaced,
    }
}

async fn active_meeting_state(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    expected_id: &str,
) -> ActiveMeetingSlot {
    let core = shared.core.lock().await;
    meeting_slot_state(core.active_meetings.get(&channel_id), expected_id)
}

async fn cleanup_meeting_if_current(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    expected_id: &str,
) {
    let mut core = shared.core.lock().await;
    let should_remove = core
        .active_meetings
        .get(&channel_id)
        .map(|m| m.id == expected_id)
        .unwrap_or(false);
    if should_remove {
        core.active_meetings.remove(&channel_id);
    }
}

// ─── Config Parsing ──────────────────────────────────────────────────────────

/// Load meeting config from agentdesk.yaml, then org.yaml, then role_map.json.
pub(super) fn load_meeting_config() -> Option<MeetingConfig> {
    if let Some(cfg) = agentdesk_config::load_meeting_config() {
        return Some(cfg);
    }
    if org_schema::org_schema_exists() {
        if let Some(cfg) = org_schema::load_meeting_config() {
            return Some(cfg);
        }
    }
    load_meeting_config_from_role_map()
}

/// Check if a channel name matches the configured meeting channel
#[allow(dead_code)]
pub(super) fn is_meeting_channel(channel_name: &str) -> bool {
    load_meeting_config()
        .map(|cfg| cfg.channel_name == channel_name)
        .unwrap_or(false)
}

// ─── Meeting Lifecycle ───────────────────────────────────────────────────────

/// Start a new meeting: select participants via Claude, then begin rounds.
/// Returns the meeting ID on success.
pub(super) async fn start_meeting(
    http: &serenity::Http,
    channel_id: ChannelId,
    agenda: &str,
    primary_provider: ProviderKind,
    reviewer_provider: ProviderKind,
    shared: &Arc<SharedData>,
) -> Result<Option<String>, Error> {
    start_meeting_with_reviewer(
        http,
        channel_id,
        agenda,
        primary_provider,
        reviewer_provider,
        Vec::new(),
        shared,
    )
    .await
}

async fn start_meeting_with_reviewer(
    http: &serenity::Http,
    channel_id: ChannelId,
    agenda: &str,
    primary_provider: ProviderKind,
    reviewer_provider: ProviderKind,
    fixed_participants: Vec<String>,
    shared: &Arc<SharedData>,
) -> Result<Option<String>, Error> {
    let config =
        load_meeting_config().ok_or("Meeting config not found in org.yaml or role_map.json")?;

    let meeting_id = generate_meeting_id();

    // #1008: drive the opt-in state machine on the start path so invalid
    // re-entries are logged uniformly.
    let _ = record_meeting_transition(&meeting_id, MeetingState::Pending, MeetingEvent::Start);

    // Register meeting as SelectingParticipants
    {
        let mut core = shared.core.lock().await;
        if core.active_meetings.contains_key(&channel_id) {
            return Err("이 채널에서 이미 회의가 진행 중이야.".into());
        }
        core.active_meetings.insert(
            channel_id,
            Meeting {
                id: meeting_id.clone(),
                channel_id: channel_id.get(),
                agenda: agenda.to_string(),
                primary_provider: primary_provider.clone(),
                reviewer_provider: reviewer_provider.clone(),
                selection_reason: None,
                participants: Vec::new(),
                transcript: Vec::new(),
                current_round: 0,
                max_rounds: config.max_rounds,
                status: MeetingStatus::SelectingParticipants,
                summary: None,
                started_at: chrono::Local::now().to_rfc3339(),
                thread_id: None,
                msg_channel: None,
            },
        );
    }

    // Create a Discord thread for the meeting so all output is contained there.
    let thread_name = format!("Meeting: {}", truncate_for_meeting(agenda, 90));
    let msg_channel: ChannelId = match create_meeting_thread(http, channel_id, &thread_name).await {
        Some(tid) => {
            // Save thread_id in Meeting struct
            let mut core = shared.core.lock().await;
            if let Some(m) = core.active_meetings.get_mut(&channel_id) {
                m.thread_id = Some(tid.get());
                m.msg_channel = Some(tid.get());
            }
            drop(core);
            tid
        }
        None => {
            tracing::warn!("[meeting] Thread creation failed, falling back to parent channel");
            channel_id
        }
    };

    let meeting_hash = meeting_query_hash(&meeting_id);
    let thread_hash = if msg_channel != channel_id {
        Some(thread_query_hash(&msg_channel.get().to_string()))
    } else {
        None
    };
    let meeting_hash_display = display_query_hash(&meeting_hash);
    let thread_hash_display = thread_hash.as_deref().map(display_query_hash);

    tracing::info!(
        meeting_id = %meeting_id,
        meeting_hash = %meeting_hash,
        thread_hash = thread_hash.as_deref().unwrap_or("-"),
        thread_channel_id = %msg_channel.get(),
        "[meeting] query hashes assigned"
    );

    let selection_status_message = send_meeting_message_with_event(
        http,
        msg_channel,
        shared,
        format!("meeting:{meeting_id}:selection-status:init"),
        build_meeting_start_status_message(
            agenda,
            &meeting_hash_display,
            thread_hash_display.as_deref(),
            &primary_provider,
            &reviewer_provider,
            None,
        ),
    )
    .await
    .ok()
    .flatten();

    // Select participants via primary provider + reviewer cross-check
    let (participants, selection_reason) = match select_participants(
        &config,
        agenda,
        primary_provider.clone(),
        reviewer_provider.clone(),
        fixed_participants.clone(),
    )
    .await
    {
        Ok((participants, selection_reason)) if !participants.is_empty() => {
            (participants, selection_reason)
        }
        Ok(_) => {
            cleanup_meeting_if_current(shared, channel_id, &meeting_id).await;
            return Err("참여자를 선정하지 못했어.".into());
        }
        Err(e) => {
            cleanup_meeting_if_current(shared, channel_id, &meeting_id).await;
            return Err(format!("참여자 선정 실패: {}", e).into());
        }
    };

    // Check if cancelled or replaced during participant selection
    if active_meeting_state(shared, channel_id, &meeting_id).await != ActiveMeetingSlot::Active {
        cleanup_meeting_if_current(shared, channel_id, &meeting_id).await;
        return Ok(None);
    }

    if let Some(status_message) = selection_status_message {
        let _ = edit_meeting_message(
            http,
            msg_channel,
            status_message,
            shared,
            build_meeting_start_status_message(
                agenda,
                &meeting_hash_display,
                thread_hash_display.as_deref(),
                &primary_provider,
                &reviewer_provider,
                Some(&selection_reason),
            ),
        )
        .await;
    }

    // Announce participants
    let participant_list: Vec<String> = participants
        .iter()
        .map(|p| format!("• {}", p.display_name))
        .collect();
    let _ = send_meeting_message_with_event(
        http,
        msg_channel,
        shared,
        format!("meeting:{meeting_id}:participants-confirmed"),
        format!(
            "👥 **참여자 확정** ({}명)\n{}",
            participants.len(),
            participant_list.join("\n")
        ),
    )
    .await;

    // Update meeting state and notify ADK
    let adk_payload = {
        let mut core = shared.core.lock().await;
        match core.active_meetings.get_mut(&channel_id) {
            Some(m) if m.id == meeting_id => {
                m.participants = participants;
                m.selection_reason = Some(selection_reason.clone());
                m.status = MeetingStatus::InProgress;
                build_meeting_status_payload(m)
            }
            _ => return Ok(None),
        }
    };

    // Persist the in-progress status through the internal API so office view can
    // show the active meeting even when auth is enabled.
    if let Some(payload) = adk_payload {
        if let Err(error) = persist_meeting_status(payload).await {
            cleanup_meeting_if_current(shared, channel_id, &meeting_id).await;
            return Err(error);
        }
    }

    // Run meeting rounds
    let max_rounds = config.max_rounds;
    for round in 1..=max_rounds {
        if active_meeting_state(shared, channel_id, &meeting_id).await != ActiveMeetingSlot::Active
        {
            cleanup_meeting_if_current(shared, channel_id, &meeting_id).await;
            return Ok(None);
        }

        let _ = send_meeting_message_with_event(
            http,
            msg_channel,
            shared,
            format!("meeting:{meeting_id}:round:{round}:header"),
            format!("─── **라운드 {}/{}** ───", round, max_rounds),
        )
        .await;

        let consensus =
            match run_meeting_round(http, channel_id, msg_channel, &meeting_id, round, shared)
                .await?
            {
                Some(consensus) => consensus,
                None => {
                    cleanup_meeting_if_current(shared, channel_id, &meeting_id).await;
                    return Ok(None);
                }
            };

        // Update round counter
        {
            let mut core = shared.core.lock().await;
            match core.active_meetings.get_mut(&channel_id) {
                Some(m) if m.id == meeting_id => {
                    m.current_round = round;
                }
                _ => return Ok(None),
            }
        }

        if consensus {
            let _ = send_meeting_message_with_event(
                http,
                msg_channel,
                shared,
                format!("meeting:{meeting_id}:consensus"),
                "✅ **합의 도달! 회의를 마무리할게.**",
            )
            .await;
            break;
        }
    }

    // Conclude meeting
    if !conclude_meeting(http, channel_id, msg_channel, &meeting_id, &config, shared).await? {
        cleanup_meeting_if_current(shared, channel_id, &meeting_id).await;
        return Ok(None);
    }

    // Save record
    if !save_meeting_record(shared, channel_id, Some(&meeting_id)).await? {
        cleanup_meeting_if_current(shared, channel_id, &meeting_id).await;
        return Ok(None);
    }
    let _ = send_meeting_message_with_event(
        http,
        msg_channel,
        shared,
        format!("meeting:{meeting_id}:record-saved"),
        "💾 **회의록 저장 완료.** memory write/capture는 자동 실행하지 않으며, 후처리는 승인 기반으로만 진행합니다.",
    )
    .await;

    // Archive the meeting thread if one was created
    if msg_channel != channel_id {
        archive_meeting_thread(http, msg_channel).await;
    }

    // Clean up
    cleanup_meeting_if_current(shared, channel_id, &meeting_id).await;

    Ok(Some(meeting_id))
}

pub(super) async fn spawn_direct_start(
    http: Arc<serenity::Http>,
    channel_id: ChannelId,
    agenda: String,
    primary_provider: ProviderKind,
    reviewer_provider: ProviderKind,
    fixed_participants: Vec<String>,
    shared: Arc<SharedData>,
) -> Result<(), String> {
    if primary_provider == reviewer_provider {
        return Err("reviewer_provider must differ from primary_provider".to_string());
    }

    let config = load_meeting_config()
        .ok_or_else(|| "Meeting config not found in org.yaml or role_map.json".to_string())?;
    let max_participants = clamp_max_participants(config.max_participants);
    validate_fixed_participants(&config, &fixed_participants, max_participants)?;

    {
        let core = shared.core.lock().await;
        if core.active_meetings.contains_key(&channel_id) {
            return Err("이 채널에서 이미 회의가 진행 중이야.".to_string());
        }
    }

    tokio::spawn(async move {
        match start_meeting_with_reviewer(
            &*http,
            channel_id,
            &agenda,
            primary_provider,
            reviewer_provider,
            fixed_participants,
            &shared,
        )
        .await
        {
            Ok(Some(id)) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ✅ Meeting completed: {id}");
            }
            Ok(None) => {}
            Err(error) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ❌ Meeting error: {error}");
                let _ = send_meeting_message(
                    &http,
                    channel_id,
                    &shared,
                    format!("❌ 회의 오류: {error}"),
                )
                .await;
            }
        }
    });

    Ok(())
}

/// Cancel a running meeting
pub(super) async fn cancel_meeting(
    http: &serenity::Http,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    let meeting_info = {
        let mut core = shared.core.lock().await;
        if let Some(m) = core.active_meetings.get_mut(&channel_id) {
            // #1008: drive the opt-in reducer; tolerant of already-cancelled
            // meetings (cancel-race idempotency) so a second cancel is a no-op.
            let _ = record_meeting_transition(&m.id, m.status.to_state(), MeetingEvent::Cancel);
            // #1008: record a cancel artifact keyed by meeting id — concurrent
            // cancels collapse onto one row.
            let _ = record_cancel_artifact(&m.id, "cancelled-by-user");
            m.status = MeetingStatus::Cancelled;
            let mc = m.msg_channel.map(ChannelId::new).unwrap_or(channel_id);
            Some((mc, m.id.clone()))
        } else {
            None
        }
    };

    if let Some((mc, meeting_id)) = meeting_info {
        // Save whatever transcript we have
        let _ = save_meeting_record(shared, channel_id, None).await;
        cleanup_meeting(shared, channel_id).await;
        let _ = send_meeting_message_with_event(
            http,
            mc,
            shared,
            meeting_cancel_event_key(channel_id, &meeting_id),
            "🛑 **회의가 취소됐어.** 현재까지 트랜스크립트가 저장됐고, memory write/capture는 자동 실행하지 않았어.",
        )
        .await;
        Ok(())
    } else {
        let _ = send_meeting_message(http, channel_id, shared, "진행 중인 회의가 없어.").await;
        Ok(())
    }
}

fn meeting_cancel_event_key(channel_id: ChannelId, meeting_id: &str) -> String {
    format!(
        "meeting:{meeting_id}:channel:{}:cancelled",
        channel_id.get()
    )
}

/// Get meeting status info
pub(super) async fn meeting_status(
    http: &serenity::Http,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    let info = {
        let core = shared.core.lock().await;
        core.active_meetings.get(&channel_id).map(|m| {
            (
                m.agenda.clone(),
                m.current_round,
                m.max_rounds,
                m.participants.len(),
                m.transcript.len(),
                m.status.clone(),
                m.primary_provider.clone(),
                m.reviewer_provider.clone(),
            )
        })
    };

    match info {
        Some((agenda, round, max_rounds, participants, utterances, status, primary, reviewer)) => {
            let status_str = match status {
                MeetingStatus::SelectingParticipants => "참여자 선정 중",
                MeetingStatus::InProgress => "진행 중",
                MeetingStatus::Concluding => "마무리 중",
                MeetingStatus::Completed => "완료",
                MeetingStatus::Cancelled => "취소됨",
            };
            let _ = send_meeting_message(
                http,
                channel_id,
                shared,
                format!(
                    "📊 **회의 현황**\n안건: {}\n상태: {}\n진행 프로바이더: {} / 리뷰 프로바이더: {}\n라운드: {}/{}\n참여자: {}명\n발언: {}개",
                    agenda,
                    status_str,
                    primary.display_name(),
                    reviewer.display_name(),
                    round,
                    max_rounds,
                    participants,
                    utterances
                ),
            )
            .await;
        }
        None => {
            let _ = send_meeting_message(http, channel_id, shared, "진행 중인 회의가 없어.").await;
        }
    }
    Ok(())
}

// ─── Internal Functions ──────────────────────────────────────────────────────

/// Select participants using primary provider + reviewer micro cross-check.
async fn select_participants(
    config: &MeetingConfig,
    agenda: &str,
    primary_provider: ProviderKind,
    reviewer_provider: ProviderKind,
    fixed_participants: Vec<String>,
) -> Result<(Vec<MeetingParticipant>, String), String> {
    let max_participants = clamp_max_participants(config.max_participants);
    validate_fixed_participants(config, &fixed_participants, max_participants)?;
    if config.available_agents.len() < MIN_MEETING_PARTICIPANTS {
        return Err(format!(
            "Meeting candidate pool has {} agents; at least {} are required. Check meeting.available_agents configuration.",
            config.available_agents.len(),
            MIN_MEETING_PARTICIPANTS
        ));
    }
    let fixed_participants = normalize_role_ids(&fixed_participants);
    let fixed_participants_fill_roster = fixed_participants.len() >= MIN_MEETING_PARTICIPANTS
        && (fixed_participants.len() >= max_participants
            || fixed_participants.len() >= config.available_agents.len());
    if fixed_participants_fill_roster {
        let participants =
            merge_selected_participants(config, &[], &fixed_participants, max_participants)?;
        let selection_reason = compact_selection_reason(&build_selection_reason_line(
            config,
            agenda,
            &participants,
            &fixed_participants,
        ))
        .unwrap_or_else(|| {
            "고정 전문 에이전트 조합으로 안건 대응 범위가 충족되어 그대로 선정함".to_string()
        });
        return Ok((participants, selection_reason));
    }
    let agents_desc: Vec<String> = config
        .available_agents
        .iter()
        .map(agent_metadata_card)
        .collect();
    let fixed_prompt = fixed_participant_prompt_lines(&fixed_participants);

    let selection_prompt = format!(
        r#"다음 안건에 대한 라운드 테이블 회의에 참여할 전문 에이전트를 선정해줘.

안건: {}

{}

후보 메타데이터 카드:
{}

선정 절차:
1. 안건 요약: 안건을 1문장으로 압축한다.
2. 필요 전문성 축: 필요한 전문성 축을 2~5개로 나눈다.
3. 후보별 적합성 비교: display_name, selection_profile, domain_summary, strengths, task_types, anti_signals, provider, provider_hint, metadata_missing을 함께 비교한다.
4. 최종 선정 JSON: 최종 role_id와 compact selection_reason을 함께 고른다.

규칙:
- {}~{}명 선정
- 고정 전문 에이전트가 있으면 반드시 포함하고, 남은 슬롯만 추가 선정한다
- keywords 단순 일치만으로 선정하지 말고 display_name/domain_summary/strengths/task_types/provider를 우선한다
- anti_signals에 걸리는 후보는 강한 이유가 없으면 제외한다
- metadata_missing이 많은 후보는 필요한 경우에만 보조적으로 선정한다
- selection_reason은 한국어 한 줄로, 줄바꿈/불릿/따옴표/생략부호(...) 없이 작성한다
- selection_reason은 안건 핵심 + 선택한 전문가의 display_name/strengths/provider 근거를 포함한다
- selection_reason을 \"핵심 전문성 커버\" 같은 추상 문장만으로 쓰지 말고 왜 이 조합인지 구체적으로 적는다
- JSON 객체로만 응답 (다른 텍스트 없이)
- 형식: {{"selected_role_ids":["role_id1","role_id2"],"selection_reason":"선정 이유"}}"#,
        agenda,
        fixed_prompt,
        agents_desc.join("\n"),
        MIN_MEETING_PARTICIPANTS,
        max_participants,
    );

    let initial_response = execute_provider_stage(
        primary_provider.clone(),
        "participant initial selection",
        selection_prompt,
        meeting_selection_stage_timeout_secs(),
    )
    .await?;
    let initial_decision = parse_participant_selection_response(&initial_response)?;

    let review_prompt = format!(
        r#"당신은 회의 참가자 선정을 비판적으로 검토하는 리뷰어다.

안건: {agenda}

사용 가능한 에이전트:
{agents}

현재 선정안:
{current}

현재 선정 사유:
{reason}

고정 전문 에이전트:
{fixed}

검토 규칙:
- 빠진 역할, 중복 역할, 안건과의 부적합만 짚어라
- 고정 전문 에이전트가 누락되면 반드시 지적하라
- 4개 이하 bullet만 사용하라
- selection_reason이 추상적이거나 display_name/strengths/provider 근거가 약하면 지적하라
- metadata_missing, anti_signals, task_types mismatch가 있으면 명시하라
- 전체를 다시 쓰지 말고, 비판적으로만 검토하라
- 도구나 명령 실행은 하지 마라"#,
        agenda = agenda,
        agents = agents_desc.join("\n"),
        current = serde_json::to_string(&initial_decision.selected_role_ids)
            .unwrap_or_else(|_| "[]".to_string()),
        reason = initial_decision.selection_reason.as_deref().unwrap_or("-"),
        fixed = fixed_participants.join(", "),
    );

    let review_notes = match execute_provider_stage(
        reviewer_provider.clone(),
        "participant selection review",
        review_prompt,
        meeting_selection_stage_timeout_secs(),
    )
    .await
    {
        Ok(notes) => notes,
        Err(err) => format!("- 리뷰 실패: {err}. 초기 선정안을 유지하고 최종 검증만 수행한다."),
    };

    let finalize_prompt = format!(
        r#"다음 안건에 대한 회의 참가자 선정을 최종 확정해줘.

안건: {agenda}

사용 가능한 에이전트:
{agents}

초기 선정안:
{initial}

초기 선정 사유:
{reason}

고정 전문 에이전트:
{fixed}

교차검증 리뷰:
{review}

규칙:
- 리뷰가 타당하면 반영하고, 타당하지 않으면 유지하라
- 최종 결과는 {min_participants}~{max_participants}명이어야 한다
- 고정 전문 에이전트는 최종 JSON에 반드시 포함한다
- 후보 메타데이터에서 metadata_missing이 많은 후보는 필요한 경우에만 유지하라
- selection_reason은 한국어 한 줄로, 줄바꿈/불릿/따옴표/생략부호(...) 없이 작성하라
- selection_reason은 안건 + 선택 전문가 display_name/strengths/provider 근거를 압축해라
- selection_reason을 추상 표현으로만 쓰지 말고 실제 조합 근거를 포함해라
- JSON 객체로만 응답하라
- 형식: {{"selected_role_ids":["role_id1","role_id2"],"selection_reason":"선정 이유"}}"#,
        agenda = agenda,
        agents = agents_desc.join("\n"),
        initial = serde_json::to_string(&initial_decision.selected_role_ids)
            .unwrap_or_else(|_| "[]".to_string()),
        reason = initial_decision.selection_reason.as_deref().unwrap_or("-"),
        fixed = fixed_participants.join(", "),
        review = review_notes.trim(),
        min_participants = MIN_MEETING_PARTICIPANTS,
        max_participants = max_participants,
    );

    let selected = match execute_provider_stage(
        primary_provider.clone(),
        "participant final selection",
        finalize_prompt,
        meeting_selection_stage_timeout_secs(),
    )
    .await
    {
        Ok(final_response) => parse_participant_selection_response(&final_response)?,
        Err(_) => initial_decision,
    };

    let participants = merge_selected_participants(
        config,
        &selected.selected_role_ids,
        &fixed_participants,
        max_participants,
    )?;
    let selection_reason = selected.selection_reason.unwrap_or_else(|| {
        build_selection_reason_line(config, agenda, &participants, &fixed_participants)
    });
    let selection_reason = compact_selection_reason(&selection_reason).unwrap_or(selection_reason);

    Ok((participants, selection_reason))
}

/// Run one round: each participant speaks in order
async fn run_meeting_round(
    http: &serenity::Http,
    channel_id: ChannelId,
    msg_channel: ChannelId,
    meeting_id: &str,
    round: u32,
    shared: &Arc<SharedData>,
) -> Result<Option<bool>, Error> {
    // Snapshot participants and transcript for this round
    let (participants, agenda, primary_provider, reviewer_provider) = {
        let core = shared.core.lock().await;
        let Some(m) = core
            .active_meetings
            .get(&channel_id)
            .filter(|m| m.id == meeting_id)
        else {
            return Ok(None);
        };
        (
            m.participants.clone(),
            m.agenda.clone(),
            m.primary_provider.clone(),
            m.reviewer_provider.clone(),
        )
    };

    for participant in &participants {
        if active_meeting_state(shared, channel_id, meeting_id).await != ActiveMeetingSlot::Active {
            return Ok(None);
        }

        // Get current transcript for context
        let transcript_text = {
            let core = shared.core.lock().await;
            let Some(m) = core
                .active_meetings
                .get(&channel_id)
                .filter(|m| m.id == meeting_id)
            else {
                return Ok(None);
            };
            format_transcript(&m.transcript)
        };

        // Execute agent turn
        match execute_agent_turn(
            participant,
            &agenda,
            channel_id.get(),
            meeting_id,
            round,
            &transcript_text,
            primary_provider.clone(),
            reviewer_provider.clone(),
        )
        .await
        {
            Ok(response) => {
                if active_meeting_state(shared, channel_id, meeting_id).await
                    != ActiveMeetingSlot::Active
                {
                    return Ok(None);
                }

                // Post to Discord
                let discord_msg = format!(
                    "**[{}]** (R{})\n{}",
                    participant.display_name, round, response
                );
                send_long_message_raw(http, msg_channel, &discord_msg, shared).await?;

                // Append to transcript
                {
                    let mut core = shared.core.lock().await;
                    match core.active_meetings.get_mut(&channel_id) {
                        Some(m) if m.id == meeting_id => {
                            m.transcript.push(MeetingUtterance {
                                role_id: participant.role_id.clone(),
                                display_name: participant.display_name.clone(),
                                round,
                                content: response,
                            });
                        }
                        _ => return Ok(None),
                    }
                }
            }
            Err(e) => {
                // Skip this agent, post error to thread
                let _ = send_meeting_message_with_event(
                    http,
                    msg_channel,
                    shared,
                    format!(
                        "meeting:{meeting_id}:round:{round}:participant:{}:error",
                        participant.role_id
                    ),
                    format!("⚠️ {} 발언 실패: {}", participant.display_name, e),
                )
                .await;
            }
        }
    }

    // Check consensus
    let consensus = {
        let core = shared.core.lock().await;
        let Some(m) = core
            .active_meetings
            .get(&channel_id)
            .filter(|m| m.id == meeting_id)
        else {
            return Ok(None);
        };
        check_consensus(&m.transcript, round, m.participants.len())
    };

    Ok(Some(consensus))
}

fn meeting_readonly_allowed_tools() -> Vec<String> {
    vec!["Read".to_string()]
}

fn participant_working_dir(participant: &MeetingParticipant) -> String {
    participant
        .workspace
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| "/".to_string())
        })
}

fn format_memory_recall_context(recall: &RecallResponse) -> String {
    let mut chunks = Vec::new();
    if let Some(shared) = recall.shared_knowledge.as_deref() {
        if !shared.trim().is_empty() {
            chunks.push(format!("## Shared Knowledge\n{}", shared.trim()));
        }
    }
    if let Some(catalog) = recall.longterm_catalog.as_deref() {
        if !catalog.trim().is_empty() {
            chunks.push(format!("## Long-term Memory Catalog\n{}", catalog.trim()));
        }
    }
    if let Some(external) = recall.external_recall.as_deref() {
        if !external.trim().is_empty() {
            chunks.push(format!("## External Recall\n{}", external.trim()));
        }
    }
    chunks.join("\n\n")
}

async fn participant_memory_recall(
    participant: &MeetingParticipant,
    provider: ProviderKind,
    channel_id: u64,
    meeting_id: &str,
    round: u32,
    agenda: &str,
    transcript: &str,
) -> String {
    let backend = build_resolved_memory_backend(&participant.memory);
    let recall = backend
        .recall(RecallRequest {
            provider,
            role_id: participant.role_id.clone(),
            channel_id,
            session_id: format!("meeting:{meeting_id}:round:{round}:{}", participant.role_id),
            dispatch_profile: DispatchProfile::Full,
            user_text: format!("{agenda}\n\n{transcript}"),
            // Meetings always need full context — agenda + transcript drives
            // the agent's response and there is no per-channel session state.
            mode: crate::services::memory::RecallMode::Full,
        })
        .await;

    for warning in &recall.warnings {
        tracing::warn!(
            "[meeting] memory recall warning meeting_id={} role_id={}: {}",
            meeting_id,
            participant.role_id,
            warning
        );
    }
    format_memory_recall_context(&recall)
}

fn meeting_readonly_system_prompt(
    participant: &MeetingParticipant,
    role_context: &str,
    memory_context: &str,
) -> String {
    format!(
        r#"You are the specialist meeting participant `{role_id}` ({display_name}).

Authoritative execution mode: `meeting_readonly`.
- You may use only read-only file/context inspection capabilities exposed by the runtime.
- You must not write files, run shell commands, capture memory, write memory, mutate repo state, call external network tools, or ask for interactive confirmation.
- Use your role prompt, identity context, and injected memory/recall context to answer from your specialist viewpoint.

## Role / IDENTITY Context
{role_context}

## Memory / Recall Context
{memory_context}"#,
        role_id = participant.role_id,
        display_name = participant.display_name,
        role_context = if role_context.trim().is_empty() {
            "(none)"
        } else {
            role_context.trim()
        },
        memory_context = if memory_context.trim().is_empty() {
            "(none)"
        } else {
            memory_context.trim()
        },
    )
}

/// Execute a single agent turn using specialist draft/final -> reviewer critique.
async fn execute_agent_turn(
    participant: &MeetingParticipant,
    agenda: &str,
    channel_id: u64,
    meeting_id: &str,
    round: u32,
    transcript: &str,
    primary_provider: ProviderKind,
    reviewer_provider: ProviderKind,
) -> Result<String, String> {
    let specialist_provider = participant
        .provider
        .clone()
        .unwrap_or_else(|| primary_provider.clone());
    let role_binding = participant.role_binding();
    let role_context = if !participant.prompt_file.is_empty() {
        load_role_prompt(&role_binding).unwrap_or_default()
    } else {
        String::new()
    };
    let memory_context = participant_memory_recall(
        participant,
        specialist_provider.clone(),
        channel_id,
        meeting_id,
        round,
        agenda,
        transcript,
    )
    .await;
    let system_prompt = meeting_readonly_system_prompt(participant, &role_context, &memory_context);
    let allowed_tools = meeting_readonly_allowed_tools();
    let working_dir = participant_working_dir(participant);
    let critique_provider = if specialist_provider == reviewer_provider {
        primary_provider.clone()
    } else {
        reviewer_provider.clone()
    };

    let draft_prompt = format!(
        r#"당신은 라운드 테이블 회의에 참여한 {name}입니다.

{role_context}

## 회의 안건
{agenda}

## 현재 라운드: {round}

## 이전 발언 기록
{transcript}

## 지시사항
- 당신의 전문 분야 관점에서 안건에 대해 의견을 제시하세요
- 이전 발언자들의 의견을 참고하고 필요시 반론/보충하세요
- 답변은 300자 이내로 간결하게 작성하세요
- 합의에 도달했다고 판단되면, 반드시 "CONSENSUS:" 로 시작하는 한 줄 요약을 마지막에 추가하세요
- 아직 논의가 더 필요하면 CONSENSUS: 키워드를 사용하지 마세요
- meeting_readonly 정책을 지키고, 쓰기/변경 작업은 절대 하지 마세요"#,
        name = participant.display_name,
        role_context = if role_context.is_empty() {
            String::new()
        } else {
            format!("## 역할 컨텍스트\n{}", role_context)
        },
        agenda = agenda,
        round = round,
        transcript = if transcript.is_empty() {
            "(아직 발언 없음)".to_string()
        } else {
            transcript.to_string()
        },
    );

    let draft = provider_exec::execute_structured(
        specialist_provider.clone(),
        draft_prompt,
        working_dir.clone(),
        Some(system_prompt.clone()),
        allowed_tools.clone(),
        participant.model.clone(),
        MEETING_TURN_STAGE_TIMEOUT_SECS,
        "meeting turn draft",
    )
    .await?;

    let critique_prompt = format!(
        r#"당신은 회의 발언 초안을 비판적으로 검토하는 리뷰어다.

발언 역할: {name}

역할 컨텍스트:
{role_context}

회의 안건:
{agenda}

현재 라운드: {round}

이전 발언 기록:
{transcript}

초안:
{draft}

검토 규칙:
- 4개 이하 bullet만 사용하라
- 누락된 핵심 포인트, 과한 주장, 리스크 누락, 역할 범위 이탈만 지적하라
- 초안을 통째로 다시 쓰지 마라
- 도구나 명령 실행은 하지 마라"#,
        name = participant.display_name,
        role_context = if role_context.is_empty() {
            "(역할 컨텍스트 없음)".to_string()
        } else {
            role_context.clone()
        },
        agenda = agenda,
        round = round,
        transcript = if transcript.is_empty() {
            "(아직 발언 없음)".to_string()
        } else {
            transcript.to_string()
        },
        draft = draft.trim(),
    );
    let critique = match execute_provider_stage(
        critique_provider,
        "meeting turn critique",
        critique_prompt,
        MEETING_TURN_STAGE_TIMEOUT_SECS,
    )
    .await
    {
        Ok(text) => text,
        Err(_) => return Ok(truncate_for_meeting(&draft, 1500)),
    };

    let final_prompt = format!(
        r#"당신은 라운드 테이블 회의에 참여한 {name}입니다.

{role_context}

회의 안건:
{agenda}

현재 라운드: {round}

이전 발언 기록:
{transcript}

초안:
{draft}

교차검증 리뷰:
{critique}

지시사항:
- 리뷰를 반영해 최종 발언을 다시 작성하라
- 답변은 300자 이내로 유지하라
- 합의에 도달했다고 판단되면, 반드시 "CONSENSUS:" 로 시작하는 한 줄 요약을 마지막에 추가하세요
- 리뷰에서 중요한 이견이 남아 있다고 판단되면 마지막 줄에 `이견:` 한 줄로 짧게 남겨라
- 도구나 명령 실행 없이 최종 발언만 작성하라"#,
        name = participant.display_name,
        role_context = if role_context.is_empty() {
            String::new()
        } else {
            format!("## 역할 컨텍스트\n{}", role_context)
        },
        agenda = agenda,
        round = round,
        transcript = if transcript.is_empty() {
            "(아직 발언 없음)".to_string()
        } else {
            transcript.to_string()
        },
        draft = draft.trim(),
        critique = critique.trim(),
    );

    match provider_exec::execute_structured(
        specialist_provider,
        final_prompt,
        working_dir,
        Some(system_prompt),
        allowed_tools,
        participant.model.clone(),
        MEETING_TURN_STAGE_TIMEOUT_SECS,
        "meeting turn final",
    )
    .await
    {
        Ok(text) => Ok(truncate_for_meeting(&text, 1500)),
        Err(_) => Ok(truncate_for_meeting(&draft, 1500)),
    }
}

/// Check if majority of participants in a given round used CONSENSUS: keyword
fn check_consensus(transcript: &[MeetingUtterance], round: u32, participant_count: usize) -> bool {
    if participant_count == 0 {
        return false;
    }
    let consensus_count = transcript
        .iter()
        .filter(|u| u.round == round && u.content.contains("CONSENSUS:"))
        .count();
    // Majority = more than half
    consensus_count * 2 > participant_count
}

/// Conclude meeting: summary agent produces minutes
async fn conclude_meeting(
    http: &serenity::Http,
    channel_id: ChannelId,
    msg_channel: ChannelId,
    meeting_id: &str,
    config: &MeetingConfig,
    shared: &Arc<SharedData>,
) -> Result<bool, Error> {
    // Update status
    {
        let mut core = shared.core.lock().await;
        match core.active_meetings.get_mut(&channel_id) {
            Some(m) if m.id == meeting_id => {
                if m.status == MeetingStatus::Cancelled {
                    return Ok(false);
                }
                m.status = MeetingStatus::Concluding;
            }
            _ => return Ok(false),
        }
    }

    let (
        agenda,
        transcript_snapshot,
        transcript_text,
        participants_list,
        primary_provider,
        reviewer_provider,
    ) = {
        let core = shared.core.lock().await;
        let Some(m) = core
            .active_meetings
            .get(&channel_id)
            .filter(|m| m.id == meeting_id)
        else {
            return Ok(false);
        };
        let t = format_transcript(&m.transcript);
        let p: Vec<String> = m
            .participants
            .iter()
            .map(|p| p.display_name.clone())
            .collect();
        (
            m.agenda.clone(),
            m.transcript.clone(),
            t,
            p.join(", "),
            m.primary_provider.clone(),
            m.reviewer_provider.clone(),
        )
    };

    // Resolve summary agent dynamically based on agenda
    let resolved_summary_agent = config.summary_agent.resolve(&agenda);
    let summary_role_context = summary_agent_context(config, &resolved_summary_agent);

    let draft_prompt = format!(
        r#"당신은 회의록을 작성하는 {agent}입니다.

{role_context}

다음 라운드 테이블 회의의 회의록을 작성해주세요.

## 안건
{agenda}

## 참여자
{participants}

## 전체 발언 기록
{transcript}

## 회의록 형식
다음 형식으로 작성하세요:

### 📋 회의록: [안건 요약]
**참여자**: [이름 목록]

#### 주요 논의
- [핵심 논의 사항 1]
- [핵심 논의 사항 2]

#### 결론
[합의 사항 또는 미합의 시 각 입장 정리]

#### Action Items
- [ ] [담당자] — [할 일]"#,
        agent = resolved_summary_agent,
        role_context = if summary_role_context.is_empty() {
            String::new()
        } else {
            format!("## 역할 컨텍스트\n{}", summary_role_context)
        },
        agenda = agenda,
        participants = participants_list,
        transcript = transcript_text,
    );

    if active_meeting_state(shared, channel_id, meeting_id).await != ActiveMeetingSlot::Active {
        return Ok(false);
    }
    let _ = send_meeting_message_with_event(
        http,
        msg_channel,
        shared,
        format!("meeting:{meeting_id}:summary:drafting"),
        "📝 **회의록 작성 중...**",
    )
    .await;

    let draft = execute_provider_stage(
        primary_provider.clone(),
        "meeting summary draft",
        draft_prompt,
        MEETING_SUMMARY_STAGE_TIMEOUT_SECS,
    )
    .await;

    let summary_text = match draft {
        Ok(draft_text) => {
            let fallback_draft = draft_text.trim().to_string();
            let critique_prompt = format!(
                r#"당신은 회의록 초안을 비판적으로 검토하는 리뷰어다.

안건:
{agenda}

참여자:
{participants}

초안:
{draft}

검토 규칙:
- 누락된 핵심 논점, 잘못된 결론, 빠진 action item, 과도한 일반화만 지적하라
- 6개 이하 bullet만 사용하라
- 회의록 전체를 다시 쓰지 마라
- 도구나 명령 실행은 하지 마라"#,
                agenda = agenda,
                participants = participants_list,
                draft = draft_text.trim(),
            );
            let critique = execute_provider_stage(
                reviewer_provider,
                "meeting summary critique",
                critique_prompt,
                MEETING_SUMMARY_STAGE_TIMEOUT_SECS,
            )
            .await;
            let final_prompt = format!(
                r#"당신은 회의록을 작성하는 {agent}입니다.

{role_context}

안건:
{agenda}

참여자:
{participants}

전체 발언 기록:
{transcript}

초안:
{draft}

교차검증 리뷰:
{critique}

지시사항:
- 리뷰에서 타당한 지적을 반영해 최종 회의록을 작성하라
- 형식은 기존 회의록 형식을 유지하라
- 미합의 사항이 남아 있으면 결론에 분리해 적어라
- 도구나 명령 실행 없이 최종 회의록만 작성하라"#,
                agent = resolved_summary_agent,
                role_context = if summary_role_context.is_empty() {
                    String::new()
                } else {
                    format!("## 역할 컨텍스트\n{}", summary_role_context)
                },
                agenda = agenda,
                participants = participants_list,
                transcript = transcript_text,
                draft = draft_text.trim(),
                critique = match critique {
                    Ok(text) => text.trim().to_string(),
                    Err(err) => format!("- 리뷰 실패: {}", err),
                },
            );
            match execute_provider_stage(
                primary_provider,
                "meeting summary final",
                final_prompt,
                MEETING_SUMMARY_STAGE_TIMEOUT_SECS,
            )
            .await
            {
                Ok(text) => {
                    let trimmed = text.trim().to_string();
                    if active_meeting_state(shared, channel_id, meeting_id).await
                        != ActiveMeetingSlot::Active
                    {
                        return Ok(false);
                    }
                    send_long_message_raw(http, msg_channel, &trimmed, shared).await?;
                    Some(trimmed)
                }
                Err(e) => {
                    let _ = send_meeting_message_with_event(
                        http,
                        msg_channel,
                        shared,
                        format!("meeting:{meeting_id}:summary:finalize-error"),
                        format!("⚠️ 회의록 최종화 실패: {} — 초안으로 저장합니다.", e),
                    )
                    .await;
                    let fallback_summary = if fallback_draft.is_empty() {
                        build_fallback_meeting_summary(
                            &agenda,
                            &participants_list,
                            &transcript_snapshot,
                        )
                    } else {
                        fallback_draft
                    };
                    let _ =
                        send_long_message_raw(http, msg_channel, &fallback_summary, shared).await;
                    Some(fallback_summary)
                }
            }
        }
        Err(e) => {
            let fallback_summary =
                build_fallback_meeting_summary(&agenda, &participants_list, &transcript_snapshot);
            let _ = send_meeting_message_with_event(
                http,
                msg_channel,
                shared,
                format!("meeting:{meeting_id}:summary:draft-error"),
                format!("⚠️ 회의록 작성 실패: {} — fallback 회의록을 저장합니다.", e),
            )
            .await;
            let _ = send_long_message_raw(http, msg_channel, &fallback_summary, shared).await;
            Some(fallback_summary)
        }
    };

    // Mark completed and save summary
    {
        let mut core = shared.core.lock().await;
        match core.active_meetings.get_mut(&channel_id) {
            Some(m) if m.id == meeting_id => {
                m.summary = summary_text;
                m.status = MeetingStatus::Completed;
            }
            _ => return Ok(false),
        }
    }

    Ok(true)
}

/// Save meeting record as Markdown to $AGENTDESK_ROOT_DIR/meetings/
async fn save_meeting_record(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    expected_id: Option<&str>,
) -> Result<bool, Error> {
    let (md, meeting_id, adk_payload) = {
        let core = shared.core.lock().await;
        let Some(m) = core
            .active_meetings
            .get(&channel_id)
            .filter(|m| meeting_matches(m, expected_id))
        else {
            return Ok(false);
        };

        let payload = build_meeting_status_payload(m);
        (build_meeting_markdown(m), m.id.clone(), payload)
    };

    let meetings_dir = super::runtime_store::agentdesk_root()
        .ok_or("Home dir not found")?
        .join("meetings");
    fs::create_dir_all(&meetings_dir)?;

    let date_str = chrono::Local::now().format("%Y-%m-%d").to_string();
    let path = meetings_dir.join(format!("{}_{}.md", date_str, meeting_id));
    fs::write(&path, md)?;

    // Persist meeting data through the direct internal API so auth-protected
    // deployments do not silently drop meeting records.
    if let Some(payload) = adk_payload {
        if let Err(error) = persist_meeting_status(payload).await {
            cleanup_meeting_if_current(shared, channel_id, &meeting_id).await;
            return Err(error);
        }
    }

    Ok(true)
}

fn memory_postprocessing_policy() -> serde_json::Value {
    serde_json::json!({
        "auto_memory_write": false,
        "auto_memory_capture": false,
        "policy": "approval_required",
        "note": "Meeting records are saved only; memory write/capture is not run automatically.",
    })
}

/// Build ADK API payload from meeting
fn build_meeting_status_payload(m: &Meeting) -> Option<serde_json::Value> {
    let status_str = match &m.status {
        MeetingStatus::Completed => "completed",
        MeetingStatus::Cancelled => "cancelled",
        _ => "in_progress",
    };
    let total_rounds = effective_round_count(m);

    let participant_names: Vec<&str> = m
        .participants
        .iter()
        .map(|p| p.display_name.as_str())
        .collect();

    let entries: Vec<serde_json::Value> = m
        .transcript
        .iter()
        .enumerate()
        .map(|(i, u)| {
            serde_json::json!({
                "seq": i + 1,
                "round": u.round,
                "speaker_role_id": u.role_id,
                "speaker_name": u.display_name,
                "content": u.content,
                "is_summary": false,
            })
        })
        .collect();

    let started_at = chrono::DateTime::parse_from_rfc3339(&m.started_at)
        .map(|dt| dt.timestamp_millis())
        .unwrap_or_else(|_| chrono::Local::now().timestamp_millis());
    let meeting_hash = meeting_query_hash(&m.id);
    let thread_hash = m
        .thread_id
        .map(|thread_id| thread_query_hash(&thread_id.to_string()));
    let selection_reason = m
        .selection_reason
        .as_deref()
        .and_then(normalize_selection_reason);

    Some(serde_json::json!({
        "id": m.id,
        "channel_id": m.channel_id.to_string(),
        "meeting_hash": meeting_hash,
        "agenda": m.agenda,
        "summary": m.summary,
        "selection_reason": selection_reason,
        "status": status_str,
        "primary_provider": m.primary_provider.as_str(),
        "reviewer_provider": m.reviewer_provider.as_str(),
        "participant_names": participant_names,
        "total_rounds": total_rounds,
        "started_at": started_at,
        "completed_at": if m.status == MeetingStatus::Completed { serde_json::Value::from(chrono::Local::now().timestamp_millis()) } else { serde_json::Value::Null },
        "thread_id": m.thread_id.map(|t| t.to_string()),
        "thread_hash": thread_hash,
        "memory_postprocessing": memory_postprocessing_policy(),
        "entries": entries,
    }))
}

/// Persist meeting data through the internal API without going through
/// auth-protected HTTP routes.
async fn persist_meeting_status(
    payload: serde_json::Value,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let body: crate::server::routes::meetings::UpsertMeetingBody = serde_json::from_value(payload)?;
    super::internal_api::upsert_meeting(body)
        .await
        .map(|_| ())
        .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { error.into() })?;
    Ok(())
}

/// Build Markdown content for a meeting
fn build_meeting_markdown(m: &Meeting) -> String {
    let now = chrono::Local::now();
    let date_str = now.format("%Y-%m-%d").to_string();
    let datetime_str = now.format("%Y-%m-%d %H:%M").to_string();
    let total_rounds = effective_round_count(m);

    let status_str = match &m.status {
        MeetingStatus::SelectingParticipants | MeetingStatus::InProgress => "진행중",
        MeetingStatus::Concluding => "마무리중",
        MeetingStatus::Completed => "완료",
        MeetingStatus::Cancelled => "취소",
    };

    let participants_inline = m
        .participants
        .iter()
        .map(|p| p.display_name.clone())
        .collect::<Vec<_>>()
        .join(", ");

    // Build transcript grouped by rounds
    let max_round = m.transcript.iter().map(|u| u.round).max().unwrap_or(0);
    let mut transcript_sections = Vec::new();
    for round in 1..=max_round {
        let mut section = format!("### 라운드 {}\n", round);
        for u in m.transcript.iter().filter(|u| u.round == round) {
            section.push_str(&format!("\n**{}**\n\n{}\n", u.display_name, u.content));
        }
        transcript_sections.push(section);
    }

    let summary_section = m
        .summary
        .clone()
        .unwrap_or_else(|| "_회의록이 작성되지 않았습니다._".to_string());
    let selection_reason_line = m
        .selection_reason
        .as_deref()
        .and_then(normalize_selection_reason)
        .map(|reason| format!("> **선정 사유**: {reason}\n"))
        .unwrap_or_default();
    let meeting_hash = meeting_query_hash(&m.id);
    let meeting_hash_display = display_query_hash(&meeting_hash);
    let thread_id = m
        .thread_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "null".to_string());
    let thread_hash = m.thread_id.map(|id| thread_query_hash(&id.to_string()));
    let thread_hash_display = thread_hash
        .as_deref()
        .map(display_query_hash)
        .unwrap_or_else(|| "-".to_string());
    let thread_hash_frontmatter = thread_hash
        .as_deref()
        .map(|value| format!("\"{value}\""))
        .unwrap_or_else(|| "null".to_string());

    format!(
        "---\ntags: [meeting, cookingheart]\ndate: {date}\nstatus: {status}\nparticipants: [{participants}]\nagenda: \"{agenda}\"\nmeeting_id: {id}\nmeeting_hash: \"{meeting_hash}\"\nthread_id: {thread_id}\nthread_hash: {thread_hash_frontmatter}\nprimary_provider: {primary_provider}\nreviewer_provider: {reviewer_provider}\nauto_memory_write: false\nauto_memory_capture: false\nmemory_postprocessing_policy: approval_required\n---\n\n# 회의록: {agenda}\n\n> **날짜**: {datetime}\n> **참여자**: {participants}\n> **라운드**: {rounds}/{max_rounds}\n> **상태**: {status}\n> **회의 해시**: {meeting_hash_display}\n> **스레드 해시**: {thread_hash_display}\n> **진행 프로바이더**: {primary_provider}\n> **리뷰 프로바이더**: {reviewer_provider}\n{selection_reason_line}> **메모리 후처리**: 자동 memory write/capture 비활성화, 승인 기반만 허용\n\n---\n\n## 요약\n\n{summary}\n\n---\n\n## 전체 발언 기록\n\n{transcript}\n",
        date = date_str,
        status = status_str,
        participants = participants_inline,
        agenda = m.agenda,
        id = m.id,
        meeting_hash = meeting_hash,
        meeting_hash_display = meeting_hash_display,
        thread_id = thread_id,
        thread_hash_frontmatter = thread_hash_frontmatter,
        thread_hash_display = thread_hash_display,
        primary_provider = m.primary_provider.as_str(),
        reviewer_provider = m.reviewer_provider.as_str(),
        selection_reason_line = selection_reason_line,
        datetime = datetime_str,
        rounds = total_rounds,
        max_rounds = m.max_rounds,
        summary = summary_section,
        transcript = transcript_sections.join("\n"),
    )
}

/// Format transcript for inclusion in prompts
fn format_transcript(transcript: &[MeetingUtterance]) -> String {
    if transcript.is_empty() {
        return String::new();
    }
    transcript
        .iter()
        .map(|u| format!("[R{} - {}]: {}", u.round, u.display_name, u.content))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Remove meeting from active_meetings
async fn cleanup_meeting(shared: &Arc<SharedData>, channel_id: ChannelId) {
    let mut core = shared.core.lock().await;
    core.active_meetings.remove(&channel_id);
}

// ─── Command Handler ─────────────────────────────────────────────────────────

/// Handle meeting commands from Discord messages.
/// Returns true if the message was a meeting command (consumed), false otherwise.
pub(super) async fn handle_meeting_command(
    http: Arc<serenity::Http>,
    channel_id: ChannelId,
    text: &str,
    default_provider: ProviderKind,
    shared: &Arc<SharedData>,
) -> Result<bool, Error> {
    let text = text.trim().to_string();

    // /meeting start [--primary claude|codex|gemini|qwen] <agenda>
    if text.starts_with("/meeting start ") {
        let request = match parse_meeting_start_text(&text, default_provider) {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(false),
            Err(message) => {
                let _ = send_meeting_message(&http, channel_id, shared, message).await;
                return Ok(true);
            }
        };

        if request.agenda.is_empty() {
            let _ = send_meeting_message(
                &http,
                channel_id,
                shared,
                "사용법: `/meeting start [--primary claude|codex|gemini|qwen] <안건>`",
            )
            .await;
            return Ok(true);
        }

        let http_clone = http.clone();
        let shared_clone = shared.clone();
        let agenda = request.agenda.clone();
        let primary_provider = request.primary_provider.clone();
        let reviewer_provider = request.primary_provider.counterpart();

        // Spawn meeting as a background task so it doesn't block message handling
        tokio::spawn(async move {
            match start_meeting(
                &*http_clone,
                channel_id,
                &agenda,
                primary_provider,
                reviewer_provider,
                &shared_clone,
            )
            .await
            {
                Ok(Some(id)) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] ✅ Meeting completed: {id}");
                }
                Ok(None) => {}
                Err(e) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] ❌ Meeting error: {e}");
                    let _ = send_meeting_message(
                        &http_clone,
                        channel_id,
                        &shared_clone,
                        format!("❌ 회의 오류: {}", e),
                    )
                    .await;
                }
            }
        });

        return Ok(true);
    }

    // /meeting stop
    if text == "/meeting stop" {
        cancel_meeting(&*http, channel_id, shared).await?;
        return Ok(true);
    }

    // /meeting status
    if text == "/meeting status" {
        meeting_status(&*http, channel_id, shared).await?;
        return Ok(true);
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveMeetingSlot, DEFAULT_MEETING_SELECTION_STAGE_TIMEOUT_SECS,
        MAX_MEETING_STAGE_TIMEOUT_SECS, MIN_MEETING_STAGE_TIMEOUT_SECS, Meeting,
        MeetingAgentConfig, MeetingConfig, MeetingStatus, MeetingUtterance, ProviderKind,
        ResolvedMemorySettings, SummaryAgentConfig, agent_metadata_card,
        build_fallback_meeting_summary, build_meeting_markdown, build_meeting_start_status_message,
        build_meeting_status_payload, build_selection_reason_line, clamp_max_participants,
        display_query_hash, effective_round_count, meeting_cancel_event_key,
        meeting_outbound_message, meeting_query_hash, meeting_slot_state, parse_meeting_start_text,
        parse_participant_selection_response, resolve_meeting_stage_timeout_secs,
        select_participants, summary_agent_context, thread_query_hash, truncate_for_meeting,
        validate_fixed_participants,
    };
    use serde_json::json;

    #[test]
    fn test_parse_meeting_start_text_defaults_to_current_provider() {
        let parsed = parse_meeting_start_text("/meeting start 신규 안건", ProviderKind::Claude)
            .unwrap()
            .unwrap();
        assert_eq!(parsed.primary_provider, ProviderKind::Claude);
        assert_eq!(parsed.agenda, "신규 안건");
    }

    #[test]
    fn test_parse_meeting_start_text_accepts_primary_flag() {
        let parsed = parse_meeting_start_text(
            "/meeting start --primary codex 신규 안건",
            ProviderKind::Claude,
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.primary_provider, ProviderKind::Codex);
        assert_eq!(parsed.agenda, "신규 안건");
    }

    #[test]
    fn test_parse_meeting_start_text_accepts_gemini_primary_flag() {
        let parsed = parse_meeting_start_text(
            "/meeting start --primary gemini 신규 안건",
            ProviderKind::Claude,
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.primary_provider, ProviderKind::Gemini);
        assert_eq!(parsed.agenda, "신규 안건");
    }

    #[test]
    fn test_parse_meeting_start_text_accepts_qwen_primary_flag() {
        let parsed = parse_meeting_start_text(
            "/meeting start --primary qwen 신규 안건",
            ProviderKind::Claude,
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.primary_provider, ProviderKind::Qwen);
        assert_eq!(parsed.agenda, "신규 안건");
    }

    #[test]
    fn meeting_outbound_message_adds_stable_metadata_for_true_retries() {
        let channel_id = poise::serenity_prelude::ChannelId::new(42);
        let first = meeting_outbound_message(
            channel_id,
            "round 1".to_string(),
            "meeting:m-1:round:1:header",
        );
        let retry = meeting_outbound_message(
            channel_id,
            "round 1".to_string(),
            "meeting:m-1:round:1:header",
        );
        let changed = meeting_outbound_message(
            channel_id,
            "round 1 changed".to_string(),
            "meeting:m-1:round:1:header",
        );

        assert_eq!(first.correlation_id.as_deref(), Some("meeting:42"));
        assert_eq!(
            first.semantic_event_id.as_deref(),
            retry.semantic_event_id.as_deref()
        );
        assert_ne!(
            first.semantic_event_id.as_deref(),
            changed.semantic_event_id.as_deref()
        );
    }

    #[test]
    fn meeting_outbound_message_normalizes_event_keys() {
        let channel_id = poise::serenity_prelude::ChannelId::new(42);
        let message = meeting_outbound_message(channel_id, "hello".to_string(), "summary done/ok");
        let semantic = message.semantic_event_id.expect("semantic event id");
        assert!(semantic.starts_with("meeting:42:summary_done_ok:"));
    }

    #[test]
    fn meeting_cancel_event_key_includes_meeting_id() {
        let channel_id = poise::serenity_prelude::ChannelId::new(42);

        assert_ne!(
            meeting_cancel_event_key(channel_id, "meeting-one"),
            meeting_cancel_event_key(channel_id, "meeting-two")
        );
        assert_eq!(
            meeting_cancel_event_key(channel_id, "meeting-one"),
            "meeting:meeting-one:channel:42:cancelled"
        );
    }

    #[test]
    fn test_resolve_meeting_stage_timeout_uses_default_when_unset_or_invalid() {
        assert_eq!(
            resolve_meeting_stage_timeout_secs(None, DEFAULT_MEETING_SELECTION_STAGE_TIMEOUT_SECS),
            DEFAULT_MEETING_SELECTION_STAGE_TIMEOUT_SECS
        );
        assert_eq!(
            resolve_meeting_stage_timeout_secs(
                Some("not-a-number"),
                DEFAULT_MEETING_SELECTION_STAGE_TIMEOUT_SECS
            ),
            DEFAULT_MEETING_SELECTION_STAGE_TIMEOUT_SECS
        );
    }

    #[test]
    fn test_resolve_meeting_stage_timeout_clamps_within_supported_range() {
        assert_eq!(
            resolve_meeting_stage_timeout_secs(Some("15"), 90),
            MIN_MEETING_STAGE_TIMEOUT_SECS
        );
        assert_eq!(resolve_meeting_stage_timeout_secs(Some("120"), 90), 120);
        assert_eq!(
            resolve_meeting_stage_timeout_secs(Some("999"), 90),
            MAX_MEETING_STAGE_TIMEOUT_SECS
        );
    }

    fn fixture_meeting(id: &str, status: MeetingStatus) -> Meeting {
        Meeting {
            id: id.to_string(),
            channel_id: 42,
            agenda: "test".to_string(),
            primary_provider: ProviderKind::Claude,
            reviewer_provider: ProviderKind::Codex,
            selection_reason: None,
            participants: Vec::new(),
            transcript: Vec::new(),
            current_round: 0,
            max_rounds: 3,
            status,
            summary: None,
            started_at: "2026-03-06T00:00:00+09:00".to_string(),
            thread_id: None,
            msg_channel: None,
        }
    }

    #[test]
    fn test_summary_agent_context_keeps_persona_when_not_in_candidate_pool() {
        let config = MeetingConfig {
            channel_name: "meeting".to_string(),
            max_rounds: 3,
            max_participants: 5,
            summary_agent: SummaryAgentConfig::Static("pmd".to_string()),
            available_agents: Vec::new(),
        };

        let context = summary_agent_context(&config, "pmd");

        assert!(context.contains("summary_agent `pmd` is not in the meeting candidate pool"));
        assert!(context.contains("Keep this summary persona as a fallback"));
    }

    #[test]
    fn test_agent_metadata_card_marks_missing_fields_and_rich_fields() {
        let legacy = MeetingAgentConfig {
            role_id: "legacy".to_string(),
            display_name: "Legacy".to_string(),
            keywords: Vec::new(),
            prompt_file: String::new(),
            domain_summary: None,
            strengths: Vec::new(),
            task_types: Vec::new(),
            anti_signals: Vec::new(),
            provider_hint: None,
            provider: None,
            model: None,
            reasoning_effort: None,
            workspace: None,
            peer_agents_enabled: true,
            memory: ResolvedMemorySettings::default(),
        };
        let rich = MeetingAgentConfig {
            role_id: "qwen".to_string(),
            display_name: "Qwen Specialist".to_string(),
            keywords: vec!["analysis".to_string()],
            prompt_file: String::new(),
            domain_summary: Some("Deep reasoning".to_string()),
            strengths: vec!["long-context synthesis".to_string()],
            task_types: vec!["analysis".to_string()],
            anti_signals: vec!["short notification".to_string()],
            provider_hint: Some("qwen".to_string()),
            provider: Some(ProviderKind::Qwen),
            model: None,
            reasoning_effort: None,
            workspace: None,
            peer_agents_enabled: true,
            memory: ResolvedMemorySettings::default(),
        };

        let legacy_card = agent_metadata_card(&legacy);
        let rich_card = agent_metadata_card(&rich);

        assert!(legacy_card.contains("metadata_missing: [keywords, domain_summary"));
        assert!(rich_card.contains("selection_profile: Qwen Specialist | provider=Qwen"));
        assert!(rich_card.contains("domain_summary: Deep reasoning"));
        assert!(rich_card.contains("provider: Qwen"));
        assert!(rich_card.contains("metadata_missing: []"));
    }

    #[test]
    fn test_parse_participant_selection_response_compacts_reason_to_one_line() {
        let decision = parse_participant_selection_response(
            r#"{
              "selected_role_ids":["openclaw-coder","openclaw-qa"],
              "selection_reason":"선정 사유:\n코딩이(Codex)의 구현 복구와 큐에이(Codex)의 회귀 검증 강점이 안건 핵심과 맞음"
            }"#,
        )
        .expect("object response should parse");

        assert_eq!(
            decision.selected_role_ids,
            vec!["openclaw-coder".to_string(), "openclaw-qa".to_string()]
        );
        assert_eq!(
            decision.selection_reason.as_deref(),
            Some("코딩이(Codex)의 구현 복구와 큐에이(Codex)의 회귀 검증 강점이 안건 핵심과 맞음")
        );
    }

    #[test]
    fn test_parse_participant_selection_response_preserves_long_reason_without_ellipsis() {
        let decision = parse_participant_selection_response(
            r#"{
              "selected_role_ids":["openclaw-coder","openclaw-qa","openclaw-brain"],
              "selection_reason":"선정 사유: 코딩이(Codex)의 구현 복구 경험과 큐에이(Codex)의 회귀 검증 강점, 똑똑이(Claude)의 구조 리뷰 역량이 함께 필요해 이 안건의 장애 원인 추적부터 재발 방지 검토까지 한 번에 커버할 수 있어 선정"
            }"#,
        )
        .expect("object response should parse");

        let reason = decision
            .selection_reason
            .as_deref()
            .expect("selection reason");
        assert!(reason.contains("코딩이(Codex)의 구현 복구 경험"));
        assert!(reason.contains("큐에이(Codex)의 회귀 검증 강점"));
        assert!(reason.contains("똑똑이(Claude)의 구조 리뷰 역량"));
        assert!(reason.contains("재발 방지 검토까지 한 번에 커버할 수 있어 선정"));
        assert!(!reason.contains('…'));
        assert!(!reason.contains("..."));
    }

    #[test]
    fn test_build_selection_reason_line_summarizes_focus_and_fixed_mix() {
        let architecture = MeetingAgentConfig {
            role_id: "arch".to_string(),
            display_name: "Architect".to_string(),
            keywords: vec!["system".to_string()],
            prompt_file: String::new(),
            domain_summary: Some("시스템 구조 설계".to_string()),
            strengths: vec!["architecture".to_string()],
            task_types: vec!["design review".to_string()],
            anti_signals: Vec::new(),
            provider_hint: None,
            provider: None,
            model: None,
            reasoning_effort: None,
            workspace: None,
            peer_agents_enabled: true,
            memory: ResolvedMemorySettings::default(),
        };
        let qa = MeetingAgentConfig {
            role_id: "qa".to_string(),
            display_name: "QA".to_string(),
            keywords: vec!["regression".to_string()],
            prompt_file: String::new(),
            domain_summary: Some("회귀 리스크 검토".to_string()),
            strengths: vec!["bug triage".to_string()],
            task_types: vec!["regression testing".to_string()],
            anti_signals: Vec::new(),
            provider_hint: None,
            provider: None,
            model: None,
            reasoning_effort: None,
            workspace: None,
            peer_agents_enabled: true,
            memory: ResolvedMemorySettings::default(),
        };
        let config = MeetingConfig {
            channel_name: "meeting".to_string(),
            max_rounds: 3,
            max_participants: 5,
            summary_agent: SummaryAgentConfig::Static("pmd".to_string()),
            available_agents: vec![architecture.clone(), qa.clone()],
        };

        let line = build_selection_reason_line(
            &config,
            "시스템 아키텍처 설계와 회귀 테스트 전략 검토",
            &[architecture.to_participant(), qa.to_participant()],
            &["arch".to_string()],
        );

        assert!(!line.starts_with("선정 사유: "));
        assert!(line.contains("Architect("));
        assert!(line.contains("QA("));
        assert!(line.contains("시스템 구조 설계"));
        assert!(line.contains("회귀") || line.contains("regression"));
        assert!(line.contains("고정 1명은 유지한 뒤 자동 1명"));
    }

    #[test]
    fn test_build_meeting_start_status_message_places_reason_under_selection_status() {
        let message = build_meeting_start_status_message(
            "새 안건",
            "#abc123",
            Some("#def456"),
            &ProviderKind::Claude,
            &ProviderKind::Qwen,
            Some("선정 사유: architecture 커버리지를 우선했어."),
        );

        assert!(
            message.contains("참여자 선정 중...\n선정 사유: architecture 커버리지를 우선했어.")
        );
        assert!(message.contains("회의 해시: #abc123\n스레드 해시: #def456"));
    }

    #[test]
    fn test_build_meeting_start_status_message_keeps_full_reason() {
        let full_reason = "코딩이(Codex)의 구현 복구 경험과 큐에이(Codex)의 회귀 검증 강점, 똑똑이(Claude)의 구조 리뷰 역량이 함께 필요해 장애 원인 추적부터 재발 방지 검토까지 한 번에 커버할 수 있어 선정";
        let message = build_meeting_start_status_message(
            "긴 안건",
            "#abc123",
            Some("#def456"),
            &ProviderKind::Claude,
            &ProviderKind::Qwen,
            Some(full_reason),
        );

        assert!(message.contains(full_reason));
        assert!(!message.contains('…'));
        assert!(message.contains(&format!("선정 사유: {full_reason}")));
    }

    #[test]
    fn test_validate_fixed_participants_uses_clamped_max_participants() {
        let make_agent = |role_id: &str| MeetingAgentConfig {
            role_id: role_id.to_string(),
            display_name: role_id.to_string(),
            keywords: Vec::new(),
            prompt_file: String::new(),
            domain_summary: None,
            strengths: Vec::new(),
            task_types: Vec::new(),
            anti_signals: Vec::new(),
            provider_hint: None,
            provider: None,
            model: None,
            reasoning_effort: None,
            workspace: None,
            peer_agents_enabled: true,
            memory: ResolvedMemorySettings::default(),
        };
        let config = MeetingConfig {
            channel_name: "meeting".to_string(),
            max_rounds: 3,
            max_participants: 99,
            summary_agent: SummaryAgentConfig::Static("pmd".to_string()),
            available_agents: vec![
                make_agent("a"),
                make_agent("b"),
                make_agent("c"),
                make_agent("d"),
                make_agent("e"),
                make_agent("f"),
            ],
        };

        let err = validate_fixed_participants(
            &config,
            &["a", "b", "c", "d", "e", "f"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
            clamp_max_participants(config.max_participants),
        )
        .expect_err("clamped max participants should reject six fixed members");

        assert!(err.contains("Too many fixed participants: 6 (max 5)"));
    }

    #[tokio::test]
    async fn test_select_participants_skips_llm_when_fixed_participants_fill_roster() {
        let make_agent = |role_id: &str| MeetingAgentConfig {
            role_id: role_id.to_string(),
            display_name: role_id.to_string(),
            keywords: Vec::new(),
            prompt_file: format!("/tmp/{role_id}.md"),
            domain_summary: None,
            strengths: Vec::new(),
            task_types: Vec::new(),
            anti_signals: Vec::new(),
            provider_hint: None,
            provider: None,
            model: None,
            reasoning_effort: None,
            workspace: None,
            peer_agents_enabled: true,
            memory: ResolvedMemorySettings::default(),
        };
        let config = MeetingConfig {
            channel_name: "meeting".to_string(),
            max_rounds: 3,
            max_participants: 2,
            summary_agent: SummaryAgentConfig::Static("pmd".to_string()),
            available_agents: vec![make_agent("a"), make_agent("b"), make_agent("c")],
        };

        let (participants, selection_reason) = select_participants(
            &config,
            "고정 전문 에이전트만으로 시작",
            ProviderKind::Claude,
            ProviderKind::Codex,
            vec!["a".to_string(), "b".to_string()],
        )
        .await
        .expect("fixed roster should bypass provider selection");

        assert_eq!(
            participants
                .iter()
                .map(|participant| participant.role_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert!(!selection_reason.trim().is_empty());
    }

    #[test]
    fn test_meeting_slot_state_matches_current_meeting() {
        let meeting = fixture_meeting("mtg-a", MeetingStatus::InProgress);
        assert_eq!(
            meeting_slot_state(Some(&meeting), "mtg-a"),
            ActiveMeetingSlot::Active
        );
    }

    #[test]
    fn test_meeting_slot_state_detects_cancelled_current_meeting() {
        let meeting = fixture_meeting("mtg-a", MeetingStatus::Cancelled);
        assert_eq!(
            meeting_slot_state(Some(&meeting), "mtg-a"),
            ActiveMeetingSlot::Cancelled
        );
    }

    #[test]
    fn test_meeting_slot_state_detects_replaced_meeting() {
        let meeting = fixture_meeting("mtg-b", MeetingStatus::InProgress);
        assert_eq!(
            meeting_slot_state(Some(&meeting), "mtg-a"),
            ActiveMeetingSlot::MissingOrReplaced
        );
    }

    #[test]
    fn test_effective_round_count_uses_transcript_round_when_current_round_lags() {
        let mut meeting = fixture_meeting("mtg-a", MeetingStatus::Cancelled);
        meeting.current_round = 0;
        meeting.transcript.push(MeetingUtterance {
            role_id: "ch-td".to_string(),
            display_name: "TD".to_string(),
            round: 1,
            content: "late round one".to_string(),
        });

        assert_eq!(effective_round_count(&meeting), 1);
    }

    #[test]
    fn test_build_meeting_status_payload_uses_effective_round_count() {
        let mut meeting = fixture_meeting("mtg-a", MeetingStatus::Cancelled);
        meeting.current_round = 0;
        meeting.thread_id = Some(123);
        meeting.selection_reason = Some("선정 사유: 핵심 전문성 조합을 우선해 선정".to_string());
        meeting.transcript.push(MeetingUtterance {
            role_id: "ch-td".to_string(),
            display_name: "TD".to_string(),
            round: 1,
            content: "late round one".to_string(),
        });

        let payload = build_meeting_status_payload(&meeting).expect("payload");
        assert_eq!(payload.get("total_rounds"), Some(&json!(1)));
        assert_eq!(
            payload.get("meeting_hash"),
            Some(&json!(meeting_query_hash("mtg-a")))
        );
        assert_eq!(
            payload.get("thread_hash"),
            Some(&json!(thread_query_hash("123")))
        );
        assert_eq!(
            payload.get("selection_reason"),
            Some(&json!("핵심 전문성 조합을 우선해 선정"))
        );
    }

    #[test]
    fn test_build_meeting_markdown_includes_query_hashes() {
        let mut meeting = fixture_meeting("mtg-a", MeetingStatus::Completed);
        meeting.thread_id = Some(123);
        meeting.selection_reason = Some("선정 사유: 핵심 전문성 조합을 우선해 선정".to_string());

        let md = build_meeting_markdown(&meeting);
        let canonical_meeting_hash = meeting_query_hash("mtg-a");
        let canonical_thread_hash = thread_query_hash("123");

        assert!(md.contains(&format!("meeting_hash: \"{}\"", canonical_meeting_hash)));
        assert!(md.contains("thread_id: 123"));
        assert!(md.contains(&format!("thread_hash: \"{}\"", canonical_thread_hash)));
        assert!(md.contains(&format!(
            "> **회의 해시**: {}",
            display_query_hash(&canonical_meeting_hash)
        )));
        assert!(md.contains(&format!(
            "> **스레드 해시**: {}",
            display_query_hash(&canonical_thread_hash)
        )));
        assert!(md.contains("> **선정 사유**: 핵심 전문성 조합을 우선해 선정"));
    }

    #[test]
    fn test_truncate_for_meeting_preserves_utf8_boundaries() {
        let agenda = "멀티바이트🙂문자를 포함한 아주 긴 회의 안건 제목";
        let truncated = truncate_for_meeting(agenda, 8);

        assert_eq!(truncated, "멀티바이트🙂문자...");
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    #[test]
    fn test_build_fallback_meeting_summary_uses_consensus_and_format() {
        let transcript = vec![
            MeetingUtterance {
                role_id: "qa".to_string(),
                display_name: "큐에이 | QA".to_string(),
                round: 1,
                content: "핵심 회귀 테스트 범위를 먼저 고정해야 합니다.\nCONSENSUS: 실패 케이스 매트릭스를 먼저 명세한다.".to_string(),
            },
            MeetingUtterance {
                role_id: "coder".to_string(),
                display_name: "코딩이 | Coder".to_string(),
                round: 1,
                content: "fixture, seed, snapshot 계약을 테스트에 넣어야 합니다.".to_string(),
            },
        ];

        let summary = build_fallback_meeting_summary(
            "회귀 테스트 설계 점검",
            "큐에이 | QA, 코딩이 | Coder",
            &transcript,
        );

        assert!(summary.contains("### 📋 회의록: 회귀 테스트 설계 점검"));
        assert!(summary.contains("**참여자**: 큐에이 | QA, 코딩이 | Coder"));
        assert!(summary.contains("#### 주요 논의"));
        assert!(summary.contains("#### 결론"));
        assert!(summary.contains("실패 케이스 매트릭스를 먼저 명세한다."));
        assert!(summary.contains("#### Action Items"));
    }
}
