//! #5951 C3t-0g — wrappers whose request reached a purge-closed actor. The
//! purge's `CloseIfIdle` is queued ahead of the wrapper's request, so the old
//! actor answers it closed. Automatic follow-up must stop there; accepted
//! work handed back to the channel must land on the successor.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::FutureExt;
use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use crate::services::discord::relay_recovery::tests::incarnation_follow_up::FollowUp;
use crate::services::discord::relay_recovery::tests::isolated_agentdesk_root;
use crate::services::discord::relay_recovery::tests::orphan_token_finish::queued;
use crate::services::discord::{self as discord, SharedData};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::registry_purge::MailboxPurgeOutcome;
use crate::services::turn_orchestrator::{
    RecoveryDoneSignal, load_channel_pending_queue_for_tests, save_channel_queue,
};

const QUEUED: u64 = 21;
const OFFERED: u64 = 22;

fn latched(signal: &RecoveryDoneSignal) -> bool {
    signal.wait().now_or_never().is_some()
}

/// Queues `wrapper`'s request on the channel's actor behind the purge's
/// `CloseIfIdle` and lets the purge finish. The returned wrapper resumes with
/// the closed actor's answer.
async fn queued_behind_purge<'a, T>(
    shared: &SharedData,
    channel: ChannelId,
    wrapper: impl Future<Output = T> + 'a,
) -> Pin<Box<dyn Future<Output = T> + 'a>> {
    let purge = shared.mailboxes.remove_idle_entry(channel);
    tokio::pin!(purge);
    assert!(futures::poll!(purge.as_mut()).is_pending());
    let mut wrapper: Pin<Box<dyn Future<Output = T> + 'a>> = Box::pin(wrapper);
    assert!(futures::poll!(wrapper.as_mut()).is_pending());
    assert_eq!(purge.await, MailboxPurgeOutcome::Removed);
    wrapper
}

/// T-E3w — a wrapper whose request the closed actor refused runs no follow-up,
/// not even the `recovery_done` mark of the actor that refused it.
#[tokio::test]
async fn refused_wrapper_request_runs_no_follow_up() {
    let _root = isolated_agentdesk_root();
    let provider = ProviderKind::Claude;
    let shared = discord::make_shared_data_for_tests();
    let mut followed_up = Vec::new();
    for (index, case) in FollowUp::ALL.into_iter().enumerate() {
        let channel = ChannelId::new(5_951_601 + index as u64);
        let old = shared.mailbox(channel);
        let message = MessageId::new(QUEUED);
        let request = case.run(&shared, &provider, channel, message, None);
        let wrapper = queued_behind_purge(&shared, channel, request).await;
        let successor = shared.mailbox(channel);
        let kickoff = successor
            .recovery_kickoff(Arc::new(CancelToken::new()), UserId::new(1), None)
            .await;
        assert!(kickoff.activated_turn(), "{case:?}: {kickoff:?}");
        wrapper.await;
        let recovering = successor.snapshot().await.recovery_started_at.is_some();
        if latched(old.recovery_done()) || latched(successor.recovery_done()) || !recovering {
            followed_up.push(case);
        }
        discord::mailbox_finish_turn(&shared, &provider, channel).await;
    }
    assert!(
        followed_up.is_empty(),
        "a refused request still ran wrapper follow-up: {followed_up:?}"
    );
}

#[derive(Clone, Copy, Debug)]
enum Restitution {
    RequeueFront,
    MergeQueueItems,
    Hydrate,
}

impl Restitution {
    async fn run(self, shared: &SharedData, provider: &ProviderKind, channel: ChannelId) {
        let offered = queued(OFFERED);
        match self {
            Self::RequeueFront => drop(
                discord::mailbox_requeue_intervention_front(shared, provider, channel, offered)
                    .await,
            ),
            Self::MergeQueueItems => {
                let items = vec![offered];
                let merge =
                    discord::mailbox_merge_restored_queue_items(shared, provider, channel, items);
                drop(merge.await)
            }
            Self::Hydrate => drop(
                discord::mailbox_hydrate_pending_queue_from_disk(shared, provider, channel).await,
            ),
        }
    }
}

/// T-E3r — work the old actor would have taken back (a front requeue,
/// restored queue items, the disk queue) lands on the
/// successor, in memory and on disk, and never on the closed actor.
#[tokio::test]
async fn refused_restitution_lands_on_the_successor() {
    let _root = isolated_agentdesk_root();
    let provider = ProviderKind::Claude;
    let shared = discord::make_shared_data_for_tests();
    let ids = |queue: &[crate::services::turn_orchestrator::Intervention]| -> Vec<u64> {
        queue.iter().map(|item| item.message_id.get()).collect()
    };
    let mut lost = Vec::new();
    for (index, row) in [
        Restitution::RequeueFront,
        Restitution::MergeQueueItems,
        Restitution::Hydrate,
    ]
    .into_iter()
    .enumerate()
    {
        let channel = ChannelId::new(5_951_611 + index as u64);
        let old = shared.mailbox(channel);
        let request = row.run(&shared, &provider, channel);
        let wrapper = queued_behind_purge(&shared, channel, request).await;
        let persistence = discord::queue_persistence_context(&shared, &provider, channel);
        let successor = shared.mailbox(channel);
        assert!(
            successor
                .enqueue(queued(QUEUED), persistence)
                .await
                .enqueued
        );
        if matches!(row, Restitution::Hydrate) {
            let disk = [queued(QUEUED), queued(OFFERED)];
            save_channel_queue(&provider, &shared.token_hash, channel, &disk, None).unwrap();
        }
        wrapper.await;
        let memory = ids(&successor.snapshot().await.intervention_queue);
        let token_hash = &shared.token_hash;
        let disk = ids(&load_channel_pending_queue_for_tests(&provider, token_hash, channel).0);
        let closed_took_work = !old.snapshot().await.intervention_queue.is_empty();
        if !memory.contains(&OFFERED) || memory != disk || closed_took_work {
            lost.push(format!(
                "{row:?}: memory={memory:?} disk={disk:?} closed={closed_took_work}"
            ));
        }
        discord::mailbox_clear_channel(&shared, &provider, channel).await;
    }
    assert!(
        lost.is_empty(),
        "restitution missed the successor:\n{}",
        lost.join("\n")
    );
}

/// T-E3t — a soft-queue take the closed actor refused says nothing about the
/// queue, so it must not spend the channel's pending catch-up retry.
#[tokio::test]
async fn refused_take_keeps_the_pending_catch_up_retry() {
    let _root = isolated_agentdesk_root();
    let provider = ProviderKind::Claude;
    let shared = discord::make_shared_data_for_tests();
    let _ = shared
        .http
        .cached_bot_token
        .set("Bot test-token".to_string());
    let channel = ChannelId::new(5_951_631);
    discord::catch_up::retry_state::arm_catch_up_retry_for_tests(&shared, channel, 1);
    let _old = shared.mailbox(channel);
    let take =
        discord::queue_dispatch::mailbox_take_next_soft_intervention(&shared, &provider, channel);
    let wrapper = queued_behind_purge(&shared, channel, take).await;
    assert!(wrapper.await.intervention.is_none());
    assert!(
        shared.catch_up_retry_pending.contains_key(&channel),
        "the refused take consumed the channel's pending catch-up retry"
    );
}
