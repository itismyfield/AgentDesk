//! Wrappers whose request reached a purge-closed actor: automatic follow-up stops there, and
//! accepted work handed back to the channel lands on the successor or is reported unrestored.

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
    RecoveryDoneSignal, load_channel_pending_dispatch_marker, load_channel_pending_queue_for_tests,
    save_channel_pending_dispatch_marker, save_channel_queue,
};

const QUEUED: u64 = 21;
const OFFERED: u64 = 22;

fn latched(signal: &RecoveryDoneSignal) -> bool {
    signal.wait().now_or_never().is_some()
}

/// Queues `wrapper`'s request behind the purge's `CloseIfIdle` and lets the purge finish;
/// the returned wrapper resumes with the closed actor's answer.
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

/// T-E3r — work the old actor would have taken back (front requeue, restored items, disk queue)
/// lands on the successor, in memory and on disk, and never on the closed actor.
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

/// Restitution refused on every retry is reported unrestored, not as an empty merge, and
/// its queue stays on disk through the later marker restore and hydrate.
#[tokio::test]
async fn exhausted_restitution_is_not_an_empty_success() {
    let _root = isolated_agentdesk_root();
    let provider = ProviderKind::Claude;
    let shared = discord::make_shared_data_for_tests();
    let (channel, token_hash) = (ChannelId::new(5_951_641), &shared.token_hash);
    save_channel_queue(&provider, token_hash, channel, &[queued(OFFERED)], None).unwrap();
    save_channel_pending_dispatch_marker(&provider, token_hash, channel, &queued(QUEUED), None)
        .unwrap();
    let _old = shared.mailbox(channel);
    let purge = shared.mailboxes.remove_idle_entry(channel);
    tokio::pin!(purge);
    assert!(futures::poll!(purge.as_mut()).is_pending());
    let items = vec![queued(OFFERED)];
    let merge = discord::mailbox_merge_restored_queue_items(&shared, &provider, channel, items);
    let result = merge.await;
    assert_eq!(result.absorbed, 0);
    assert!(
        result.persistence_error.is_some(),
        "read as empty: {result:?}"
    );
    assert_eq!(purge.await, MailboxPurgeOutcome::Removed);
    let marker = queued(QUEUED);
    discord::mailbox_merge_restored_dispatch_marker(&shared, &provider, channel, marker, None)
        .await;
    for step in ["marker restore", "hydrate"] {
        let memory = shared.mailbox(channel).snapshot().await.intervention_queue;
        let disk = load_channel_pending_queue_for_tests(&provider, token_hash, channel).0;
        for (place, queue) in [("memory", memory), ("disk", disk)] {
            let ids: Vec<u64> = queue.iter().map(|item| item.message_id.get()).collect();
            assert_eq!(ids, [QUEUED, OFFERED], "{place} after {step}");
        }
        discord::mailbox_hydrate_pending_queue_from_disk(&shared, &provider, channel).await;
    }
}

/// A successor's soft take, restart drain and front requeue keep a queue left only on disk,
/// not rewrite it away.
#[tokio::test]
async fn whole_queue_writes_keep_a_disk_only_queue() {
    let _root = isolated_agentdesk_root();
    let provider = ProviderKind::Claude;
    let shared = discord::make_shared_data_for_tests();
    let token_hash = &shared.token_hash;
    for (index, arm) in ["take", "drain", "requeue"].into_iter().enumerate() {
        let channel = ChannelId::new(5_951_651 + index as u64);
        save_channel_queue(&provider, token_hash, channel, &[queued(OFFERED)], None).unwrap();
        let persistence = discord::queue_persistence_context(&shared, &provider, channel);
        let mailbox = shared.mailbox(channel);
        if arm == "take" {
            let taken = mailbox.take_next_soft(persistence).await.intervention;
            assert_eq!(taken.map(|item| item.message_id.get()), Some(OFFERED));
            continue;
        }
        let expected: &[u64] = if arm == "drain" {
            mailbox.restart_drain(persistence).await;
            &[OFFERED]
        } else {
            assert!(
                mailbox
                    .requeue_front(queued(QUEUED), persistence)
                    .await
                    .enqueued
            );
            &[QUEUED, OFFERED]
        };
        let disk = load_channel_pending_queue_for_tests(&provider, token_hash, channel).0;
        let ids: Vec<u64> = disk.iter().map(|item| item.message_id.get()).collect();
        assert_eq!(ids, expected, "{arm}");
    }
}

/// A queue file that exists but cannot be read (broken JSON, no read permission) stops the
/// marker restore, take, drain and requeue with an error, leaving file and memory as they were.
#[tokio::test]
#[cfg(unix)]
async fn unreadable_disk_queue_stops_whole_queue_writes() {
    let _root = isolated_agentdesk_root();
    let shared = discord::make_shared_data_for_tests();
    let arms = ["marker", "take", "drain", "requeue"];
    let rows = arms
        .into_iter()
        .flat_map(|arm| [(arm, "json"), (arm, "mode")]);
    let mut broken = Vec::new();
    for (index, (arm, fault)) in rows.enumerate() {
        let channel = ChannelId::new(5_951_661 + index as u64);
        if let Err(why) = unreadable_queue_row(&shared, channel, arm, fault).await {
            broken.push(format!("{arm}/{fault}: {why}"));
        }
    }
    assert!(broken.is_empty(), "{broken:#?}");
}

#[cfg(unix)]
async fn unreadable_queue_row(
    shared: &SharedData,
    channel: ChannelId,
    arm: &str,
    fault: &str,
) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let (provider, token_hash) = (ProviderKind::Claude, &shared.token_hash);
    save_channel_queue(&provider, token_hash, channel, &[queued(OFFERED)], None).unwrap();
    let dir = discord::runtime_store::discord_pending_queue_root().unwrap();
    let path = dir
        .join(provider.as_str())
        .join(token_hash)
        .join(format!("{}.json", channel.get()));
    let mode = |bits| std::fs::set_permissions(&path, std::fs::Permissions::from_mode(bits));
    let before = if fault == "json" {
        std::fs::write(&path, "[{broken").unwrap();
        "[{broken".to_owned()
    } else {
        let content = std::fs::read_to_string(&path).unwrap();
        mode(0o000).unwrap();
        content
    };
    let persistence = discord::queue_persistence_context(shared, &provider, channel);
    let mailbox = shared.mailbox(channel);
    let (error, handed_out) = match arm {
        "marker" => {
            let marker = queued(QUEUED);
            save_channel_pending_dispatch_marker(&provider, token_hash, channel, &marker, None)
                .unwrap();
            let restored = mailbox
                .merge_restored_dispatch_marker(marker, None, persistence)
                .await;
            let kept = load_channel_pending_dispatch_marker(&provider, token_hash, channel);
            (restored.persistence_error, kept.is_none())
        }
        "take" => {
            let taken = mailbox.take_next_soft(persistence).await;
            let handed_out = taken.intervention.is_some() || taken.dispatch_lease.is_some();
            (taken.persistence_error, handed_out)
        }
        "drain" => (
            mailbox.restart_drain(persistence).await.persistence_error,
            false,
        ),
        _ => {
            let requeued = mailbox.requeue_front(queued(QUEUED), persistence).await;
            (requeued.persistence_error, requeued.enqueued)
        }
    };
    let memory = mailbox.snapshot().await.intervention_queue.len();
    let _ = mode(0o644);
    let after = std::fs::read_to_string(&path).map_err(|error| error.kind().to_string());
    match (error, handed_out, memory, after) {
        (Some(_), false, 0, Ok(after)) if after == before => Ok(()),
        (error, handed_out, memory, after) => Err(format!(
            "error {error:?}, dispatched/enqueued/marker spent {handed_out}, memory {memory}, \
             file {:?}",
            after.map(|after| after == before)
        )),
    }
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
