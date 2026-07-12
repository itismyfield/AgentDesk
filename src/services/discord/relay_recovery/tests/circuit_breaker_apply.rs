use super::*;

#[tokio::test]
async fn durable_reattach_circuit_open_preserves_every_live_turn_authority() {
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let (_root_guard, root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Codex;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let channel = ChannelId::new(4_465_001);
    let user_message = MessageId::new(4_465_101);
    let response_message = MessageId::new(4_465_201);
    let tmux_session = "AgentDesk-codex-adk-cdx-4465";
    let output_path = root_dir.path().join("relay-4465-live.jsonl");
    std::fs::write(&output_path, "{\"type\":\"assistant\"}\n").expect("seed output");

    let token = start_test_turn(&shared, channel, user_message).await;
    token
        .tmux_session
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .replace(tmux_session.to_string());
    shared.restart.global_active.store(1, Ordering::Relaxed);
    let mut state = super::super::super::inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        Some("adk-cdx".to_string()),
        343_742_347,
        user_message.get(),
        response_message.get(),
        "long-running live turn".to_string(),
        Some("provider-session-4465".to_string()),
        Some(tmux_session.to_string()),
        Some(output_path.to_string_lossy().to_string()),
        None,
        0,
    );
    state.last_watcher_relayed_offset = Some(0);
    super::super::super::inflight::save_inflight_state_create_new(&state).expect("seed inflight");
    let (watcher, watcher_cancel) = test_watcher_handle(tmux_session, &output_path);
    shared.tmux_watchers.insert(channel, watcher);

    let snapshot = RelayHealthSnapshot {
        provider: provider.as_str().to_string(),
        channel_id: channel.get(),
        active_turn: RelayActiveTurn::Foreground,
        tmux_session: Some(tmux_session.to_string()),
        tmux_alive: Some(true),
        watcher_attached: true,
        watcher_owner_channel_id: Some(channel.get()),
        watcher_owns_live_relay: true,
        bridge_inflight_present: true,
        bridge_current_msg_id: Some(response_message.get()),
        mailbox_has_cancel_token: true,
        mailbox_active_user_msg_id: Some(user_message.get()),
        last_capture_offset: Some(128),
        last_relay_offset: 0,
        unread_bytes: Some(128),
        desynced: true,
        ..snapshot()
    };
    let mut decision = plan_relay_recovery(
        &snapshot,
        RelayStallState::TmuxAliveRelayDead,
        chrono::Utc::now().timestamp_millis(),
    );
    decision.affected.finalizer_turn_id = Some(state.effective_finalizer_turn_id());
    for expected_attempt in 1..=decision.auto_heal.max_attempts_per_window {
        assert!(matches!(
            circuit_breaker::reserve_current_episode(
                &provider,
                &decision,
                decision.auto_heal.max_attempts_per_window,
            ),
            circuit_breaker::CircuitReservation::Reserved { attempt, .. }
                if attempt == expected_attempt
        ));
    }
    clear_auto_heal_attempts_for_tests();

    let mailbox_before = super::super::super::mailbox_snapshot(&shared, channel).await;
    let inflight_before = serde_json::to_value(
        super::super::super::inflight::load_inflight_state_read_only(&provider, channel.get())
            .expect("inflight before"),
    )
    .expect("serialize inflight before");
    let response = apply_relay_recovery_plan(
        &registry,
        &shared,
        &provider,
        decision,
        chrono::Utc::now().timestamp_millis(),
        RelayRecoveryApplySource::StallWatchdog,
    )
    .await;
    let mailbox_after = super::super::super::mailbox_snapshot(&shared, channel).await;
    let inflight_after = serde_json::to_value(
        super::super::super::inflight::load_inflight_state_read_only(&provider, channel.get())
            .expect("inflight after"),
    )
    .expect("serialize inflight after");

    assert!(response.skipped);
    assert!(!response.applied);
    assert_eq!(
        response.decision.auto_heal.skipped_reason,
        Some("durable_reattach_circuit_open")
    );
    assert!(Arc::ptr_eq(
        mailbox_before.cancel_token.as_ref().expect("token before"),
        mailbox_after.cancel_token.as_ref().expect("token after")
    ));
    assert_eq!(
        mailbox_before.active_user_message_id,
        mailbox_after.active_user_message_id
    );
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
    assert_eq!(inflight_after, inflight_before);
    let watcher_after = shared
        .tmux_watchers
        .get(&channel)
        .expect("watcher retained");
    assert!(Arc::ptr_eq(&watcher_after.cancel, &watcher_cancel));
    assert!(!watcher_cancel.load(Ordering::Relaxed));
    assert!(!token.cancelled.load(Ordering::Relaxed));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_reserved_dead_frontier_apply_preserves_episode_and_reattaches_watcher() {
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let (_root_guard, root_dir) = isolated_agentdesk_root();
    if !crate::services::platform::tmux::is_available() {
        eprintln!("skipping #4465 production-shaped reattach: tmux unavailable");
        return;
    }
    let provider = ProviderKind::Claude;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    registry
        .register_http(
            provider.as_str().to_string(),
            Arc::new(poise::serenity_prelude::Http::new("Bot test-token")),
        )
        .await;
    let channel = ChannelId::new(4_465_002);
    let user_message = MessageId::new(4_465_102);
    let response_message = MessageId::new(4_465_202);
    let tmux_session = format!("AgentDesk-claude-e2e4465-{}-cc", std::process::id());
    let created = crate::services::platform::tmux::create_session(&tmux_session, None, "sleep 60")
        .expect("create tmux session");
    assert!(created.status.success());
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        &tmux_session,
        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
    )
    .expect("runtime marker");
    let output_path = root_dir.path().join("relay-4465-first-reserved.jsonl");
    std::fs::write(&output_path, vec![b'x'; 128]).expect("seed output");

    let token = start_test_turn(&shared, channel, user_message).await;
    token
        .tmux_session
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .replace(tmux_session.clone());
    shared.restart.global_active.store(1, Ordering::Relaxed);
    let mut state = super::super::super::inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        Some("adk-claude".to_string()),
        343_742_347,
        user_message.get(),
        response_message.get(),
        "first reserved dead frontier".to_string(),
        Some("provider-session-4465-first".to_string()),
        Some(tmux_session.clone()),
        Some(output_path.to_string_lossy().to_string()),
        None,
        0,
    );
    state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    state.turn_nonce = Some("nonce-4465-first".to_string());
    state.set_relay_owner_kind(super::super::super::inflight::RelayOwnerKind::Watcher);
    super::super::super::inflight::save_inflight_state_create_new(&state).expect("seed inflight");
    let (watcher, old_cancel) = test_watcher_handle(&tmux_session, &output_path);
    watcher.last_heartbeat_ts_ms.store(1, Ordering::Release);
    shared.tmux_watchers.insert(channel, watcher);

    let snapshot = RelayHealthSnapshot {
        provider: provider.as_str().to_string(),
        channel_id: channel.get(),
        active_turn: RelayActiveTurn::Foreground,
        tmux_session: Some(tmux_session.clone()),
        tmux_alive: Some(true),
        watcher_attached: true,
        watcher_owner_channel_id: Some(channel.get()),
        watcher_owns_live_relay: true,
        bridge_inflight_present: true,
        bridge_current_msg_id: Some(response_message.get()),
        mailbox_has_cancel_token: true,
        mailbox_active_user_msg_id: Some(user_message.get()),
        last_capture_offset: Some(128),
        last_relay_offset: 0,
        unread_bytes: Some(128),
        desynced: true,
        ..snapshot()
    };
    let mut decision = plan_relay_recovery(
        &snapshot,
        RelayStallState::TmuxAliveRelayDead,
        chrono::Utc::now().timestamp_millis(),
    );
    decision.affected.finalizer_turn_id = Some(state.effective_finalizer_turn_id());
    let response = apply_relay_recovery_plan(
        &registry,
        &shared,
        &provider,
        decision,
        chrono::Utc::now().timestamp_millis(),
        RelayRecoveryApplySource::ProbeAutoHeal,
    )
    .await;

    let _ = crate::services::platform::tmux::kill_session(
        &tmux_session,
        "#4465 first-reserved test cleanup",
    );
    assert!(response.applied, "{response:?}");
    let apply = response.apply_result.expect("apply result");
    assert_eq!(apply.reattach_watcher_spawned, Some(true));
    assert_eq!(apply.reattach_watcher_replaced, Some(true));
    assert!(old_cancel.load(Ordering::Relaxed));
    assert!(!token.cancelled.load(Ordering::Relaxed));
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
    let after =
        super::super::super::inflight::load_inflight_state_read_only(&provider, channel.get())
            .expect("exact live episode survives");
    assert_eq!(after.user_msg_id, state.user_msg_id);
    assert_eq!(after.session_id, state.session_id);
    assert_eq!(after.output_path, state.output_path);
    assert_eq!(after.turn_nonce, state.turn_nonce);
    assert!(
        after.readopted_from_inflight,
        "readoption marker must commit while exact authority is still held"
    );
    assert_eq!(
        shared.readopted_mailbox_turn_pin_for_test(&provider, channel.get()),
        Some(
            super::super::super::readopted_mailbox_ledger::ReadoptedMailboxTurnPin::from_state(
                &after,
            ),
        ),
        "in-memory readoption authority must be scoped to the same stable turn"
    );
    let mut progressed_same_turn = after.clone();
    progressed_same_turn.current_msg_id += 1;
    progressed_same_turn.session_id = Some("provider-session-4465-progressed".to_string());
    progressed_same_turn.tmux_session_name = Some(format!("{tmux_session}-progressed"));
    progressed_same_turn.output_path = Some(
        root_dir
            .path()
            .join("relay-4465-progressed.jsonl")
            .display()
            .to_string(),
    );
    progressed_same_turn.input_fifo_path = Some(
        root_dir
            .path()
            .join("relay-4465-progressed.fifo")
            .display()
            .to_string(),
    );
    progressed_same_turn
        .set_relay_owner_kind(super::super::super::inflight::RelayOwnerKind::StandbyRelay);
    shared.mark_readopted_mailbox_owner_finished_for_episode(
        &provider,
        channel.get(),
        progressed_same_turn.request_owner_user_id,
        progressed_same_turn.effective_finalizer_turn_id(),
        &progressed_same_turn,
    );
    assert!(
        shared.is_readopted_mailbox_owner(
            &provider,
            channel.get(),
            progressed_same_turn.request_owner_user_id,
            progressed_same_turn.effective_finalizer_turn_id(),
        ),
        "legitimate handoff progress must not prevent the terminal finish stamp"
    );
    shared.record_readopted_mailbox_owner_for_episode(
        &provider,
        channel.get(),
        after.request_owner_user_id,
        after.effective_finalizer_turn_id(),
        &after,
    );
    let mut different_turn = progressed_same_turn;
    different_turn.turn_nonce = Some("nonce-4465-successor".to_string());
    shared.mark_readopted_mailbox_owner_finished_for_episode(
        &provider,
        channel.get(),
        different_turn.request_owner_user_id,
        different_turn.effective_finalizer_turn_id(),
        &different_turn,
    );
    assert!(
        !shared.is_readopted_mailbox_owner(
            &provider,
            channel.get(),
            different_turn.request_owner_user_id,
            different_turn.effective_finalizer_turn_id(),
        ),
        "a different stable turn must not inherit the finish stamp"
    );
    let mailbox = super::super::super::mailbox_snapshot(&shared, channel).await;
    assert!(Arc::ptr_eq(
        mailbox.cancel_token.as_ref().expect("live token retained"),
        &token,
    ));
}
