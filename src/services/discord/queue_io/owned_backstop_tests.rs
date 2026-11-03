//! An input owner's queue is woken by its own events: no backstop timer schedules or fires a kick
//! for it, while an unowned channel's backstop still kicks.
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::services::discord::queue_io::input_owner::admission::{AdmissionTicket, admit};

/// Counts every kick a backstop fires at `channel`; no kick runs past the count.
fn kicks(channel: ChannelId) -> (Arc<AtomicUsize>, impl Drop) {
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    let hook = super::super::set_idle_queue_kick_hook_for_tests(Arc::new(move |_, _, at, _| {
        let seen = seen.clone();
        Box::pin(async move {
            if at == channel {
                seen.fetch_add(1, Ordering::SeqCst);
            }
            Some(IdleQueueKickoffChannelOutcome::default())
        })
    }));
    (count, hook)
}

/// Whether the owner holds a grant of its front, which only a drive wake gives.
async fn granted(mailbox: &crate::services::turn_orchestrator::ChannelMailboxHandle) -> bool {
    let read = mailbox.v2_request(|slot| Ok(slot.as_ref().unwrap().front_outstanding()));
    read.await.unwrap()
}

/// An early deferred kick fires nothing at an owned queue, a later schedule is one owner wake, and
/// for 3 to 60 seconds no backstop kicks it, while an unowned channel's schedule still kicks.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn no_backstop_schedules_or_fires_a_kick_at_an_owned_queue() {
    let _root = crate::services::discord::relay_recovery::tests::isolated_agentdesk_root();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let (owned, plain) = (ChannelId::new(7_676_201), ChannelId::new(7_676_202));
    let provider = ProviderKind::Claude;
    let (owned_kicks, hook) = kicks(owned);
    schedule_deferred_idle_queue_kickoff(shared.clone(), provider.clone(), owned, "before_owner");
    tokio::task::yield_now().await;
    let mailbox = shared.mailbox(owned);
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let file = dir.join("claude/owned-backstop/7676201.json");
    mailbox
        .v2_adopt(file, "owned-backstop".into())
        .await
        .unwrap();
    let original: crate::services::turn_orchestrator::PendingQueueItem =
        serde_json::from_value(serde_json::json!({"author_id":7,"message_id":76_762_011,
            "channel_id":owned.get(),"source_message_ids":[76_762_011],"queued_generation":1,
            "text":"owned front"}))
        .unwrap();
    let admitted = admit(&mailbox, &original, AdmissionTicket::new()).await;
    assert!(matches!(
        admitted,
        super::super::input_owner::admission::Admission::Accepted(_)
    ));
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(
        owned_kicks.load(Ordering::SeqCst),
        0,
        "the earlier schedule fired nothing"
    );
    assert!(!granted(&mailbox).await, "nor woke the owner");
    schedule_deferred_idle_queue_kickoff_immediate(
        shared.clone(),
        provider.clone(),
        owned,
        "owned",
    );
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert!(
        granted(&mailbox).await,
        "a schedule is one direct owner wake"
    );
    assert!(!arm_slow_idle_queue_backstop_if_queue_nonempty(&shared, &provider, owned, "o").await);
    let outcome = IdleQueueKickoffChannelOutcome::default();
    let armed = arm_event_backstop_after_no_start_if_queue_nonempty(
        &shared, &provider, owned, outcome, "o",
    );
    assert!(!armed.await);
    for at in [5, 30, 60] {
        tokio::time::sleep(std::time::Duration::from_secs(at)).await;
        assert_eq!(owned_kicks.load(Ordering::SeqCst), 0, "{at}s");
    }
    drop(hook);
    let (plain_kicks, _hook) = kicks(plain);
    schedule_deferred_idle_queue_kickoff(shared.clone(), provider, plain, "unowned");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    assert!(plain_kicks.load(Ordering::SeqCst) >= 1);
}

/// A schedule coalesced into a slot whose task started before the owner took the channel is a new
/// event: once that task finds the owner, it wakes it, and still kicks nothing.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_schedule_coalesced_after_the_owner_took_the_channel_wakes_it() {
    let _root = crate::services::discord::relay_recovery::tests::isolated_agentdesk_root();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let owned = ChannelId::new(7_676_203);
    let provider = ProviderKind::Claude;
    let (owned_kicks, _hook) = kicks(owned);
    schedule_deferred_idle_queue_kickoff(shared.clone(), provider.clone(), owned, "before_owner");
    tokio::task::yield_now().await;
    let mailbox = shared.mailbox(owned);
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let file = dir.join("claude/owned-backstop/7676203.json");
    mailbox
        .v2_adopt(file, "owned-backstop".into())
        .await
        .unwrap();
    let original: crate::services::turn_orchestrator::PendingQueueItem =
        serde_json::from_value(serde_json::json!({"author_id":7,"message_id":76_762_031,
            "channel_id":owned.get(),"source_message_ids":[76_762_031],"queued_generation":1,
            "text":"owned front"}))
        .unwrap();
    let _ = admit(&mailbox, &original, AdmissionTicket::new()).await;
    schedule_deferred_idle_queue_kickoff(shared.clone(), provider, owned, "after_owner");
    assert!(!granted(&mailbox).await, "coalesced into the waiting task");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert!(
        granted(&mailbox).await,
        "the coalesced schedule woke the owner"
    );
    assert_eq!(owned_kicks.load(Ordering::SeqCst), 0);
}

/// While the slow cycle's kick runs, V1 drains, the owner takes the channel and a new event's
/// schedule coalesces into the cycle's slot: once that cycle ends, the schedule is the owner's wake.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_schedule_coalesced_during_the_slow_cycles_kick_wakes_the_new_owner() {
    let _root = crate::services::discord::relay_recovery::tests::isolated_agentdesk_root();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = ChannelId::new(7_676_204);
    let provider = ProviderKind::Claude;
    let persistence = crate::services::discord::queue_dispatch::persistence_context;
    let queued =
        crate::services::turn_orchestrator::ChannelMailboxRegistry::queued_for_test(76_762_040);
    let context = persistence(&shared, &provider, channel);
    shared
        .mailbox(channel)
        .replace_queue(vec![queued], context)
        .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let hook = Arc::new(move |shared: Arc<SharedData>, provider, at, _| {
        let seen = seen.clone();
        Box::pin(async move {
            if at != channel {
                return None;
            }
            seen.fetch_add(1, Ordering::SeqCst);
            let mailbox = shared.mailbox(at);
            mailbox
                .replace_queue(Vec::new(), persistence(&shared, &provider, at))
                .await;
            let dir = crate::services::discord::runtime_store::discord_pending_queue_root();
            let file = dir.unwrap().join("claude/owned-backstop/7676204.json");
            mailbox
                .v2_ensure(file, "owned-backstop".into())
                .await
                .unwrap();
            let original: crate::services::turn_orchestrator::PendingQueueItem =
                serde_json::from_value(serde_json::json!({"author_id":7,"message_id":76_762_041,
                    "channel_id":at.get(),"source_message_ids":[76_762_041],
                    "queued_generation":1,"text":"owned front"}))
                .unwrap();
            let _ = admit(&mailbox, &original, AdmissionTicket::new()).await;
            schedule_deferred_idle_queue_kickoff(shared.clone(), provider, at, "after_owner");
            Some(IdleQueueKickoffChannelOutcome::default())
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = _> + Send>>
    });
    let _hook = super::super::set_idle_queue_kick_hook_for_tests(hook);
    let outcome = IdleQueueKickoffChannelOutcome::default();
    let armed = arm_event_backstop_after_no_start_if_queue_nonempty(
        &shared, &provider, channel, outcome, "slow",
    );
    assert!(armed.await, "the unowned backlog arms the slow cycle");
    let cycle = DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY + std::time::Duration::from_secs(1);
    tokio::time::sleep(cycle).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the slow cycle kicked once"
    );
    let mailbox = shared.mailbox(channel);
    assert!(
        granted(&mailbox).await,
        "the coalesced schedule woke the owner"
    );
}

/// A schedule coalesced after the slow cycle's owner check answered "not owned", as the owner
/// took the channel, wakes that owner exactly once.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_schedule_coalesced_while_the_owner_check_answers_wakes_the_owner_once() {
    let _root = crate::services::discord::relay_recovery::tests::isolated_agentdesk_root();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = ChannelId::new(7_676_205);
    let provider = ProviderKind::Claude;
    let persistence = crate::services::discord::queue_dispatch::persistence_context;
    let queued =
        crate::services::turn_orchestrator::ChannelMailboxRegistry::queued_for_test(76_762_050);
    let context = persistence(&shared, &provider, channel);
    shared
        .mailbox(channel)
        .replace_queue(vec![queued], context)
        .await;
    let fired = Arc::new(AtomicUsize::new(0));
    let (seen, at) = (fired.clone(), shared.clone());
    let hook: OwnerAnsweredHook = Arc::new(move |owned| {
        let (seen, shared, provider) = (seen.clone(), at.clone(), ProviderKind::Claude);
        Box::pin(async move {
            if owned || seen.fetch_add(1, Ordering::SeqCst) > 0 {
                return;
            }
            let mailbox = shared.mailbox(channel);
            let context = persistence(&shared, &provider, channel);
            mailbox.replace_queue(Vec::new(), context).await;
            let dir = crate::services::discord::runtime_store::discord_pending_queue_root();
            let file = dir.unwrap().join("claude/owned-backstop/7676205.json");
            mailbox
                .v2_ensure(file, "owned-backstop".into())
                .await
                .unwrap();
            let original: crate::services::turn_orchestrator::PendingQueueItem =
                serde_json::from_value(serde_json::json!({"author_id":7,"message_id":76_762_051,
                    "channel_id":channel.get(),"source_message_ids":[76_762_051],
                    "queued_generation":1,"text":"owned front"}))
                .unwrap();
            let _ = admit(&mailbox, &original, AdmissionTicket::new()).await;
            schedule_deferred_idle_queue_kickoff(shared.clone(), provider, channel, "after_owner");
        })
    });
    OWNER_ANSWERED_HOOKS.insert(channel, hook);
    let outcome = IdleQueueKickoffChannelOutcome::default();
    let armed = arm_event_backstop_after_no_start_if_queue_nonempty(
        &shared, &provider, channel, outcome, "slow",
    );
    assert!(armed.await, "the unowned backlog arms the slow cycle");
    let cycle = DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY + std::time::Duration::from_secs(1);
    tokio::time::sleep(cycle).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    OWNER_ANSWERED_HOOKS.remove(&channel);
    assert_eq!(
        fired.load(Ordering::SeqCst),
        1,
        "the owner check answered once unowned"
    );
    assert!(
        granted(&shared.mailbox(channel)).await,
        "the coalesced schedule woke the owner"
    );
    let wakes = OWNER_WAKES_FOR_TESTS.get(&channel).map(|n| *n);
    assert_eq!(wakes, Some(1), "one schedule is one owner wake");
    assert!(
        !shared.restart.deferred_hook_channels.contains_key(&channel),
        "the slot was released with nothing left"
    );
}
