use super::*;

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
            let _ = record_meeting_transition(
                &meeting_id,
                MeetingState::Starting,
                MeetingEvent::ProviderFailed,
            );
            cleanup_meeting_if_current(shared, channel_id, &meeting_id).await;
            return Err("참여자를 선정하지 못했어.".into());
        }
        Err(e) => {
            let _ = record_meeting_transition(
                &meeting_id,
                MeetingState::Starting,
                MeetingEvent::ProviderFailed,
            );
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
                    let from_state = if m.current_round == 0 {
                        MeetingState::Starting
                    } else {
                        m.status.to_state()
                    };
                    let _ = record_meeting_transition(
                        &meeting_id,
                        from_state,
                        MeetingEvent::RoundComplete,
                    );
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
