//! A closing or recovery operation's resume notice, given once after its own cleanup and terminal
//! barrier through the existing kickoff (one owner wake for an owned queue); a refusal gives none.
use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;

/// Whether the channel's queue has an installed owner, which only this notice may resume.
pub(in crate::services::discord) async fn owned(shared: &SharedData, channel: ChannelId) -> bool {
    #[cfg(unix)]
    return super::ingress::installed(&shared.mailbox(channel)).await == Ok(true);
    #[cfg(not(unix))]
    {
        let _ = (shared, channel);
        false
    }
}

/// Tells the channel's queue that the operation named by `reason` released what held it.
pub(in crate::services::discord) fn closed(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    reason: &'static str,
) {
    crate::services::discord::queue_io::schedule_deferred_idle_queue_kickoff_immediate(
        shared.clone(),
        provider.clone(),
        channel,
        reason,
    );
}
