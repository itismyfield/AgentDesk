use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{
    AutoArchiveDuration, ChannelId, ChannelType, CreateMessage,
    builder::{CreateThread, EditThread},
};

use crate::services::memory::{RecallRequest, RecallResponse, build_resolved_memory_backend};
use crate::services::provider::ProviderKind;
use crate::services::provider_exec;

use super::agentdesk_config;
use super::formatting::send_long_message_raw;
use super::org_schema;
use super::role_map::load_meeting_config as load_meeting_config_from_role_map;
use super::settings::{ResolvedMemorySettings, RoleBinding, load_role_prompt};
use super::{DispatchProfile, SharedData, rate_limit_wait};

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

pub(super) struct Meeting {
    pub id: String,
    pub channel_id: u64,
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
            memory: self.memory.clone(),
        }
    }
}

const DEFAULT_MAX_PARTICIPANTS: usize = 5;
const MIN_MEETING_PARTICIPANTS: usize = 2;
const MEETING_SELECTION_STAGE_TIMEOUT_SECS: u64 = 45;
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

    format!(
        r#"- role_id: {role_id}
  display_name: {display_name}
  keywords: {keywords}
  domain_summary: {domain_summary}
  strengths: {strengths}
  task_types: {task_types}
  anti_signals: {anti_signals}
  provider_hint: {provider_hint}
  metadata_missing: {metadata_missing}"#,
        role_id = agent.role_id,
        display_name = agent.display_name,
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
        provider_hint = agent
            .provider_hint
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("metadata_missing"),
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
                        "provider_hint": agent.provider_hint.clone(),
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
    match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        provider_exec::execute_simple(provider, prompt),
    )
    .await
    {
        Ok(result) => result.map(|text| text.trim().to_string()),
        Err(_) => Err(format!("{stage_label} timeout after {timeout_secs}s")),
    }
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
                channel_id: channel_id.get(),
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

    let meeting_hash = meeting_query_hash(&meeting_id);
    let thread_hash = if msg_channel != channel_id {
        Some(thread_query_hash(&msg_channel.get().to_string()))
    } else {
        None
    };
    let thread_hash_line = thread_hash
        .as_deref()
        .map(|hash| format!("\n스레드 해시: {hash}"))
        .unwrap_or_default();

    rate_limit_wait(shared, msg_channel).await;
    let _ = msg_channel
        .send_message(
            http,
            CreateMessage::new().content(format!(
                "📋 **라운드 테이블 회의 시작**\n안건: {}\n회의 해시: {}{}\n진행 프로바이더: {} / 리뷰 프로바이더: {}\n참여자 선정 중...",
                agenda,
                meeting_hash,
                thread_hash_line,
                primary_provider.display_name(),
                reviewer_provider.display_name()
            )),
        )
        .await;

    // Select participants via primary provider + reviewer cross-check
    let participants = match select_participants(
        &config,
        agenda,
        primary_provider,
        reviewer_provider,
        fixed_participants,
    )
    .await
    {
        Ok(p) if !p.is_empty() => p,
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
                "👥 **참여자 확정** ({}명)\n{}",
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
    rate_limit_wait(shared, msg_channel).await;
    let _ = msg_channel
        .send_message(
            http,
            CreateMessage::new()
                .content("💾 **회의록 저장 완료.** memory write/capture는 자동 실행하지 않으며, 후처리는 승인 기반으로만 진행합니다."),
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

    let config = load_meeting_config()
        .ok_or_else(|| "Meeting config not found in org.yaml or role_map.json".to_string())?;
    validate_fixed_participants(&config, &fixed_participants, config.max_participants)?;

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
                    .content("🛑 **회의가 취소됐어.** 현재까지 트랜스크립트가 저장됐고, memory write/capture는 자동 실행하지 않았어."),
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
                        "📊 **회의 현황**\n안건: {}\n상태: {}\n진행 프로바이더: {} / 리뷰 프로바이더: {}\n라운드: {}/{}\n참여자: {}명\n발언: {}개",
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

/// Select participants using primary provider + reviewer micro cross-check.
async fn select_participants(
    config: &MeetingConfig,
    agenda: &str,
    primary_provider: ProviderKind,
    reviewer_provider: ProviderKind,
    fixed_participants: Vec<String>,
) -> Result<Vec<MeetingParticipant>, String> {
    let max_participants = clamp_max_participants(config.max_participants);
    let fixed_participants = normalize_role_ids(&fixed_participants);
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
3. 후보별 적합성 비교: domain_summary, strengths, task_types, anti_signals, provider_hint, metadata_missing을 함께 비교한다.
4. 최종 선정 JSON: 최종 role_id만 고른다.

규칙:
- {}~{}명 선정
- 고정 전문 에이전트가 있으면 반드시 포함하고, 남은 슬롯만 추가 선정한다
- keywords 단순 일치만으로 선정하지 말고 domain_summary/strengths/task_types를 우선한다
- anti_signals에 걸리는 후보는 강한 이유가 없으면 제외한다
- metadata_missing이 많은 후보는 필요한 경우에만 보조적으로 선정한다
- JSON 배열로만 응답 (다른 텍스트 없이)
- 형식: ["role_id1", "role_id2", ...]"#,
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
        MEETING_SELECTION_STAGE_TIMEOUT_SECS,
    )
    .await?;
    let initial_selected = parse_json_array_fragment(&initial_response)?;

    let review_prompt = format!(
        r#"당신은 회의 참가자 선정을 비판적으로 검토하는 리뷰어다.

안건: {agenda}

사용 가능한 에이전트:
{agents}

현재 선정안:
{current}

고정 전문 에이전트:
{fixed}

검토 규칙:
- 빠진 역할, 중복 역할, 안건과의 부적합만 짚어라
- 고정 전문 에이전트가 누락되면 반드시 지적하라
- 4개 이하 bullet만 사용하라
- metadata_missing, anti_signals, task_types mismatch가 있으면 명시하라
- 전체를 다시 쓰지 말고, 비판적으로만 검토하라
- 도구나 명령 실행은 하지 마라"#,
        agenda = agenda,
        agents = agents_desc.join("\n"),
        current = serde_json::to_string(&initial_selected).unwrap_or_else(|_| "[]".to_string()),
        fixed = fixed_participants.join(", "),
    );

    let review_notes = match execute_provider_stage(
        reviewer_provider.clone(),
        "participant selection review",
        review_prompt,
        MEETING_SELECTION_STAGE_TIMEOUT_SECS,
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

고정 전문 에이전트:
{fixed}

교차검증 리뷰:
{review}

규칙:
- 리뷰가 타당하면 반영하고, 타당하지 않으면 유지하라
- 최종 결과는 {min_participants}~{max_participants}명이어야 한다
- 고정 전문 에이전트는 최종 JSON에 반드시 포함한다
- 후보 메타데이터에서 metadata_missing이 많은 후보는 필요한 경우에만 유지하라
- JSON 배열로만 응답하라
- 형식: ["role_id1", "role_id2", ...]"#,
        agenda = agenda,
        agents = agents_desc.join("\n"),
        initial = serde_json::to_string(&initial_selected).unwrap_or_else(|_| "[]".to_string()),
        fixed = fixed_participants.join(", "),
        review = review_notes.trim(),
        min_participants = MIN_MEETING_PARTICIPANTS,
        max_participants = max_participants,
    );

    let selected = match execute_provider_stage(
        primary_provider.clone(),
        "participant final selection",
        finalize_prompt,
        MEETING_SELECTION_STAGE_TIMEOUT_SECS,
    )
    .await
    {
        Ok(final_response) => parse_json_array_fragment(&final_response)?,
        Err(_) => initial_selected,
    };

    merge_selected_participants(config, &selected, &fixed_participants, max_participants)
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
    let specialist_provider = participant.provider.clone().unwrap_or(primary_provider);
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
        reviewer_provider,
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

    let draft = execute_provider_stage(
        primary_provider.clone(),
        "meeting summary draft",
        draft_prompt,
        MEETING_SUMMARY_STAGE_TIMEOUT_SECS,
    )
    .await;

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

    Some(serde_json::json!({
        "id": m.id,
        "channel_id": m.channel_id.to_string(),
        "meeting_hash": meeting_hash,
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
        "thread_hash": thread_hash,
        "memory_postprocessing": memory_postprocessing_policy(),
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
    let meeting_hash = meeting_query_hash(&m.id);
    let thread_id = m
        .thread_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "null".to_string());
    let thread_hash_display = m
        .thread_id
        .map(|id| thread_query_hash(&id.to_string()))
        .unwrap_or_else(|| "-".to_string());
    let thread_hash_frontmatter = if thread_hash_display == "-" {
        "null".to_string()
    } else {
        format!("\"{thread_hash_display}\"")
    };

    format!(
        "---\ntags: [meeting, cookingheart]\ndate: {date}\nstatus: {status}\nparticipants: [{participants}]\nagenda: \"{agenda}\"\nmeeting_id: {id}\nmeeting_hash: \"{meeting_hash}\"\nthread_id: {thread_id}\nthread_hash: {thread_hash_frontmatter}\nprimary_provider: {primary_provider}\nreviewer_provider: {reviewer_provider}\nauto_memory_write: false\nauto_memory_capture: false\nmemory_postprocessing_policy: approval_required\n---\n\n# 회의록: {agenda}\n\n> **날짜**: {datetime}\n> **참여자**: {participants}\n> **라운드**: {rounds}/{max_rounds}\n> **상태**: {status}\n> **회의 해시**: {meeting_hash}\n> **스레드 해시**: {thread_hash_display}\n> **진행 프로바이더**: {primary_provider}\n> **리뷰 프로바이더**: {reviewer_provider}\n> **메모리 후처리**: 자동 memory write/capture 비활성화, 승인 기반만 허용\n\n---\n\n## 요약\n\n{summary}\n\n---\n\n## 전체 발언 기록\n\n{transcript}\n",
        date = date_str,
        status = status_str,
        participants = participants_inline,
        agenda = m.agenda,
        id = m.id,
        meeting_hash = meeting_hash,
        thread_id = thread_id,
        thread_hash_frontmatter = thread_hash_frontmatter,
        thread_hash_display = thread_hash_display,
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
        ActiveMeetingSlot, Meeting, MeetingAgentConfig, MeetingConfig, MeetingStatus,
        MeetingUtterance, ProviderKind, ResolvedMemorySettings, SummaryAgentConfig,
        agent_metadata_card, build_meeting_markdown, build_meeting_status_payload,
        effective_round_count, meeting_query_hash, meeting_slot_state, parse_meeting_start_text,
        summary_agent_context, thread_query_hash,
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

    fn fixture_meeting(id: &str, status: MeetingStatus) -> Meeting {
        Meeting {
            id: id.to_string(),
            channel_id: 42,
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
        assert!(rich_card.contains("domain_summary: Deep reasoning"));
        assert!(rich_card.contains("metadata_missing: []"));
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
    }

    #[test]
    fn test_build_meeting_markdown_includes_query_hashes() {
        let mut meeting = fixture_meeting("mtg-a", MeetingStatus::Completed);
        meeting.thread_id = Some(123);

        let md = build_meeting_markdown(&meeting);

        assert!(md.contains(&format!(
            "meeting_hash: \"{}\"",
            meeting_query_hash("mtg-a")
        )));
        assert!(md.contains("thread_id: 123"));
        assert!(md.contains(&format!("thread_hash: \"{}\"", thread_query_hash("123"))));
        assert!(md.contains(&format!("> **회의 해시**: {}", meeting_query_hash("mtg-a"))));
        assert!(md.contains(&format!("> **스레드 해시**: {}", thread_query_hash("123"))));
    }
}
