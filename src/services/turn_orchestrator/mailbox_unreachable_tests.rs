use std::sync::Arc;

use tokio::sync::mpsc;

use super::actor_hydrate_regression_tests::make_intervention;
use super::{
    ChannelMailboxHandle, ChannelMailboxRegistry, EnqueueRefusalReason, GLOBAL_CHANNEL_MAILBOXES,
    MailboxUnreachable, QueuePersistenceContext, RecoveryDoneSignal, save_channel_queue,
    spawn_channel_mailbox,
};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;

pub(crate) fn closed_handle() -> ChannelMailboxHandle {
    let (sender, receiver) = mpsc::unbounded_channel();
    drop(receiver);
    ChannelMailboxHandle {
        sender,
        recovery_done: Arc::new(RecoveryDoneSignal::new()),
    }
}

impl ChannelMailboxRegistry {
    pub(crate) fn insert_unreachable_for_test(&self, channel_id: ChannelId) {
        let handle = closed_handle();
        self.handles.insert(channel_id, handle.clone());
        GLOBAL_CHANNEL_MAILBOXES.insert(channel_id, handle);
    }

    pub(crate) fn insert_reply_dropping_for_test(&self, channel_id: ChannelId) {
        let handle = reply_dropping_handle();
        self.handles.insert(channel_id, handle.clone());
        GLOBAL_CHANNEL_MAILBOXES.insert(channel_id, handle);
    }
}

fn reply_dropping_handle() -> ChannelMailboxHandle {
    let (sender, mut receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(msg) = receiver.recv().await {
            drop(msg);
        }
    });
    ChannelMailboxHandle {
        sender,
        recovery_done: Arc::new(RecoveryDoneSignal::new()),
    }
}

async fn assert_turn_queries_unreachable(handle: &ChannelMailboxHandle) {
    assert_eq!(handle.has_active_turn().await, Err(MailboxUnreachable));
    assert_eq!(
        handle.has_blocking_active_turn().await,
        Err(MailboxUnreachable)
    );
    assert!(matches!(
        handle.cancel_token().await,
        Err(MailboxUnreachable)
    ));
}

#[tokio::test]
async fn measured_idle_is_distinct_from_unreachable_actor() {
    let live = spawn_channel_mailbox(
        ChannelId::new(6046),
        Default::default(),
        Arc::new(RecoveryDoneSignal::new()),
    );
    assert_eq!(live.has_active_turn().await, Ok(false));
    assert_eq!(live.has_blocking_active_turn().await, Ok(false));
    assert!(matches!(live.cancel_token().await, Ok(None)));

    assert_turn_queries_unreachable(&closed_handle()).await;
    assert_turn_queries_unreachable(&reply_dropping_handle()).await;
}

#[tokio::test]
async fn requeue_front_to_unreachable_actor_names_the_refusal() {
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, "unreachable", None);
    let intervention = make_intervention(6_046_002, "head", std::time::Instant::now());

    let result = closed_handle()
        .requeue_front(intervention, persistence)
        .await;

    assert!(!result.enqueued);
    assert_eq!(
        result.refusal_reason,
        Some(EnqueueRefusalReason::ActorUnreachable)
    );
}

#[tokio::test]
async fn fallible_queue_requests_report_an_unreachable_actor_instead_of_defaults() {
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, "unreachable", None);
    for handle in [closed_handle(), reply_dropping_handle()] {
        assert!(handle.try_snapshot().await.is_err());
        assert_eq!(
            handle.try_purge_queue(persistence.clone(), true).await,
            Err(MailboxUnreachable)
        );
        assert!(matches!(
            handle
                .try_hydrate_pending_queue_from_disk(persistence.clone())
                .await,
            Err(MailboxUnreachable)
        ));
    }
}

fn live_mailbox(channel_id: u64) -> ChannelMailboxHandle {
    spawn_channel_mailbox(
        ChannelId::new(channel_id),
        Default::default(),
        Arc::new(RecoveryDoneSignal::new()),
    )
}

fn flat_queue_path(channel_id: u64) -> std::path::PathBuf {
    crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join(ProviderKind::Claude.as_str())
        .join(format!("{channel_id}.json"))
}

/// A token-less purge's empty save unlinks the flat queue file the all-token sweep skips.
#[tokio::test]
async fn purge_counts_the_flat_queue_file_its_empty_save_unlinks() {
    let temp = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let (channel_id, provider) = (6_038_201, ProviderKind::Claude);
    let flat = QueuePersistenceContext::new(&provider, "", None);
    let item = make_intervention(6_038_202, "left on disk", std::time::Instant::now());
    save_channel_queue(&provider, "", ChannelId::new(channel_id), &[item], None).unwrap();
    let live = live_mailbox(channel_id);

    let purged = live.try_purge_queue(flat.clone(), true).await.unwrap();
    assert_eq!((purged.drained, purged.disk_files_removed), (0, 0));
    assert_eq!(purged.own_files_removed, Some(1), "{purged:?}");
    assert_eq!(purged.queue_len_after, 0);
    assert!(
        !flat_queue_path(channel_id).exists(),
        "premise: the empty save unlinked it"
    );

    let again = live.try_purge_queue(flat, true).await.unwrap();
    assert_eq!(
        again.own_files_removed,
        Some(0),
        "nothing left to unlink: {again:?}"
    );
}

/// A purge whose empty save failed rolls its queue back, and its reply must say so.
#[tokio::test]
async fn purge_reports_the_queue_it_rolled_back_after_a_failed_empty_save() {
    let temp = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let (channel_id, provider) = (6_038_203, ProviderKind::Claude);
    let flat = QueuePersistenceContext::new(&provider, "", None);
    let live = live_mailbox(channel_id);
    let item = make_intervention(6_038_204, "survives the purge", std::time::Instant::now());
    live.replace_queue(
        vec![item],
        QueuePersistenceContext::new(&provider, "tokB", None),
    )
    .await;
    std::fs::create_dir_all(flat_queue_path(channel_id)).unwrap();

    let purged = live.try_purge_queue(flat.clone(), true).await.unwrap();
    assert_eq!(
        (purged.drained, purged.disk_files_removed),
        (0, 1),
        "{purged:?}"
    );
    assert_eq!(
        purged.own_files_removed,
        Some(0),
        "a failed save unlinked nothing"
    );
    assert_eq!(
        purged.queue_len_after, 1,
        "the rolled-back item is still queued"
    );
    assert_eq!(live.snapshot().await.intervention_queue.len(), 1);

    std::fs::remove_dir(flat_queue_path(channel_id)).unwrap();
    let emptied = live.try_purge_queue(flat, true).await.unwrap();
    assert_eq!(
        (emptied.drained, emptied.queue_len_after),
        (1, 0),
        "{emptied:?}"
    );
}
