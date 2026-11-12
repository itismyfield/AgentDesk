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
async fn persistence_failure_rolls_back_owner_return_and_preserves_dispatch_reservation() {
    use crate::services::turn_orchestrator::test_support::{fail_queue_saves, queue_save_faults};

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
    let handle = shared.mailbox(channel_id);
    let persistence = crate::services::turn_orchestrator::QueuePersistenceContext::new(
        &provider,
        &shared.token_hash,
        None,
    );
    let tail = user_intervention(4_794_113, "queued behind reserved head");
    assert!(
        handle
            .enqueue(tail.clone(), persistence.clone())
            .await
            .enqueued
    );
    let marker_before = crate::services::turn_orchestrator::load_channel_pending_dispatch_marker(
        &provider,
        &shared.token_hash,
        channel_id,
    )
    .expect("reserved head marker");
    let lease = taken.dispatch_lease.unwrap();
    let returned = user_intervention(message_id.get(), "must roll back");
    fail_queue_saves(channel_id, 1);
    let outcome = handle
        .return_dequeued_head(returned.clone(), persistence.clone(), lease.clone())
        .await;
    assert_eq!(
        queue_save_faults(channel_id, false),
        0,
        "save fault must be consumed"
    );
    assert!(
        outcome
            .persistence_error
            .as_deref()
            .is_some_and(|error| { error.contains("injected pending queue save failure") })
    );
    assert!(outcome.persistence_error.is_some());
    let surfaced = race_loss_persistence_failure(channel_id, outcome.persistence_error.as_deref())
        .expect_err("durable enqueue failure must reach the caller");
    assert!(
        surfaced
            .to_string()
            .contains("failed to persist queued intake")
    );
    let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
    assert_eq!(snapshot.intervention_queue.len(), 1);
    assert_eq!(snapshot.intervention_queue[0].message_id, tail.message_id);
    assert_eq!(snapshot.intervention_queue[0].text, tail.text);
    assert_eq!(snapshot.pending_user_dispatch, Some(message_id));
    let marker_after = crate::services::turn_orchestrator::load_channel_pending_dispatch_marker(
        &provider,
        &shared.token_hash,
        channel_id,
    )
    .expect("failed return preserves marker");
    assert_eq!(marker_after.0.message_id, marker_before.0.message_id);
    assert_eq!(marker_after.0.text, marker_before.0.text);
    let (disk_queue, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
        &provider,
        &shared.token_hash,
        channel_id,
    );
    assert_eq!(
        disk_queue
            .iter()
            .map(|item| item.message_id)
            .collect::<Vec<_>>(),
        vec![tail.message_id]
    );
    let retried = handle
        .return_dequeued_head(returned, persistence, lease)
        .await;
    assert!(retried.enqueued && retried.persistence_error.is_none());
    let snapshot = handle.snapshot().await;
    assert_eq!(snapshot.pending_user_dispatch, None);
    assert_eq!(
        snapshot
            .intervention_queue
            .iter()
            .map(|item| item.message_id)
            .collect::<Vec<_>>(),
        vec![message_id, tail.message_id]
    );
    assert!(
        crate::services::turn_orchestrator::load_channel_pending_dispatch_marker(
            &provider,
            &shared.token_hash,
            channel_id,
        )
        .is_some()
    );
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

#[tokio::test(flavor = "current_thread")]
async fn c1_actor_unreachable_race_loss_clears_only_matching_pending_attempt() {
    use crate::services::discord::turn_view_reconciler::{self, TurnViewIdentity, TurnViewTarget};
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = ChannelId::new(6_325_461);
    let message = MessageId::new(100_000_000_632_546);
    let http = Arc::new(serenity::http::Http::new("fixture"));
    let target = TurnViewTarget::intake_user_message(channel, message);
    let attempt = shared
        .turn_view_reconciler
        .note_turn_started_with_attempt(
            &shared,
            target,
            turn_view_reconciler::turn_view_owner_for_message(
                channel,
                message,
                shared.restart.current_generation,
            ),
            TurnViewIdentity::Test("race-loss"),
            "seed_pending",
        )
        .await
        .attempt()
        .unwrap();
    shared.mailboxes.insert_reply_dropping_for_test(channel);
    let before = shared.turn_view_reconciler.ops().len();
    let result = handle_race_loss_enqueue(
        &http,
        &shared,
        "fixture",
        &ProviderKind::Claude,
        channel,
        channel,
        TurnKind::Foreground,
        UserId::new(1),
        message,
        "requeued",
        &None,
        false,
        false,
        &[],
        false,
        &None,
        Some(attempt),
        false,
        QueuedIntakeCause::RaceLoss,
        crate::services::turn_orchestrator::QueuedRoute {
            target: channel,
            parent: None,
            role: None,
        },
    )
    .await;
    assert!(result.is_err());
    let ops = shared.turn_view_reconciler.ops();
    assert!(
        ops[before..]
            .iter()
            .any(|op| op.target == target && !op.add),
        "refused intake clears its pending reaction"
    );
    shared.mailboxes.remove_fixture_for_test(channel);
}

#[tokio::test]
async fn reserved_id_receipt_is_refused_without_touching_owner_or_other_ids() {
    use crate::services::turn_orchestrator::{EnqueueRefusalReason, QueuePersistenceContext};
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = ChannelId::new(8_091_101);
    let handle = shared.mailbox(channel);
    let ctx = QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None);
    let mut head = user_intervention(8_091_102, "reserved");
    let absorbed = MessageId::new(8_091_104);
    head.source_message_ids.insert(0, absorbed);
    assert!(handle.enqueue(head.clone(), ctx.clone()).await.enqueued);
    let taken = handle.take_next_soft(ctx.clone()).await;
    assert!(taken.dispatch_lease.is_some());
    let refused = enqueue_race_loss_requeued_intervention(
        &shared,
        &ProviderKind::Claude,
        channel,
        head.message_id,
        head.clone(),
        QueuedIntakeCause::RaceLoss,
    )
    .await;
    assert!(
        !refused.enqueued,
        "a receipt cannot return another caller's reserved head"
    );
    assert_eq!(
        refused.refusal_reason,
        Some(EnqueueRefusalReason::SourceIdPendingOrActive)
    );
    let absorbed_receipt = handle
        .enqueue(user_intervention(absorbed.get(), "absorbed"), ctx.clone())
        .await;
    assert!(!absorbed_receipt.enqueued);
    assert_eq!(
        absorbed_receipt.refusal_reason,
        Some(EnqueueRefusalReason::SourceIdPendingOrActive)
    );
    let snapshot = handle.snapshot().await;
    assert!(snapshot.intervention_queue.is_empty());
    assert_eq!(snapshot.pending_user_dispatch, Some(head.message_id));
    assert!(
        crate::services::turn_orchestrator::load_channel_pending_dispatch_marker(
            &ProviderKind::Claude,
            &shared.token_hash,
            channel,
        )
        .is_some()
    );
    assert!(
        handle
            .enqueue(user_intervention(8_091_103, "other"), ctx)
            .await
            .enqueued
    );
    shared.mailboxes.remove_fixture_for_test(channel);
}

#[tokio::test]
async fn released_reservation_and_completed_turn_accept_fresh_receipts() {
    use crate::services::turn_orchestrator::QueuePersistenceContext;
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = ChannelId::new(8_091_201);
    let handle = shared.mailbox(channel);
    let ctx = QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None);
    let head = user_intervention(8_091_202, "released");
    assert!(handle.enqueue(head.clone(), ctx.clone()).await.enqueued);
    let taken = handle.take_next_soft(ctx.clone()).await;
    assert!(taken.dispatch_lease.is_some());
    assert!(
        handle
            .abandon_pending_dispatch(head.message_id, ctx.clone())
            .await
    );
    assert!(handle.enqueue(head.clone(), ctx.clone()).await.enqueued);
    let taken = handle.take_next_soft(ctx.clone()).await;
    assert!(taken.intervention.is_some());
    assert!(
        handle
            .try_start_turn(
                Arc::new(crate::services::provider::CancelToken::new()),
                head.author_id,
                head.message_id
            )
            .await
    );
    handle.finish_turn(ctx.clone()).await;
    assert!(handle.enqueue(head, ctx).await.enqueued);
    shared.mailboxes.remove_fixture_for_test(channel);
}

#[tokio::test]
async fn owner_return_preserves_marker_rejects_wrong_lease_and_runs_once() {
    use crate::services::turn_orchestrator::{EnqueueRefusalReason, QueuePersistenceContext};
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = ChannelId::new(8_091_301);
    let other = ChannelId::new(8_091_311);
    let handle = shared.mailbox(channel);
    let ctx = QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None);
    let head = user_intervention(8_091_302, "owner return");
    assert!(handle.enqueue(head.clone(), ctx.clone()).await.enqueued);
    let taken = handle.take_next_soft(ctx.clone()).await;
    assert!(
        shared
            .mailbox(other)
            .enqueue(user_intervention(8_091_312, "other owner"), ctx.clone())
            .await
            .enqueued
    );
    let foreign = shared.mailbox(other).take_next_soft(ctx.clone()).await;
    let denied = handle
        .return_dequeued_head(head.clone(), ctx.clone(), foreign.dispatch_lease.unwrap())
        .await;
    assert_eq!(
        denied.refusal_reason,
        Some(EnqueueRefusalReason::SourceIdPendingOrActive)
    );
    let returned = handle
        .return_dequeued_head(head.clone(), ctx.clone(), taken.dispatch_lease.unwrap())
        .await;
    assert!(returned.enqueued && returned.persistence_error.is_none());
    assert!(
        crate::services::turn_orchestrator::load_channel_pending_dispatch_marker(
            &ProviderKind::Claude,
            &shared.token_hash,
            channel,
        )
        .is_some(),
        "return itself must not consume the marker"
    );
    let taken = handle.take_next_soft(ctx.clone()).await;
    let (returned, receipt) = tokio::join!(
        handle.return_dequeued_head(head.clone(), ctx.clone(), taken.dispatch_lease.unwrap()),
        handle.enqueue(head.clone(), ctx.clone()),
    );
    assert!(returned.enqueued && returned.persistence_error.is_none());
    assert!(!receipt.enqueued);
    assert!(matches!(
        receipt.refusal_reason,
        Some(
            EnqueueRefusalReason::SourceIdPendingOrActive
                | EnqueueRefusalReason::SourceIdAlreadyQueued
        )
    ));
    assert!(handle.snapshot().await.pending_user_dispatch.is_none());
    let replay = handle.enqueue(head.clone(), ctx.clone()).await;
    assert!(!replay.enqueued);
    assert_eq!(handle.snapshot().await.intervention_queue.len(), 1);
    let next = handle.take_next_soft(ctx.clone()).await;
    assert_eq!(next.intervention.unwrap().message_id, head.message_id);
    assert!(
        handle
            .try_start_turn(
                Arc::new(crate::services::provider::CancelToken::new()),
                head.author_id,
                head.message_id
            )
            .await
    );
    handle.finish_turn(ctx.clone()).await;
    assert!(handle.take_next_soft(ctx).await.intervention.is_none());
    shared.mailboxes.remove_fixture_for_test(channel);
    shared.mailboxes.remove_fixture_for_test(other);
}
