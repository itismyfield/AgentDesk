use super::*;

/// Check if majority of participants in a given round used CONSENSUS: keyword
pub(super) fn check_consensus(transcript: &[MeetingUtterance], round: u32, participant_count: usize) -> bool {
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
pub(super) async fn conclude_meeting(
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
                let _ = record_meeting_transition(
                    meeting_id,
                    m.status.to_state(),
                    MeetingEvent::Summarize,
                );
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
                let _ = record_meeting_transition(
                    meeting_id,
                    m.status.to_state(),
                    MeetingEvent::MarkComplete,
                );
                m.status = MeetingStatus::Completed;
            }
            _ => return Ok(false),
        }
    }

    Ok(true)
}

/// Save meeting record as Markdown to $AGENTDESK_ROOT_DIR/meetings/
pub(super) async fn save_meeting_record(
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

    let meetings_dir = runtime_store::agentdesk_root()
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
pub(super) fn build_meeting_status_payload(m: &Meeting) -> Option<serde_json::Value> {
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
pub(super) async fn persist_meeting_status(
    payload: serde_json::Value,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let body: meeting_artifact_store::UpsertMeetingBody = serde_json::from_value(payload)?;
    internal_api::upsert_meeting(body)
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
pub(super) fn format_transcript(transcript: &[MeetingUtterance]) -> String {
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
pub(super) async fn cleanup_meeting(shared: &Arc<SharedData>, channel_id: ChannelId) {
    let mut core = shared.core.lock().await;
    core.active_meetings.remove(&channel_id);
}
