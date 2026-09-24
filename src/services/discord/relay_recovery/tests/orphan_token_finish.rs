use super::*;
use crate::services::turn_orchestrator::{Intervention, InterventionMode};

fn queued(message_id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(message_id),
        queued_generation: crate::services::discord::runtime_store::load_generation(),
        source_message_ids: vec![MessageId::new(message_id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("queued behind orphan token {message_id}"),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

async fn enqueue_three(shared: &Arc<SharedData>, provider: &ProviderKind, channel: ChannelId) {
    for id in 1..=3 {
        crate::services::discord::mailbox_enqueue_intervention(
            shared,
            provider,
            channel,
            queued(9_000 + id),
        )
        .await;
    }
}

/// The decision the operator lane plans from its own snapshot, before apply.
async fn planned_decision(registry: &HealthRegistry, channel: ChannelId) -> RelayRecoveryDecision {
    run_relay_recovery_at(
        registry,
        Some("codex"),
        channel.get(),
        false,
        chrono::Utc::now().timestamp_millis(),
    )
    .await
    .expect("dry run should plan")
    .decision
}

#[tokio::test]
async fn orphan_token_apply_finishes_the_snapshot_episode_and_keeps_the_queue() {
    let (_root_guard, _root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Codex;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let channel = ChannelId::new(5_996_301);
    let token = start_test_turn(&shared, channel, MessageId::new(5_996_310)).await;
    enqueue_three(&shared, &provider, channel).await;
    shared.restart.global_active.store(1, Ordering::Relaxed);
    let decision = planned_decision(&registry, channel).await;
    assert_eq!(
        decision.action,
        RelayRecoveryActionKind::ClearOrphanPendingToken
    );

    let result = apply_relay_recovery_decision(
        &registry,
        &shared,
        &provider,
        &decision,
        None,
        RelayRecoveryApplySource::Manual,
    )
    .await;

    assert_eq!(result.status, "applied");
    assert!(result.removed_mailbox_token);
    assert_eq!(result.post_mailbox_queue_depth, Some(3));
    let after = super::super::mailbox_snapshot(&shared, channel).await;
    assert!(after.cancel_token.is_none());
    assert_eq!(
        after.intervention_queue.len(),
        3,
        "finishing the orphan must not supersede the queued messages"
    );
    assert!(token.cancelled.load(Ordering::Relaxed));
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    assert!(
        shared.restart.deferred_hook_channels.contains_key(&channel),
        "the preserved queue must be handed to a kickoff owner"
    );
}

#[tokio::test]
async fn orphan_token_apply_leaves_a_successor_episode_untouched() {
    let (_root_guard, _root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Codex;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let channel = ChannelId::new(5_996_401);
    let anchor = MessageId::new(5_996_410);
    start_test_turn(&shared, channel, anchor).await;
    enqueue_three(&shared, &provider, channel).await;
    let stale = planned_decision(&registry, channel).await;

    // A->B under the same message id: B is backdated so only its nonce, not its
    // start instant, separates it from the planned episode.
    super::super::mailbox_finish_turn(&shared, &provider, channel).await;
    let successor = start_test_turn(&shared, channel, anchor).await;
    shared
        .mailbox(channel)
        .age_active_turn_for_test(std::time::Duration::from_secs(60))
        .await;
    shared.restart.global_active.store(1, Ordering::Relaxed);

    let result = apply_relay_recovery_decision(
        &registry,
        &shared,
        &provider,
        &stale,
        None,
        RelayRecoveryApplySource::Manual,
    )
    .await;

    assert!(!result.removed_mailbox_token);
    assert_eq!(result.status, "orphan_token_episode_changed");
    let after = super::super::mailbox_snapshot(&shared, channel).await;
    assert!(
        after
            .cancel_token
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &successor))
    );
    assert_eq!(after.active_user_message_id, Some(anchor));
    assert_eq!(after.intervention_queue.len(), 3);
    assert!(!successor.cancelled.load(Ordering::Relaxed));
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn manual_recovery_finishes_a_measured_dead_orphan_and_promotes_its_queue() {
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let (_root_guard, _root_dir) = isolated_agentdesk_root();
    if !crate::services::platform::tmux::is_available() {
        eprintln!("skipping measured-death manual recovery: tmux unavailable");
        return;
    }
    let provider = ProviderKind::Codex;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let channel = ChannelId::new(5_996_501);
    let token = start_test_turn(&shared, channel, MessageId::new(5_996_510)).await;
    token.bind_unmanaged_session_name(&format!("plain-shell-5996-manual-{}", std::process::id()));
    enqueue_three(&shared, &provider, channel).await;
    shared.restart.global_active.store(1, Ordering::Relaxed);

    let response = run_relay_recovery_at(
        &registry,
        Some("codex"),
        channel.get(),
        true,
        chrono::Utc::now().timestamp_millis()
            + ORPHAN_PENDING_TOKEN_ADMISSION_GRACE.as_millis() as i64,
    )
    .await
    .expect("manual recovery should evaluate");

    assert_eq!(response.decision.evidence.tmux_alive, Some(false));
    assert!(response.applied, "{:?}", response.decision.auto_heal);
    let after = super::super::mailbox_snapshot(&shared, channel).await;
    assert!(after.cancel_token.is_none());
    assert_eq!(after.intervention_queue.len(), 3);
    assert!(token.cancelled.load(Ordering::Relaxed));
    assert!(shared.restart.deferred_hook_channels.contains_key(&channel));
}
