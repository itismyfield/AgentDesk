//! After a server restart, the continuation a finished restart still owes is admitted again.
use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use crate::services::discord::SharedData;

/// Re-admits the channel's ready, unsubmitted continuation under its stored ticket: one that is
/// already queued or already submitted stays single.
pub(in crate::services::discord) async fn resume_owed(
    shared: &Arc<SharedData>,
    channel: ChannelId,
) {
    let Some(intent) = super::tmux::stored(shared, channel).await else {
        return;
    };
    if intent.pending().is_none() || !intent.ready {
        return;
    }
    let Some(episode) = intent.interrupted_episode else {
        return;
    };
    let owed = super::tmux::Owed {
        operation: intent.operation,
        native_session: intent.native_session,
        episode,
    };
    if let Err(error) = super::tmux::admit(shared, &shared.provider, channel, owed).await {
        tracing::warn!(channel_id = channel.get(), %error, "owed effort continuation not admitted");
    }
}
