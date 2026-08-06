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
        QueuedIntakeCause::SessionTransitionBusy,
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
        QueuedIntakeCause::RaceLoss,
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
        QueuedIntakeCause::RaceLoss,
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

async fn yield_spawned_tasks() {
    for _ in 0..6 {
        tokio::task::yield_now().await;
    }
}

type RecordedKicks = Arc<std::sync::Mutex<Vec<&'static str>>>;

fn record_idle_queue_kicks(
    channel_id: ChannelId,
) -> (
    RecordedKicks,
    crate::services::discord::queue_io::IdleQueueKickHookResetForTests,
) {
    let kicks: RecordedKicks = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = kicks.clone();
    let reset = crate::services::discord::queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(
        move |shared, provider, channel, reason| {
            let recorded = recorded.clone();
            Box::pin(async move {
                if channel != channel_id {
                    return None;
                }
                recorded.lock().expect("recorded kicks").push(reason);
                let taken = crate::services::discord::mailbox_take_next_automatic_intervention(
                    &shared, &provider, channel,
                )
                .await;
                Some(crate::services::discord::IdleQueueKickoffChannelOutcome {
                    started: taken.intervention.is_some(),
                })
            })
        },
    ));
    (kicks, reset)
}

/// #5170 A, under-enforcement direction: a `SessionTransitionBusy` intake is
/// not a lost race and must not re-arm the immediate edge-trigger recheck.
/// That recheck was the engine of the observed spin — each rotation re-entered
/// intake against a transition it could not take, which requeued and spawned
/// the next rotation. The durable enqueue itself is unchanged (asserted here so
/// the crash-loss window stays closed).
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn transition_busy_requeue_replaces_the_immediate_rekick_with_the_slow_backstop_5170() {
    let tmp = tempfile::tempdir().expect("temp runtime root");
    let _env = crate::config::set_agentdesk_root_for_test(tmp.path());

    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(5_170_100);
    let message_id = MessageId::new(5_170_101);
    let (kicks, _hook) = record_idle_queue_kicks(channel_id);

    let outcome = enqueue_race_loss_requeued_intervention(
        &shared,
        &provider,
        channel_id,
        message_id,
        user_intervention(message_id.get(), "transition busy intake"),
        QueuedIntakeCause::SessionTransitionBusy,
    )
    .await;
    assert!(
        outcome.enqueued && outcome.persistence_error.is_none(),
        "the durable enqueue is unchanged by the cause tag"
    );
    yield_spawned_tasks().await;

    // The channel is fully idle here: no holder, no transition guard. The
    // race-loss cause would kick immediately (see the control test below); the
    // transition-busy cause must not, because the transition it just failed to
    // take is still in flight.
    assert!(
        kicks.lock().expect("recorded kicks").is_empty(),
        "a transition-busy requeue must not re-kick into the transition it just lost"
    );

    // Over-suppression direction: the backlog must still have an owner.
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    yield_spawned_tasks().await;
    assert_eq!(
        kicks.lock().expect("recorded kicks").as_slice(),
        ["intake_session_transition_busy"],
        "the slow fail-open backstop must drain the backlog the requeue declined to kick"
    );
    assert!(
        crate::services::discord::mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .is_empty(),
        "the backstop kick must consume the queued message"
    );
}

/// Control for the test above: a genuine mailbox race loss keeps the immediate
/// recheck, so the #5170 split narrows the edge-trigger to the cause that
/// actually owns an edge rather than removing it.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn race_loss_requeue_still_takes_the_immediate_idle_recheck_5170() {
    let tmp = tempfile::tempdir().expect("temp runtime root");
    let _env = crate::config::set_agentdesk_root_for_test(tmp.path());

    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(5_170_200);
    let message_id = MessageId::new(5_170_201);
    let (kicks, _hook) = record_idle_queue_kicks(channel_id);

    let outcome = enqueue_race_loss_requeued_intervention(
        &shared,
        &provider,
        channel_id,
        message_id,
        user_intervention(message_id.get(), "race loss requeue"),
        QueuedIntakeCause::RaceLoss,
    )
    .await;
    assert!(outcome.enqueued && outcome.persistence_error.is_none());
    yield_spawned_tasks().await;

    assert_eq!(
        kicks.lock().expect("recorded kicks").as_slice(),
        ["race_loss_requeue_idle_recheck"],
        "a real race loss against an already-finished opponent keeps its immediate recheck"
    );
}
