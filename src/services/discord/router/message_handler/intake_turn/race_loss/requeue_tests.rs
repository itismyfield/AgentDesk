use super::*;

struct EnvReset(Option<std::ffi::OsString>);

impl Drop for EnvReset {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }
}

fn user_intervention(id: u64, text: &str) -> Intervention {
    Intervention {
        author_id: UserId::new(id),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: crate::services::discord::runtime_store::process_generation(),
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: text.to_string(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn busy_transition_is_durable_before_the_guard_is_released() {
    let tmp = tempfile::tempdir().expect("temp runtime root");
    let _env = crate::config::set_agentdesk_root_for_test(tmp.path());

    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_794_101);
    let message_id = MessageId::new(4_794_102);
    let held = shared
        .session_transition_lock(channel_id)
        .lock_owned()
        .await;

    assert!(
        super::super::super::super::turn_start::try_intake_runtime_transition_after_redirect(
            &shared,
            channel_id,
            (None, false, "/fallback".to_string()),
        )
        .await
        .is_err(),
        "busy intake must not wait outside durable storage"
    );
    let outcome = enqueue_race_loss_requeued_intervention(
        &shared,
        &provider,
        channel_id,
        message_id,
        user_intervention(message_id.get(), "resume transition queue"),
    )
    .await;
    assert!(outcome.enqueued);

    let (disk_queue, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
        &provider,
        &shared.token_hash,
        channel_id,
    );
    assert_eq!(disk_queue.len(), 1);
    assert_eq!(disk_queue[0].message_id, message_id);
    assert_eq!(disk_queue[0].text, "resume transition queue");
    assert!(
        shared
            .session_transition_lock(channel_id)
            .try_lock_owned()
            .is_err(),
        "durability must be established while `/resume` still owns the transition"
    );
    drop(held);
}

#[tokio::test(flavor = "current_thread")]
async fn persistence_failure_rolls_back_queue_and_clears_dispatch_reservation() {
    let tmp = tempfile::tempdir().expect("temp runtime root");
    let _env = crate::config::set_agentdesk_root_for_test(tmp.path());

    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_794_111);
    let message_id = MessageId::new(4_794_112);
    let seed = crate::services::discord::mailbox_enqueue_intervention(
        &shared,
        &provider,
        channel_id,
        user_intervention(message_id.get(), "reserved before persistence failure"),
    )
    .await;
    assert!(seed.enqueued && seed.persistence_error.is_none());
    let taken = shared
        .mailbox(channel_id)
        .take_next_soft(
            crate::services::turn_orchestrator::QueuePersistenceContext::new(
                &provider,
                &shared.token_hash,
                None,
            ),
        )
        .await;
    assert!(taken.intervention.is_some());
    assert_eq!(
        crate::services::discord::mailbox_snapshot(&shared, channel_id)
            .await
            .pending_user_dispatch,
        Some(message_id)
    );
    let blocking_root = tmp.path().join("blocking-root");
    std::fs::write(&blocking_root, "not a directory").expect("blocking root file");
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", &blocking_root) };

    let outcome = enqueue_race_loss_requeued_intervention(
        &shared,
        &provider,
        channel_id,
        message_id,
        user_intervention(message_id.get(), "must roll back"),
    )
    .await;
    assert!(outcome.persistence_error.is_some());
    let surfaced = race_loss_persistence_failure(channel_id, outcome.persistence_error.as_deref())
        .expect_err("durable enqueue failure must reach the caller");
    assert!(
        surfaced
            .to_string()
            .contains("failed to persist queued intake")
    );
    let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
    assert!(snapshot.intervention_queue.is_empty());
    assert_eq!(snapshot.pending_user_dispatch, None);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "current_thread")]
async fn race_loss_requeue_suppresses_post_enqueue_idle_kick_while_holder_active() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
    let tmp = tempfile::tempdir().expect("temp runtime root");
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_078_100);
    let holder_msg = MessageId::new(4_078_102);

    assert!(
        crate::services::discord::mailbox_try_start_turn(
            &shared,
            channel_id,
            Arc::new(CancelToken::new()),
            UserId::new(4_078_102),
            holder_msg,
        )
        .await,
        "seed the active holder that owns the completion wake edge"
    );

    let outcome = enqueue_race_loss_requeued_intervention(
        &shared,
        &provider,
        channel_id,
        MessageId::new(4_078_101),
        user_intervention(4_078_101, "race loss requeue"),
    )
    .await;
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    assert!(outcome.enqueued);
    assert_eq!(
        shared
            .restart
            .deferred_hook_backlog
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "race-loss requeue must not arm a post-enqueue kick/backstop"
    );
    assert!(
        !shared
            .restart
            .deferred_hook_channels
            .contains_key(&channel_id),
        "race-loss requeue must not arm a post-enqueue kick/backstop"
    );
    let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
    assert!(snapshot.cancel_token.is_some());
    assert_eq!(snapshot.active_user_message_id, Some(holder_msg));
    assert_eq!(snapshot.intervention_queue.len(), 1);
}
