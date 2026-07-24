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
use super::meeting_artifact_store::{
    self, MeetingArtifactKind, MeetingArtifactRepo, StoreOutcome,
};
use super::meeting_state_machine::{self as msm, MeetingEvent, MeetingState};
use super::{internal_api, runtime_store};
use super::org_schema;
use super::outbound::delivery::{deliver_outbound, first_raw_message_id};
use super::outbound::message::{OutboundOperation, OutboundTarget};
use super::outbound::{
    DeliveryResult, DiscordOutboundClient, DiscordOutboundMessage, DiscordOutboundPolicy,
    outbound_fingerprint, shared_outbound_deduper,
};
use super::role_map::load_meeting_config as load_meeting_config_from_role_map;
use super::settings::{ResolvedMemorySettings, RoleBinding, load_role_prompt};
use super::{DispatchProfile, SharedData, rate_limit_wait};
use crate::services::dispatches::discord_delivery::{
    DispatchMessagePostError, DispatchMessagePostErrorKind,
};

mod lifecycle;
mod records;
mod rounds;
mod selection;
mod selection_runtime;

pub(super) use lifecycle::{
    cancel_meeting, load_meeting_config, meeting_status, spawn_direct_start, start_meeting,
};
pub(in crate::services::discord) use selection_runtime::send_meeting_message;
pub(crate) use selection_runtime::list_available_agent_options;

use records::{
    build_meeting_status_payload, check_consensus, cleanup_meeting, conclude_meeting,
    format_transcript, persist_meeting_status, save_meeting_record,
};
use rounds::{run_meeting_round, select_participants};
use selection::{
    agent_metadata_card, build_meeting_start_status_message, clamp_max_participants,
    compact_selection_reason, normalize_selection_reason, summary_agent_context,
};
use selection_runtime::{
    archive_meeting_thread, build_fallback_meeting_summary, build_selection_reason_line,
    create_meeting_thread, edit_meeting_message, execute_provider_stage,
    fixed_participant_prompt_lines, meeting_selection_stage_timeout_secs,
    merge_selected_participants, normalize_role_ids, parse_participant_selection_response,
    send_meeting_message_with_event, truncate_for_meeting, validate_fixed_participants,
};

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
    // #3034: config field carried from the org-schema/role-map meeting
    // section; no in-code reader after the name-match helper was removed.
    #[allow(dead_code)]
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

fn parse_primary_provider_arg(
    raw: Option<&str>,
    default_provider: ProviderKind,
) -> Result<ProviderKind, String> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => match ProviderKind::from_str(value) {
            Some(provider) if provider.is_supported() => Ok(provider),
            _ => Err(format!(
                "지원하지 않는 provider야: `{}` (`claude`, `codex`, `gemini`, `opencode`, `qwen` 중 하나여야 함)",
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
            "사용법: `/meeting start [--primary claude|codex|gemini|opencode|qwen] <안건>`"
                .to_string(),
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
            "사용법: `/meeting start [--primary claude|codex|gemini|opencode|qwen] <안건>`"
                .to_string(),
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

    // /meeting start [--primary claude|codex|gemini|opencode|qwen] <agenda>
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
                "사용법: `/meeting start [--primary claude|codex|gemini|opencode|qwen] <안건>`",
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
