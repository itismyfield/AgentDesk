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
/// whose turn they cannot rule out. Unix-only: its sole caller is the tmux module.
#[cfg(unix)]
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

/// Gate for the watcher-direct session-idle commit. An unreachable actor warns
/// because, unlike an active turn, it persists until the mailbox is purged.
#[cfg(unix)]
pub(in crate::services::discord) async fn mailbox_blocks_session_idle_commit(
    shared: &SharedData,
    channel_id: ChannelId,
    tmux_session_name: &str,
    provider: &crate::services::provider::ProviderKind,
) -> bool {
    match shared.mailbox(channel_id).cancel_token().await {
        Ok(None) => false,
        Ok(Some(_)) => {
            tracing::debug!(
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                provider = %provider.as_str(),
                "skipping watcher-direct terminal session-idle commit; mailbox turn is active"
            );
            true
        }
        Err(_) => {
            tracing::warn!(
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                provider = %provider.as_str(),
                "skipping watcher-direct terminal session-idle commit; mailbox actor unreachable until purged"
            );
            true
        }
    }
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
    #[cfg(unix)]
    use super::{mailbox_blocks_session_idle_commit, mailbox_has_active_turn_or_unreachable};
    use super::{
        mailbox_has_active_turn, mailbox_has_blocking_active_turn,
        mailbox_has_blocking_active_turn_or_unreachable, wait_for_turn_end,
    };
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
        #[cfg(unix)]
        assert!(mailbox_has_active_turn_or_unreachable(&shared, channel_id).await);
        assert!(mailbox_has_blocking_active_turn_or_unreachable(&shared, channel_id).await);
    }

    /// Levels of events emitted from this module while the returned guard lives.
    #[cfg(unix)]
    fn capture_probe_levels() -> (
        std::sync::Arc<std::sync::Mutex<Vec<tracing::Level>>>,
        tracing::subscriber::DefaultGuard,
    ) {
        use tracing_subscriber::layer::SubscriberExt;
        struct Levels(std::sync::Arc<std::sync::Mutex<Vec<tracing::Level>>>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Levels {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                if event.metadata().target().ends_with("mailbox_probe") {
                    self.0.lock().unwrap().push(*event.metadata().level());
                }
            }
        }
        let levels = std::sync::Arc::default();
        let subscriber =
            tracing_subscriber::registry().with(Levels(std::sync::Arc::clone(&levels)));
        (levels, tracing::subscriber::set_default(subscriber))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn watcher_direct_idle_commit_blocks_on_turn_quietly_and_on_unreachable_loudly() {
        use crate::services::provider::{CancelToken, ProviderKind};
        use poise::serenity_prelude::{MessageId, UserId};
        let shared = make_shared_data_for_tests();
        let (idle, busy, dead) = (
            ChannelId::new(6_046_002),
            ChannelId::new(6_046_003),
            ChannelId::new(6_046_004),
        );
        assert!(
            shared
                .mailbox(busy)
                .try_start_turn(
                    std::sync::Arc::new(CancelToken::new()),
                    UserId::new(7),
                    MessageId::new(77)
                )
                .await
        );
        shared.mailboxes.insert_unreachable_for_test(dead);
        let provider = ProviderKind::Claude;

        let (levels, _guard) = capture_probe_levels();
        assert!(!mailbox_blocks_session_idle_commit(&shared, idle, "s", &provider).await);
        assert!(levels.lock().unwrap().is_empty());
        assert!(mailbox_blocks_session_idle_commit(&shared, busy, "s", &provider).await);
        assert_eq!(*levels.lock().unwrap(), [tracing::Level::DEBUG]);
        assert!(mailbox_blocks_session_idle_commit(&shared, dead, "s", &provider).await);
        assert_eq!(
            *levels.lock().unwrap(),
            [tracing::Level::DEBUG, tracing::Level::WARN]
        );
    }
}
