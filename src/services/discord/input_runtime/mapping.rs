//! Reads the writer's parent → dispatch-thread map without retaining a reference across await.

use dashmap::DashMap;
use poise::serenity_prelude::ChannelId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Check {
    Empty,
    MappedThread,
    /// The writer map or its registration/provider coverage could not be verified.
    Unavailable,
}

/// After covered writers drain, only deletion can change this channel's incident edges.
pub(crate) fn inspect(parents: &DashMap<ChannelId, ChannelId>, channel: u64) -> Check {
    let channel = ChannelId::new(channel);
    if parents
        .iter()
        .any(|edge| *edge.key() == channel || *edge.value() == channel)
    {
        Check::MappedThread
    } else {
        Check::Empty
    }
}
