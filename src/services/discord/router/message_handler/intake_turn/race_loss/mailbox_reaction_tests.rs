use super::*;

struct ScopedRuntimeRoot {
    _lock: std::sync::MutexGuard<'static, ()>,
    _temp: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}

impl Drop for ScopedRuntimeRoot {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }
}

fn scoped_runtime_root() -> ScopedRuntimeRoot {
    let lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
    let temp = tempfile::tempdir().expect("temp runtime root");
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
    ScopedRuntimeRoot {
        _lock: lock,
        _temp: temp,
        previous,
    }
}

fn user_intervention(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(id),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: crate::services::discord::runtime_store::load_generation(),
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "race loss mailbox repair".to_string(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

fn mailbox_add_count(shared: &SharedData, message_id: MessageId) -> usize {
    shared
        .turn_view_reconciler
        .ops()
        .iter()
        .filter(|op| op.target.message_id == message_id && op.add && op.emoji == '📬')
        .count()
}

#[tokio::test(flavor = "current_thread")]
async fn failed_queue_reaction_sends_exactly_one_referenced_fallback() {
    let _root = scoped_runtime_root();
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared)
        .expect("fresh shared data")
        .turn_view_reconciler =
        crate::services::discord::turn_view_reconciler::TurnViewReconciler::with_test_deliveries(
            vec![crate::services::discord::turn_view_reconciler::TurnViewDelivery::Failed],
        );
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    let channel_id = ChannelId::new(455_400_000_000_080);
    let message_id = MessageId::new(455_400_000_000_081);
    assert!(
        crate::services::discord::outbound::reaction_control::take_test_reply_deliveries()
            .is_empty()
    );

    mailbox_reaction::note_queue_pending(
        &shared,
        &http,
        channel_id,
        message_id,
        crate::services::discord::queue_reactions::QUEUE_STANDALONE_PENDING_REACTION,
        None,
    )
    .await;

    assert_eq!(
        crate::services::discord::outbound::reaction_control::take_test_reply_deliveries(),
        vec![crate::services::discord::outbound::reaction_control::ReactionControlReplyReason::QueueReactionFailed],
        "failed race-loss reaction must emit exactly one referenced fallback"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_stale_attempt_cannot_clear_newer_pending() {
    let _root = scoped_runtime_root();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    let channel_id = ChannelId::new(455_400_000_000_090);
    let message_id = MessageId::new(455_400_000_000_091);
    let stale_attempt = crate::services::discord::turn_view_reconciler::note_intake_turn_started_current_with_attempt(
        &shared,
        &http,
        channel_id,
        message_id,
        "test_seed_rejected_stale_attempt",
    )
    .await
    .attempt()
    .expect("first start attempt");
    let current_attempt = crate::services::discord::turn_view_reconciler::note_intake_turn_started_current_with_attempt(
        &shared,
        &http,
        channel_id,
        message_id,
        "test_seed_rejected_current_attempt",
    )
    .await
    .attempt()
    .expect("second start attempt");
    assert_ne!(stale_attempt, current_attempt);
    let ops_before = shared.turn_view_reconciler.ops().len();

    mailbox_reaction::clear_rejected_attempt_pending(
        &shared,
        &http,
        channel_id,
        message_id,
        Some(stale_attempt),
    )
    .await;

    assert_eq!(
        shared.turn_view_reconciler.ops().len(),
        ops_before,
        "a delayed rejected attempt must not remove the newer attempt's hourglass"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn standalone_without_start_attempt_adds_mailbox_once() {
    let _root = scoped_runtime_root();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    let channel_id = ChannelId::new(455_400_000_000_100);
    let message_id = MessageId::new(455_400_000_000_101);

    mailbox_reaction::note_queue_pending(
        &shared,
        &http,
        channel_id,
        message_id,
        crate::services::discord::queue_reactions::QUEUE_STANDALONE_PENDING_REACTION,
        None,
    )
    .await;

    assert_eq!(
        mailbox_add_count(&shared, message_id),
        1,
        "race-loss standalone enqueue must imperatively add 📬 without a start attempt"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn matching_start_rollback_and_imperative_add_coalesce_without_double_mailbox() {
    let _root = scoped_runtime_root();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    let channel_id = ChannelId::new(455_400_000_000_110);
    let message_id = MessageId::new(455_400_000_000_111);
    let attempt = crate::services::discord::turn_view_reconciler::note_intake_turn_started_current_with_attempt(
        &shared,
        &http,
        channel_id,
        message_id,
        "test_seed_race_loss_pending",
    )
    .await
    .attempt()
    .expect("pending start attempt");

    mailbox_reaction::note_queue_pending(
        &shared,
        &http,
        channel_id,
        message_id,
        crate::services::discord::queue_reactions::QUEUE_STANDALONE_PENDING_REACTION,
        Some(attempt),
    )
    .await;

    assert_eq!(
        mailbox_add_count(&shared, message_id),
        1,
        "rollback plus imperative ensure must not double-add 📬"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn stale_start_attempt_repairs_mailbox_from_live_queue_truth() {
    let _root = scoped_runtime_root();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(455_400_000_000_120);
    let message_id = MessageId::new(455_400_000_000_121);
    assert!(
        crate::services::discord::mailbox_try_start_turn(
            &shared,
            channel_id,
            Arc::new(CancelToken::new()),
            UserId::new(455_400_000_000_122),
            MessageId::new(455_400_000_000_122),
        )
        .await
    );
    let stale_attempt = crate::services::discord::turn_view_reconciler::note_intake_turn_started_current_with_attempt(
        &shared,
        &http,
        channel_id,
        message_id,
        "test_seed_stale_attempt",
    )
    .await
    .attempt()
    .expect("first start attempt");
    let current_attempt = crate::services::discord::turn_view_reconciler::note_intake_turn_started_current_with_attempt(
        &shared,
        &http,
        channel_id,
        message_id,
        "test_seed_current_attempt",
    )
    .await
    .attempt()
    .expect("second start attempt");
    assert_ne!(stale_attempt, current_attempt);
    let enqueue = enqueue_race_loss_requeued_intervention(
        &shared,
        &provider,
        channel_id,
        message_id,
        user_intervention(message_id.get()),
    )
    .await;
    assert!(enqueue.enqueued);

    mailbox_reaction::note_queue_pending(
        &shared,
        &http,
        channel_id,
        message_id,
        crate::services::discord::queue_reactions::QUEUE_STANDALONE_PENDING_REACTION,
        Some(stale_attempt),
    )
    .await;

    assert_eq!(
        mailbox_add_count(&shared, message_id),
        1,
        "actual mailbox membership must repair 📬 despite a stale rollback attempt"
    );
    let ops = shared.turn_view_reconciler.ops();
    assert_eq!(
        ops.iter()
            .filter(|op| op.target.message_id == message_id && !op.add && op.emoji == '⏳')
            .count(),
        1,
        "queue repair must remove the pending hourglass before adding the marker-only queued state"
    );
    assert_eq!(
        ops.iter()
            .filter(|op| op.target.message_id == message_id && op.add && op.emoji == '⏳')
            .count(),
        1,
        "same-state redispatch must not add a second pending hourglass"
    );
    let mut visible = Vec::new();
    for op in ops.iter().filter(|op| op.target.message_id == message_id) {
        if op.add {
            if !visible.contains(&op.emoji) {
                visible.push(op.emoji);
            }
        } else {
            visible.retain(|emoji| *emoji != op.emoji);
        }
    }
    assert_eq!(visible, vec!['📬']);
}

#[tokio::test(flavor = "current_thread")]
async fn recently_finalized_message_requeued_in_live_mailbox_repairs_mailbox_marker() {
    let _root = scoped_runtime_root();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(455_400_000_000_130);
    let message_id = MessageId::new(455_400_000_000_131);
    let active_message_id = MessageId::new(455_400_000_000_132);

    crate::services::discord::turn_view_reconciler::note_intake_turn_started_current(
        &shared,
        &http,
        channel_id,
        message_id,
        "test_seed_recently_finalized_start",
    )
    .await;
    crate::services::discord::turn_view_reconciler::note_intake_turn_completed(
        &shared,
        &http,
        channel_id,
        message_id,
        shared.restart.current_generation,
        "test_seed_recently_finalized_terminal",
    )
    .await;
    assert!(
        crate::services::discord::mailbox_try_start_turn(
            &shared,
            channel_id,
            Arc::new(CancelToken::new()),
            UserId::new(active_message_id.get()),
            active_message_id,
        )
        .await
    );

    let enqueue = enqueue_race_loss_requeued_intervention(
        &shared,
        &provider,
        channel_id,
        message_id,
        user_intervention(message_id.get()),
    )
    .await;
    assert!(
        enqueue.enqueued,
        "the duplicate race loser genuinely requeues M"
    );
    let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
    assert!(snapshot.intervention_queue.iter().any(|intervention| {
        intervention.message_id == message_id
            || intervention.source_message_ids.contains(&message_id)
    }));

    mailbox_reaction::note_queue_pending(
        &shared,
        &http,
        channel_id,
        message_id,
        crate::services::discord::queue_reactions::QUEUE_STANDALONE_PENDING_REACTION,
        None,
    )
    .await;

    assert_eq!(
        mailbox_add_count(&shared, message_id),
        1,
        "recently-finalized suppression must yield to authoritative live mailbox membership"
    );
}
