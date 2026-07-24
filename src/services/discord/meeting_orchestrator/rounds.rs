use super::*;

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
            channel_name: None,
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
#[allow(clippy::too_many_arguments)]
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
