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

/// A deferred kick scheduled before the owner took the queue fires nothing at it; a schedule after
/// is one direct owner wake; the slow arms set nothing: across 2, 5, 30 and 60 seconds no backstop
/// kicks the owned queue, while an unowned channel's schedule still kicks.
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
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
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
