use super::*;

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

pub(super) async fn execute_provider_stage(
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

pub(super) fn meeting_selection_stage_timeout_secs() -> u64 {
    resolve_meeting_stage_timeout_secs(
        std::env::var("AGENTDESK_MEETING_SELECTION_TIMEOUT_SECS")
            .ok()
            .as_deref(),
        DEFAULT_MEETING_SELECTION_STAGE_TIMEOUT_SECS,
    )
}

/// Create a Discord thread (without a parent message) for a meeting.
/// Returns the thread's ChannelId on success, or None on failure.
pub(super) async fn create_meeting_thread(
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
pub(super) async fn archive_meeting_thread(http: &serenity::Http, thread_channel_id: ChannelId) {
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

pub(super) async fn send_meeting_message_with_event(
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
            shared_outbound_deduper(),
            message,
            None,
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
    DiscordOutboundMessage::new(
        format!("meeting:{}", channel_id.get()),
        format!(
            "meeting:{}:{}:{content_hash}",
            channel_id.get(),
            normalize_meeting_event_key(event_key)
        ),
        content,
        OutboundTarget::Channel(channel_id),
        DiscordOutboundPolicy::preserve_inline_content(),
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

pub(super) async fn edit_meeting_message(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: serenity::MessageId,
    shared: &Arc<SharedData>,
    content: impl Into<String>,
) -> Result<(), String> {
    let content = content.into();
    let message =
        meeting_outbound_message(channel_id, content, &format!("edit:{}", message_id.get()))
            .with_operation(OutboundOperation::Edit { message_id });
    meeting_delivery_result(
        deliver_outbound(
            &MeetingOutboundClient { http, shared },
            shared_outbound_deduper(),
            message,
            None,
        )
        .await,
    )
    .map(|_| ())
}

fn meeting_delivery_result(result: DeliveryResult) -> Result<Option<serenity::MessageId>, String> {
    match result {
        DeliveryResult::Sent { messages, .. } | DeliveryResult::Fallback { messages, .. } => {
            first_raw_message_id(&messages)
                .as_deref()
                .ok_or_else(|| "meeting delivery returned no message id".to_string())
                .and_then(parse_meeting_message_id)
                .map(Some)
        }
        DeliveryResult::Duplicate {
            existing_messages, ..
        } => first_raw_message_id(&existing_messages)
            .as_deref()
            .map(parse_meeting_message_id)
            .transpose(),
        DeliveryResult::Skip { reason } => {
            tracing::info!(?reason, "[meeting] outbound delivery skipped");
            Ok(None)
        }
        DeliveryResult::TransientFailure { reason }
        | DeliveryResult::PermanentFailure { reason }
        | DeliveryResult::ConfirmedMissing { reason } => Err(reason),
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

pub(super) fn parse_participant_selection_response(
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

pub(super) fn build_selection_reason_line(
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

pub(super) fn normalize_role_ids(role_ids: &[String]) -> Vec<String> {
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

pub(super) fn fixed_participant_prompt_lines(fixed_role_ids: &[String]) -> String {
    if fixed_role_ids.is_empty() {
        "고정 전문 에이전트: 없음".to_string()
    } else {
        format!(
            "고정 전문 에이전트: {}\n- 이 role_id들은 최종 참가자에 반드시 포함한다.\n- 진행자는 남은 슬롯만 자동 선정한다.",
            fixed_role_ids.join(", ")
        )
    }
}

pub(super) fn merge_selected_participants(
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

pub(super) fn validate_fixed_participants(
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

pub(super) fn truncate_for_meeting(text: &str, max_chars: usize) -> String {
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

pub(super) fn build_fallback_meeting_summary(
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
