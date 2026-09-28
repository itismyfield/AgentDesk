//! A queued row whose source already has a confirmed terminal delivery in the
//! completed-turn ledger must not reach the provider again when the queue drains.

use super::*;
use crate::services::discord::outbound::completed_turn_ledger;
use crate::services::turn_orchestrator::{
    QueueExitKind, TakeNextSoftResult, load_channel_pending_dispatch_marker,
    load_channel_pending_queue_for_tests,
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
