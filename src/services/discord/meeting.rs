use std::collections::HashSet;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};
use serenity::{
    AutoArchiveDuration, ChannelId, ChannelType, CreateMessage,
    builder::{CreateThread, EditThread},
};

use crate::services::memory::{RecallRequest, build_memory_backend};
use crate::services::provider::ProviderKind;
use crate::services::provider_exec;

use super::formatting::send_long_message_raw;
use super::org_schema;
use super::prompt_builder::DispatchProfile;
use super::role_map::load_meeting_config as load_meeting_config_from_role_map;
use super::settings::{RoleBinding, load_longterm_memory_catalog, load_role_prompt};
use super::{SharedData, rate_limit_wait};

// ─── Data Structures ─────────────────────────────────────────────────────────

const MIN_MEETING_PARTICIPANTS: usize = 2;
const MAX_MEETING_PARTICIPANTS: usize = 5;
const MEETING_READONLY_ALLOWED_TOOLS: &[&str] = &["Read"];
const MEETING_SELECTION_STAGE_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct MeetingExpertOption {
    pub role_id: String,
    pub display_name: String,
    pub keywords: Vec<String>,
    pub domain_summary: Option<String>,
    pub strengths: Vec<String>,
    pub task_types: Vec<String>,
    pub anti_signals: Vec<String>,
    pub provider_hint: Option<String>,
    pub metadata_missing: bool,
    pub metadata_confidence: String,
}

#[derive(Clone, Debug)]
pub(super) struct MeetingParticipant {
    pub role_id: String,
    pub display_name: String,
    pub binding: RoleBinding,
    pub workspace: Option<String>,
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

pub(super) struct Meeting {
    pub id: String,
    pub agenda: String,
    pub primary_provider: ProviderKind,
    pub reviewer_provider: ProviderKind,
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
    pub summary_agent: SummaryAgentConfig,
    pub available_agents: Vec<MeetingAgentConfig>,
    pub agent_registry: Vec<MeetingAgentConfig>,
}

#[derive(Clone, Debug)]
pub(super) struct MeetingAgentConfig {
    pub role_id: String,
    pub display_name: String,
    pub keywords: Vec<String>,
    pub domain_summary: Option<String>,
    pub strengths: Vec<String>,
    pub task_types: Vec<String>,
    pub anti_signals: Vec<String>,
    pub provider_hint: Option<String>,
    pub metadata_missing: bool,
    pub metadata_confidence: String,
    pub binding: RoleBinding,
    pub workspace: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MeetingSelectionResponse {
    #[serde(default)]
    selected_role_ids: Vec<String>,
    #[serde(default)]
    selected: Vec<MeetingSelectionChoice>,
}

#[derive(Debug, Deserialize)]
struct MeetingSelectionChoice {
    role_id: String,
}

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

/// Generate a unique meeting ID (timestamp + random hex)
fn generate_meeting_id() -> String {
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let random: u32 = rand::Rng::r#gen(&mut rand::thread_rng());
    format!("mtg-{}-{:08x}", ts, random)
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

fn parse_selected_role_ids_response(text: &str) -> Result<Vec<String>, String> {
    if let Ok(role_ids) = parse_json_array_fragment(text) {
        return Ok(role_ids);
    }

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

    let parsed = serde_json::from_str::<MeetingSelectionResponse>(json_str)
        .map_err(|e| format!("Failed to parse meeting selection JSON: {}", e))?;

    let selected_role_ids = if parsed.selected_role_ids.is_empty() {
        parsed
            .selected
            .into_iter()
            .map(|entry| entry.role_id)
            .collect::<Vec<_>>()
    } else {
        parsed.selected_role_ids
    };

    if selected_role_ids.is_empty() {
        return Err("Meeting selection JSON did not include any selected role IDs".to_string());
    }

    Ok(selected_role_ids)
}

fn truncate_for_meeting(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    trimmed.chars().take(max_chars).collect::<String>() + "..."
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

/// Load meeting config from org.yaml or role_map.json "meeting" section
pub(super) fn load_meeting_config() -> Option<MeetingConfig> {
    if org_schema::org_schema_exists() {
        if let Some(cfg) = org_schema::load_meeting_config() {
            return Some(cfg);
        }
    }
    load_meeting_config_from_role_map()
}

pub(crate) fn load_meeting_expert_options() -> Vec<MeetingExpertOption> {
    load_meeting_config()
        .map(|config| {
            config
                .available_agents
                .into_iter()
                .map(|agent| MeetingExpertOption {
                    role_id: agent.role_id,
                    display_name: agent.display_name,
                    keywords: agent.keywords,
                    domain_summary: agent.domain_summary,
                    strengths: agent.strengths,
                    task_types: agent.task_types,
                    anti_signals: agent.anti_signals,
                    provider_hint: agent.provider_hint,
                    metadata_missing: agent.metadata_missing,
                    metadata_confidence: agent.metadata_confidence,
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn derive_agent_metadata_quality(
    domain_summary: Option<&str>,
    strengths: &[String],
    task_types: &[String],
    anti_signals: &[String],
) -> (bool, String) {
    let has_domain_summary = domain_summary
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some();
    let has_strengths = !strengths.is_empty();
    let has_task_types = !task_types.is_empty();
    let has_anti_signals = !anti_signals.is_empty();

    let core_count =
        usize::from(has_domain_summary) + usize::from(has_strengths) + usize::from(has_task_types);

    let metadata_confidence = if core_count == 3 && has_anti_signals {
        "high"
    } else if core_count >= 2 {
        "medium"
    } else {
        "low"
    };

    (
        metadata_confidence != "high",
        metadata_confidence.to_string(),
    )
}

fn normalize_role_id_list(values: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    let mut seen = HashSet::new();

    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        if seen.insert(trimmed.to_string()) {
            normalized.push(trimmed.to_string());
        }
    }

    normalized
}

pub(crate) fn validate_fixed_participant_role_ids(
    primary_provider: &ProviderKind,
    reviewer_provider: &ProviderKind,
    requested_role_ids: &[String],
) -> Result<Vec<String>, String> {
    let normalized = normalize_role_id_list(requested_role_ids);
    if normalized.len() > MAX_MEETING_PARTICIPANTS {
        return Err(format!(
            "fixed_participants cannot exceed {} experts",
            MAX_MEETING_PARTICIPANTS
        ));
    }

    let config =
        load_meeting_config().ok_or("Meeting config not found in org.yaml or role_map.json")?;
    let candidate_pool = canonical_candidate_pool(&config, primary_provider, reviewer_provider);

    for role_id in &normalized {
        if !candidate_pool.iter().any(|agent| agent.role_id == *role_id) {
            return Err(format!(
                "fixed participant '{}' is not available for meetings",
                role_id
            ));
        }
    }

    Ok(normalized)
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

pub(crate) async fn start_meeting_with_reviewer(
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
                agenda: agenda.to_string(),
                primary_provider: primary_provider.clone(),
                reviewer_provider: reviewer_provider.clone(),
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
    let thread_name = format!("Meeting: {}", &agenda[..agenda.len().min(90)]);
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

    rate_limit_wait(shared, msg_channel).await;
    let _ = msg_channel
        .send_message(
            http,
            CreateMessage::new().content(format!(
                "📋 **라운드 테이블 회의 시작**\n안건: {}\n진행자: {}\n리뷰어: {}\n{}\n전문 에이전트 선정 중...",
                agenda,
                primary_provider.display_name(),
                reviewer_provider.display_name(),
                if fixed_participants.is_empty() {
                    "고정: 없음".to_string()
                } else {
                    format!("고정: {}", fixed_participants.join(", "))
                }
            )),
        )
        .await;

    // Select participants via primary provider + reviewer cross-check
    let participants = match select_participants(
        http,
        shared,
        channel_id,
        msg_channel,
        &meeting_id,
        &config,
        agenda,
        primary_provider,
        reviewer_provider,
        &fixed_participants,
    )
    .await
    {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => {
            send_meeting_selection_status(
                http,
                shared,
                msg_channel,
                "❌ **참가자 선정 실패**\nreason: 참여자를 선정하지 못했어.",
            )
            .await;
            cleanup_meeting_if_current(shared, channel_id, &meeting_id).await;
            return Err("참여자를 선정하지 못했어.".into());
        }
        Err(e) => {
            send_meeting_selection_status(
                http,
                shared,
                msg_channel,
                format!("❌ **참가자 선정 실패**\nreason: {}", e),
            )
            .await;
            cleanup_meeting_if_current(shared, channel_id, &meeting_id).await;
            return Err(format!("참여자 선정 실패: {}", e).into());
        }
    };

    // Check if cancelled or replaced during participant selection
    if active_meeting_state(shared, channel_id, &meeting_id).await != ActiveMeetingSlot::Active {
        cleanup_meeting_if_current(shared, channel_id, &meeting_id).await;
        return Ok(None);
    }

    // Announce participants
    let participant_list: Vec<String> = participants
        .iter()
        .map(|p| format!("• {}", p.display_name))
        .collect();
    rate_limit_wait(shared, msg_channel).await;
    let _ = msg_channel
        .send_message(
            http,
            CreateMessage::new().content(format!(
                "👥 **전문 에이전트 확정** ({}명)\n{}",
                participants.len(),
                participant_list.join("\n")
            )),
        )
        .await;

    // Update meeting state and notify ADK
    let adk_payload = {
        let mut core = shared.core.lock().await;
        match core.active_meetings.get_mut(&channel_id) {
            Some(m) if m.id == meeting_id => {
                m.participants = participants;
                m.status = MeetingStatus::InProgress;
                build_meeting_status_payload(m)
            }
            _ => return Ok(None),
        }
    };

    // POST in_progress status to own HTTP server so office view can show active meeting
    if let Some(payload) = adk_payload {
        let port = shared.api_port;
        tokio::spawn(async move {
            let _ = post_meeting_status(payload, port).await;
        });
    }

    // Run meeting rounds
    let max_rounds = config.max_rounds;
    for round in 1..=max_rounds {
        if active_meeting_state(shared, channel_id, &meeting_id).await != ActiveMeetingSlot::Active
        {
            cleanup_meeting_if_current(shared, channel_id, &meeting_id).await;
            return Ok(None);
        }

        rate_limit_wait(shared, msg_channel).await;
        let _ = msg_channel
            .send_message(
                http,
                CreateMessage::new()
                    .content(format!("─── **라운드 {}/{}** ───", round, max_rounds)),
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
            rate_limit_wait(shared, msg_channel).await;
            let _ = msg_channel
                .send_message(
                    http,
                    CreateMessage::new().content("✅ **합의 도달! 회의를 마무리할게.**"),
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

    // Archive the meeting thread if one was created
    if msg_channel != channel_id {
        archive_meeting_thread(http, msg_channel).await;
    }

    // Clean up
    cleanup_meeting_if_current(shared, channel_id, &meeting_id).await;

    Ok(Some(meeting_id))
}

pub(crate) async fn spawn_direct_start(
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

    if load_meeting_config().is_none() {
        return Err("Meeting config not found in org.yaml or role_map.json".to_string());
    }

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
                rate_limit_wait(&shared, channel_id).await;
                let _ = channel_id
                    .send_message(
                        &*http,
                        CreateMessage::new().content(format!("❌ 회의 오류: {}", error)),
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
            m.status = MeetingStatus::Cancelled;
            let mc = m.msg_channel.map(ChannelId::new).unwrap_or(channel_id);
            Some(mc)
        } else {
            None
        }
    };

    if let Some(mc) = meeting_info {
        // Save whatever transcript we have
        let _ = save_meeting_record(shared, channel_id, None).await;
        cleanup_meeting(shared, channel_id).await;
        rate_limit_wait(shared, mc).await;
        let _ = mc
            .send_message(
                http,
                CreateMessage::new()
                    .content("🛑 **회의가 취소됐어.** 현재까지 트랜스크립트가 저장됐어."),
            )
            .await;
        Ok(())
    } else {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .send_message(http, CreateMessage::new().content("진행 중인 회의가 없어."))
            .await;
        Ok(())
    }
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

    rate_limit_wait(shared, channel_id).await;
    match info {
        Some((agenda, round, max_rounds, participants, utterances, status, primary, reviewer)) => {
            let status_str = match status {
                MeetingStatus::SelectingParticipants => "참여자 선정 중",
                MeetingStatus::InProgress => "진행 중",
                MeetingStatus::Concluding => "마무리 중",
                MeetingStatus::Completed => "완료",
                MeetingStatus::Cancelled => "취소됨",
            };
            let _ = channel_id
                .send_message(
                    http,
                    CreateMessage::new().content(format!(
                        "📊 **회의 현황**\n안건: {}\n상태: {}\n진행자: {} / 리뷰어: {}\n라운드: {}/{}\n전문 에이전트: {}명\n발언: {}개",
                        agenda,
                        status_str,
                        primary.display_name(),
                        reviewer.display_name(),
                        round,
                        max_rounds,
                        participants,
                        utterances
                    )),
                )
                .await;
        }
        None => {
            let _ = channel_id
                .send_message(http, CreateMessage::new().content("진행 중인 회의가 없어."))
                .await;
        }
    }
    Ok(())
}

// ─── Internal Functions ──────────────────────────────────────────────────────

#[derive(Debug)]
struct MeetingExpertRuntime {
    provider: ProviderKind,
    request: provider_exec::StructuredExecRequest,
}

fn meeting_readonly_tools() -> Vec<String> {
    MEETING_READONLY_ALLOWED_TOOLS
        .iter()
        .map(|tool| (*tool).to_string())
        .collect()
}

fn default_meeting_working_dir() -> String {
    std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "/".to_string())
}

fn build_meeting_memory_context(
    shared_knowledge: Option<&str>,
    external_recall: Option<&str>,
    longterm_catalog: Option<&str>,
) -> String {
    let mut sections = Vec::new();

    if let Some(shared_knowledge) = shared_knowledge
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        sections.push(format!("[Shared Knowledge]\n{}", shared_knowledge));
    }

    if let Some(external_recall) = external_recall
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        sections.push(format!("[Recall]\n{}", external_recall));
    }

    if let Some(longterm_catalog) = longterm_catalog
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        sections.push(format!("[Long-term Memory Catalog]\n{}", longterm_catalog));
    }

    sections.join("\n\n")
}

fn build_meeting_expert_system_prompt(
    participant: &MeetingParticipant,
    role_context: &str,
    memory_context: &str,
) -> String {
    let mut sections = vec![format!(
        "당신은 라운드 테이블 회의에 참여하는 전문 에이전트 `{}` ({})이다.\n\
         - 당신은 회의 운영자(진행자/리뷰어)가 아니라 실제 참가자다.\n\
         - 회의 중에는 `meeting_readonly` 정책을 따른다.\n\
         - memory write, capture, 파일 수정, 외부 상태 변경, 자동 저장은 금지다.\n\
         - 제공된 역할 프롬프트와 recall을 근거로만 발언하라.\n\
         - 답변은 한국어로 작성하라.",
        participant.display_name, participant.role_id
    )];

    if !role_context.trim().is_empty() {
        sections.push(format!("[Role Prompt]\n{}", role_context.trim()));
    }

    if !memory_context.trim().is_empty() {
        sections.push(memory_context.trim().to_string());
    }

    sections.join("\n\n")
}

async fn build_meeting_expert_runtime(
    participant: &MeetingParticipant,
    channel_id: ChannelId,
    meeting_id: &str,
    agenda: &str,
    round: u32,
    transcript: &str,
    fallback_provider: &ProviderKind,
) -> MeetingExpertRuntime {
    let provider = participant
        .binding
        .provider
        .clone()
        .unwrap_or_else(|| fallback_provider.clone());

    let role_context = load_role_prompt(&participant.binding).unwrap_or_default();
    let (_, memory_backend) = build_memory_backend(Some(&participant.binding));
    let memory_recall = memory_backend
        .recall(RecallRequest {
            provider: provider.clone(),
            role_id: participant.role_id.clone(),
            channel_id: channel_id.get(),
            session_id: meeting_id.to_string(),
            dispatch_profile: DispatchProfile::Full,
            user_text: format!(
                "안건: {agenda}\n현재 라운드: {round}\n이전 발언 기록:\n{transcript}",
                agenda = agenda,
                round = round,
                transcript = if transcript.is_empty() {
                    "(아직 발언 없음)"
                } else {
                    transcript
                },
            ),
        })
        .await;

    for warning in &memory_recall.warnings {
        tracing::warn!(
            "[meeting] recall warning for participant {}: {}",
            participant.role_id,
            warning
        );
    }

    let longterm_catalog = memory_recall
        .longterm_catalog
        .clone()
        .or_else(|| load_longterm_memory_catalog(&participant.role_id));
    let memory_context = build_meeting_memory_context(
        memory_recall.shared_knowledge.as_deref(),
        memory_recall.external_recall.as_deref(),
        longterm_catalog.as_deref(),
    );

    let mut request = provider_exec::StructuredExecRequest::new(
        participant
            .workspace
            .clone()
            .unwrap_or_else(default_meeting_working_dir),
    );
    request.system_prompt = Some(build_meeting_expert_system_prompt(
        participant,
        &role_context,
        &memory_context,
    ));
    request.allowed_tools = meeting_readonly_tools();
    request.model = participant.binding.model.clone();

    MeetingExpertRuntime { provider, request }
}

async fn execute_meeting_expert_prompt(
    participant: &MeetingParticipant,
    channel_id: ChannelId,
    meeting_id: &str,
    agenda: &str,
    round: u32,
    transcript: &str,
    fallback_provider: &ProviderKind,
    prompt: String,
) -> Result<String, String> {
    let runtime = build_meeting_expert_runtime(
        participant,
        channel_id,
        meeting_id,
        agenda,
        round,
        transcript,
        fallback_provider,
    )
    .await;

    provider_exec::execute_structured(runtime.provider, prompt, runtime.request)
        .await
        .map(|text| truncate_for_meeting(&text, 1500))
}

fn canonical_candidate_pool<'a>(
    config: &'a MeetingConfig,
    primary_provider: &ProviderKind,
    reviewer_provider: &ProviderKind,
) -> Vec<&'a MeetingAgentConfig> {
    let primary_role_id = primary_provider.as_str();
    let reviewer_role_id = reviewer_provider.as_str();

    config
        .available_agents
        .iter()
        .filter(|agent| agent.role_id != primary_role_id && agent.role_id != reviewer_role_id)
        .collect()
}

fn join_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "없음".to_string()
    } else {
        values.join(", ")
    }
}

fn format_optional_or_none(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("없음")
        .to_string()
}

fn build_selection_candidate_card(agent: &MeetingAgentConfig, fixed: bool) -> String {
    let bound_provider = agent
        .binding
        .provider
        .as_ref()
        .map(ProviderKind::as_str)
        .unwrap_or("unspecified");

    format!(
        "- role_id: {role_id}\n  display_name: {display_name}\n  fixed: {fixed}\n  bound_provider: {bound_provider}\n  provider_hint: {provider_hint}\n  metadata_missing: {metadata_missing}\n  metadata_confidence: {metadata_confidence}\n  keywords: {keywords}\n  domain_summary: {domain_summary}\n  strengths: {strengths}\n  task_types: {task_types}\n  anti_signals: {anti_signals}",
        role_id = agent.role_id,
        display_name = agent.display_name,
        fixed = if fixed { "yes" } else { "no" },
        bound_provider = bound_provider,
        provider_hint = format_optional_or_none(agent.provider_hint.as_deref()),
        metadata_missing = if agent.metadata_missing { "yes" } else { "no" },
        metadata_confidence = agent.metadata_confidence,
        keywords = join_or_none(&agent.keywords),
        domain_summary = format_optional_or_none(agent.domain_summary.as_deref()),
        strengths = join_or_none(&agent.strengths),
        task_types = join_or_none(&agent.task_types),
        anti_signals = join_or_none(&agent.anti_signals),
    )
}

fn build_meeting_participants(
    candidate_pool: &[&MeetingAgentConfig],
    selected_role_ids: &[String],
    required_role_ids: &[String],
) -> Result<Vec<MeetingParticipant>, String> {
    let normalized_selected = normalize_role_id_list(selected_role_ids);

    if normalized_selected.len() < MIN_MEETING_PARTICIPANTS
        || normalized_selected.len() > MAX_MEETING_PARTICIPANTS
    {
        return Err(format!(
            "Invalid participant count after cross-check: {}",
            normalized_selected.len()
        ));
    }

    for required_role_id in required_role_ids {
        if !normalized_selected
            .iter()
            .any(|role_id| role_id == required_role_id)
        {
            return Err(format!(
                "Fixed participant '{}' is missing from final selection",
                required_role_id
            ));
        }
    }

    normalized_selected
        .into_iter()
        .map(|role_id| {
            let agent = candidate_pool
                .iter()
                .find(|candidate| candidate.role_id == role_id)
                .ok_or_else(|| format!("Unknown meeting participant '{}'", role_id))?;

            Ok(MeetingParticipant {
                role_id: agent.role_id.clone(),
                display_name: agent.display_name.clone(),
                binding: agent.binding.clone(),
                workspace: agent.workspace.clone(),
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectionResolution {
    selected_role_ids: Vec<String>,
    warning_message: Option<String>,
}

fn fallback_to_initial_selection(
    initial_selected: &[String],
    stage_label: &str,
    error: &str,
) -> SelectionResolution {
    SelectionResolution {
        selected_role_ids: normalize_role_id_list(initial_selected),
        warning_message: Some(format!(
            "⚠️ **{} 실패**\nreason: {}\n1차 선정안으로 계속 진행합니다.",
            stage_label, error
        )),
    }
}

fn resolve_final_selection(
    initial_selected: &[String],
    final_response: Result<String, String>,
) -> SelectionResolution {
    match final_response {
        Ok(text) => match parse_selected_role_ids_response(&text) {
            Ok(selected_role_ids) => SelectionResolution {
                selected_role_ids,
                warning_message: None,
            },
            Err(error) => fallback_to_initial_selection(
                initial_selected,
                "전문 에이전트 최종 확정",
                &format!("응답 파싱 실패: {}", error),
            ),
        },
        Err(error) => {
            fallback_to_initial_selection(initial_selected, "전문 에이전트 최종 확정", &error)
        }
    }
}

async fn send_meeting_selection_status(
    http: &serenity::Http,
    shared: &Arc<SharedData>,
    msg_channel: ChannelId,
    content: impl Into<String>,
) {
    rate_limit_wait(shared, msg_channel).await;
    let _ = msg_channel
        .send_message(http, CreateMessage::new().content(content.into()))
        .await;
}

async fn execute_selection_stage(
    http: &serenity::Http,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    msg_channel: ChannelId,
    meeting_id: &str,
    provider: ProviderKind,
    stage_key: &str,
    stage_label: &str,
    prompt: String,
) -> Result<String, String> {
    tracing::info!(
        "[meeting] selection stage start meeting_id={} channel_id={} stage={} provider={}",
        meeting_id,
        channel_id.get(),
        stage_key,
        provider.as_str()
    );
    send_meeting_selection_status(
        http,
        shared,
        msg_channel,
        format!(
            "⏳ **{}**\nprovider: {}",
            stage_label,
            provider.display_name()
        ),
    )
    .await;

    let started_at = Instant::now();
    let response = provider_exec::execute_simple_with_timeout(
        provider.clone(),
        prompt,
        MEETING_SELECTION_STAGE_TIMEOUT,
        &format!("meeting selection stage `{}`", stage_key),
    )
    .await;
    let elapsed_ms = started_at.elapsed().as_millis();

    match response {
        Ok(text) => {
            tracing::info!(
                "[meeting] selection stage completed meeting_id={} channel_id={} stage={} provider={} elapsed_ms={}",
                meeting_id,
                channel_id.get(),
                stage_key,
                provider.as_str(),
                elapsed_ms
            );
            Ok(text)
        }
        Err(error) => {
            tracing::error!(
                "[meeting] selection stage failed meeting_id={} channel_id={} stage={} provider={} elapsed_ms={} error={}",
                meeting_id,
                channel_id.get(),
                stage_key,
                provider.as_str(),
                elapsed_ms,
                error
            );
            Err(format!("{} 실패: {}", stage_label, error))
        }
    }
}

/// Select participants using primary provider + reviewer micro cross-check.
async fn select_participants(
    http: &serenity::Http,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    msg_channel: ChannelId,
    meeting_id: &str,
    config: &MeetingConfig,
    agenda: &str,
    primary_provider: ProviderKind,
    reviewer_provider: ProviderKind,
    fixed_participant_role_ids: &[String],
) -> Result<Vec<MeetingParticipant>, String> {
    let candidate_pool = canonical_candidate_pool(config, &primary_provider, &reviewer_provider);
    let fixed_role_ids = normalize_role_id_list(fixed_participant_role_ids);

    for fixed_role_id in &fixed_role_ids {
        if !candidate_pool
            .iter()
            .any(|agent| agent.role_id == *fixed_role_id)
        {
            return Err(format!(
                "Fixed participant '{}' is not available in candidate pool",
                fixed_role_id
            ));
        }
    }

    let fixed_desc = if fixed_role_ids.is_empty() {
        "없음".to_string()
    } else {
        fixed_role_ids
            .iter()
            .filter_map(|role_id| {
                candidate_pool
                    .iter()
                    .find(|agent| agent.role_id == *role_id)
                    .map(|agent| format!("{} ({})", agent.role_id, agent.display_name))
            })
            .collect::<Vec<_>>()
            .join(", ")
    };

    let agents_desc: Vec<String> = candidate_pool
        .iter()
        .map(|agent| {
            build_selection_candidate_card(
                agent,
                fixed_role_ids
                    .iter()
                    .any(|role_id| role_id == &agent.role_id),
            )
        })
        .collect();

    let selection_prompt = format!(
        r#"다음 안건에 대한 라운드 테이블 회의에 참여할 전문 에이전트를 선정해줘.

안건: {}

고정 전문 에이전트:
{}

사용 가능한 전문 에이전트:
{}

내부적으로 반드시 다음 순서로 판단하라:
1. 안건을 한두 문장으로 해석한다.
2. 이 안건을 다루는 데 필요한 전문성 축을 2~4개 도출한다.
3. 각 후보를 전문성 축, strengths, task_types, anti_signals, provider_hint, metadata_missing 여부로 비교한다.
4. 고정 전문 에이전트를 포함한 최종 조합을 고른다.

선정 규칙:
- 고정 전문 에이전트는 모두 포함
- 최종 결과는 2~5명
- 서로 다른 전문성 축을 균형 있게 커버하라
- `metadata_missing=yes` 후보는 꼭 필요할 때만 포함하라
- anti_signals와 안건이 강하게 충돌하는 후보는 제외하라
- provider_hint는 보조 신호로만 쓰고, 실제 역할 적합성이 더 중요하다

응답 형식:
- JSON만 출력, 다른 텍스트 금지
- 형식:
  {{
    "agenda_summary": "짧은 요약",
    "required_axes": [
      {{"axis": "축1", "reason": "왜 필요한지", "priority": "high"}}
    ],
    "selected": [
      {{
        "role_id": "role_id1",
        "why_selected": "선정 이유",
        "covers_axes": ["축1"],
        "fixed": true,
        "confidence": "high"
      }}
    ],
    "rejected": [
      {{"role_id": "role_id3", "reason": "제외 이유"}}
    ],
    "selection_gaps": []
  }}"#,
        agenda,
        fixed_desc,
        agents_desc.join("\n")
    );

    let initial_response = execute_selection_stage(
        http,
        shared,
        channel_id,
        msg_channel,
        meeting_id,
        primary_provider.clone(),
        "initial_selection",
        "전문 에이전트 1차 선정",
        selection_prompt,
    )
    .await?;
    let initial_selected = parse_selected_role_ids_response(&initial_response)?;

    let review_prompt = format!(
        r#"당신은 회의 참가자 선정을 비판적으로 검토하는 리뷰어다.

안건: {agenda}

반드시 포함해야 하는 고정 전문 에이전트:
{fixed}

사용 가능한 전문 에이전트:
{agents}

현재 선정안:
{current}

검토 규칙:
- 빠진 전문성 축, 과한 중복, 안건과의 부적합만 짚어라
- 고정 전문 에이전트 누락 여부를 먼저 확인하라
- metadata_missing 후보가 불필요하게 많이 포함됐는지 확인하라
- anti_signals와 안건의 충돌이 있는지 확인하라
- 4개 이하 bullet만 사용하라
- 전체를 다시 쓰지 말고, 비판적으로만 검토하라
- 도구나 명령 실행은 하지 마라"#,
        agenda = agenda,
        fixed = fixed_desc,
        agents = agents_desc.join("\n"),
        current = serde_json::to_string(&initial_selected).unwrap_or_else(|_| "[]".to_string()),
    );

    let review_notes = execute_selection_stage(
        http,
        shared,
        channel_id,
        msg_channel,
        meeting_id,
        reviewer_provider.clone(),
        "selection_review",
        "전문 에이전트 선정 리뷰",
        review_prompt,
    )
    .await;

    let review_notes = match review_notes {
        Ok(text) => text,
        Err(error) => {
            let fallback =
                fallback_to_initial_selection(&initial_selected, "전문 에이전트 선정 리뷰", &error);
            tracing::warn!(
                "[meeting] selection review fallback meeting_id={} channel_id={} warning={}",
                meeting_id,
                channel_id.get(),
                fallback.warning_message.as_deref().unwrap_or_default()
            );
            if let Some(warning) = fallback.warning_message.as_deref() {
                send_meeting_selection_status(http, shared, msg_channel, warning).await;
            }
            return build_meeting_participants(
                &candidate_pool,
                &fallback.selected_role_ids,
                &fixed_role_ids,
            );
        }
    };

    let finalize_prompt = format!(
        r#"다음 안건에 대한 회의 참가자 선정을 최종 확정해줘.

안건: {agenda}

반드시 포함해야 하는 고정 전문 에이전트:
{fixed}

사용 가능한 전문 에이전트:
{agents}

초기 선정안:
{initial}

교차검증 리뷰:
{review}

규칙:
- 리뷰가 타당하면 반영하고, 타당하지 않으면 유지하라
- 고정 전문 에이전트는 모두 포함해야 한다
- 최종 결과는 2~5명이어야 한다
- 전문성 축 커버가 겹치기만 하지 않도록 균형을 맞춰라
- JSON만 응답하라
- 형식:
  {{
    "agenda_summary": "짧은 요약",
    "required_axes": [
      {{"axis": "축1", "reason": "왜 필요한지", "priority": "high"}}
    ],
    "selected": [
      {{
        "role_id": "role_id1",
        "why_selected": "선정 이유",
        "covers_axes": ["축1"],
        "fixed": true,
        "confidence": "high"
      }}
    ],
    "rejected": [
      {{"role_id": "role_id3", "reason": "제외 이유"}}
    ],
    "selection_gaps": []
  }}"#,
        agenda = agenda,
        fixed = fixed_desc,
        agents = agents_desc.join("\n"),
        initial = serde_json::to_string(&initial_selected).unwrap_or_else(|_| "[]".to_string()),
        review = review_notes.trim(),
    );

    let final_resolution = resolve_final_selection(
        &initial_selected,
        execute_selection_stage(
            http,
            shared,
            channel_id,
            msg_channel,
            meeting_id,
            primary_provider.clone(),
            "selection_finalize",
            "전문 에이전트 최종 확정",
            finalize_prompt,
        )
        .await,
    );

    if let Some(warning) = final_resolution.warning_message.as_deref() {
        tracing::warn!(
            "[meeting] selection finalize fallback meeting_id={} channel_id={} warning={}",
            meeting_id,
            channel_id.get(),
            warning
        );
        send_meeting_selection_status(http, shared, msg_channel, warning).await;
    }

    build_meeting_participants(
        &candidate_pool,
        &final_resolution.selected_role_ids,
        &fixed_role_ids,
    )
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
            channel_id,
            meeting_id,
            participant,
            &agenda,
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
                rate_limit_wait(shared, msg_channel).await;
                let _ = msg_channel
                    .send_message(
                        http,
                        CreateMessage::new()
                            .content(format!("⚠️ {} 발언 실패: {}", participant.display_name, e)),
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

/// Execute a single agent turn using primary draft -> reviewer critique -> primary final.
async fn execute_agent_turn(
    channel_id: ChannelId,
    meeting_id: &str,
    participant: &MeetingParticipant,
    agenda: &str,
    round: u32,
    transcript: &str,
    primary_provider: ProviderKind,
    reviewer_provider: ProviderKind,
) -> Result<String, String> {
    let draft_prompt = format!(
        r#"## 회의 안건
{agenda}

## 현재 라운드: {round}

## 이전 발언 기록
{transcript}

## 지시사항
- 당신의 전문 분야 관점과 memory/recall 근거를 바탕으로 안건에 대해 의견을 제시하세요
- 이전 발언자들의 의견을 참고하고 필요시 반론/보충하세요
- 답변은 300자 이내로 간결하게 작성하세요
- 합의에 도달했다고 판단되면, 반드시 "CONSENSUS:" 로 시작하는 한 줄 요약을 마지막에 추가하세요
- 아직 논의가 더 필요하면 CONSENSUS: 키워드를 사용하지 마세요
- 도구나 명령 실행 없이 답변만 작성하세요"#,
        agenda = agenda,
        round = round,
        transcript = if transcript.is_empty() {
            "(아직 발언 없음)".to_string()
        } else {
            transcript.to_string()
        },
    );

    let draft = execute_meeting_expert_prompt(
        participant,
        channel_id,
        meeting_id,
        agenda,
        round,
        transcript,
        &primary_provider,
        draft_prompt,
    )
    .await?;

    let critique_prompt = format!(
        r#"당신은 회의 발언 초안을 비판적으로 검토하는 리뷰어다.

발언 역할: {name}

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
        agenda = agenda,
        round = round,
        transcript = if transcript.is_empty() {
            "(아직 발언 없음)".to_string()
        } else {
            transcript.to_string()
        },
        draft = draft.trim(),
    );
    let critique = provider_exec::execute_simple(reviewer_provider, critique_prompt).await?;

    let final_prompt = format!(
        r#"회의 안건:
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

    execute_meeting_expert_prompt(
        participant,
        channel_id,
        meeting_id,
        agenda,
        round,
        transcript,
        &primary_provider,
        final_prompt,
    )
    .await
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

    let (agenda, transcript_text, participants_list, primary_provider, reviewer_provider) = {
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
            t,
            p.join(", "),
            m.primary_provider.clone(),
            m.reviewer_provider.clone(),
        )
    };

    // Resolve summary agent dynamically based on agenda
    let resolved_summary_agent = config.summary_agent.resolve(&agenda);

    // Find summary agent's prompt file
    let summary_prompt_file = config
        .agent_registry
        .iter()
        .find(|a| a.role_id == resolved_summary_agent)
        .map(|a| a.binding.prompt_file.clone())
        .unwrap_or_default();

    let summary_role_context = if !summary_prompt_file.is_empty() {
        load_role_prompt(&RoleBinding {
            role_id: resolved_summary_agent.clone(),
            prompt_file: summary_prompt_file,
            provider: None,
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: true,
            memory: Default::default(),
        })
        .unwrap_or_default()
    } else {
        String::new()
    };

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

    rate_limit_wait(shared, msg_channel).await;
    if active_meeting_state(shared, channel_id, meeting_id).await != ActiveMeetingSlot::Active {
        return Ok(false);
    }
    let _ = msg_channel
        .send_message(
            http,
            CreateMessage::new().content("📝 **회의록 작성 중...**"),
        )
        .await;

    let draft = provider_exec::execute_simple(primary_provider.clone(), draft_prompt).await;

    let summary_text = match draft {
        Ok(draft_text) => {
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
            let critique = provider_exec::execute_simple(reviewer_provider, critique_prompt).await;
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
            match provider_exec::execute_simple(primary_provider, final_prompt).await {
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
                    rate_limit_wait(shared, msg_channel).await;
                    let _ = msg_channel
                        .send_message(
                            http,
                            CreateMessage::new().content(format!("⚠️ 회의록 작성 실패: {}", e)),
                        )
                        .await;
                    None
                }
            }
        }
        Err(e) => {
            rate_limit_wait(shared, msg_channel).await;
            let _ = msg_channel
                .send_message(
                    http,
                    CreateMessage::new().content(format!("⚠️ 회의록 작성 실패: {}", e)),
                )
                .await;
            None
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

    // POST meeting data to own HTTP server (fire-and-forget, ignore errors)
    if let Some(payload) = adk_payload {
        let port = shared.api_port;
        tokio::spawn(async move {
            let _ = post_meeting_status(payload, port).await;
        });
    }

    Ok(true)
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

    Some(serde_json::json!({
        "id": m.id,
        "agenda": m.agenda,
        "summary": m.summary,
        "status": status_str,
        "primary_provider": m.primary_provider.as_str(),
        "reviewer_provider": m.reviewer_provider.as_str(),
        "participant_names": participant_names,
        "total_rounds": total_rounds,
        "started_at": started_at,
        "completed_at": if m.status == MeetingStatus::Completed { serde_json::Value::from(chrono::Local::now().timestamp_millis()) } else { serde_json::Value::Null },
        "thread_id": m.thread_id.map(|t| t.to_string()),
        "entries": entries,
    }))
}

/// POST meeting data to own HTTP server
async fn post_meeting_status(
    payload: serde_json::Value,
    api_port: u16,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let _ = client
        .post(crate::config::local_api_url(
            api_port,
            "/api/round-table-meetings",
        ))
        .json(&payload)
        .send()
        .await?;
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

    format!(
        "---\ntags: [meeting, cookingheart]\ndate: {date}\nstatus: {status}\nparticipants: [{participants}]\nagenda: \"{agenda}\"\nmeeting_id: {id}\nprimary_provider: {primary_provider}\nreviewer_provider: {reviewer_provider}\n---\n\n# 회의록: {agenda}\n\n> **날짜**: {datetime}\n> **참여 전문 에이전트**: {participants}\n> **라운드**: {rounds}/{max_rounds}\n> **상태**: {status}\n> **진행자**: {primary_provider}\n> **리뷰어**: {reviewer_provider}\n\n---\n\n## 요약\n\n{summary}\n\n---\n\n## 전체 발언 기록\n\n{transcript}\n",
        date = date_str,
        status = status_str,
        participants = participants_inline,
        agenda = m.agenda,
        id = m.id,
        primary_provider = m.primary_provider.as_str(),
        reviewer_provider = m.reviewer_provider.as_str(),
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
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id
                    .send_message(&*http, CreateMessage::new().content(message))
                    .await;
                return Ok(true);
            }
        };

        if request.agenda.is_empty() {
            rate_limit_wait(shared, channel_id).await;
            let _ = channel_id
                .send_message(
                    &*http,
                    CreateMessage::new().content(
                        "사용법: `/meeting start [--primary claude|codex|gemini|qwen] <안건>`",
                    ),
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
                    rate_limit_wait(&shared_clone, channel_id).await;
                    let _ = channel_id
                        .send_message(
                            &*http_clone,
                            CreateMessage::new().content(format!("❌ 회의 오류: {}", e)),
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
        ActiveMeetingSlot, MAX_MEETING_PARTICIPANTS, MIN_MEETING_PARTICIPANTS, Meeting,
        MeetingAgentConfig, MeetingConfig, MeetingStatus, MeetingUtterance, ProviderKind,
        SummaryAgentConfig, build_meeting_participants, build_meeting_status_payload,
        canonical_candidate_pool, derive_agent_metadata_quality, effective_round_count,
        fallback_to_initial_selection, meeting_readonly_tools, meeting_slot_state,
        parse_meeting_start_text, parse_selected_role_ids_response, resolve_final_selection,
    };
    use crate::services::discord::settings::RoleBinding;
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

    fn fixture_meeting(id: &str, status: MeetingStatus) -> Meeting {
        Meeting {
            id: id.to_string(),
            agenda: "test".to_string(),
            primary_provider: ProviderKind::Claude,
            reviewer_provider: ProviderKind::Codex,
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
        meeting.transcript.push(MeetingUtterance {
            role_id: "ch-td".to_string(),
            display_name: "TD".to_string(),
            round: 1,
            content: "late round one".to_string(),
        });

        let payload = build_meeting_status_payload(&meeting).expect("payload");
        assert_eq!(payload.get("total_rounds"), Some(&json!(1)));
    }

    #[test]
    fn test_meeting_readonly_tools_only_allow_read() {
        assert_eq!(meeting_readonly_tools(), vec!["Read".to_string()]);
    }

    #[test]
    fn test_derive_agent_metadata_quality_marks_full_metadata_high() {
        let (missing, confidence) = derive_agent_metadata_quality(
            Some("구현 품질과 아키텍처 리스크를 본다"),
            &["리뷰".to_string()],
            &["설계".to_string()],
            &["브랜딩 단독 담당".to_string()],
        );

        assert!(!missing);
        assert_eq!(confidence, "high");
    }

    fn fixture_agent(role_id: &str) -> MeetingAgentConfig {
        MeetingAgentConfig {
            role_id: role_id.to_string(),
            display_name: role_id.to_uppercase(),
            keywords: Vec::new(),
            domain_summary: None,
            strengths: Vec::new(),
            task_types: Vec::new(),
            anti_signals: Vec::new(),
            provider_hint: None,
            metadata_missing: true,
            metadata_confidence: "low".to_string(),
            binding: RoleBinding {
                role_id: role_id.to_string(),
                prompt_file: String::new(),
                provider: None,
                model: None,
                reasoning_effort: None,
                peer_agents_enabled: true,
                memory: Default::default(),
            },
            workspace: None,
        }
    }

    #[test]
    fn test_canonical_candidate_pool_excludes_operator_role_ids() {
        let config = MeetingConfig {
            channel_name: "meeting-room".to_string(),
            max_rounds: 3,
            summary_agent: SummaryAgentConfig::Static("td".to_string()),
            available_agents: vec![
                fixture_agent("claude"),
                fixture_agent("td"),
                fixture_agent("codex"),
                fixture_agent("pd"),
            ],
            agent_registry: vec![
                fixture_agent("claude"),
                fixture_agent("td"),
                fixture_agent("codex"),
                fixture_agent("pd"),
            ],
        };

        let candidates =
            canonical_candidate_pool(&config, &ProviderKind::Claude, &ProviderKind::Codex);
        let role_ids: Vec<&str> = candidates
            .iter()
            .map(|agent| agent.role_id.as_str())
            .collect();

        assert_eq!(role_ids, vec!["td", "pd"]);
        assert!(role_ids.len() >= MIN_MEETING_PARTICIPANTS);
        assert!(role_ids.len() <= MAX_MEETING_PARTICIPANTS);
    }

    #[test]
    fn test_build_meeting_participants_requires_fixed_invites_to_be_present() {
        let agents = vec![
            fixture_agent("td"),
            fixture_agent("pd"),
            fixture_agent("uxd"),
        ];
        let candidate_pool: Vec<&MeetingAgentConfig> = agents.iter().collect();

        let error = build_meeting_participants(
            &candidate_pool,
            &["td".to_string(), "pd".to_string()],
            &["uxd".to_string()],
        )
        .expect_err("missing fixed invite should fail");

        assert_eq!(
            error,
            "Fixed participant 'uxd' is missing from final selection"
        );
    }

    #[test]
    fn test_build_meeting_participants_accepts_five_experts_with_fixed_invites() {
        let agents = vec![
            fixture_agent("td"),
            fixture_agent("pd"),
            fixture_agent("uxd"),
            fixture_agent("qad"),
            fixture_agent("devops"),
        ];
        let candidate_pool: Vec<&MeetingAgentConfig> = agents.iter().collect();

        let participants = build_meeting_participants(
            &candidate_pool,
            &[
                "td".to_string(),
                "pd".to_string(),
                "uxd".to_string(),
                "qad".to_string(),
                "devops".to_string(),
            ],
            &["td".to_string(), "uxd".to_string()],
        )
        .expect("five experts should be allowed");

        let role_ids: Vec<&str> = participants
            .iter()
            .map(|participant| participant.role_id.as_str())
            .collect();

        assert_eq!(participants.len(), MAX_MEETING_PARTICIPANTS);
        assert_eq!(role_ids, vec!["td", "pd", "uxd", "qad", "devops"]);
    }

    #[test]
    fn test_parse_selected_role_ids_response_accepts_structured_json() {
        let parsed = parse_selected_role_ids_response(
            r#"{
  "selected_role_ids": ["td", "pd"],
  "required_axes": ["기획", "구현"],
  "selection_notes": ["축 균형 유지"]
}"#,
        )
        .expect("structured response should parse");

        assert_eq!(parsed, vec!["td".to_string(), "pd".to_string()]);
    }

    #[test]
    fn test_parse_selected_role_ids_response_accepts_spec_selected_shape() {
        let parsed = parse_selected_role_ids_response(
            r#"{
  "agenda_summary": "요약",
  "required_axes": [{"axis":"설계","reason":"핵심","priority":"high"}],
  "selected": [
    {"role_id":"ch-td","why_selected":"설계 검토","covers_axes":["설계"],"fixed":false,"confidence":"high"},
    {"role_id":"ch-pd","why_selected":"제품 판단","covers_axes":["제품"],"fixed":false,"confidence":"medium"}
  ],
  "rejected": [],
  "selection_gaps": []
}"#,
        )
        .expect("selected shape should parse");

        assert_eq!(parsed, vec!["ch-td".to_string(), "ch-pd".to_string()]);
    }

    #[test]
    fn test_fallback_to_initial_selection_keeps_initial_ids_and_warning() {
        let resolution = fallback_to_initial_selection(
            &["td".to_string(), "pd".to_string()],
            "전문 에이전트 선정 리뷰",
            "meeting selection stage `selection_review` timed out after 45s",
        );

        assert_eq!(
            resolution.selected_role_ids,
            vec!["td".to_string(), "pd".to_string()]
        );
        assert_eq!(
            resolution.warning_message,
            Some(
                "⚠️ **전문 에이전트 선정 리뷰 실패**\nreason: meeting selection stage `selection_review` timed out after 45s\n1차 선정안으로 계속 진행합니다.".to_string()
            )
        );
    }

    #[test]
    fn test_resolve_final_selection_falls_back_to_initial_when_final_response_is_invalid() {
        let resolution = resolve_final_selection(
            &["td".to_string(), "pd".to_string()],
            Ok("not json".to_string()),
        );

        assert_eq!(
            resolution.selected_role_ids,
            vec!["td".to_string(), "pd".to_string()]
        );
        assert!(
            resolution
                .warning_message
                .as_deref()
                .unwrap_or_default()
                .contains("응답 파싱 실패")
        );
    }
}
