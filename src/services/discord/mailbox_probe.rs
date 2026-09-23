use poise::serenity_prelude as serenity;
use serenity::ChannelId;

use super::SharedData;

/// Unreachable reads as idle: a dead actor admits no turn either way, the one
/// reclaim caller (session idle cleanup) has independent liveness guards, and
/// "busy" would permanently hold a live parent channel or voice path.
pub(in crate::services::discord) async fn mailbox_has_active_turn(
    shared: &SharedData,
    channel_id: ChannelId,
) -> bool {
    shared
        .mailbox(channel_id)
        .has_active_turn()
        .await
        .unwrap_or(false)
}

/// #3167 — true only when a *real* (non-background) active turn holds the
/// slot. The external-input dequeue uses this instead of
/// `mailbox_has_active_turn` so a continuously-cycling background turn
/// (monitor relay / self-paced TUI loop) does not starve a queued user
/// intervention. Unreachable reads as idle, as in `mailbox_has_active_turn`.
pub(in crate::services::discord) async fn mailbox_has_blocking_active_turn(
    shared: &SharedData,
    channel_id: ChannelId,
) -> bool {
    shared
        .mailbox(channel_id)
        .has_blocking_active_turn()
        .await
        .unwrap_or(false)
}

/// Unreachable reads as busy: for callers that must not act on a channel
/// whose turn they cannot rule out.
pub(in crate::services::discord) async fn mailbox_has_active_turn_or_unreachable(
    shared: &SharedData,
    channel_id: ChannelId,
) -> bool {
    shared
        .mailbox(channel_id)
        .has_active_turn()
        .await
        .unwrap_or(true)
}

/// Blocking-turn counterpart of `mailbox_has_active_turn_or_unreachable`.
pub(in crate::services::discord) async fn mailbox_has_blocking_active_turn_or_unreachable(
    shared: &SharedData,
    channel_id: ChannelId,
) -> bool {
    shared
        .mailbox(channel_id)
        .has_blocking_active_turn()
        .await
        .unwrap_or(true)
}

/// Waits for the channel's turn to end; an unreachable actor never counts as ended.
pub(in crate::services::discord) async fn wait_for_turn_end(
    shared: &SharedData,
    channel_id: ChannelId,
    timeout: std::time::Duration,
) -> bool {
    let start = tokio::time::Instant::now();
    while shared.mailbox(channel_id).has_active_turn().await != Ok(false) {
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    true
}

#[cfg(test)]
mod mailbox_unreachable_tests {
    use super::{mailbox_has_active_turn, mailbox_has_blocking_active_turn, wait_for_turn_end};
    use crate::services::discord::make_shared_data_for_tests;
    use poise::serenity_prelude::ChannelId;

    #[tokio::test]
    async fn unreachable_actor_is_idle_to_wrapper_but_never_ends_the_turn_wait() {
        let shared = make_shared_data_for_tests();
        let channel_id = ChannelId::new(6_046_001);
        shared.mailboxes.insert_unreachable_for_test(channel_id);

        assert!(!mailbox_has_active_turn(&shared, channel_id).await);
        assert!(!mailbox_has_blocking_active_turn(&shared, channel_id).await);
        assert!(!wait_for_turn_end(&shared, channel_id, std::time::Duration::ZERO).await);
    }
}
