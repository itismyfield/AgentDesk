use super::*;

#[test]
fn episode_pin_rejects_every_handoff_consumed_authority_axis() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let mut state = super::inflight::InflightTurnState::new(
        ProviderKind::Claude,
        4_465_879,
        Some("episode-a".to_string()),
        11,
        12,
        13,
        "pin axes".to_string(),
        Some("session-a".to_string()),
        Some("tmux-a".to_string()),
        Some("/tmp/output-a".to_string()),
        Some("/tmp/input-a".to_string()),
        0,
    );
    state.finalizer_turn_id = 12;
    state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    state.set_relay_owner_kind(super::inflight::RelayOwnerKind::Watcher);
    state.turn_nonce = Some("nonce-a".to_string());
    let pin = super::inflight::InflightEpisodePin::from_state(&state);
    let variants = [
        {
            let mut value = state.clone();
            value.request_owner_user_id += 1;
            value
        },
        {
            let mut value = state.clone();
            value.current_msg_id += 1;
            value
        },
        {
            let mut value = state.clone();
            value.runtime_kind = Some(RuntimeHandoffKind::LegacyTmuxWrapper);
            value
        },
        {
            let mut value = state.clone();
            value.input_fifo_path = Some("/tmp/input-b".to_string());
            value
        },
        {
            let mut value = state.clone();
            value.set_relay_owner_kind(super::inflight::RelayOwnerKind::None);
            value
        },
        {
            let mut value = state.clone();
            value.channel_name = Some("episode-b".to_string());
            value
        },
        {
            let mut value = state.clone();
            value.terminal_delivery_committed = true;
            value
        },
    ];
    for replacement in variants {
        assert!(!pin.matches_state(&replacement));
    }
}

#[test]
fn terminal_commit_between_reservation_and_adoption_invalidates_episode_pin() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let provider = ProviderKind::Claude;
    let mut reserved = super::inflight::InflightTurnState::new(
        provider.clone(),
        4_465_878,
        Some("terminal-race".to_string()),
        11,
        12,
        13,
        "terminal commit race".to_string(),
        Some("session-terminal-race".to_string()),
        Some("tmux-terminal-race".to_string()),
        Some("/tmp/terminal-race.jsonl".to_string()),
        None,
        0,
    );
    reserved.turn_nonce = Some("terminal-race-nonce".to_string());
    super::inflight::save_inflight_state(&reserved).expect("seed reserved episode");
    let reserved = super::inflight::load_inflight_state(&provider, reserved.channel_id)
        .expect("load reserved episode");
    let pin = super::inflight::InflightEpisodePin::from_state(&reserved);
    let identity = super::inflight::InflightTurnIdentity::from_state(&reserved);

    assert_eq!(
        super::inflight::commit_watcher_terminal_delivery_locked(
            &provider,
            reserved.channel_id,
            &identity,
            reserved.tmux_session_name.as_deref().expect("tmux session"),
            super::inflight::WatcherTerminalCommitPatch {
                full_response: "terminal".to_string(),
                last_offset: 8,
                last_watcher_relayed_offset: Some(8),
                last_watcher_relayed_generation_mtime_ns: None,
            },
        ),
        super::inflight::WatcherTerminalCommitOutcome::Committed,
    );

    let adoption = super::inflight::adopt_and_lock_inflight_episode(
        &reserved,
        &identity,
        &pin,
        reserved.turn_start_offset,
        None,
    );
    assert!(matches!(
        adoption,
        Err(super::inflight::GuardedSaveOutcome::IdentityMismatch)
    ));
}

#[test]
fn durable_episode_authority_lexically_covers_every_handoff_side_effect() {
    let handoff = include_str!("episode_handoff.rs");
    let terminal_guard = handoff
        .find("guard.state().terminal_delivery_committed")
        .expect("lock-held terminal commit rejection");
    let core_mutation = handoff
        .find("let mut core = shared.core.lock().await")
        .expect("core mutation boundary");
    assert!(terminal_guard < core_mutation);
    for mutation in [
        "note_footer_suppressed_for_message_takeover",
        "let session = core",
        "reregister_active_turn_from_inflight_under_episode_guard",
        "register_rehydrated_tmux_runtime_binding",
    ] {
        handoff
            .find(mutation)
            .unwrap_or_else(|| panic!("missing guarded mutation: {mutation}"));
    }
    assert!(
        !handoff.contains("drop(locked_episode)"),
        "child must return the live authority guard to watcher handoff"
    );

    let parent = include_str!("mod.rs");
    let acquire = parent
        .find("adopt_and_lock_inflight_episode")
        .expect("atomic adoption authority acquisition");
    let commit = parent
        .find("episode_handoff::commit_episode_side_effects")
        .expect("episode side-effect commit");
    let claim = parent[commit..]
        .find("claim_rebind_watcher")
        .map(|relative| commit + relative)
        .expect("guarded watcher claim");
    let spawn = parent[claim..]
        .find("spawn_observed_tmux_watcher")
        .map(|relative| claim + relative)
        .expect("guarded watcher spawn");
    let release = parent[spawn..]
        .find("drop(locked_episode);")
        .map(|relative| spawn + relative)
        .expect("authority release after watcher spawn");
    assert!(acquire < commit && commit < claim && claim < spawn && spawn < release);
}

#[test]
fn crossed_codex_turn_forces_watcher_replacement_before_normal_reuse() {
    let parent = include_str!("mod.rs");
    for required in [
        "Some(&existing.started_at)",
        "Some(existing.turn_source)",
        "let already_relayed_response = if discard_restored_render_seed",
        "let restored_turn = if discard_restored_render_seed",
    ] {
        assert!(
            parent.contains(required),
            "crossed-turn production wiring lost required guard: {required}"
        );
    }
    assert!(parent.contains("claim_rebind_watcher("));
    assert!(parent.contains("discard_restored_render_seed,"));

    let helper = include_str!("watcher_claim.rs");
    let branch = helper
        .find("if crossed_codex_turn")
        .expect("crossed-turn watcher claim branch");
    let forced = helper[branch..]
        .find("claim_or_replace_watcher")
        .map(|relative| branch + relative)
        .expect("crossed Codex turn must force watcher replacement");
    let normal = helper[forced..]
        .find("claim_or_reuse_watcher")
        .map(|relative| forced + relative)
        .expect("ordinary rebind must retain healthy-watcher reuse");
    assert!(branch < forced && forced < normal);
}

#[cfg(unix)]
#[test]
fn replacement_after_adoption_before_claim_is_untouched() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    if !crate::services::platform::tmux::is_available() {
        eprintln!("skipping post-adoption exact-episode test: tmux unavailable");
        return;
    }

    let provider = ProviderKind::Claude;
    let channel_id = 4_465_880_000_001_u64;
    let channel = ChannelId::new(channel_id);
    let tmux_session = format!("AgentDesk-claude-e2e4465-{}-cc", std::process::id());
    let created = crate::services::platform::tmux::create_session(&tmux_session, None, "sleep 60")
        .expect("create tmux session");
    assert!(created.status.success());
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        &tmux_session,
        RuntimeHandoffKind::ClaudeTui,
    )
    .expect("runtime marker");

    let output_path = tmp
        .path()
        .join("48fdb7f3-0000-4000-8000-000000004465.jsonl");
    std::fs::write(&output_path, vec![b'x'; 128]).expect("seed transcript");
    let mut episode_a = super::inflight::InflightTurnState::new(
        provider.clone(),
        channel_id,
        None,
        343_742_347,
        4_465_881,
        4_465_882,
        "post adoption guard".to_string(),
        Some("episode-a-session".to_string()),
        Some(tmux_session.clone()),
        Some(output_path.display().to_string()),
        None,
        0,
    );
    episode_a.turn_nonce = Some("episode-a-nonce".to_string());
    episode_a.set_relay_owner_kind(super::inflight::RelayOwnerKind::None);
    super::inflight::save_inflight_state(&episode_a).expect("seed episode A");
    let authoritative_a = super::inflight::load_inflight_state(&provider, channel_id)
        .expect("authoritative episode A");
    let reserved_pin = super::inflight::InflightEpisodePin::from_state(&authoritative_a);

    let reached = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let resume = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let _barrier = install_post_adoption_claim_barrier(PostAdoptionClaimBarrier {
        reached: reached.clone(),
        resume: resume.clone(),
    });
    let http = std::sync::Arc::new(serenity::Http::new("Bot test-token"));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let shared = runtime.block_on(async { crate::services::discord::make_shared_data_for_tests() });
    let (result, replacement_before, replacement_token, replacement_watcher_cancel) = runtime
        .block_on(async {
            let replacement_token =
                std::sync::Arc::new(crate::services::provider::CancelToken::new());
            assert!(
                super::mailbox_try_start_turn(
                    &shared,
                    channel,
                    replacement_token.clone(),
                    UserId::new(episode_a.request_owner_user_id),
                    MessageId::new(episode_a.effective_finalizer_turn_id()),
                )
                .await
            );
            shared
                .restart
                .global_active
                .store(1, std::sync::atomic::Ordering::Relaxed);
            shared.turn_finalizer.register_start(
                super::turn_finalizer::TurnKey::new(
                    channel,
                    episode_a.effective_finalizer_turn_id(),
                    shared.restart.current_generation,
                ),
                provider.clone(),
                super::inflight::RelayOwnerKind::None,
                &shared,
            );
            assert!(
                !shared
                    .turn_finalizer
                    .has_live_watcher_pending(channel, shared.restart.current_generation,)
                    .await
            );
            {
                let mut core = shared.core.lock().await;
                core.sessions.insert(
                    channel,
                    DiscordSession {
                        session_id: Some("episode-b-session".to_string()),
                        memento_context_loaded: false,
                        memento_reflected: false,
                        current_path: Some("/episode-b/worktree".to_string()),
                        history: Vec::new(),
                        pending_uploads: Vec::new(),
                        cleared: false,
                        remote_profile_name: None,
                        channel_id: Some(channel_id),
                        channel_name: Some("episode-b".to_string()),
                        category_name: None,
                        last_active: tokio::time::Instant::now(),
                        worktree: None,
                        born_generation: shared.restart.current_generation,
                    },
                );
            }
            let replacement_output = tmp
                .path()
                .join("58fdb7f3-0000-4000-8000-000000004465.jsonl");
            std::fs::write(&replacement_output, b"episode-b").expect("B output fixture");
            let replacement_binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: replacement_output.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("episode-b-session".to_string()),
                last_offset: 7,
                relay_last_offset: Some(5),
            };
            crate::services::tui_prompt_dedupe::register_rehydrated_tmux_runtime_binding(
                provider.as_str(),
                &tmux_session,
                channel_id,
                replacement_binding.clone(),
            );
            super::footer_view_reconciler::register_completion_footer_target_for_test(
                channel,
                MessageId::new(episode_a.current_msg_id),
                &provider,
            );
            let replacement_watcher_cancel =
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            shared.tmux_watchers.insert(
                channel,
                TmuxWatcherHandle {
                    tmux_session_name: tmux_session.clone(),
                    output_path: replacement_output.display().to_string(),
                    paused: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    resume_offset: std::sync::Arc::new(std::sync::Mutex::new(None)),
                    cancel: replacement_watcher_cancel.clone(),
                    pause_epoch: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    turn_delivered: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    last_heartbeat_ts_ms: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(1)),
                },
            );
            let task = {
                let shared = shared.clone();
                let http = http.clone();
                let provider = provider.clone();
                let tmux_session = tmux_session.clone();
                tokio::spawn(async move {
                    rebind_inflight_for_channel(
                        &http,
                        &shared,
                        &provider,
                        channel_id,
                        Some(tmux_session),
                        ManualRebindOverrides::default(),
                        Some(&reserved_pin),
                    )
                    .await
                })
            };
            reached.wait().await;
            let mut episode_b = episode_a.clone();
            episode_b.session_id = Some("episode-b-session".to_string());
            episode_b.output_path = Some(replacement_output.display().to_string());
            episode_b.turn_nonce = Some("episode-b-nonce".to_string());
            super::inflight::save_inflight_state(&episode_b).expect("install episode B");
            let replacement_before = serde_json::to_value(
                super::inflight::load_inflight_state(&provider, channel_id)
                    .expect("episode B before claim"),
            )
            .expect("serialize B before");
            resume.wait().await;
            (
                task.await.expect("rebind task"),
                replacement_before,
                replacement_token,
                replacement_watcher_cancel,
            )
        });

    let _ = crate::services::platform::tmux::kill_session(&tmux_session, "#4465 test cleanup");
    assert!(matches!(result, Err(RebindError::InflightEpisodeChanged)));
    assert_eq!(shared.tmux_watchers.len(), 1, "B keeps its sole watcher");
    let watcher = shared
        .tmux_watchers
        .get(&channel)
        .expect("B watcher survives");
    assert!(std::sync::Arc::ptr_eq(
        &watcher.cancel,
        &replacement_watcher_cancel
    ));
    assert!(!replacement_watcher_cancel.load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(
        serde_json::to_value(
            super::inflight::load_inflight_state(&provider, channel_id)
                .expect("episode B survives"),
        )
        .expect("serialize B after"),
        replacement_before,
        "episode B must be byte-semantic untouched"
    );
    let mailbox = runtime.block_on(super::mailbox_snapshot(&shared, channel));
    assert!(std::sync::Arc::ptr_eq(
        mailbox.cancel_token.as_ref().expect("B mailbox token"),
        &replacement_token,
    ));
    assert_eq!(
        shared
            .restart
            .global_active
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert!(
        shared
            .readopted_mailbox_turn_pin_for_test(&provider, channel_id)
            .is_none(),
        "rejected A must not leave a readoption ledger entry for B"
    );
    assert!(
        !runtime.block_on(
            shared
                .turn_finalizer
                .has_live_watcher_pending(channel, shared.restart.current_generation,)
        )
    );
    let core = runtime.block_on(shared.core.lock());
    let session = core.sessions.get(&channel).expect("B session survives");
    assert_eq!(session.session_id.as_deref(), Some("episode-b-session"));
    assert_eq!(session.current_path.as_deref(), Some("/episode-b/worktree"));
    drop(core);
    assert_eq!(
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&tmux_session),
        Some(crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: tmp
                .path()
                .join("58fdb7f3-0000-4000-8000-000000004465.jsonl")
                .display()
                .to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("episode-b-session".to_string()),
            last_offset: 7,
            relay_last_offset: Some(5),
        })
    );
    assert!(super::footer_view_reconciler::completion_footer_has_registered_target(channel));
    let _ = crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&tmux_session);
    let _ = super::footer_view_reconciler::note_footer_suppressed_for_message_takeover(
        channel,
        MessageId::new(episode_a.current_msg_id),
    );
}

#[cfg(unix)]
#[test]
fn replacement_writer_linearizes_after_episode_authority_handoff() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    if !crate::services::platform::tmux::is_available() {
        eprintln!("skipping episode-authority serialization test: tmux unavailable");
        return;
    }

    let provider = ProviderKind::Claude;
    let channel_id = 4_465_880_000_002_u64;
    let tmux_session = format!("AgentDesk-claude-e2e4465-{}-lock", std::process::id());
    let created = crate::services::platform::tmux::create_session(&tmux_session, None, "sleep 60")
        .expect("create tmux session");
    assert!(created.status.success());
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        &tmux_session,
        RuntimeHandoffKind::ClaudeTui,
    )
    .expect("runtime marker");
    let output_path = tmp
        .path()
        .join("68fdb7f3-0000-4000-8000-000000004465.jsonl");
    std::fs::write(&output_path, vec![b'x'; 128]).expect("seed transcript");
    let mut episode_a = super::inflight::InflightTurnState::new(
        provider.clone(),
        channel_id,
        None,
        343_742_347,
        4_465_883,
        4_465_884,
        "authority serialization".to_string(),
        Some("episode-a-session".to_string()),
        Some(tmux_session.clone()),
        Some(output_path.display().to_string()),
        None,
        0,
    );
    episode_a.turn_nonce = Some("episode-a-authority".to_string());
    super::inflight::save_inflight_state(&episode_a).expect("seed A");
    let episode_a = super::inflight::load_inflight_state(&provider, channel_id).expect("load A");
    let reserved_pin = super::inflight::InflightEpisodePin::from_state(&episode_a);
    let mut episode_b = episode_a.clone();
    episode_b.session_id = Some("episode-b-after-handoff".to_string());
    episode_b.turn_nonce = Some("episode-b-after-handoff".to_string());

    let reached = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let resume = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let _barrier = install_episode_authority_held_barrier(EpisodeAuthorityHeldBarrier {
        reached: reached.clone(),
        resume: resume.clone(),
    });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let shared = runtime.block_on(async { crate::services::discord::make_shared_data_for_tests() });
    let http = std::sync::Arc::new(serenity::Http::new("Bot test-token"));
    let (result, writer_finished_while_guarded) = runtime.block_on(async {
        let task = {
            let shared = shared.clone();
            let http = http.clone();
            let provider = provider.clone();
            let tmux_session = tmux_session.clone();
            tokio::spawn(async move {
                rebind_inflight_for_channel(
                    &http,
                    &shared,
                    &provider,
                    channel_id,
                    Some(tmux_session),
                    ManualRebindOverrides::default(),
                    Some(&reserved_pin),
                )
                .await
            })
        };
        reached.wait().await;
        let writer = tokio::task::spawn_blocking(move || {
            super::inflight::save_inflight_state(&episode_b).expect("B writes after authority");
        });
        tokio::time::sleep(std::time::Duration::from_millis(75)).await;
        let prematurely_finished = writer.is_finished();
        resume.wait().await;
        let result = task.await.expect("rebind task");
        writer.await.expect("replacement writer");
        (result, prematurely_finished)
    });

    let _ = crate::services::platform::tmux::kill_session(&tmux_session, "#4465 test cleanup");
    assert!(
        !writer_finished_while_guarded,
        "B writer must block on A's canonical episode authority"
    );
    assert!(
        result.is_ok(),
        "A linearizes its full handoff first: {result:?}"
    );
    let after = super::inflight::load_inflight_state(&provider, channel_id).expect("load B");
    assert_eq!(after.session_id.as_deref(), Some("episode-b-after-handoff"));
    assert_eq!(after.turn_nonce.as_deref(), Some("episode-b-after-handoff"));
}

#[test]
fn relay_setup_failure_rollbacks_are_exact_episode_guarded() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let provider = ProviderKind::Codex;

    let mut adopted = super::inflight::InflightTurnState::new(
        provider.clone(),
        4_465_890,
        None,
        1,
        4_465_891,
        4_465_892,
        "codex rollback".to_string(),
        Some("adopted-session".to_string()),
        Some("AgentDesk-codex-rollback".to_string()),
        Some("/tmp/adopted.jsonl".to_string()),
        None,
        0,
    );
    adopted.turn_nonce = Some("adopted-nonce".to_string());
    super::inflight::save_inflight_state(&adopted).expect("seed adopted row");
    let adopted = super::inflight::load_inflight_state(&provider, adopted.channel_id)
        .expect("load adopted row");
    let adopted_pin = super::inflight::InflightEpisodePin::from_state(&adopted);
    let mut replacement = adopted.clone();
    replacement.session_id = Some("replacement-session".to_string());
    replacement.output_path = Some("/tmp/replacement.jsonl".to_string());
    replacement.turn_nonce = Some("replacement-nonce".to_string());
    super::inflight::save_inflight_state(&replacement).expect("install replacement");
    let replacement_before = serde_json::to_value(
        super::inflight::load_inflight_state(&provider, replacement.channel_id)
            .expect("replacement before rollback"),
    )
    .expect("serialize replacement");
    let outcome = PendingRebindInflightRollback::RestoreExistingAdoption {
        state: adopted.clone(),
        expected: super::inflight::InflightTurnIdentity::from_state(&adopted),
        expected_turn_start_offset: adopted.turn_start_offset,
        expected_last_offset_for_rebase: None,
        expected_episode: Some(adopted_pin),
    }
    .apply();
    assert!(outcome.contains("IdentityMismatch"), "{outcome}");
    assert_eq!(
        serde_json::to_value(
            super::inflight::load_inflight_state(&provider, replacement.channel_id)
                .expect("replacement survives restore rollback"),
        )
        .expect("serialize after rollback"),
        replacement_before
    );

    let mut origin = adopted.clone();
    origin.channel_id += 10;
    origin.user_msg_id = 0;
    origin.finalizer_turn_id = 0;
    origin.rebind_origin = true;
    super::inflight::save_inflight_state(&origin).expect("seed rebind origin");
    let origin = super::inflight::load_inflight_state(&provider, origin.channel_id)
        .expect("load rebind origin");
    let mut replacement_origin = origin.clone();
    replacement_origin.session_id = Some("replacement-origin-session".to_string());
    replacement_origin.turn_nonce = Some("replacement-origin-nonce".to_string());
    super::inflight::save_inflight_state(&replacement_origin).expect("replace origin");
    let origin_before = serde_json::to_value(
        super::inflight::load_inflight_state(&provider, origin.channel_id)
            .expect("replacement origin before rollback"),
    )
    .expect("serialize origin replacement");
    let outcome = PendingRebindInflightRollback::ClearRebindOrigin {
        provider: provider.clone(),
        channel_id: origin.channel_id,
        expected: super::inflight::InflightTurnIdentity::from_state(&origin),
        expected_turn_nonce: origin.turn_nonce.clone(),
    }
    .apply();
    assert!(outcome.contains("UserMsgMismatch"), "{outcome}");
    assert_eq!(
        serde_json::to_value(
            super::inflight::load_inflight_state(&provider, origin.channel_id)
                .expect("replacement origin survives rollback"),
        )
        .expect("serialize origin after"),
        origin_before
    );
}
