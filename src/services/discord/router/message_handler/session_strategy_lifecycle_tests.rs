use super::*;

/// Codex rollout-state tests below drive `observe_codex_tui_rollout_state_for_cwd_with_sessions`,
/// whose no-session-id and provider-session-id paths now read/write the
/// process-global rollout index (`cached_indexed_rollouts`). Share the SAME lock
/// the `rollout_index` / `session` cache tests use so a rollout-state test cannot
/// mutate `roots` between another cache test's reset and its empty-state
/// assertion under default parallel `cargo test`. The guard also resets the cache
/// on acquisition, isolating each test.
#[cfg(unix)]
fn lock_rollout_cache_test() -> std::sync::MutexGuard<'static, ()> {
    crate::services::codex_tui::rollout_index::lock_cache_for_tests()
}

#[tokio::test]
async fn launch_runtime_refresh_preserves_pending_reset_when_path_is_stale() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel_id = serenity::ChannelId::new(4_794_003);
    {
        let mut data = shared.core.lock().await;
        data.sessions.insert(
            channel_id,
            DiscordSession {
                session_id: None,
                memento_context_loaded: false,
                memento_reflected: false,
                current_path: Some("/missing/resume-worktree".to_string()),
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                remote_profile_name: None,
                channel_id: Some(channel_id.get()),
                channel_name: Some("resume-reset".to_string()),
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: 0,
            },
        );
    }

    let current = (
        "/previous/launch-path".to_string(),
        Some("01234567-89ab-cdef-0123-456789abcdef".to_string()),
        true,
        "runtime_cached_provider_session",
    );
    let runtime =
        resolve_channel_runtime_for_launch(&shared, &ProviderKind::Claude, channel_id, current)
            .await;

    assert_eq!(runtime.0, "/previous/launch-path");
    assert_eq!(runtime.1, None);
    assert!(!runtime.2);
    assert_eq!(runtime.3, "explicit_provider_reset");
}

#[tokio::test]
async fn launch_runtime_refresh_rejects_rebound_session_when_target_path_is_stale() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel_id = serenity::ChannelId::new(4_794_004);
    let prior_session_id = "11111111-1111-1111-1111-111111111111";
    let rebound_session_id = "22222222-2222-2222-2222-222222222222";
    {
        let mut data = shared.core.lock().await;
        data.sessions.insert(
            channel_id,
            DiscordSession {
                session_id: Some(rebound_session_id.to_string()),
                memento_context_loaded: false,
                memento_reflected: false,
                current_path: Some("/removed/rebound-worktree".to_string()),
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                remote_profile_name: None,
                channel_id: Some(channel_id.get()),
                channel_name: Some("resume-stale-target".to_string()),
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: 0,
            },
        );
    }

    let runtime = resolve_channel_runtime_for_launch(
        &shared,
        &ProviderKind::Claude,
        channel_id,
        (
            "/prior/launch-path".to_string(),
            Some(prior_session_id.to_string()),
            true,
            "runtime_cached_provider_session",
        ),
    )
    .await;

    assert_eq!(runtime.0, "/prior/launch-path");
    assert_eq!(runtime.1.as_deref(), Some(prior_session_id));
    assert_ne!(runtime.1.as_deref(), Some(rebound_session_id));
}

#[test]
fn invalid_redirect_path_falls_back_without_pairing_redirect_session_id() {
    let original_channel = serenity::ChannelId::new(4_794_005);
    let redirect_channel = serenity::ChannelId::new(4_794_006);
    let original_session_id = "33333333-3333-3333-3333-333333333333";
    let redirect_session_id = "44444444-4444-4444-4444-444444444444";
    let mut sessions = std::collections::HashMap::new();
    sessions.insert(
        redirect_channel,
        DiscordSession {
            session_id: Some(redirect_session_id.to_string()),
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: Some("/removed/redirect-worktree".to_string()),
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: Some(redirect_channel.get()),
            channel_name: None,
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: 0,
        },
    );
    let original = (
        Some(original_session_id.to_string()),
        true,
        "/prior/redirect-path".to_string(),
    );

    let resolved = session_runtime_state_after_redirect(
        &mut sessions,
        original_channel,
        redirect_channel,
        original.clone(),
    );

    assert_eq!(resolved, original);
    assert_ne!(resolved.0.as_deref(), Some(redirect_session_id));
}

#[tokio::test]
async fn claimed_runtime_refresh_adopts_late_resume_binding() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel_id = serenity::ChannelId::new(4_794_007);
    let prior_cwd = tempfile::tempdir().expect("prior cwd");
    let rebound_cwd = tempfile::tempdir().expect("rebound cwd");
    let prior_session_id = "55555555-5555-5555-5555-555555555555";
    let rebound_session_id = "66666666-6666-6666-6666-666666666666";
    {
        let mut data = shared.core.lock().await;
        data.sessions.insert(
            channel_id,
            DiscordSession {
                session_id: Some(rebound_session_id.to_string()),
                memento_context_loaded: false,
                memento_reflected: false,
                current_path: Some(rebound_cwd.path().display().to_string()),
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                remote_profile_name: None,
                channel_id: Some(channel_id.get()),
                channel_name: Some("resume-late-binding".to_string()),
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: 0,
            },
        );
    }
    let mut current_path = prior_cwd.path().display().to_string();
    let mut session_id = Some(prior_session_id.to_string());
    let mut loaded = true;
    let mut reason = "runtime_cached_provider_session";

    refresh_claimed_runtime_for_launch(
        &shared,
        &ProviderKind::Claude,
        channel_id,
        true,
        (&mut current_path, &mut session_id, &mut loaded, &mut reason),
    )
    .await;

    assert_eq!(current_path, rebound_cwd.path().display().to_string());
    assert_eq!(session_id.as_deref(), Some(rebound_session_id));
    assert!(!loaded);
    assert_eq!(reason, "runtime_session_rebound");
}

#[test]
fn session_strategy_lifecycle_event_records_fresh_and_resumed_details() {
    let fresh = session_strategy_lifecycle_event(None, "no_cached_provider_session", None);
    match fresh {
        TurnEvent::SessionFresh(details) => {
            assert_eq!(details.reason, "no_cached_provider_session");
            assert_eq!(details.provider_session_id, None);
            assert_eq!(details.fingerprint, None);
            assert_eq!(details.recovery_message_count, None);
        }
        other => panic!("expected session_fresh event, got {other:?}"),
    }

    let fresh_with_recovery = session_strategy_lifecycle_event(None, "idle_timeout", Some(9));
    match fresh_with_recovery {
        TurnEvent::SessionFresh(details) => {
            assert_eq!(details.recovery_message_count, Some(9));
        }
        other => panic!("expected session_fresh event, got {other:?}"),
    }

    let resumed = session_strategy_lifecycle_event(
        Some("provider-session-123"),
        "db_provider_session_restored",
        Some(9),
    );
    match resumed {
        TurnEvent::SessionResumed(details) => {
            assert_eq!(details.reason, "db_provider_session_restored");
            assert_eq!(
                details.provider_session_id.as_deref(),
                Some("provider-session-123")
            );
            assert_eq!(
                details.fingerprint.as_deref(),
                Some(
                    crate::services::observability::turn_lifecycle::provider_session_fingerprint(
                        "provider-session-123",
                    )
                    .as_str()
                )
            );
            assert_eq!(details.recovery_message_count, None);
        }
        other => panic!("expected session_resumed event, got {other:?}"),
    }
}

#[test]
fn db_provider_session_restore_existing_cli_renders_session_panel_line() {
    assert!(
        should_emit_session_strategy_lifecycle(
            Some("provider-session-123"),
            "db_provider_session_restored",
            false,
        ),
        "cold DB restore must surface a session lifecycle row even when tmux survived restart"
    );
    assert!(
        !should_emit_session_strategy_lifecycle(
            Some("provider-session-123"),
            "runtime_cached_provider_session",
            false,
        ),
        "steady-state turn-to-turn continuation must stay suppressed"
    );

    let event = session_strategy_lifecycle_event(
        Some("provider-session-123"),
        "db_provider_session_restored",
        None,
    );
    let kind = event.meta().kind;
    let details = event.details_json();

    match &event {
        TurnEvent::SessionResumed(details) => {
            assert_eq!(details.reason, "db_provider_session_restored");
            assert_eq!(
                details.provider_session_id.as_deref(),
                Some("provider-session-123")
            );
        }
        other => panic!("expected session_resumed event, got {other:?}"),
    }

    let events =
        crate::services::discord::placeholder_live_events::PlaceholderLiveEvents::default();
    let channel_id = serenity::ChannelId::new(3_653);
    assert!(events.set_session_panel_lifecycle_event(channel_id, None, kind, &details));

    // #3983 item4: the session line is no longer in the every-tick footer — it is
    // emitted once, at the top, via the one-shot banner claim.
    let footer = events.render_status_panel(
        channel_id,
        &crate::services::provider::ProviderKind::Claude,
        1_700_000_000,
    );
    assert!(!footer.contains("기존 세션 복원"));

    let banner = events
        .claim_session_banner_line(channel_id, &crate::services::provider::ProviderKind::Claude)
        .expect("restored session yields a one-shot banner");
    assert!(banner.contains("기존 세션 복원"));
    assert!(banner.contains("provider session claude#provider…"));
    assert!(!banner.contains("📋 세션 복원"));
}

#[test]
fn cli_just_spawned_for_emit_handles_none_and_blank_session_names() {
    // Non-tmux mode (ProcessBackend / no managed session) always
    // re-spawns the CLI per turn, so the helper must report "just
    // spawned" for None / blank tmux session names.
    assert!(cli_just_spawned_for_emit(None));
    assert!(cli_just_spawned_for_emit(Some("")));
    assert!(cli_just_spawned_for_emit(Some("   ")));
}

#[test]
fn watchdog_timeout_cancel_request_uses_canonical_cancel_source() {
    let channel_id = serenity::ChannelId::new(1479671301387059200);
    let mut inflight = InflightTurnState::new(
        ProviderKind::Codex,
        channel_id.get(),
        Some("adk-cdx".to_string()),
        343742347365974026,
        1501205715878936748,
        1501205715878936749,
        "work on issue".to_string(),
        Some("provider-session".to_string()),
        Some("AgentDesk-codex-adk-cdx".to_string()),
        Some("/tmp/agentdesk-output.jsonl".to_string()),
        None,
        0,
    );
    inflight.dispatch_id = Some("dispatch-1748".to_string());
    inflight.session_key = Some("mac-mini:AgentDesk-codex-adk-cdx".to_string());

    let request = watchdog_timeout_cancel_request(
        &ProviderKind::Codex,
        channel_id,
        Some(&inflight),
        Some(2),
        true,
    );

    assert_eq!(request.reason, WATCHDOG_TIMEOUT_REASON);
    assert_eq!(request.surface, WATCHDOG_TIMEOUT_CANCEL_SOURCE);
    assert_eq!(
        request.lifecycle_path,
        "mailbox_cancel_active_turn.watchdog_timeout"
    );
    assert_eq!(request.queue_depth, Some(2));
    assert!(request.queue_preserved);
    assert!(request.termination_recorded);
    assert_eq!(
        request.correlation.dispatch_id.as_deref(),
        Some("dispatch-1748")
    );
    assert_eq!(
        request.correlation.session_key.as_deref(),
        Some("mac-mini:AgentDesk-codex-adk-cdx")
    );
    assert_eq!(
        request.correlation.turn_id.as_deref(),
        Some("discord:1479671301387059200:1501205715878936748")
    );
}

#[cfg(unix)]
#[test]
fn claude_tui_inflight_diagnostic_state_uses_persisted_timestamp_format() {
    let mut inflight = InflightTurnState::new(
        ProviderKind::Claude,
        1479671301387059200,
        Some("adk-cc".to_string()),
        343742347365974026,
        1501205715878936748,
        1501205715878936749,
        "continue".to_string(),
        Some("provider-session".to_string()),
        Some("AgentDesk-claude-adk-cc".to_string()),
        Some("/tmp/agentdesk-output.jsonl".to_string()),
        None,
        0,
    );

    assert_eq!(
        classify_inflight_diagnostic_state(Some(&inflight)),
        "present"
    );

    inflight.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);
    assert_eq!(
        classify_inflight_diagnostic_state(Some(&inflight)),
        "watcher_owned"
    );

    inflight.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::None);
    inflight.updated_at = (chrono::Local::now()
        - chrono::Duration::seconds(
            crate::services::discord::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64 + 1,
        ))
    .format("%Y-%m-%d %H:%M:%S")
    .to_string();
    assert_eq!(classify_inflight_diagnostic_state(Some(&inflight)), "stale");

    inflight.updated_at = "not-a-timestamp".to_string();
    assert_eq!(
        classify_inflight_diagnostic_state(Some(&inflight)),
        "stale_unparseable_updated_at"
    );
}

#[cfg(unix)]
#[test]
fn claude_tui_structured_busy_followup_blocks_before_prompt_submit() {
    let snapshot = HostedTuiPromptReadinessSnapshot {
        prompt_marker_detected: false,
        prompt_draft_detected: false,
        tmux_pane_alive: true,
        capture_available: true,
        pane_tail: "Thinking...\nRunning tool".to_string(),
    };

    let diagnostic = classify_claude_tui_followup_submission(
        &snapshot,
        "attached",
        Some(1479671301387059200),
        "missing",
        crate::services::tui_turn_state::TuiTurnState::Streaming,
        "AgentDesk-claude-adk-cdx-direct",
    )
    .expect("structured busy TUI turn should block follow-up submission");

    assert!(diagnostic.previous_tui_turn_still_running);
    assert!(!diagnostic.prompt_marker_detected);
    assert_eq!(diagnostic.watcher_state, "attached");
    assert_eq!(diagnostic.inflight_state, "missing");
    assert_eq!(
        diagnostic.watcher_owner_channel_id,
        Some(1479671301387059200)
    );
}

#[cfg(unix)]
#[test]
fn claude_tui_jsonl_authoritative_busy_diagnostic_does_not_capture_pane() {
    let snapshot = HostedTuiPromptReadinessSnapshot::jsonl_authoritative(true);

    let diagnostic = classify_claude_tui_followup_submission(
        &snapshot,
        "attached",
        Some(1479671301387059200),
        "missing",
        crate::services::tui_turn_state::TuiTurnState::Streaming,
        "AgentDesk-claude-adk-cdx-direct",
    )
    .expect("structured busy TUI turn should block follow-up submission");

    assert!(diagnostic.previous_tui_turn_still_running);
    assert!(!diagnostic.capture_available);
    assert_eq!(
        diagnostic.pane_tail,
        "<not captured; JSONL turn state is authoritative>"
    );
}

#[cfg(unix)]
#[test]
fn claude_tui_ready_or_dead_pane_does_not_busy_block_followup() {
    let ready = HostedTuiPromptReadinessSnapshot {
        prompt_marker_detected: true,
        prompt_draft_detected: false,
        tmux_pane_alive: true,
        capture_available: true,
        pane_tail: ">".to_string(),
    };
    assert!(
        classify_claude_tui_followup_submission(
            &ready,
            "attached",
            Some(1),
            "present",
            crate::services::tui_turn_state::TuiTurnState::Unknown,
            "AgentDesk-claude-ready",
        )
        .is_none()
    );

    let dead = HostedTuiPromptReadinessSnapshot {
        prompt_marker_detected: false,
        prompt_draft_detected: false,
        tmux_pane_alive: false,
        capture_available: false,
        pane_tail: "<capture unavailable>".to_string(),
    };
    assert!(
        classify_claude_tui_followup_submission(
            &dead,
            "missing",
            None,
            "stale",
            crate::services::tui_turn_state::TuiTurnState::Unknown,
            "AgentDesk-claude-dead",
        )
        .is_none()
    );
}

#[cfg(unix)]
#[test]
fn claude_tui_transcript_idle_overrides_busy_pane_scrape() {
    let snapshot = HostedTuiPromptReadinessSnapshot {
        prompt_marker_detected: false,
        prompt_draft_detected: false,
        tmux_pane_alive: true,
        capture_available: true,
        pane_tail: "old assistant output with no visible prompt marker".to_string(),
    };

    assert!(
        classify_claude_tui_followup_submission(
            &snapshot,
            "attached",
            Some(1),
            "missing",
            crate::services::tui_turn_state::TuiTurnState::Idle,
            "AgentDesk-claude-ready",
        )
        .is_none()
    );
}

#[cfg(unix)]
#[test]
fn claude_tui_transcript_idle_with_prompt_draft_reaches_provider_recovery() {
    let snapshot = HostedTuiPromptReadinessSnapshot {
        prompt_marker_detected: false,
        prompt_draft_detected: true,
        tmux_pane_alive: true,
        capture_available: true,
        pane_tail: "❯ [TUI-REL-F815-CC-5] stranded draft".to_string(),
    };

    assert!(
        classify_claude_tui_followup_submission(
            &snapshot,
            "attached",
            Some(1),
            "missing",
            crate::services::tui_turn_state::TuiTurnState::Idle,
            "AgentDesk-claude-ready",
        )
        .is_none(),
        "idle transcript plus draft is a provider recovery case, not a router busy block"
    );
}

#[cfg(unix)]
#[test]
fn claude_tui_unknown_transcript_with_prompt_draft_reaches_provider_recovery() {
    let snapshot = HostedTuiPromptReadinessSnapshot {
        prompt_marker_detected: false,
        prompt_draft_detected: true,
        tmux_pane_alive: true,
        capture_available: true,
        pane_tail: "❯ [TUI-REL-F815-CC-5] stranded draft".to_string(),
    };

    assert!(
        classify_claude_tui_followup_submission(
            &snapshot,
            "missing",
            None,
            "missing",
            crate::services::tui_turn_state::TuiTurnState::Unknown,
            "AgentDesk-claude-ready",
        )
        .is_none(),
        "unknown transcript plus draft is a provider recovery case, not a router busy block"
    );
    assert!(
        classify_claude_tui_followup_submission(
            &snapshot,
            "cancelled",
            Some(1479671298497183835),
            "missing",
            crate::services::tui_turn_state::TuiTurnState::Unknown,
            "AgentDesk-claude-ready",
        )
        .is_none(),
        "cancelled watcher plus draft has no active relay evidence; let provider recovery handle it"
    );
}

#[cfg(unix)]
#[test]
fn codex_tui_visible_prompt_draft_reaches_provider_recovery() {
    let snapshot = HostedTuiPromptReadinessSnapshot {
            prompt_marker_detected: true,
            prompt_draft_detected: true,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "› Run /review on my current changes\n\n  gpt-5.5 · gpt-5.5 xhigh · ~/repo · repo · main".to_string(),
        };

    assert!(hosted_tui_draft_should_enter_provider_recovery(
        &ProviderKind::Codex,
        &snapshot
    ));
    assert!(
        !hosted_tui_draft_should_enter_provider_recovery(&ProviderKind::Claude, &snapshot),
        "Claude keeps transcript-busy authority; only Codex compact drafts bypass router busy preflight"
    );
}

#[cfg(unix)]
#[test]
fn claude_tui_unknown_transcript_with_stranded_draft_ignores_persistent_watcher() {
    let snapshot = HostedTuiPromptReadinessSnapshot {
        prompt_marker_detected: false,
        prompt_draft_detected: true,
        tmux_pane_alive: true,
        capture_available: true,
        pane_tail: "❯ possible redraw".to_string(),
    };

    assert!(
        classify_claude_tui_followup_submission(
            &snapshot,
            "attached",
            Some(1),
            "missing",
            crate::services::tui_turn_state::TuiTurnState::Unknown,
            "AgentDesk-claude-active",
        )
        .is_none(),
        "a persistent attached watcher is idle coverage, not active-turn evidence by itself"
    );
}

#[cfg(unix)]
#[test]
fn claude_tui_unknown_transcript_with_active_inflight_still_blocks() {
    let snapshot = HostedTuiPromptReadinessSnapshot {
        prompt_marker_detected: false,
        prompt_draft_detected: true,
        tmux_pane_alive: true,
        capture_available: true,
        pane_tail: "❯ possible redraw".to_string(),
    };

    assert!(
        classify_claude_tui_followup_submission(
            &snapshot,
            "missing",
            None,
            "present",
            crate::services::tui_turn_state::TuiTurnState::Unknown,
            "AgentDesk-claude-active",
        )
        .is_some(),
        "active inflight evidence keeps unknown transcript conservative"
    );
}

#[cfg(unix)]
#[test]
fn claude_tui_transcript_busy_can_block_even_if_prompt_marker_is_visible() {
    let snapshot = HostedTuiPromptReadinessSnapshot {
        prompt_marker_detected: true,
        prompt_draft_detected: false,
        tmux_pane_alive: true,
        capture_available: true,
        pane_tail: "Ready for input (type message + Enter)".to_string(),
    };

    let diagnostic = classify_claude_tui_followup_submission(
        &snapshot,
        "attached",
        Some(1),
        "present",
        crate::services::tui_turn_state::TuiTurnState::Streaming,
        "AgentDesk-claude-streaming",
    )
    .expect("transcript streaming state must be authoritative over pane marker");

    assert!(diagnostic.prompt_marker_detected);
    assert_eq!(
        diagnostic.transcript_turn_state,
        crate::services::tui_turn_state::TuiTurnState::Streaming
    );
}

#[cfg(unix)]
#[test]
fn claude_busy_preflight_uses_idle_transcript_wait_when_transcript_exists() {
    let cwd = tempfile::tempdir().expect("create temp cwd");
    let claude_home = tempfile::tempdir().expect("create temp claude home");
    let session_id = "01234567-89ab-cdef-0123-456789abcdef";
    let transcript_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        cwd.path(),
        session_id,
        Some(claude_home.path()),
    )
    .expect("resolve transcript path");
    std::fs::create_dir_all(transcript_path.parent().expect("transcript parent"))
        .expect("create transcript parent");
    std::fs::write(
        &transcript_path,
        r#"{"type":"system","subtype":"turn_duration","session_id":"s"}"#,
    )
    .expect("write transcript");

    let wait_strategy = hosted_tui_busy_preflight_readiness_wait_with_claude_home(
        &ProviderKind::Claude,
        cwd.path().to_str(),
        Some(session_id),
        None,
        Some(claude_home.path()),
        None,
        Some(std::time::SystemTime::UNIX_EPOCH),
    );

    assert_eq!(
        wait_strategy,
        HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOrIdleTranscript(transcript_path)
    );
}

#[cfg(unix)]
#[test]
fn claude_busy_preflight_falls_back_when_transcript_is_unavailable() {
    let cwd = tempfile::tempdir().expect("create temp cwd");
    let claude_home = tempfile::tempdir().expect("create temp claude home");

    assert_eq!(
        hosted_tui_busy_preflight_readiness_wait_with_claude_home(
            &ProviderKind::Claude,
            cwd.path().to_str(),
            None,
            None,
            Some(claude_home.path()),
            None,
            Some(std::time::SystemTime::UNIX_EPOCH),
        ),
        HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOnly
    );
    assert_eq!(
        hosted_tui_busy_preflight_readiness_wait_with_claude_home(
            &ProviderKind::Claude,
            cwd.path().to_str(),
            Some("not-a-uuid"),
            None,
            Some(claude_home.path()),
            None,
            Some(std::time::SystemTime::UNIX_EPOCH),
        ),
        HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOnly
    );
    assert_eq!(
        hosted_tui_busy_preflight_readiness_wait_with_claude_home(
            &ProviderKind::Claude,
            cwd.path().to_str(),
            Some("01234567-89ab-cdef-0123-456789abcdef"),
            None,
            Some(claude_home.path()),
            None,
            Some(std::time::SystemTime::UNIX_EPOCH),
        ),
        HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOnly
    );
}

#[cfg(unix)]
#[test]
fn codex_busy_preflight_keeps_codex_readiness_wait() {
    let cwd = tempfile::tempdir().expect("create temp cwd");

    let wait_strategy = hosted_tui_busy_preflight_readiness_wait_with_claude_home(
        &ProviderKind::Codex,
        cwd.path().to_str(),
        Some("01234567-89ab-cdef-0123-456789abcdef"),
        None,
        None,
        None,
        None,
    );

    assert_eq!(wait_strategy, HostedTuiBusyPreflightReadinessWait::Codex);
}

#[cfg(unix)]
#[test]
fn codex_rollout_idle_state_allows_followup() {
    let _cache = lock_rollout_cache_test();
    // observe_codex_tui_rollout_state_for_cwd_with_sessions returns Idle
    // when the most recent rollout envelope signals task_complete.
    let cwd = tempfile::tempdir().expect("create temp cwd");
    let sessions = tempfile::tempdir().expect("create temp sessions dir");
    let rollout_path = sessions.path().join("rollout-test-idle.jsonl");
    std::fs::write(
            &rollout_path,
            format!(
                concat!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"s\",\"cwd\":\"{}\"}}}}\n",
                    "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"t1\"}}}}\n"
                ),
                cwd.path().display()
            ),
        )
        .expect("write rollout file");

    let state = observe_codex_tui_rollout_state_for_cwd_with_sessions(
        cwd.path().to_str(),
        Some("s"),
        Some(sessions.path()),
        None,
    );

    assert_eq!(
        state,
        crate::services::tui_turn_state::TuiTurnState::Idle,
        "task_complete envelope must yield Idle so followup is not blocked"
    );
}

#[cfg(unix)]
#[test]
fn codex_rollout_user_submitted_blocks_followup() {
    // classify_claude_tui_followup_submission blocks when Codex rollout
    // signals UserSubmitted (user message written but agent not yet streaming).
    let snapshot = HostedTuiPromptReadinessSnapshot {
        prompt_marker_detected: false,
        prompt_draft_detected: false,
        tmux_pane_alive: true,
        capture_available: true,
        pane_tail: String::new(),
    };

    let diagnostic = classify_claude_tui_followup_submission(
        &snapshot,
        "attached",
        None,
        "present",
        crate::services::tui_turn_state::TuiTurnState::UserSubmitted,
        "AgentDesk-codex-test",
    );

    assert!(
        diagnostic.is_some(),
        "UserSubmitted state must block follow-up injection"
    );
    assert_eq!(
        diagnostic.unwrap().transcript_turn_state,
        crate::services::tui_turn_state::TuiTurnState::UserSubmitted
    );
}

#[cfg(unix)]
#[test]
fn codex_rollout_no_file_treats_as_idle() {
    let _cache = lock_rollout_cache_test();
    // When no rollout file exists for the cwd, the gate must not fire
    // (session hasn't started yet or cwd doesn't match any rollout).
    let cwd = tempfile::tempdir().expect("create temp cwd");
    let empty_sessions = tempfile::tempdir().expect("create empty sessions dir");

    let state = observe_codex_tui_rollout_state_for_cwd_with_sessions(
        cwd.path().to_str(),
        None,
        Some(empty_sessions.path()),
        None,
    );

    assert_eq!(
        state,
        crate::services::tui_turn_state::TuiTurnState::Idle,
        "missing rollout file must yield Idle (session not started)"
    );
}

#[cfg(unix)]
#[test]
fn codex_rollout_provider_session_id_wins_over_newer_same_cwd_rollout() {
    let _cache = lock_rollout_cache_test();
    let cwd = tempfile::tempdir().expect("create temp cwd");
    let sessions = tempfile::tempdir().expect("create temp sessions dir");
    let selected_rollout = sessions.path().join("rollout-selected-idle.jsonl");
    let other_rollout = sessions.path().join("rollout-other-streaming.jsonl");
    std::fs::write(
            &selected_rollout,
            format!(
                concat!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"selected\",\"cwd\":\"{}\"}}}}\n",
                    "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"t1\"}}}}\n"
                ),
                cwd.path().display()
            ),
        )
        .expect("write selected rollout");
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(
            &other_rollout,
            format!(
                concat!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"other\",\"cwd\":\"{}\"}}}}\n",
                    "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"function_call\",\"name\":\"run\",\"call_id\":\"c1\"}}}}\n"
                ),
                cwd.path().display()
            ),
        )
        .expect("write other rollout");

    let state = observe_codex_tui_rollout_state_for_cwd_with_sessions(
        cwd.path().to_str(),
        Some("selected"),
        Some(sessions.path()),
        None,
    );

    assert_eq!(
        state,
        crate::services::tui_turn_state::TuiTurnState::Idle,
        "provider session id must beat a newer rollout from another session in the same cwd"
    );
}

#[cfg(unix)]
#[test]
fn codex_rollout_runtime_binding_path_wins_over_newer_same_cwd_rollout() {
    let cwd = tempfile::tempdir().expect("create temp cwd");
    let sessions = tempfile::tempdir().expect("create temp sessions dir");
    let bound_rollout = sessions.path().join("rollout-bound-idle.jsonl");
    let other_rollout = sessions.path().join("rollout-other-streaming.jsonl");
    std::fs::write(
            &bound_rollout,
            format!(
                concat!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"bound\",\"cwd\":\"{}\"}}}}\n",
                    "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"t1\"}}}}\n"
                ),
                cwd.path().display()
            ),
        )
        .expect("write bound rollout");
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(
            &other_rollout,
            format!(
                concat!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"other\",\"cwd\":\"{}\"}}}}\n",
                    "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"function_call\",\"name\":\"run\",\"call_id\":\"c1\"}}}}\n"
                ),
                cwd.path().display()
            ),
        )
        .expect("write other rollout");
    let runtime_binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
        output_path: bound_rollout.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some("bound".to_string()),
        last_offset: 0,
        relay_last_offset: None,
    };

    let state = observe_codex_tui_rollout_state_for_cwd_with_sessions(
        cwd.path().to_str(),
        None,
        Some(sessions.path()),
        Some(&runtime_binding),
    );

    assert_eq!(
        state,
        crate::services::tui_turn_state::TuiTurnState::Idle,
        "pane-bound runtime binding must beat a newer rollout from another session in the same cwd"
    );
}

#[cfg(unix)]
#[test]
fn codex_rollout_runtime_binding_cross_cwd_is_unknown() {
    let cwd = tempfile::tempdir().expect("create temp cwd");
    let other_cwd = tempfile::tempdir().expect("create other cwd");
    let sessions = tempfile::tempdir().expect("create temp sessions dir");
    let rollout_path = sessions.path().join("rollout-cross-cwd.jsonl");
    std::fs::write(
            &rollout_path,
            format!(
                concat!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"bound\",\"cwd\":\"{}\"}}}}\n",
                    "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"t1\"}}}}\n"
                ),
                other_cwd.path().display()
            ),
        )
        .expect("write cross-cwd rollout");
    let runtime_binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
        output_path: rollout_path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some("bound".to_string()),
        last_offset: 0,
        relay_last_offset: None,
    };

    let state = observe_codex_tui_rollout_state_for_cwd_with_sessions(
        cwd.path().to_str(),
        None,
        Some(sessions.path()),
        Some(&runtime_binding),
    );

    assert_eq!(
        state,
        crate::services::tui_turn_state::TuiTurnState::Unknown,
        "stale tmux runtime bindings must not make readiness decisions for a different cwd"
    );
}

#[cfg(unix)]
#[test]
fn codex_rollout_without_binding_or_session_is_unknown_when_same_cwd_rollout_exists() {
    let _cache = lock_rollout_cache_test();
    let cwd = tempfile::tempdir().expect("create temp cwd");
    let sessions = tempfile::tempdir().expect("create temp sessions dir");
    let rollout_path = sessions.path().join("rollout-ambiguous.jsonl");
    std::fs::write(
            &rollout_path,
            format!(
                concat!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"ambiguous\",\"cwd\":\"{}\"}}}}\n",
                    "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"t1\"}}}}\n"
                ),
                cwd.path().display()
            ),
        )
        .expect("write ambiguous rollout");

    let state = observe_codex_tui_rollout_state_for_cwd_with_sessions(
        cwd.path().to_str(),
        None,
        Some(sessions.path()),
        None,
    );

    assert_eq!(
        state,
        crate::services::tui_turn_state::TuiTurnState::Unknown,
        "without a tmux binding or provider session id, same-cwd rollout files are not pane-bound enough to decide readiness"
    );
}

#[cfg(unix)]
#[test]
fn codex_rollout_without_binding_or_session_conservatively_blocks_busy_same_cwd_rollout() {
    let _cache = lock_rollout_cache_test();
    let cwd = tempfile::tempdir().expect("create temp cwd");
    let sessions = tempfile::tempdir().expect("create temp sessions dir");
    let rollout_path = sessions.path().join("rollout-ambiguous-busy.jsonl");
    std::fs::write(
            &rollout_path,
            format!(
                concat!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"ambiguous\",\"cwd\":\"{}\"}}}}\n",
                    "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"function_call\",\"name\":\"run\",\"call_id\":\"c1\"}}}}\n"
                ),
                cwd.path().display()
            ),
        )
        .expect("write ambiguous busy rollout");

    let state = observe_codex_tui_rollout_state_for_cwd_with_sessions(
        cwd.path().to_str(),
        None,
        Some(sessions.path()),
        None,
    );

    assert_eq!(
        state,
        crate::services::tui_turn_state::TuiTurnState::Streaming,
        "an unbound same-cwd rollout is ambiguous, but a busy envelope must still block unsafe prompt injection"
    );
}

#[cfg(unix)]
#[test]
fn successful_busy_wait_recaptures_offset_past_previous_turn_bytes() {
    use std::io::Write;

    let dir = tempfile::tempdir().expect("create temp dir");
    let output_path = dir.path().join("claude-tui-transcript.jsonl");
    std::fs::write(&output_path, b"already delivered\n").expect("write initial transcript");
    let stale_offset = std::fs::metadata(&output_path).unwrap().len();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&output_path)
        .unwrap()
        .write_all(b"previous turn bytes appended during busy wait\n")
        .expect("append previous-turn bytes");

    let corrected_offset =
        recapture_inflight_offset_after_successful_busy_wait(output_path.to_str(), stale_offset);
    let transcript = std::fs::read(&output_path).expect("read transcript");
    let stale_window = &transcript[stale_offset as usize..];
    let corrected_window = &transcript[corrected_offset as usize..];

    assert!(
        String::from_utf8_lossy(stale_window).contains("previous turn bytes"),
        "test setup must prove the stale offset would recover previous-turn bytes"
    );
    assert_eq!(
        corrected_window, b"",
        "corrected new-turn offset must skip bytes appended while waiting"
    );
}

#[test]
fn parse_dispatch_context_hints_extracts_auto_queue_retry_resume_session() {
    let hints = parse_dispatch_context_hints(
        Some(
            r#"{"auto_queue_retry_resume_session_id":" thread-1585 ","reset_provider_state":false}"#,
        ),
        Some("implementation"),
    );

    assert_eq!(
        hints.retry_resume_session_id.as_deref(),
        Some("thread-1585")
    );
    assert!(!hints.reset_provider_state);
}

#[test]
fn provider_worktree_isolation_policy_keeps_main_provider_on_main_workspace() {
    assert!(!should_force_provider_worktree_isolation(false, None, None,));
}

#[test]
fn provider_worktree_isolation_policy_forces_non_main_provider_channel() {
    assert!(should_force_provider_worktree_isolation(true, None, None));
}

#[test]
fn provider_worktree_isolation_policy_honors_override_false() {
    assert!(!should_force_provider_worktree_isolation(
        true,
        Some(false),
        None,
    ));
}

#[test]
fn provider_worktree_isolation_policy_bypasses_review_e2e_and_consultation_dispatches() {
    for dispatch_type in ["review", "e2e-test", "consultation"] {
        assert!(
            !should_force_provider_worktree_isolation(true, None, Some(dispatch_type)),
            "{dispatch_type} dispatches should bypass provider-channel isolation"
        );
    }
}

// #3208 — helper: write a Claude JSONL transcript for `<cwd>` under a temp
// claude_home, returning the resolved transcript path. The transcript ends with
// a `system/turn_duration` terminator → `observe_*` classifies it as `Idle`.
#[cfg(unix)]
fn write_idle_claude_transcript(
    claude_home: &std::path::Path,
    cwd: &std::path::Path,
    session_id: &str,
) -> std::path::PathBuf {
    let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        cwd,
        session_id,
        Some(claude_home),
    )
    .expect("resolve transcript path");
    std::fs::create_dir_all(path.parent().expect("transcript parent"))
        .expect("create transcript parent");
    std::fs::write(
        &path,
        concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]},"timestamp":"2026-06-07T07:29:12Z"}"#,
            "\n",
            r#"{"type":"system","subtype":"turn_duration","durationMs":142231,"pendingBackgroundAgentCount":5,"timestamp":"2026-06-07T07:29:13Z"}"#,
            "\n",
        ),
    )
    .expect("write transcript");
    path
}

// #3208 (B): a genuinely-idle TUI whose live session runs in a *rotating
// worktree* (pane_cwd) — distinct from the channel's configured workspace
// (current_path) — and whose Claude session_id is NOT carried into intake (the
// common `runtime_cached_provider_session` resume case). The preflight readiness
// resolver MUST find the worktree transcript via the pane cwd and engage the
// idle-JSONL fallback (ClaudePromptMarkerOrIdleTranscript) instead of falling
// back to the prompt-marker-only wait that times out at 45s while the screen
// shows "Waiting for N background agents to finish".
#[cfg(unix)]
#[test]
fn claude_busy_preflight_resolves_worktree_transcript_when_session_id_missing() {
    let claude_home = tempfile::tempdir().expect("create temp claude home");
    let workspace = tempfile::tempdir().expect("create temp workspace cwd");
    let worktree = tempfile::tempdir().expect("create temp worktree cwd");
    // The running session writes its transcript under the WORKTREE project dir.
    let worktree_transcript = write_idle_claude_transcript(
        claude_home.path(),
        worktree.path(),
        "6a053a02-fd2d-4329-b421-9f49eb7d5683",
    );

    // Resolver must locate the worktree transcript even with session_id=None and
    // current_path pointing at the (empty) configured workspace.
    let resolved = resolve_claude_followup_transcript_path(
        workspace.path().to_str(),
        None,
        Some(worktree.path()),
        Some(claude_home.path()),
    );
    assert_eq!(
        resolved.as_deref(),
        Some(worktree_transcript.as_path()),
        "resolver must adopt the worktree transcript via pane cwd"
    );

    // And the preflight wait must therefore allow the idle-transcript fallback.
    let wait_strategy = hosted_tui_busy_preflight_readiness_wait_with_claude_home(
        &ProviderKind::Claude,
        workspace.path().to_str(),
        None,
        Some(worktree.path()),
        Some(claude_home.path()),
        None,
        Some(std::time::SystemTime::UNIX_EPOCH),
    );
    assert_eq!(
        wait_strategy,
        HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOrIdleTranscript(
            worktree_transcript
        ),
        "idle worktree transcript must engage the JSONL fallback, not prompt-marker-only"
    );
}

// #3208 (B): the resolved worktree transcript (turn ended; background agents
// still running) must observe as Idle — a genuinely-idle TUI must never be
// classified busy. This is the false-busy that produced the 45s timeout.
#[cfg(unix)]
#[test]
fn claude_idle_worktree_transcript_observes_idle_not_busy() {
    let claude_home = tempfile::tempdir().expect("create temp claude home");
    let worktree = tempfile::tempdir().expect("create temp worktree cwd");
    let transcript = write_idle_claude_transcript(
        claude_home.path(),
        worktree.path(),
        "6a053a02-fd2d-4329-b421-9f49eb7d5683",
    );

    let provider = ProviderKind::Claude;
    let probe = crate::services::tui_turn_state::JsonlTurnStateProbe::new(&provider, &transcript);
    let state = crate::services::tui_turn_state::TuiTurnStateProbe::observe(&probe);
    assert_eq!(
        state,
        crate::services::tui_turn_state::TuiTurnState::Idle,
        "turn_duration terminator with pending background agents is Idle, not busy"
    );
    assert!(!state.is_busy());
}

// #3208 (A): when the prior turn is genuinely in-flight (authoritative JSONL
// Streaming/UserSubmitted), the follow-up classifier flags a busy diagnostic
// with `previous_tui_turn_still_running=true`. The intake layer routes this to
// the queue-defer path WITHOUT entering the 45s readiness poll (gated on
// `transcript_turn_state.is_busy()`), so no readiness-timeout error surfaces.
#[cfg(unix)]
#[test]
fn claude_busy_followup_defers_to_queue_without_readiness_poll() {
    for busy in [
        crate::services::tui_turn_state::TuiTurnState::Streaming,
        crate::services::tui_turn_state::TuiTurnState::UserSubmitted,
    ] {
        assert!(
            busy.is_busy(),
            "{busy:?} must be the gate that skips the 45s readiness poll"
        );
        let snapshot = HostedTuiPromptReadinessSnapshot::jsonl_authoritative(true);
        let diagnostic = classify_claude_tui_followup_submission(
            &snapshot,
            "attached",
            None,
            "missing",
            busy,
            "AgentDesk-claude-adk-cc",
        )
        .expect("busy turn must yield a defer diagnostic");
        assert!(
            diagnostic.previous_tui_turn_still_running,
            "genuine busy turn must mark previous_tui_turn_still_running"
        );
        assert_eq!(diagnostic.transcript_turn_state, busy);
    }
}

// #3212 — helper: write a BUSY Claude JSONL transcript (last meaningful
// envelope is `assistant` with no terminator) → `observe_*` classifies it as
// `Streaming` (busy). Used to model a concurrent still-running session.
#[cfg(unix)]
fn write_busy_claude_transcript(
    claude_home: &std::path::Path,
    cwd: &std::path::Path,
    session_id: &str,
) -> std::path::PathBuf {
    let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        cwd,
        session_id,
        Some(claude_home),
    )
    .expect("resolve transcript path");
    std::fs::create_dir_all(path.parent().expect("transcript parent"))
        .expect("create transcript parent");
    std::fs::write(
        &path,
        concat!(
            r#"{"type":"user","message":{"content":[{"type":"text","text":"go"}]},"timestamp":"2026-06-07T07:30:00Z"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]},"timestamp":"2026-06-07T07:30:01Z"}"#,
            "\n",
        ),
    )
    .expect("write busy transcript");
    path
}

#[cfg(unix)]
fn pin_mtime(path: &std::path::Path, mtime: std::time::SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open transcript for mtime pin")
        .set_modified(mtime)
        .expect("set transcript mtime");
}

#[cfg(unix)]
fn claude_runtime_binding(
    output_path: &std::path::Path,
) -> crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
    crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        output_path: output_path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: None,
        last_offset: 0,
        relay_last_offset: None,
    }
}

// #3212 (codex P1) RED→GREEN: two distinct-UUID Claude transcripts live in the
// SAME cwd (two concurrent same-cwd sessions). With no per-session identity the
// old resolver returned the newest-mtime transcript — the OTHER session's. The
// runtime binding pins this session's transcript, so the resolver MUST return
// the bound one even though it is the OLDER of the two.
#[cfg(unix)]
#[test]
fn resolver_runtime_binding_beats_newer_same_cwd_other_session() {
    let claude_home = tempfile::tempdir().expect("claude home");
    let cwd = tempfile::tempdir().expect("cwd");

    // THIS session's transcript (bound) — older mtime.
    let mine = write_busy_claude_transcript(
        claude_home.path(),
        cwd.path(),
        "11111111-1111-1111-1111-111111111111",
    );
    // OTHER concurrent session in the same cwd — NEWER mtime, and Idle (finished).
    let other = write_idle_claude_transcript(
        claude_home.path(),
        cwd.path(),
        "22222222-2222-2222-2222-222222222222",
    );
    let base = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    pin_mtime(&mine, base);
    pin_mtime(&other, base + std::time::Duration::from_secs(60));

    let binding = claude_runtime_binding(&mine);
    let resolved = resolve_claude_followup_transcript_path_with_identity(
        cwd.path().to_str(),
        None,
        Some(cwd.path()),
        Some(claude_home.path()),
        Some(&binding),
        Some(std::time::SystemTime::UNIX_EPOCH),
        &std::collections::HashSet::new(),
    );
    assert_eq!(
        resolved.as_deref(),
        Some(mine.as_path()),
        "runtime binding identity must win over the newer other-session transcript"
    );

    // And the resolved (this-session) transcript observes BUSY — so a follow-up
    // is correctly deferred, NOT injected because the other session went idle.
    let provider = ProviderKind::Claude;
    let probe = crate::services::tui_turn_state::JsonlTurnStateProbe::new(
        &provider,
        resolved.as_ref().unwrap(),
    );
    let state = crate::services::tui_turn_state::TuiTurnStateProbe::observe(&probe);
    assert!(
        state.is_busy(),
        "this session is still busy; resolving the idle other-session transcript would false-ready"
    );
}

// #3212 RED→GREEN: a NEWER stale/other-session transcript whose mtime predates
// this session's launch must be rejected by the launch-mtime cutoff. With no
// runtime binding and only a stale candidate, the resolver returns None rather
// than adopting the prior session's transcript.
#[cfg(unix)]
#[test]
fn resolver_rejects_pre_launch_stale_transcript_via_cutoff() {
    let claude_home = tempfile::tempdir().expect("claude home");
    let cwd = tempfile::tempdir().expect("cwd");

    let stale = write_idle_claude_transcript(
        claude_home.path(),
        cwd.path(),
        "33333333-3333-3333-3333-333333333333",
    );
    let base = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    pin_mtime(&stale, base);
    // Session launched AFTER the stale transcript was last written.
    let launch_cutoff = base + std::time::Duration::from_secs(300);

    let resolved = resolve_claude_followup_transcript_path_with_identity(
        cwd.path().to_str(),
        None,
        Some(cwd.path()),
        Some(claude_home.path()),
        None,
        Some(launch_cutoff),
        &std::collections::HashSet::new(),
    );
    assert_eq!(
        resolved, None,
        "a transcript older than this session's launch belongs to a prior session and must not be adopted"
    );

    // Sanity: WITHOUT the cutoff (UNIX_EPOCH), the single candidate is adopted —
    // proving the None above is the cutoff guard, not a missing project dir.
    let resolved_no_cutoff = resolve_claude_followup_transcript_path_with_identity(
        cwd.path().to_str(),
        None,
        Some(cwd.path()),
        Some(claude_home.path()),
        None,
        Some(std::time::SystemTime::UNIX_EPOCH),
        &std::collections::HashSet::new(),
    );
    assert_eq!(resolved_no_cutoff.as_deref(), Some(stale.as_path()));
}

// #3212 RED→GREEN: an EXISTING stale transcript at the exact
// (current_path, session_id) path must NOT short-circuit the happy path when a
// runtime binding points at the real (different) live transcript. The binding's
// per-session identity is authoritative and must beat the stale exact-match.
#[cfg(unix)]
#[test]
fn resolver_runtime_binding_beats_stale_exact_session_id_match() {
    let claude_home = tempfile::tempdir().expect("claude home");
    let cwd = tempfile::tempdir().expect("cwd");
    let session_id = "44444444-4444-4444-4444-444444444444";

    // Stale transcript at the exact (cwd, session_id) location (a prior run that
    // reused this session_id, now finished/idle).
    let stale_exact = write_idle_claude_transcript(claude_home.path(), cwd.path(), session_id);
    // The actual live transcript the watcher is bound to (different UUID, busy).
    let live = write_busy_claude_transcript(
        claude_home.path(),
        cwd.path(),
        "55555555-5555-5555-5555-555555555555",
    );
    let base = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    // Make the stale exact-match the NEWER file to stress that mtime alone is wrong.
    pin_mtime(&live, base);
    pin_mtime(&stale_exact, base + std::time::Duration::from_secs(60));

    let binding = claude_runtime_binding(&live);
    let resolved = resolve_claude_followup_transcript_path_with_identity(
        cwd.path().to_str(),
        Some(session_id),
        Some(cwd.path()),
        Some(claude_home.path()),
        Some(&binding),
        Some(std::time::SystemTime::UNIX_EPOCH),
        &std::collections::HashSet::new(),
    );
    assert_eq!(
        resolved.as_deref(),
        Some(live.as_path()),
        "runtime binding must beat the stale exact (current_path, session_id) match"
    );
}

// #3212 RED→GREEN: two concurrent same-cwd transcripts, NO runtime binding and
// NO usable session_id (the production resume case). The resolver must refuse to
// guess (ambiguity guard → None) rather than pick the newest mtime, which could
// be a finished other-session (false-ready) or another busy turn (wrong queue).
#[cfg(unix)]
#[test]
fn resolver_ambiguous_multi_uuid_same_cwd_refuses_to_guess() {
    let claude_home = tempfile::tempdir().expect("claude home");
    let cwd = tempfile::tempdir().expect("cwd");

    let a = write_busy_claude_transcript(
        claude_home.path(),
        cwd.path(),
        "66666666-6666-6666-6666-666666666666",
    );
    let b = write_idle_claude_transcript(
        claude_home.path(),
        cwd.path(),
        "77777777-7777-7777-7777-777777777777",
    );
    let base = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    pin_mtime(&a, base);
    pin_mtime(&b, base + std::time::Duration::from_secs(60));

    let resolved = resolve_claude_followup_transcript_path_with_identity(
        cwd.path().to_str(),
        None,
        Some(cwd.path()),
        Some(claude_home.path()),
        None,
        Some(std::time::SystemTime::UNIX_EPOCH),
        &std::collections::HashSet::new(),
    );
    assert_eq!(
        resolved, None,
        "two same-cwd candidates with no identity is ambiguous; guessing newest is the P1 bug"
    );

    // Excluding the other session's claimed transcript collapses to a single
    // unambiguous candidate → the resolver may then safely adopt it.
    let exclude: std::collections::HashSet<std::path::PathBuf> = [b.clone()].into_iter().collect();
    let resolved_excluded = resolve_claude_followup_transcript_path_with_identity(
        cwd.path().to_str(),
        None,
        Some(cwd.path()),
        Some(claude_home.path()),
        None,
        Some(std::time::SystemTime::UNIX_EPOCH),
        &exclude,
    );
    assert_eq!(
        resolved_excluded.as_deref(),
        Some(a.as_path()),
        "excluding the other live session's transcript disambiguates the cwd fallback"
    );
}

// #3212 (codex P1-2) RED→GREEN: the pane_cwd holds TWO qualifying transcripts
// (ambiguous concurrent sessions) while a DIFFERENT current_path holds a single
// candidate. The old per-cwd `continue` fell through past the ambiguous pane_cwd
// and adopted the lone current_path transcript. The hard ambiguity guard must
// instead collect candidates across BOTH cwds and return None — never guess.
#[cfg(unix)]
#[test]
fn resolver_pane_cwd_ambiguous_must_not_fall_through_to_current_path() {
    let claude_home = tempfile::tempdir().expect("claude home");
    let pane = tempfile::tempdir().expect("pane cwd");
    let workspace = tempfile::tempdir().expect("workspace cwd");

    // Two concurrent same-cwd transcripts under the pane cwd → ambiguous.
    let pane_a = write_busy_claude_transcript(
        claude_home.path(),
        pane.path(),
        "88888888-8888-8888-8888-888888888888",
    );
    let pane_b = write_idle_claude_transcript(
        claude_home.path(),
        pane.path(),
        "99999999-9999-9999-9999-999999999999",
    );
    // A single (lone) candidate under the configured workspace cwd.
    let ws_only = write_idle_claude_transcript(
        claude_home.path(),
        workspace.path(),
        "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
    );
    let base = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    pin_mtime(&pane_a, base);
    pin_mtime(&pane_b, base + std::time::Duration::from_secs(60));
    pin_mtime(&ws_only, base + std::time::Duration::from_secs(120));

    let resolved = resolve_claude_followup_transcript_path_with_identity(
        workspace.path().to_str(),
        None,
        Some(pane.path()),
        Some(claude_home.path()),
        None,
        Some(std::time::SystemTime::UNIX_EPOCH),
        &std::collections::HashSet::new(),
    );
    assert_eq!(
        resolved, None,
        "ambiguous pane_cwd must hard-stop, not fall through and adopt the lone current_path candidate"
    );
}

// #3212 (codex P1-1) GREEN: a SINGLE post-launch candidate (newer than the
// launch cutoff) with no binding/UUID IS adopted — the legitimate JSONL benefit
// is retained for the common single-session resume case.
#[cfg(unix)]
#[test]
fn resolver_adopts_single_post_launch_candidate() {
    let claude_home = tempfile::tempdir().expect("claude home");
    let cwd = tempfile::tempdir().expect("cwd");

    let live = write_idle_claude_transcript(
        claude_home.path(),
        cwd.path(),
        "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
    );
    let base = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    // Session launched BEFORE the transcript was last written → qualifies.
    let launch_cutoff = base - std::time::Duration::from_secs(60);
    pin_mtime(&live, base);

    let resolved = resolve_claude_followup_transcript_path_with_identity(
        cwd.path().to_str(),
        None,
        Some(cwd.path()),
        Some(claude_home.path()),
        None,
        Some(launch_cutoff),
        &std::collections::HashSet::new(),
    );
    assert_eq!(
        resolved.as_deref(),
        Some(live.as_path()),
        "a single candidate newer than this session's launch is unambiguous and must be adopted"
    );
}

// #3212 (codex P1-1) RED→GREEN: when the launch cutoff CANNOT be obtained
// (`None`), the cwd-mtime fallback is disabled entirely — an unverified single
// candidate must NOT be adopted (accept false-busy over false-ready). Stronger
// identities are absent here, so the resolver returns None.
#[cfg(unix)]
#[test]
fn resolver_conservative_none_when_launch_cutoff_unavailable() {
    let claude_home = tempfile::tempdir().expect("claude home");
    let cwd = tempfile::tempdir().expect("cwd");

    let candidate = write_idle_claude_transcript(
        claude_home.path(),
        cwd.path(),
        "cccccccc-cccc-cccc-cccc-cccccccccccc",
    );
    let base = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    pin_mtime(&candidate, base);

    let resolved = resolve_claude_followup_transcript_path_with_identity(
        cwd.path().to_str(),
        None,
        Some(cwd.path()),
        Some(claude_home.path()),
        None,
        None,
        &std::collections::HashSet::new(),
    );
    assert_eq!(
        resolved, None,
        "no reliable launch time → must not adopt an unverified cwd candidate (false-ready guard)"
    );

    // Sanity: WITH a permissive cutoff the same single candidate IS adopted —
    // proving the None above is the conservative-cutoff guard, not a missing dir.
    let resolved_with_cutoff = resolve_claude_followup_transcript_path_with_identity(
        cwd.path().to_str(),
        None,
        Some(cwd.path()),
        Some(claude_home.path()),
        None,
        Some(std::time::SystemTime::UNIX_EPOCH),
        &std::collections::HashSet::new(),
    );
    assert_eq!(resolved_with_cutoff.as_deref(), Some(candidate.as_path()));
}
