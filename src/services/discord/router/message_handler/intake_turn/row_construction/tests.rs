use super::*;
use crate::services::discord::{self as discord, inflight};
use std::sync::atomic::{AtomicUsize, Ordering};

const PROVIDER: ProviderKind = ProviderKind::Claude;

struct Root {
    _lock: std::sync::MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
}

impl Drop for Root {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }
}

fn scoped_root() -> Root {
    let lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let dir = tempfile::tempdir().expect("temp runtime root");
    let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", dir.path()) };
    Root {
        _lock: lock,
        dir,
        prev,
    }
}

fn queued(id: u64, sources: &[u64]) -> Intervention {
    let mut item = super::super::super::super::response_format::build_race_requeued_intervention(
        serenity::UserId::new(7),
        MessageId::new(id),
        &format!("text {id}"),
        false,
        None,
        false,
        false,
        Vec::new(),
        None,
    );
    item.source_message_ids = sources.iter().copied().map(MessageId::new).collect();
    item.source_message_queued_generations.clear();
    item
}

fn row(channel: ChannelId, head: u64, sources: &[u64], nonce: Option<&str>) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        PROVIDER,
        channel.get(),
        None,
        7,
        head,
        900,
        format!("text {head}"),
        None,
        None,
        None,
        None,
        0,
    );
    state.source_message_ids = sources.to_vec();
    state.turn_nonce = nonce.map(str::to_owned);
    state
}

/// Dequeues the head and claims it the way intake does, returning the actor.
async fn claim_head(
    shared: &Arc<SharedData>,
    channel: ChannelId,
) -> (Intervention, Arc<CancelToken>) {
    let persistence = discord::queue_persistence_context(shared, &PROVIDER, channel);
    let taken = shared.mailbox(channel).take_next_soft(persistence).await;
    let head = taken.intervention.expect("queued head");
    let actor = Arc::new(CancelToken::new());
    let owner = serenity::UserId::new(7);
    assert!(
        discord::mailbox_try_start_turn(shared, channel, actor.clone(), owner, head.message_id)
            .await
    );
    discord::increment_global_active(shared, "row_construction_test");
    (head, actor)
}

async fn seeded(shared: &Arc<SharedData>, channel: ChannelId, items: Vec<Intervention>) {
    for item in items {
        let enqueue = discord::mailbox_enqueue_intervention(shared, &PROVIDER, channel, item);
        discord::queue_io::with_post_enqueue_idle_queue_kick_suppressed(enqueue).await;
    }
}

fn seed_foreign_row(channel: ChannelId) {
    inflight::save_inflight_state_create_new(&row(channel, 0, &[], Some("foreign")))
        .expect("seed the foreign id-0 row");
}

async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

fn ids(item: &Intervention) -> Vec<u64> {
    item.source_message_ids.iter().map(|id| id.get()).collect()
}

/// The foreign row refuses the start: our lease, counter and actor are given back
/// and the merged message returns to the queue front ahead of B, with its sources.
#[tokio::test(flavor = "current_thread")]
async fn foreign_row_releases_own_lease_and_requeues_head_first() {
    let _root = scoped_root();
    let shared = discord::make_shared_data_for_tests();
    let channel = ChannelId::new(6_288_100);
    seeded(
        &shared,
        channel,
        vec![
            queued(6_288_101, &[6_288_101, 6_288_102]),
            queued(6_288_103, &[]),
        ],
    )
    .await;
    let active_before = shared.restart.global_active.load(Ordering::Relaxed);
    let (head, actor) = claim_head(&shared, channel).await;
    seed_foreign_row(channel);

    let state = row(
        channel,
        head.message_id.get(),
        &ids(&head),
        actor.turn_nonce(),
    );
    let refused =
        construct_or_refuse(&shared, &PROVIDER, &actor, serenity::UserId::new(7), state).await;
    let Err(RefusedStart::Requeued(outcome)) = refused else {
        panic!("foreign row must refuse and requeue")
    };
    assert!(
        outcome.enqueued,
        "requeue refused: {:?}",
        outcome.refusal_reason
    );

    let snapshot = discord::mailbox_snapshot(&shared, channel).await;
    assert!(
        snapshot.cancel_token.is_none(),
        "our lease must be released"
    );
    let queue: Vec<u64> = snapshot
        .intervention_queue
        .iter()
        .map(|item| item.message_id.get())
        .collect();
    assert_eq!(
        queue,
        vec![6_288_101, 6_288_103],
        "refused head must return in front of B"
    );
    assert_eq!(
        ids(&snapshot.intervention_queue[0]),
        vec![6_288_101, 6_288_102]
    );
    assert_eq!(
        shared.restart.global_active.load(Ordering::Relaxed),
        active_before
    );
    assert!(actor.cancelled.load(Ordering::Relaxed));

    // Once the foreign row is gone the same message starts and owns the row.
    assert!(inflight::delete_inflight_state_file(
        &PROVIDER,
        channel.get()
    ));
    let (again, actor) = claim_head(&shared, channel).await;
    assert_eq!(again.message_id.get(), 6_288_101);
    let state = row(channel, 6_288_101, &ids(&again), actor.turn_nonce());
    let constructed =
        construct_or_refuse(&shared, &PROVIDER, &actor, serenity::UserId::new(7), state).await;
    assert!(constructed.is_ok(), "an empty slot must construct the row");
    let persisted =
        inflight::load_inflight_state_read_only(&PROVIDER, channel.get()).expect("our row");
    assert_eq!(persisted.user_msg_id, 6_288_101);
}

/// When another writer already released our episode and a successor holds the
/// slot, the refusal leaves the successor, the queue and the counter untouched.
#[tokio::test(flavor = "current_thread")]
async fn lost_lease_leaves_successor_queue_and_counter_alone() {
    let _root = scoped_root();
    let shared = discord::make_shared_data_for_tests();
    let channel = ChannelId::new(6_288_200);
    seeded(
        &shared,
        channel,
        vec![
            queued(6_288_201, &[]),
            queued(6_288_202, &[]),
            queued(6_288_203, &[]),
        ],
    )
    .await;
    let (head, stale_actor) = claim_head(&shared, channel).await;
    let cleared = discord::mailbox_finish::mailbox_finish_turn(&shared, &PROVIDER, channel).await;
    assert!(
        cleared.removed_token.is_some(),
        "the /clear stand-in takes our lease"
    );
    discord::saturating_decrement_global_active(&shared);
    let (_, successor) = claim_head(&shared, channel).await;
    let active_before = shared.restart.global_active.load(Ordering::Relaxed);
    let queue_before = discord::mailbox_snapshot(&shared, channel)
        .await
        .intervention_queue
        .len();
    seed_foreign_row(channel);

    let state = row(
        channel,
        head.message_id.get(),
        &[],
        stale_actor.turn_nonce(),
    );
    let refused = construct_or_refuse(
        &shared,
        &PROVIDER,
        &stale_actor,
        serenity::UserId::new(7),
        state,
    )
    .await;
    assert!(matches!(refused, Err(RefusedStart::LeaseLost)));

    let snapshot = discord::mailbox_snapshot(&shared, channel).await;
    let holder = snapshot.cancel_token.expect("successor keeps the slot");
    assert!(Arc::ptr_eq(&holder, &successor));
    assert_eq!(
        snapshot.active_user_message_id,
        Some(MessageId::new(6_288_202))
    );
    assert_eq!(
        snapshot.intervention_queue.len(),
        queue_before,
        "a lost lease must not requeue"
    );
    assert_eq!(
        shared.restart.global_active.load(Ordering::Relaxed),
        active_before
    );
}

/// An internal store failure is not a foreign row: the start proceeds as before.
#[tokio::test(flavor = "current_thread")]
async fn internal_store_failure_still_constructs() {
    let root = scoped_root();
    let shared = discord::make_shared_data_for_tests();
    let channel = ChannelId::new(6_288_300);
    seeded(&shared, channel, vec![queued(6_288_301, &[])]).await;
    let (head, actor) = claim_head(&shared, channel).await;
    let blocker = root.dir.path().join("not-a-dir");
    std::fs::write(&blocker, b"file").expect("blocker file");
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", &blocker) };

    let state = row(channel, head.message_id.get(), &[], actor.turn_nonce());
    let result =
        construct_or_refuse(&shared, &PROVIDER, &actor, serenity::UserId::new(7), state).await;
    assert!(result.is_ok(), "Internal must keep the fail-open start");
    let snapshot = discord::mailbox_snapshot(&shared, channel).await;
    assert!(
        snapshot
            .cancel_token
            .is_some_and(|holder| Arc::ptr_eq(&holder, &actor))
    );
}

/// The requeued message waits for the slow backstop, not the 2s fast kick,
/// so a lingering foreign row is retried about once a minute instead of spinning.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn refused_start_is_retried_by_the_slow_backstop_only() {
    let _root = scoped_root();
    let shared = discord::make_shared_data_for_tests();
    let channel = ChannelId::new(6_288_400);
    let kicks = Arc::new(AtomicUsize::new(0));
    let counted = kicks.clone();
    let _hook =
        discord::queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(move |_, _, id, reason| {
            let counted = counted.clone();
            Box::pin(async move {
                if id == channel && reason == "intake_refused_start" {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
                None
            })
        }));
    seeded(&shared, channel, vec![queued(6_288_401, &[])]).await;
    let (head, actor) = claim_head(&shared, channel).await;
    seed_foreign_row(channel);
    let state = row(channel, head.message_id.get(), &[], actor.turn_nonce());
    let refused =
        construct_or_refuse(&shared, &PROVIDER, &actor, serenity::UserId::new(7), state).await;
    assert!(matches!(refused, Err(RefusedStart::Requeued(_))));

    // Let spawned retry tasks register their timers before time moves.
    settle().await;
    tokio::time::advance(std::time::Duration::from_secs(3)).await;
    settle().await;
    assert_eq!(kicks.load(Ordering::SeqCst), 0, "no fast kick within 3s");
    let snapshot = discord::mailbox_snapshot(&shared, channel).await;
    assert_eq!(snapshot.intervention_queue[0].message_id, head.message_id);

    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    settle().await;
    assert!(
        kicks.load(Ordering::SeqCst) >= 1,
        "the slow backstop retries the message"
    );
}
