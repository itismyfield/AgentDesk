//! #5951 C3t-0i — follow-up after an accepted mailbox request belongs to the
//! actor incarnation that accepted it. A registry purge between the reply and
//! the follow-up recreates the channel's actor; re-resolving by channel would
//! then act on the successor.

use super::*;
use crate::services::turn_orchestrator::RecoveryDoneSignal;
use crate::services::turn_orchestrator::registry_purge::MailboxPurgeOutcome;
use futures::FutureExt;
use std::future::Future;
use std::pin::Pin;

#[derive(Clone, Copy, Debug)]
enum FollowUp {
    FinishOwned,
    FinishCancelled,
    Finish,
    FinishIfMatches,
    FinishEpisode,
    ClearRecoveryMarker,
    ClearChannel,
}

impl FollowUp {
    const ALL: [Self; 7] = [
        Self::FinishOwned,
        Self::FinishCancelled,
        Self::Finish,
        Self::FinishIfMatches,
        Self::FinishEpisode,
        Self::ClearRecoveryMarker,
        Self::ClearChannel,
    ];

    /// The wrappers that mark only after removing a token need one to remove.
    fn needs_live_turn(self) -> bool {
        matches!(
            self,
            Self::FinishCancelled | Self::FinishIfMatches | Self::FinishEpisode
        )
    }

    fn run<'a>(
        self,
        shared: &'a SharedData,
        provider: &'a ProviderKind,
        channel: ChannelId,
        message: MessageId,
        nonce: Option<String>,
    ) -> Pin<Box<dyn Future<Output = ()> + 'a>> {
        use crate::services::discord;
        Box::pin(async move {
            match self {
                Self::FinishOwned => {
                    discord::mailbox_finish_owned_turn(shared, provider, channel).await;
                }
                Self::FinishCancelled => {
                    discord::mailbox_finish_cancelled_turn(shared, channel).await;
                }
                Self::Finish => {
                    discord::mailbox_finish_turn(shared, provider, channel).await;
                }
                Self::FinishIfMatches => {
                    discord::mailbox_finish_turn_if_matches(shared, provider, channel, message)
                        .await;
                }
                Self::FinishEpisode => {
                    discord::mailbox_finish_turn_if_matches_episode_started_before(
                        shared,
                        provider,
                        channel,
                        message,
                        nonce,
                        std::time::Instant::now(),
                    )
                    .await;
                }
                Self::ClearRecoveryMarker => {
                    discord::mailbox_clear_recovery_marker(shared, channel).await;
                }
                Self::ClearChannel => {
                    discord::mailbox_clear_channel(shared, provider, channel).await;
                }
            }
        })
    }
}

fn latched(signal: &RecoveryDoneSignal) -> bool {
    signal.wait().now_or_never().is_some()
}

/// T-E3x — every wrapper is polled first, so the old actor ACCEPTS its request
/// ahead of the purge's `CloseIfIdle`. The purge then removes the idle actor
/// and a successor starts its own recovery before the wrapper resumes. The
/// wrapper's `recovery_done` mark must not wake a watcher of that recovery.
#[tokio::test]
async fn accepted_wrapper_follow_up_never_latches_a_successor_recovery() {
    let (_root_guard, _root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Claude;
    let (_registry, shared) = registry_with_shared(provider.clone()).await;
    let mut latched_successors = Vec::new();
    for (index, case) in FollowUp::ALL.into_iter().enumerate() {
        let channel = ChannelId::new(5_951_701 + index as u64);
        let message = MessageId::new(5_951_801 + index as u64);
        let old = shared.mailbox(channel);
        let mut nonce = None;
        if case.needs_live_turn() {
            let token = start_test_turn(&shared, channel, message).await;
            token
                .cancelled
                .store(matches!(case, FollowUp::FinishCancelled), Ordering::Relaxed);
            nonce = token.turn_nonce().map(str::to_owned);
        }

        let mut wrapper = case.run(&shared, &provider, channel, message, nonce);
        assert!(
            futures::poll!(wrapper.as_mut()).is_pending(),
            "{case:?}: the request is queued on the old actor"
        );
        assert_eq!(
            shared.mailboxes.remove_idle_entry(channel).await,
            MailboxPurgeOutcome::Removed,
            "{case:?}: the old actor answered first, then was idle and purged"
        );
        assert!(old.snapshot().await.cancel_token.is_none());

        let successor_signal = shared.mailboxes.recovery_done(channel);
        let kickoff = shared
            .mailbox(channel)
            .recovery_kickoff(
                Arc::new(CancelToken::new()),
                UserId::new(1),
                Some(MessageId::new(5_951_901 + index as u64)),
            )
            .await;
        assert!(kickoff.activated_turn(), "{case:?}: {kickoff:?}");
        assert!(!latched(&successor_signal));

        wrapper.await;
        let successor = shared.mailbox(channel).snapshot().await;
        assert!(
            successor.recovery_started_at.is_some(),
            "{case:?}: the successor's recovery is still in progress"
        );
        let global =
            crate::services::turn_orchestrator::ChannelMailboxRegistry::global_recovery_done(
                channel,
            );
        if latched(&successor_signal) || global.as_deref().is_some_and(latched) {
            latched_successors.push(case);
        }
        crate::services::discord::mailbox_finish_turn(&shared, &provider, channel).await;
    }
    assert!(
        latched_successors.is_empty(),
        "an accepted old-actor follow-up latched the successor's recovery_done: \
         {latched_successors:?}"
    );
}

/// T-E3q — the force-purge finish belongs to the actor that accepted the
/// purge. Here that actor is purged before the finish runs and a successor
/// holds a cancelled turn of its own; the finish must leave that turn to its
/// owner.
#[tokio::test]
async fn force_purge_finish_never_reaches_a_successor_actor() {
    let (_root_guard, _root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Claude;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let registry = Arc::new(registry);
    let channel = ChannelId::new(5_951_721);
    let old = shared.mailbox(channel);
    let killed = start_test_turn(&shared, channel, MessageId::new(5_951_821)).await;
    killed.cancelled.store(true, Ordering::Relaxed);

    let target = crate::services::turn_lifecycle::TurnLifecycleTarget {
        provider: Some(provider.clone()),
        channel_id: Some(channel),
        tmux_name: String::new(),
    };
    let purge = crate::services::queue::force_purge_channel_mailbox(Some(&registry), &target, None);
    tokio::pin!(purge);
    assert!(futures::poll!(purge.as_mut()).is_pending());
    assert!(
        old.snapshot().await.cancel_token.is_none(),
        "the old actor accepted the purge and released the killed anchor"
    );
    assert_eq!(
        shared.mailboxes.remove_idle_entry(channel).await,
        MailboxPurgeOutcome::Removed
    );
    let successor_turn = start_test_turn(&shared, channel, MessageId::new(5_951_822)).await;
    successor_turn.cancelled.store(true, Ordering::Relaxed);

    assert_eq!(purge.await, Some(0));
    let live = shared.mailbox(channel).snapshot().await.cancel_token;
    assert!(
        live.is_some_and(|token| Arc::ptr_eq(&token, &successor_turn)),
        "the post-purge finish released the successor's turn"
    );
    crate::services::discord::mailbox_finish_cancelled_turn(&shared, channel).await;
}
