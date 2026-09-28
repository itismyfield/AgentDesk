//! A queued row whose source already has a confirmed terminal delivery in the
//! completed-turn ledger must not reach the provider again when the queue drains.

use super::*;
use crate::services::discord::outbound::completed_turn_ledger;
use crate::services::turn_orchestrator::{
    QueueExitKind, SourceMessageQueuedGeneration, TakeNextSoftResult,
    load_channel_pending_dispatch_marker, load_channel_pending_queue_for_tests,
};

const HANDOFF_BODY: &str = "[family-counsel → project-agentdesk 핸드오프] 제안 수용";

struct ScopedRuntimeRoot {
    _lock: std::sync::MutexGuard<'static, ()>,
    temp: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
}

impl Drop for ScopedRuntimeRoot {
    fn drop(&mut self) {
        unsafe {
            match self.prev.take() {
                Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
            }
        }
    }
}

fn scoped_runtime_root() -> ScopedRuntimeRoot {
    let lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
    let temp = tempfile::tempdir().expect("temp runtime root");
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
    ScopedRuntimeRoot {
        _lock: lock,
        temp,
        prev,
    }
}

fn queued(id: u64, text: &str) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: true,
        message_id: MessageId::new(id),
        queued_generation: crate::services::discord::runtime_store::process_generation(),
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: vec![SourceMessageQueuedGeneration::new(
            MessageId::new(id),
            crate::services::discord::runtime_store::process_generation(),
        )],
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

async fn enqueue(shared: &Arc<SharedData>, channel_id: ChannelId, item: Intervention) {
    let outcome = with_post_enqueue_idle_queue_kick_suppressed(mailbox_enqueue_intervention(
        shared,
        &ProviderKind::Claude,
        channel_id,
        item,
    ))
    .await;
    assert!(outcome.enqueued, "fixture enqueue refused: {outcome:?}");
}

async fn actor_take(shared: &Arc<SharedData>, channel_id: ChannelId) -> TakeNextSoftResult {
    shared
        .mailbox(channel_id)
        .take_next_soft(queue_persistence_context(
            shared,
            &ProviderKind::Claude,
            channel_id,
        ))
        .await
}

/// The turn's terminal delivery commits after its copy was queued, as in production.
fn deliver_after_enqueue(channel_id: ChannelId, message_id: u64) {
    std::thread::sleep(std::time::Duration::from_millis(3));
    completed_turn_ledger::append_completed_turn(
        &ProviderKind::Claude,
        channel_id.get(),
        message_id,
    );
}

fn exits(result: &TakeNextSoftResult) -> Vec<(u64, QueueExitKind)> {
    result
        .queue_exit_events
        .iter()
        .map(|event| (event.intervention.message_id.get(), event.kind))
        .collect()
}

fn disk_queue_ids(shared: &SharedData, channel_id: ChannelId) -> Vec<u64> {
    load_channel_pending_queue_for_tests(&ProviderKind::Claude, &shared.token_hash, channel_id)
        .0
        .iter()
        .map(|item| item.message_id.get())
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn released_mailbox_does_not_reinject_a_ledger_settled_row_but_delivers_a_same_body_new_id() {
    let _root = scoped_runtime_root();
    let shared = make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(6_288_100);
    let handoff = MessageId::new(6_288_102);

    let occupant = MessageId::new(6_288_101);
    let token = Arc::new(CancelToken::new());
    assert!(mailbox_try_start_turn(&shared, channel_id, token, UserId::new(1), occupant).await);
    enqueue(&shared, channel_id, queued(handoff.get(), HANDOFF_BODY)).await;
    deliver_after_enqueue(channel_id, handoff.get());

    let while_occupied = idle_queue_take_next_soft_if_ready(&shared, &provider, channel_id).await;
    assert!(while_occupied.intervention.is_none());
    assert_eq!(disk_queue_ids(&shared, channel_id), vec![handoff.get()]);

    mailbox_finish_turn(&shared, &provider, channel_id).await;
    let drained = idle_queue_take_next_soft_if_ready(&shared, &provider, channel_id).await;
    assert!(
        drained.intervention.is_none(),
        "a row whose turn already delivered must not be re-injected on release"
    );
    assert!(drained.persistence_error.is_none());
    assert!(
        mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .is_empty()
    );
    assert!(disk_queue_ids(&shared, channel_id).is_empty());

    let fresh = MessageId::new(6_288_103);
    enqueue(&shared, channel_id, queued(fresh.get(), HANDOFF_BODY)).await;
    let delivered = idle_queue_take_next_soft_if_ready(&shared, &provider, channel_id)
        .await
        .intervention
        .expect("a new message with the same body is new input and must dispatch");
    assert_eq!(delivered.message_id, fresh);
    assert_eq!(delivered.text, HANDOFF_BODY);
}

#[tokio::test(flavor = "current_thread")]
async fn restart_restored_dispatch_marker_of_a_settled_turn_leaves_as_superseded() {
    let _root = scoped_runtime_root();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(6_288_200);
    let handoff = MessageId::new(6_288_201);

    let before_restart = make_shared_data_for_tests();
    enqueue(
        &before_restart,
        channel_id,
        queued(handoff.get(), HANDOFF_BODY),
    )
    .await;
    let taken = actor_take(&before_restart, channel_id).await;
    assert_eq!(
        taken.intervention.map(|item| item.message_id),
        Some(handoff)
    );
    deliver_after_enqueue(channel_id, handoff.get());
    let token_hash = before_restart.token_hash.clone();
    assert!(load_channel_pending_dispatch_marker(&provider, &token_hash, channel_id).is_some());
    drop(before_restart);

    let after_restart = make_shared_data_for_tests();
    assert_eq!(after_restart.token_hash, token_hash);
    let result = actor_take(&after_restart, channel_id).await;
    assert!(result.intervention.is_none());
    assert_eq!(
        exits(&result),
        vec![(handoff.get(), QueueExitKind::Superseded)]
    );
    assert!(load_channel_pending_dispatch_marker(&provider, &token_hash, channel_id).is_none());
    assert!(disk_queue_ids(&after_restart, channel_id).is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn merged_row_drops_only_the_settled_source_and_dispatches_the_rest() {
    let _root = scoped_runtime_root();
    let shared = make_shared_data_for_tests();
    let channel_id = ChannelId::new(6_288_300);
    let settled = MessageId::new(6_288_301);
    let pending = MessageId::new(6_288_302);
    let mut first = queued(settled.get(), "already answered");
    first.merge_consecutive = true;
    let mut second = queued(pending.get(), "still waiting");
    second.merge_consecutive = true;
    enqueue(&shared, channel_id, first).await;
    enqueue(&shared, channel_id, second).await;
    let merged = mailbox_snapshot(&shared, channel_id)
        .await
        .intervention_queue;
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].source_message_ids, vec![settled, pending]);
    deliver_after_enqueue(channel_id, settled.get());

    let result = actor_take(&shared, channel_id).await;
    assert_eq!(
        exits(&result),
        vec![(settled.get(), QueueExitKind::Superseded)]
    );
    let dispatched = result
        .intervention
        .expect("the unsettled source must still dispatch");
    assert_eq!(dispatched.source_message_ids, vec![pending]);
    assert_eq!(dispatched.text, "still waiting");
}

#[tokio::test(flavor = "current_thread")]
async fn absent_or_unreadable_ledger_settles_nothing() {
    let root = scoped_runtime_root();
    let shared = make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let absent_channel = ChannelId::new(6_288_400);
    let torn_channel = ChannelId::new(6_288_410);
    let absent_id = MessageId::new(6_288_401);
    let torn_id = MessageId::new(6_288_411);
    let torn_path = root
        .temp
        .path()
        .join("runtime/discord_completed_turn_ledger/claude")
        .join(format!("{}.json", torn_channel.get()));
    std::fs::create_dir_all(torn_path.parent().unwrap()).unwrap();
    std::fs::write(
        &torn_path,
        format!("{{\"entries\":[{{\"user_msg_id\":{}", torn_id),
    )
    .unwrap();
    assert!(completed_turn_ledger::settled_user_msg_ids(&provider, torn_channel.get()).is_empty());

    for (channel_id, id) in [(absent_channel, absent_id), (torn_channel, torn_id)] {
        enqueue(&shared, channel_id, queued(id.get(), HANDOFF_BODY)).await;
        let result = actor_take(&shared, channel_id).await;
        assert!(exits(&result).is_empty());
        assert_eq!(result.intervention.map(|item| item.message_id), Some(id));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn out_of_band_delivery_recorded_under_another_id_does_not_settle_the_queued_row() {
    let _root = scoped_runtime_root();
    let shared = make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(6_288_500);
    let queued_handoff = MessageId::new(6_288_501);
    let tui_direct_anchor = MessageId::new(6_288_502);
    enqueue(
        &shared,
        channel_id,
        queued(queued_handoff.get(), HANDOFF_BODY),
    )
    .await;
    completed_turn_ledger::append_completed_turn(
        &provider,
        channel_id.get(),
        tui_direct_anchor.get(),
    );

    let result = actor_take(&shared, channel_id).await;
    assert!(exits(&result).is_empty());
    assert_eq!(
        result.intervention.map(|item| item.message_id),
        Some(queued_handoff),
        "settlement is by source id only; an identical body delivered under another id is not evidence"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn settlement_rolls_back_with_the_dequeue_when_queue_persistence_fails() {
    let root = scoped_runtime_root();
    let shared = make_shared_data_for_tests();
    let channel_id = ChannelId::new(6_288_600);
    let settled = MessageId::new(6_288_601);
    enqueue(&shared, channel_id, queued(settled.get(), HANDOFF_BODY)).await;
    deliver_after_enqueue(channel_id, settled.get());
    let queue_path = root
        .temp
        .path()
        .join("runtime/discord_pending_queue/claude")
        .join(&shared.token_hash)
        .join(format!("{}.json", channel_id.get()));
    std::fs::remove_file(&queue_path).unwrap();
    std::fs::create_dir(&queue_path).unwrap();

    let result = actor_take(&shared, channel_id).await;
    assert!(result.persistence_error.is_some());
    assert!(exits(&result).is_empty());
    let live = mailbox_snapshot(&shared, channel_id)
        .await
        .intervention_queue;
    assert_eq!(
        live.iter().map(|item| item.message_id).collect::<Vec<_>>(),
        vec![settled],
        "an unpersisted settlement must not leave memory and disk disagreeing"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_requeue_after_an_earlier_completed_episode_of_the_same_id_still_dispatches() {
    let _root = scoped_runtime_root();
    let shared = make_shared_data_for_tests();
    let channel_id = ChannelId::new(6_288_700);
    let reused = MessageId::new(6_288_701);
    completed_turn_ledger::append_completed_turn(
        &ProviderKind::Claude,
        channel_id.get(),
        reused.get(),
    );
    std::thread::sleep(std::time::Duration::from_millis(3));
    enqueue(&shared, channel_id, queued(reused.get(), HANDOFF_BODY)).await;

    let result = actor_take(&shared, channel_id).await;
    assert!(exits(&result).is_empty());
    assert_eq!(
        result.intervention.map(|item| item.message_id),
        Some(reused),
        "a delivery committed before this copy was queued settles an earlier episode, not this one"
    );
}

/// A delivered episode commits after the rows it answers were queued.
fn deliver_episode(channel_id: ChannelId, primary: u64, turn_nonce: Option<&str>) {
    std::thread::sleep(std::time::Duration::from_millis(5));
    completed_turn_ledger::append_completed_episode(
        &ProviderKind::Claude,
        channel_id.get(),
        primary,
        turn_nonce,
    );
}

async fn enqueue_merged_pair(shared: &Arc<SharedData>, channel_id: ChannelId, h: u64, p: u64) {
    for (id, text) in [(h, "absorbed request"), (p, "primary request")] {
        let mut item = queued(id, text);
        item.merge_consecutive = true;
        enqueue(shared, channel_id, item).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn an_absorbed_copy_requeued_before_the_claim_is_settled_by_the_merged_delivery() {
    let _root = scoped_runtime_root();
    let shared = make_shared_data_for_tests();
    let channel_id = ChannelId::new(6_288_800);
    let (h, p) = (6_288_801, 6_288_802);
    let occupant = MessageId::new(6_288_803);
    assert!(
        mailbox_try_start_turn(
            &shared,
            channel_id,
            Arc::new(CancelToken::new()),
            UserId::new(7),
            occupant
        )
        .await
    );
    enqueue_merged_pair(&shared, channel_id, h, p).await;
    mailbox_finish_turn(&shared, &ProviderKind::Claude, channel_id).await;
    let taken = actor_take(&shared, channel_id).await;
    assert_eq!(
        taken
            .intervention
            .as_ref()
            .map(|item| item.source_message_ids.clone()),
        Some(vec![MessageId::new(h), MessageId::new(p)])
    );
    // A catch-up copy of H lands in the dequeue -> claim window.
    enqueue(&shared, channel_id, queued(h, "absorbed request")).await;
    let token = Arc::new(CancelToken::new());
    let nonce = token.turn_nonce().expect("turn nonce").to_owned();
    assert!(
        mailbox_try_start_turn(
            &shared,
            channel_id,
            token,
            UserId::new(7),
            MessageId::new(p)
        )
        .await
    );
    assert_eq!(
        mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .len(),
        1,
        "the claim purges only P"
    );
    deliver_episode(channel_id, p, Some(&nonce));
    mailbox_finish_turn(&shared, &ProviderKind::Claude, channel_id).await;
    drop(taken);

    let result = actor_take(&shared, channel_id).await;
    assert!(result.persistence_error.is_none());
    assert_eq!(
        result
            .intervention
            .as_ref()
            .map(|item| item.message_id.get()),
        None,
        "H was answered by P's merged episode"
    );
    assert_eq!(exits(&result), vec![(h, QueueExitKind::Superseded)]);
}

#[tokio::test(flavor = "current_thread")]
async fn a_delivered_absorbed_copy_is_settled_before_a_new_input_merges() {
    absorbed_copy_merge(false, "memory").await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_late_published_completion_settles_each_sources_original_enqueue() {
    absorbed_copy_merge(true, "memory").await;
}

#[tokio::test(flavor = "current_thread")]
async fn late_completion_retains_source_times_through_queue_and_dispatch_restore() {
    for restore in ["queue", "dispatch"] {
        absorbed_copy_merge(true, restore).await;
    }
}

async fn absorbed_copy_merge(delayed: bool, restore: &str) {
    let _root = scoped_runtime_root();
    let shared = make_shared_data_for_tests();
    let channel_id = ChannelId::new(6_289_400);
    let (h, p) = (6_289_401, 6_289_402);
    let occupant = MessageId::new(6_289_403);
    assert!(
        mailbox_try_start_turn(
            &shared,
            channel_id,
            Arc::new(CancelToken::new()),
            UserId::new(7),
            occupant
        )
        .await
    );
    enqueue_merged_pair(&shared, channel_id, h, p).await;
    mailbox_finish_turn(&shared, &ProviderKind::Claude, channel_id).await;
    let taken = actor_take(&shared, channel_id).await;
    assert_eq!(
        taken
            .intervention
            .as_ref()
            .map(|item| item.source_message_ids.clone()),
        Some(vec![MessageId::new(h), MessageId::new(p)])
    );
    // A catch-up copy of H lands in the dequeue -> claim window.
    let mut copy = queued(h, "absorbed request");
    copy.merge_consecutive = true;
    copy.created_at -= std::time::Duration::from_secs(2);
    if let Some(us) = &mut copy.source_message_queued_generations[0].enqueued_at_epoch_us {
        *us -= 2_000_000;
    }
    enqueue(&shared, channel_id, copy).await;
    let token = Arc::new(CancelToken::new());
    let nonce = token.turn_nonce().expect("turn nonce").to_owned();
    assert!(
        mailbox_try_start_turn(
            &shared,
            channel_id,
            token,
            UserId::new(7),
            MessageId::new(p)
        )
        .await
    );
    assert_eq!(
        mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .len(),
        1,
        "the claim purges only P"
    );
    let committed_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        - 1000;
    let k = MessageId::new(6_289_404);
    completed_turn_ledger::append_before_publish_for_tests(
        channel_id.get(),
        k.get(),
        "earlier-k",
        committed_ms,
        || {},
    )
    .unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        completed_turn_ledger::append_before_publish_for_tests(
            channel_id.get(),
            p,
            &nonce,
            committed_ms,
            move || {
                ready_tx.send(()).unwrap();
                resume_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
            },
        )
        .unwrap();
    });
    ready_rx.await.unwrap();
    assert!(
        !completed_turn_ledger::settled_user_msg_ids(&ProviderKind::Claude, channel_id.get())
            .contains(&h)
    );
    let mut writer = Some(writer);
    if !delayed {
        resume_tx.send(()).unwrap();
        writer.take().unwrap().join().unwrap();
    }
    let mut fresh = queued(k.get(), "new request");
    fresh.merge_consecutive = true;
    let enqueued = shared
        .mailbox(channel_id)
        .enqueue(
            fresh,
            queue_persistence_context(&shared, &ProviderKind::Claude, channel_id),
        )
        .await;
    assert!(enqueued.enqueued, "{:?}", enqueued.refusal_reason);
    assert!(enqueued.persistence_error.is_none());
    drop(taken);
    if restore == "dispatch" {
        mailbox_finish_turn(&shared, &ProviderKind::Claude, channel_id).await;
        let pending = actor_take(&shared, channel_id).await;
        assert_eq!(
            pending.intervention.as_ref().unwrap().source_message_ids,
            vec![MessageId::new(h), k]
        );
        assert!(pending.queue_exit_events.is_empty());
    }
    if delayed {
        assert!(enqueued.merged);
        assert!(enqueued.queue_exit_events.is_empty());
        resume_tx.send(()).unwrap();
        writer.take().unwrap().join().unwrap();
    }
    if restore != "dispatch" {
        mailbox_finish_turn(&shared, &ProviderKind::Claude, channel_id).await;
    }
    let shared = if restore == "memory" {
        shared
    } else {
        drop(shared);
        make_shared_data_for_tests()
    };
    let result = actor_take(&shared, channel_id).await;
    assert!(result.persistence_error.is_none());
    let dispatched = result.intervention.expect("the new request must dispatch");
    assert_eq!(
        dispatched.source_message_ids,
        vec![k],
        "dispatch must contain K alone; H was already answered by P/n"
    );
    assert_eq!(dispatched.text, "new request");
    assert_eq!(dispatched.message_id, k);
    let all_exits: Vec<_> = enqueued
        .queue_exit_events
        .iter()
        .chain(&result.queue_exit_events)
        .collect();
    assert_eq!(all_exits.len(), 1);
    let settled = all_exits[0];
    assert_eq!(settled.kind, QueueExitKind::Superseded);
    assert_eq!(
        settled.intervention.source_message_ids,
        vec![MessageId::new(h)]
    );
    assert_eq!(settled.intervention.text, "absorbed request");
    assert!(disk_queue_ids(&shared, channel_id).is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn a_restored_merged_marker_of_a_delivered_episode_settles_every_source() {
    let _root = scoped_runtime_root();
    let channel_id = ChannelId::new(6_288_900);
    let (h, p) = (6_288_901, 6_288_902);
    let before_restart = make_shared_data_for_tests();
    enqueue_merged_pair(&before_restart, channel_id, h, p).await;
    let taken = actor_take(&before_restart, channel_id).await;
    assert_eq!(
        taken.intervention.map(|item| item.message_id.get()),
        Some(p)
    );
    completed_turn_ledger::record_merged_alias(
        &ProviderKind::Claude,
        channel_id.get(),
        p,
        "restored-episode",
        &[h],
    );
    deliver_episode(channel_id, p, Some("restored-episode"));
    let token_hash = before_restart.token_hash.clone();
    assert!(
        load_channel_pending_dispatch_marker(&ProviderKind::Claude, &token_hash, channel_id)
            .is_some()
    );
    drop(before_restart);

    let after_restart = make_shared_data_for_tests();
    let result = actor_take(&after_restart, channel_id).await;
    assert!(result.persistence_error.is_none());
    assert_eq!(exits(&result), vec![(p, QueueExitKind::Superseded)]);
    assert_eq!(
        result.intervention.map(|item| item.message_id.get()),
        None,
        "the restored H was answered by the same episode"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_newest_alias_at_the_rowless_cap_settles_its_source_after_delivery() {
    let _root = scoped_runtime_root();
    let shared = make_shared_data_for_tests();
    let channel_id = ChannelId::new(6_289_000);
    let (h, p) = (6_289_100, 6_289_200);
    enqueue(&shared, channel_id, queued(h + 64, "newest alias source")).await;
    for offset in 0..65 {
        completed_turn_ledger::record_merged_alias(
            &ProviderKind::Claude,
            channel_id.get(),
            p + offset,
            "n",
            &[h + offset],
        );
    }
    deliver_episode(channel_id, p + 64, Some("n"));

    let result = actor_take(&shared, channel_id).await;
    assert_eq!(
        result
            .intervention
            .as_ref()
            .map(|item| item.message_id.get()),
        None
    );
    assert_eq!(exits(&result), vec![(h + 64, QueueExitKind::Superseded)]);
}

#[tokio::test(flavor = "current_thread")]
async fn an_absorbed_source_takes_its_backing_episode_time_not_a_later_episode_of_the_head() {
    let _root = scoped_runtime_root();
    let shared = make_shared_data_for_tests();
    let channel_id = ChannelId::new(6_289_300);
    let (h, p) = (6_289_301, 6_289_302);
    completed_turn_ledger::record_merged_alias(
        &ProviderKind::Claude,
        channel_id.get(),
        p,
        "old",
        &[h],
    );
    deliver_episode(channel_id, p, Some("old"));
    std::thread::sleep(std::time::Duration::from_millis(5));
    enqueue(
        &shared,
        channel_id,
        queued(h, "new copy after the old episode"),
    )
    .await;
    deliver_episode(channel_id, p, Some("unrelated-new"));

    let result = actor_take(&shared, channel_id).await;
    assert!(exits(&result).is_empty());
    assert_eq!(
        result.intervention.map(|item| item.message_id.get()),
        Some(h),
        "only the old backing episode dates H, and it predates this copy"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn pre_merge_settlement_preserves_a_requeued_episode_and_unsettled_sources() {
    let _root = scoped_runtime_root();
    let shared = make_shared_data_for_tests();
    let channel_id = ChannelId::new(6_289_500);
    let (h, reused, k) = (6_289_501, 6_289_502, 6_289_503);
    deliver_after_enqueue(channel_id, reused);
    std::thread::sleep(std::time::Duration::from_millis(5));
    for (id, text) in [(h, "answered source"), (reused, "new episode")] {
        let mut item = queued(id, text);
        item.merge_consecutive = true;
        enqueue(&shared, channel_id, item).await;
    }
    deliver_after_enqueue(channel_id, h);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let mut fresh = queued(k, "new request");
    fresh.merge_consecutive = true;
    let result = shared
        .mailbox(channel_id)
        .enqueue(
            fresh,
            queue_persistence_context(&shared, &ProviderKind::Claude, channel_id),
        )
        .await;
    assert!(
        result.enqueued && result.merged,
        "{:?}",
        result.refusal_reason
    );
    assert!(result.persistence_error.is_none());
    assert_eq!(result.queue_exit_events.len(), 1);
    let exit = &result.queue_exit_events[0];
    assert_eq!(exit.kind, QueueExitKind::Superseded);
    assert_eq!(
        exit.intervention.source_message_ids,
        vec![MessageId::new(h)]
    );
    assert_eq!(exit.intervention.text, "answered source");
    let persisted =
        load_channel_pending_queue_for_tests(&ProviderKind::Claude, &shared.token_hash, channel_id)
            .0;
    assert_eq!(persisted.len(), 1);
    assert_eq!(
        persisted[0].source_message_ids,
        vec![MessageId::new(reused), MessageId::new(k)]
    );
    assert_eq!(persisted[0].text, "new episode\nnew request");

    drop(shared);
    let restored = make_shared_data_for_tests();
    let drained = actor_take(&restored, channel_id).await;
    assert!(drained.queue_exit_events.is_empty());
    let dispatched = drained
        .intervention
        .expect("both new inputs must survive restart");
    assert_eq!(
        dispatched.source_message_ids,
        persisted[0].source_message_ids
    );
    assert_eq!(dispatched.text, persisted[0].text);
}

#[tokio::test(flavor = "current_thread")]
async fn pre_merge_settlement_rolls_back_when_queue_persistence_fails() {
    let root = scoped_runtime_root();
    let shared = make_shared_data_for_tests();
    let channel_id = ChannelId::new(6_289_600);
    let (h, k) = (6_289_601, 6_289_602);
    let mut old = queued(h, "answered source");
    old.merge_consecutive = true;
    enqueue(&shared, channel_id, old).await;
    deliver_after_enqueue(channel_id, h);
    let queue_path = root
        .temp
        .path()
        .join("runtime/discord_pending_queue/claude")
        .join(&shared.token_hash)
        .join(format!("{}.json", channel_id.get()));
    std::fs::remove_file(&queue_path).unwrap();
    std::fs::create_dir(&queue_path).unwrap();
    let mut fresh = queued(k, "new request");
    fresh.merge_consecutive = true;
    let persistence = queue_persistence_context(&shared, &ProviderKind::Claude, channel_id);
    let result = shared
        .mailbox(channel_id)
        .enqueue(fresh.clone(), persistence.clone())
        .await;
    assert!(!result.enqueued);
    assert!(result.persistence_error.is_some());
    assert!(result.queue_exit_events.is_empty());
    let live = mailbox_snapshot(&shared, channel_id)
        .await
        .intervention_queue;
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].source_message_ids, vec![MessageId::new(h)]);
    assert_eq!(live[0].text, "answered source");

    std::fs::remove_dir(&queue_path).unwrap();
    let retry = shared.mailbox(channel_id).enqueue(fresh, persistence).await;
    assert!(retry.enqueued);
    assert!(retry.persistence_error.is_none());
    assert_eq!(retry.queue_exit_events.len(), 1);
    assert_eq!(retry.queue_exit_events[0].kind, QueueExitKind::Superseded);
    assert_eq!(
        retry.queue_exit_events[0].intervention.message_id,
        MessageId::new(h)
    );
    let drained = actor_take(&shared, channel_id).await;
    assert_eq!(
        drained.intervention.unwrap().source_message_ids,
        vec![MessageId::new(k)]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn restart_first_enqueue_settles_unfinished_disk_copy() {
    for marker in [false, true] {
        for observed in [false, true] {
            let _root = scoped_runtime_root();
            let channel = ChannelId::new(6_289_700);
            let (h, k) = (6_289_701, 6_289_702);
            let shared = make_shared_data_for_tests();
            let mut copy = queued(h, "answered copy");
            copy.created_at -= std::time::Duration::from_secs(2);
            if let Some(us) = &mut copy.source_message_queued_generations[0].enqueued_at_epoch_us {
                *us -= 2_000_000;
            }
            copy.merge_consecutive = true;
            enqueue(&shared, channel, copy).await;
            if marker {
                assert!(actor_take(&shared, channel).await.intervention.is_some());
            }
            completed_turn_ledger::append_completed_turn(&ProviderKind::Claude, channel.get(), h);
            drop(shared);
            let shared = make_shared_data_for_tests();
            let observation = observed.then(|| {
                crate::services::turn_orchestrator::ChannelMailboxSnapshot::no_actor(channel)
                    .claim_observation
            });
            let mut fresh = queued(k, "new request");
            fresh.merge_consecutive = true;
            let result = shared
                .mailbox(channel)
                .enqueue_observed(
                    fresh,
                    queue_persistence_context(&shared, &ProviderKind::Claude, channel),
                    observation,
                )
                .await;
            assert!(result.enqueued && result.persistence_error.is_none());
            assert_eq!(result.queue_exit_events.len(), 1);
            assert_eq!(result.queue_exit_events[0].kind, QueueExitKind::Superseded);
            assert_eq!(
                result.queue_exit_events[0].intervention.source_message_ids,
                vec![MessageId::new(h)]
            );
            let memory = mailbox_snapshot(&shared, channel).await.intervention_queue;
            assert_eq!(memory.len(), 1);
            assert_eq!(memory[0].source_message_ids, vec![MessageId::new(k)]);
            assert_eq!(disk_queue_ids(&shared, channel), vec![k]);
            let next = actor_take(&shared, channel).await;
            assert!(next.queue_exit_events.is_empty());
            assert_eq!(
                next.intervention.unwrap().source_message_ids,
                vec![MessageId::new(k)]
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_source_times_fall_back_to_the_row_episode_boundary() {
    for marker in [false, true] {
        for format in ["source", "legacy", "ownerless"] {
            for offset_us in [-2_000_000_i64, -1000, -1, 0, 1, 1000, 1_000_000] {
                if format != "source" && offset_us % 1000 != 0 {
                    continue;
                }
                let root = scoped_runtime_root();
                let shared = make_shared_data_for_tests();
                let provider = &ProviderKind::Claude;
                let hash = &shared.token_hash;
                let (owners, epoch) = ("source_message_queued_generations", "enqueued_at_epoch_us");
                let channel = ChannelId::new(6_289_800);
                let h = 6_289_801;
                let committed_ms = 1_700_000_000_000_u64;
                let enqueued_us = (committed_ms * 1000).checked_add_signed(offset_us).unwrap();
                enqueue(&shared, channel, queued(h, "legacy row")).await;
                if marker {
                    assert!(actor_take(&shared, channel).await.intervention.is_some());
                }
                let path = root
                    .temp
                    .path()
                    .join("runtime/discord_pending_queue/claude")
                    .join(&shared.token_hash)
                    .join(format!(
                        "{}.{}",
                        channel.get(),
                        if marker { "dispatch" } else { "json" }
                    ));
                let mut payload: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                let row = if marker {
                    &mut payload
                } else {
                    &mut payload[0]
                };
                row["created_at_wall_time_ms"] = serde_json::json!(if format == "source" {
                    committed_ms - 5000
                } else {
                    enqueued_us / 1000
                });
                if format == "ownerless" {
                    row.as_object_mut().unwrap().remove(owners);
                } else {
                    let owner = &mut row[owners][0];
                    if format == "source" {
                        owner[epoch] = serde_json::json!(enqueued_us);
                    } else {
                        owner.as_object_mut().unwrap().remove(epoch);
                    }
                }
                std::fs::write(&path, serde_json::to_vec(&payload).unwrap()).unwrap();
                completed_turn_ledger::append_before_publish_for_tests(
                    channel.get(),
                    h,
                    "old",
                    committed_ms,
                    || {},
                )
                .unwrap();
                let loaded = if marker {
                    load_channel_pending_dispatch_marker(provider, hash, channel)
                        .unwrap()
                        .0
                } else {
                    load_channel_pending_queue_for_tests(provider, hash, channel)
                        .0
                        .remove(0)
                };
                assert_eq!(
                    loaded.source_message_queued_generations[0].enqueued_at_epoch_us,
                    Some(enqueued_us),
                    "{format} marker={marker} offset={offset_us}"
                );
                drop(shared);
                let result = actor_take(&make_shared_data_for_tests(), channel).await;
                assert_eq!(result.intervention.is_none(), offset_us < 0);
                assert_eq!(
                    exits(&result),
                    if offset_us < 0 {
                        vec![(h, QueueExitKind::Superseded)]
                    } else {
                        vec![]
                    }
                );
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn clock_sample_delay_cannot_backdate_a_new_episode() {
    for human in [false, true] {
        let _root = scoped_runtime_root();
        let shared = make_shared_data_for_tests();
        let channel = ChannelId::new(6_289_900);
        let h = 6_289_901;
        let committed_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            - 1;
        completed_turn_ledger::append_before_publish_for_tests(
            channel.get(),
            h,
            "old",
            committed_ms,
            || {},
        )
        .unwrap();
        let mut item = queued(h, "new episode after completion");
        item.source_message_queued_generations = vec![if human {
            SourceMessageQueuedGeneration::user_instruction(
                MessageId::new(h),
                item.queued_generation,
            )
        } else {
            SourceMessageQueuedGeneration::new(MessageId::new(h), item.queued_generation)
        }];
        let created_us = item.source_message_queued_generations[0].enqueued_at_epoch_us;
        let called = std::rc::Rc::new(std::cell::Cell::new(false));
        let hook_called = called.clone();
        Intervention::source_clock_sample_hook_for_tests(move || {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            std::thread::sleep(std::time::Duration::from_millis(now_ms - committed_ms + 5));
            hook_called.set(true);
        });
        enqueue(&shared, channel, item).await;
        assert!(
            called.get(),
            "the actual Enqueue must cross the injected clock delay"
        );
        let result = actor_take(&shared, channel).await;
        assert!(
            result.queue_exit_events.is_empty(),
            "a previous completion must not supersede the new episode: {:?}",
            exits(&result)
        );
        let dispatched = result.intervention.unwrap();
        assert_eq!(dispatched.message_id.get(), h);
        assert!(created_us.is_some_and(|us| us > committed_ms * 1000));
        assert_eq!(
            dispatched.source_message_queued_generations[0].enqueued_at_epoch_us,
            created_us
        );
    }
}
