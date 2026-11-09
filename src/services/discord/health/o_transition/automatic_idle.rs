//! Cleanup needs a successful mailbox observation; absent or unreachable actors are unknown.

use std::time::Duration;

use poise::serenity_prelude::ChannelId;

use crate::services::discord::SharedData;
use crate::services::turn_orchestrator::ChannelMailboxSnapshot;

pub(super) async fn idle(shared: &SharedData, channel: u64) -> bool {
    let Some(mailbox) = shared.mailbox_peek(ChannelId::new(channel)) else {
        return false;
    };
    let Ok(Ok(snapshot)) =
        tokio::time::timeout(Duration::from_secs(2), mailbox.try_snapshot()).await
    else {
        return false;
    };
    snapshot_idle(&snapshot)
}

/// A V2 front an attempt carries holds cleanup as a reserved dispatch does.
fn snapshot_idle(snapshot: &ChannelMailboxSnapshot) -> bool {
    snapshot.cancel_token.is_none()
        && !snapshot.holds_input()
        && snapshot.pending_user_dispatch.is_none()
        && snapshot.recovery_started_at.is_none()
}

#[cfg(test)]
#[path = "automatic_idle_tests.rs"]
mod tests;
