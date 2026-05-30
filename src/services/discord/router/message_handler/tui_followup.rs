use super::*;

pub(super) const CLAUDE_TUI_BUSY_FOLLOWUP_NOTICE: &str = "⚠ Claude TUI가 아직 이전 터미널 턴을 처리 중이라 이 메시지를 주입하지 않았습니다. 현재 응답이 끝난 뒤 다시 보내 주세요.";
pub(super) const CLAUDE_TUI_BUSY_FOLLOWUP_ALREADY_QUEUED_NOTICE: &str =
    "📬 이 메시지는 이미 큐에 들어가 있어 추가 적재하지 않았습니다. 큐 결과를 기다려 주세요.";
pub(super) const CLAUDE_TUI_BUSY_FOLLOWUP_DEDUP_NOTICE: &str =
    "📬 방금 동일한 메시지가 큐에 적재되어 중복으로 무시했습니다. 큐 결과를 기다려 주세요.";
pub(super) const CLAUDE_TUI_BUSY_FOLLOWUP_QUEUE_UNREACHABLE_NOTICE: &str =
    "⚠ 내부 처리 큐에 접근하지 못해 이 메시지를 적재하지 못했습니다. 잠시 후 다시 보내 주세요.";
pub(super) fn claude_tui_busy_followup_refusal_notice(
    reason: Option<crate::services::turn_orchestrator::EnqueueRefusalReason>,
) -> &'static str {
    match reason {
        Some(crate::services::turn_orchestrator::EnqueueRefusalReason::SourceIdAlreadyQueued) => {
            CLAUDE_TUI_BUSY_FOLLOWUP_ALREADY_QUEUED_NOTICE
        }
        Some(crate::services::turn_orchestrator::EnqueueRefusalReason::LastItemDedup) => {
            CLAUDE_TUI_BUSY_FOLLOWUP_DEDUP_NOTICE
        }
        Some(crate::services::turn_orchestrator::EnqueueRefusalReason::ActorUnreachable) => {
            CLAUDE_TUI_BUSY_FOLLOWUP_QUEUE_UNREACHABLE_NOTICE
        }
        None => CLAUDE_TUI_BUSY_FOLLOWUP_NOTICE,
    }
}
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClaudeTuiBusyFollowupDiagnostic {
    pub(super) tmux_session_name: String,
    pub(super) prompt_marker_detected: bool,
    pub(super) prompt_draft_detected: bool,
    pub(super) previous_tui_turn_still_running: bool,
    pub(super) tmux_pane_alive: bool,
    pub(super) capture_available: bool,
    pub(super) watcher_state: &'static str,
    pub(super) watcher_owner_channel_id: Option<u64>,
    pub(super) inflight_state: &'static str,
    pub(super) transcript_turn_state: crate::services::tui_turn_state::TuiTurnState,
    pub(super) pane_tail: String,
}

#[cfg(unix)]
impl ClaudeTuiBusyFollowupDiagnostic {
    pub(super) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "tmux_session_name": self.tmux_session_name,
            "prompt_marker_detected": self.prompt_marker_detected,
            "prompt_draft_detected": self.prompt_draft_detected,
            "previous_tui_turn_still_running": self.previous_tui_turn_still_running,
            "tmux_pane_alive": self.tmux_pane_alive,
            "capture_available": self.capture_available,
            "watcher_state": self.watcher_state,
            "watcher_owner_channel_id": self.watcher_owner_channel_id,
            "inflight_state": self.inflight_state,
            "transcript_turn_state": self.transcript_turn_state.as_str(),
            "pane_tail": self.pane_tail,
        })
    }
}

#[cfg(unix)]
pub(super) fn classify_inflight_diagnostic_state(
    inflight: Option<&InflightTurnState>,
) -> &'static str {
    let Some(inflight) = inflight else {
        return "missing";
    };
    let Some(updated_at_unix) =
        super::super::super::inflight::parse_updated_at_unix(&inflight.updated_at)
    else {
        return "stale_unparseable_updated_at";
    };
    let age_secs = chrono::Local::now()
        .timestamp()
        .saturating_sub(updated_at_unix);
    if age_secs >= super::super::super::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64 {
        "stale"
    } else if inflight.effective_relay_owner_kind()
        == super::super::super::inflight::RelayOwnerKind::Watcher
    {
        "watcher_owned"
    } else if inflight.effective_relay_owner_kind()
        == super::super::super::inflight::RelayOwnerKind::StandbyRelay
    {
        "standby_relay_owned"
    } else if inflight.effective_relay_owner_kind()
        == super::super::super::inflight::RelayOwnerKind::Unknown
    {
        "relay_owner_unknown"
    } else {
        "present"
    }
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HostedTuiPromptReadinessSnapshot {
    pub(super) prompt_marker_detected: bool,
    pub(super) prompt_draft_detected: bool,
    pub(super) tmux_pane_alive: bool,
    pub(super) capture_available: bool,
    pub(super) pane_tail: String,
}

#[cfg(unix)]
impl HostedTuiPromptReadinessSnapshot {
    pub(super) fn jsonl_authoritative(tmux_pane_alive: bool) -> Self {
        Self {
            prompt_marker_detected: false,
            prompt_draft_detected: false,
            tmux_pane_alive,
            capture_available: false,
            pane_tail: "<not captured; JSONL turn state is authoritative>".to_string(),
        }
    }
}

#[cfg(unix)]
pub(super) fn classify_claude_tui_followup_submission(
    snapshot: &HostedTuiPromptReadinessSnapshot,
    watcher_state: &'static str,
    watcher_owner_channel_id: Option<u64>,
    inflight_state: &'static str,
    transcript_turn_state: crate::services::tui_turn_state::TuiTurnState,
    tmux_session_name: &str,
) -> Option<ClaudeTuiBusyFollowupDiagnostic> {
    let structured_turn_busy = transcript_turn_state.is_busy();
    let draft_blocks_submission =
        snapshot.tmux_pane_alive && snapshot.prompt_draft_detected && inflight_state != "missing";
    if !structured_turn_busy && !draft_blocks_submission {
        return None;
    }
    Some(ClaudeTuiBusyFollowupDiagnostic {
        tmux_session_name: tmux_session_name.to_string(),
        prompt_marker_detected: snapshot.prompt_marker_detected,
        prompt_draft_detected: snapshot.prompt_draft_detected,
        previous_tui_turn_still_running: structured_turn_busy,
        tmux_pane_alive: snapshot.tmux_pane_alive,
        capture_available: snapshot.capture_available,
        watcher_state,
        watcher_owner_channel_id,
        inflight_state,
        transcript_turn_state,
        pane_tail: snapshot.pane_tail.clone(),
    })
}

#[cfg(unix)]
pub(super) fn hosted_tui_draft_should_enter_provider_recovery(
    provider: &ProviderKind,
    snapshot: &HostedTuiPromptReadinessSnapshot,
) -> bool {
    matches!(provider, ProviderKind::Codex)
        && snapshot.tmux_pane_alive
        && snapshot.prompt_marker_detected
        && snapshot.prompt_draft_detected
}

#[cfg(unix)]
pub(super) fn observe_claude_tui_transcript_state_for_session(
    current_path: Option<&str>,
    session_id: Option<&str>,
) -> crate::services::tui_turn_state::TuiTurnState {
    let (Some(current_path), Some(session_id)) = (current_path, session_id) else {
        return crate::services::tui_turn_state::TuiTurnState::Unknown;
    };
    let Ok(transcript_path) = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        std::path::Path::new(current_path),
        session_id,
        None,
    ) else {
        return crate::services::tui_turn_state::TuiTurnState::Unknown;
    };
    let provider = ProviderKind::Claude;
    let probe =
        crate::services::tui_turn_state::JsonlTurnStateProbe::new(&provider, &transcript_path);
    crate::services::tui_turn_state::TuiTurnStateProbe::observe(&probe)
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum HostedTuiBusyPreflightReadinessWait {
    Codex,
    ClaudePromptMarkerOnly,
    ClaudePromptMarkerOrIdleTranscript(std::path::PathBuf),
}

#[cfg(unix)]
pub(super) fn hosted_tui_busy_preflight_readiness_wait(
    provider: &ProviderKind,
    current_path: Option<&str>,
    session_id: Option<&str>,
) -> HostedTuiBusyPreflightReadinessWait {
    hosted_tui_busy_preflight_readiness_wait_with_claude_home(
        provider,
        current_path,
        session_id,
        None,
    )
}

#[cfg(unix)]
pub(super) fn hosted_tui_busy_preflight_readiness_wait_with_claude_home(
    provider: &ProviderKind,
    current_path: Option<&str>,
    session_id: Option<&str>,
    claude_home: Option<&std::path::Path>,
) -> HostedTuiBusyPreflightReadinessWait {
    if matches!(provider, ProviderKind::Codex) {
        return HostedTuiBusyPreflightReadinessWait::Codex;
    }
    let (Some(current_path), Some(session_id)) = (current_path, session_id) else {
        return HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOnly;
    };
    let Ok(transcript_path) = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        std::path::Path::new(current_path),
        session_id,
        claude_home,
    ) else {
        return HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOnly;
    };
    // Missing Claude JSONL files currently observe as Idle. Only pass a
    // transcript path to the fallback once the file exists, so cold sessions
    // still require the visible prompt marker before we inject a follow-up.
    if transcript_path.exists() {
        HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOrIdleTranscript(transcript_path)
    } else {
        HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOnly
    }
}

#[cfg(unix)]
pub(super) fn observe_codex_tui_rollout_state_for_cwd(
    current_path: Option<&str>,
    tmux_session_name: Option<&str>,
    provider_session_id: Option<&str>,
) -> crate::services::tui_turn_state::TuiTurnState {
    let runtime_binding = tmux_session_name
        .and_then(crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session)
        .filter(|binding| {
            binding.runtime_kind == crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
        });
    observe_codex_tui_rollout_state_for_cwd_with_sessions(
        current_path,
        provider_session_id,
        None,
        runtime_binding.as_ref(),
    )
}

#[cfg(unix)]
pub(super) fn observe_codex_tui_rollout_state_for_cwd_with_sessions(
    current_path: Option<&str>,
    provider_session_id: Option<&str>,
    sessions_dir: Option<&std::path::Path>,
    runtime_binding: Option<&crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
) -> crate::services::tui_turn_state::TuiTurnState {
    let Some(current_path) = current_path else {
        return crate::services::tui_turn_state::TuiTurnState::Unknown;
    };
    let cwd = std::path::Path::new(current_path);
    if let Some(binding) = runtime_binding {
        let rollout_path = std::path::Path::new(&binding.output_path);
        if std::fs::metadata(rollout_path).is_err() {
            return crate::services::tui_turn_state::TuiTurnState::Unknown;
        }
        if !crate::services::codex_tui::rollout_tail::rollout_file_matches_cwd(rollout_path, cwd) {
            return crate::services::tui_turn_state::TuiTurnState::Unknown;
        }
        return crate::services::codex_tui::rollout_tail::observe_rollout_turn_state(rollout_path);
    }
    let resolved = sessions_dir
        .map(std::path::Path::to_path_buf)
        .or_else(|| crate::services::codex_tui::rollout_tail::default_codex_sessions_dir());
    let Some(sessions_dir) = resolved else {
        return crate::services::tui_turn_state::TuiTurnState::Unknown;
    };
    if let Some(provider_session_id) = provider_session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let selection = crate::services::codex_tui::session::resolve_codex_tui_session(
            Some(provider_session_id),
            cwd,
            Some(&sessions_dir),
            false,
        );
        if let Some(rollout_path) = selection.rollout_path.as_deref() {
            return crate::services::codex_tui::rollout_tail::observe_rollout_turn_state(
                rollout_path,
            );
        }
        return crate::services::tui_turn_state::TuiTurnState::Unknown;
    }
    let Some(rollout_path) = crate::services::codex_tui::rollout_tail::latest_rollout_for_cwd_since(
        cwd,
        std::time::SystemTime::UNIX_EPOCH,
        &sessions_dir,
    ) else {
        // No rollout file found for this cwd — treat as idle (session not yet started).
        return crate::services::tui_turn_state::TuiTurnState::Idle;
    };
    let rollout_state =
        crate::services::codex_tui::rollout_tail::observe_rollout_turn_state(&rollout_path);
    if rollout_state.is_busy() {
        return rollout_state;
    }
    crate::services::tui_turn_state::TuiTurnState::Unknown
}

#[cfg(unix)]
pub(super) fn tui_busy_followup_diagnostic(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: Option<&str>,
    remote_profile_present: bool,
    current_path: Option<&str>,
    session_id: Option<&str>,
) -> Option<ClaudeTuiBusyFollowupDiagnostic> {
    if !matches!(provider, ProviderKind::Claude | ProviderKind::Codex) || remote_profile_present {
        return None;
    }
    let tmux_session_name = tmux_session_name?;
    let selection =
        crate::services::provider_hosting::resolve_provider_session_selection_with_channel(
            provider,
            claude::is_tmux_available(),
            Some(channel_id.get()),
        );
    if selection.driver != crate::services::provider_hosting::ProviderSessionDriver::TuiHosting
        || crate::services::claude_tui::hook_server::current_hook_endpoint().is_none()
        || !crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_session_name)
    {
        return None;
    }

    let watcher_entry = shared
        .tmux_watchers
        .iter()
        .find(|entry| entry.tmux_session_name == tmux_session_name);
    let owner_channel_id = shared
        .tmux_watchers
        .owner_channel_for_tmux_session(tmux_session_name)
        .map(|channel_id| channel_id.get());
    let (watcher_state, watcher_owner_channel_id) = watcher_entry
        .as_ref()
        .map(|entry| {
            let state = if entry.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                "cancelled"
            } else if entry.heartbeat_stale() {
                "stale"
            } else if entry.paused.load(std::sync::atomic::Ordering::Relaxed) {
                "paused"
            } else {
                "attached"
            };
            (state, owner_channel_id)
        })
        .unwrap_or(("missing", None));
    let previous_inflight =
        super::super::super::inflight::load_inflight_state(provider, channel_id.get());
    let inflight_state = classify_inflight_diagnostic_state(previous_inflight.as_ref());
    let transcript_turn_state = match provider {
        ProviderKind::Claude => {
            observe_claude_tui_transcript_state_for_session(current_path, session_id)
        }
        ProviderKind::Codex => observe_codex_tui_rollout_state_for_cwd(
            current_path,
            Some(tmux_session_name),
            session_id,
        ),
        _ => crate::services::tui_turn_state::TuiTurnState::Unknown,
    };
    if transcript_turn_state == crate::services::tui_turn_state::TuiTurnState::Idle {
        return None;
    }
    if transcript_turn_state.is_busy() {
        let snapshot = HostedTuiPromptReadinessSnapshot::jsonl_authoritative(true);
        return classify_claude_tui_followup_submission(
            &snapshot,
            watcher_state,
            watcher_owner_channel_id,
            inflight_state,
            transcript_turn_state,
            tmux_session_name,
        );
    }

    let snapshot = match provider {
        ProviderKind::Codex => {
            let snapshot =
                crate::services::codex_tui::input::prompt_readiness_snapshot(tmux_session_name);
            HostedTuiPromptReadinessSnapshot {
                prompt_marker_detected: snapshot.composer_marker_detected,
                prompt_draft_detected: snapshot.prompt_draft_detected,
                tmux_pane_alive: snapshot.tmux_pane_alive,
                capture_available: snapshot.capture_available,
                pane_tail: snapshot.pane_tail,
            }
        }
        _ => {
            let snapshot =
                crate::services::claude_tui::input::prompt_readiness_snapshot(tmux_session_name);
            HostedTuiPromptReadinessSnapshot {
                prompt_marker_detected: snapshot.prompt_marker_detected,
                prompt_draft_detected: snapshot.prompt_draft_detected,
                tmux_pane_alive: snapshot.tmux_pane_alive,
                capture_available: snapshot.capture_available,
                pane_tail: snapshot.pane_tail,
            }
        }
    };
    if hosted_tui_draft_should_enter_provider_recovery(provider, &snapshot) {
        return None;
    }
    classify_claude_tui_followup_submission(
        &snapshot,
        watcher_state,
        watcher_owner_channel_id,
        inflight_state,
        transcript_turn_state,
        tmux_session_name,
    )
}

pub(super) async fn enqueue_busy_tui_followup_for_retry(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    request_owner: serenity::UserId,
    user_msg_id: serenity::MessageId,
    user_text: &str,
    reply_context: Option<String>,
    has_reply_boundary: bool,
    merge_consecutive: bool,
    pending_uploads: Vec<String>,
    voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
) -> MailboxEnqueueOutcome {
    super::super::super::mailbox_enqueue_intervention(
        shared,
        provider,
        channel_id,
        build_race_requeued_intervention(
            request_owner,
            user_msg_id,
            user_text,
            reply_context,
            has_reply_boundary,
            merge_consecutive,
            pending_uploads,
            voice_announcement,
        ),
    )
    .await
}

#[cfg(unix)]
pub(super) fn recapture_inflight_offset_after_successful_busy_wait(
    output_path: Option<&str>,
    previous_offset: u64,
) -> u64 {
    output_path
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|metadata| metadata.len())
        .unwrap_or(previous_offset)
}
