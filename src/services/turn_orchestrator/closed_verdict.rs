//! #5951 C3t-0g — for the arms below an empty answer reads as "nothing to do",
//! so they reply through a [`VerdictReply`]: callers tell a purge-closed
//! actor's refusal (and a dead actor) apart and stop before any follow-up.

use poise::serenity_prelude::{ChannelId, MessageId};
use tokio::sync::oneshot;

use super::{
    ChannelMailboxHandle, ChannelMailboxMsg, ClearChannelResult, HydratePendingQueueResult,
    Intervention, MailboxUnreachable, QueuePersistenceContext, TakeNextSoftResult,
};

/// Why the actor did not carry out a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MailboxRefusal {
    /// A registry purge closed the actor; the registry serves the channel
    /// with a fresh one.
    Closed,
    /// The actor task is gone.
    Unreachable,
}

/// Reply of a verdict arm: `send` is its answer; only the closed gate refuses.
pub(super) struct VerdictReply<T>(oneshot::Sender<Option<T>>);

impl<T> VerdictReply<T> {
    pub(super) fn send(self, value: T) -> Result<(), Option<T>> {
        self.0.send(Some(value))
    }

    /// The closed gate's answer; returns `arm` for its log line.
    pub(super) fn refuse(self, arm: &'static str) -> &'static str {
        let _ = self.0.send(None);
        arm
    }
}

impl ChannelMailboxHandle {
    async fn request_verdict<T>(
        &self,
        build: impl FnOnce(VerdictReply<T>) -> ChannelMailboxMsg,
    ) -> Result<T, MailboxRefusal> {
        match self.request(|reply| build(VerdictReply(reply))).await {
            Ok(Some(value)) => Ok(value),
            Ok(None) => Err(MailboxRefusal::Closed),
            Err(MailboxUnreachable) => Err(MailboxRefusal::Unreachable),
        }
    }

    pub(crate) async fn clear_recovery_marker_or_refused(&self) -> Result<(), MailboxRefusal> {
        self.request_verdict(|reply| ChannelMailboxMsg::ClearRecoveryMarker { reply })
            .await
    }

    pub(crate) async fn take_soft_matching_or_refused(
        &self,
        persistence: QueuePersistenceContext,
        primary_message_id: Option<MessageId>,
    ) -> Result<TakeNextSoftResult, MailboxRefusal> {
        self.request_verdict(|reply| ChannelMailboxMsg::TakeNextSoft {
            persistence,
            primary_message_id,
            reply,
        })
        .await
    }

    pub(crate) async fn clear_or_refused(
        &self,
        persistence: QueuePersistenceContext,
    ) -> Result<ClearChannelResult, MailboxRefusal> {
        self.request_verdict(|reply| ChannelMailboxMsg::Clear { persistence, reply })
            .await
    }

    pub(crate) async fn hydrate_pending_queue_from_disk_or_refused(
        &self,
        persistence: QueuePersistenceContext,
    ) -> Result<HydratePendingQueueResult, MailboxRefusal> {
        self.request_verdict(|reply| ChannelMailboxMsg::HydratePendingQueueFromDisk {
            persistence,
            reply,
        })
        .await
    }

    pub(crate) async fn merge_restored_queue_items_or_refused(
        &self,
        items: Vec<Intervention>,
        persistence: QueuePersistenceContext,
    ) -> Result<HydratePendingQueueResult, MailboxRefusal> {
        self.request_verdict(|reply| ChannelMailboxMsg::MergeRestoredQueueItems {
            items,
            persistence,
            reply,
        })
        .await
    }

    pub(crate) async fn merge_restored_dispatch_marker_or_refused(
        &self,
        marker: Intervention,
        restored_override: Option<ChannelId>,
        persistence: QueuePersistenceContext,
    ) -> Result<HydratePendingQueueResult, MailboxRefusal> {
        self.request_verdict(|reply| ChannelMailboxMsg::MergeRestoredDispatchMarker {
            marker,
            restored_override,
            persistence,
            reply,
        })
        .await
    }
}
