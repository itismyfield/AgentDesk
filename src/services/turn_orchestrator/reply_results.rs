//! #5242 — mailbox reply result types, moved verbatim out of a registered giant.

use poise::serenity_prelude::{ChannelId, MessageId};

use super::QueueExitEvent;

pub(crate) struct HasPendingSoftQueueResult {
    pub(crate) has_pending: bool,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
    // Uniform queue-mutation persistence-result surface; no consumer yet.
    // See `FinishTurnResult`.
    #[allow(dead_code)]
    pub(crate) persistence_error: Option<String>,
}

#[derive(Default)]
pub(crate) struct TryStartTurnResult {
    pub(crate) started: bool,
    /// The recovery fence refused the claim's episode as already ended.
    pub(crate) refused_released_episode: bool,
    /// #6035 PR-S: the active turn's absorbed ids; the claim's own only when `started`.
    pub(crate) absorbed_source_ids: Vec<MessageId>,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
    pub(crate) persistence_error: Option<String>,
}

pub(crate) struct RestartDrainResult {
    pub(crate) queued_count: usize,
    pub(crate) persistence_error: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct QueuePersistenceFailure {
    pub(crate) channel_id: ChannelId,
    pub(crate) error: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RestartDrainAllResult {
    pub(crate) queued_count: usize,
    pub(crate) persistence_errors: Vec<QueuePersistenceFailure>,
}
