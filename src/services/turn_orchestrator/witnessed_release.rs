//! #6199 — the THREAD-GUARD's release of one witnessed episode. The actor matches the exact
//! episode, runs the caller's route withdrawal and releases the token in one step, so a
//! successor's claim, and the route it publishes after that claim, come after the withdrawal.

use super::*;

/// Runs inside the actor between the episode match and the token release: it must not block.
pub(crate) trait ReleaseWithdrawal: Send + 'static {
    /// What it withdrew.
    fn withdraw(self: Box<Self>) -> Vec<ChannelId>;
}

/// The episode a THREAD-GUARD witnessed: its anchor, its nonce and a start cutoff.
pub(crate) struct WitnessedEpisodeRelease {
    anchor: MessageId,
    nonce: String,
    started_before: Instant,
    withdrawal: Box<dyn ReleaseWithdrawal>,
    persistence: QueuePersistenceContext,
}

impl WitnessedEpisodeRelease {
    /// `None` for an empty nonce, which would match an episode that carries none.
    pub(crate) fn new(
        anchor: MessageId,
        nonce: String,
        started_before: Instant,
        withdrawal: Box<dyn ReleaseWithdrawal>,
        persistence: QueuePersistenceContext,
    ) -> Option<Self> {
        (!nonce.is_empty()).then_some(Self {
            anchor,
            nonce,
            started_before,
            withdrawal,
            persistence,
        })
    }
}

pub(crate) struct WitnessedReleaseResult {
    pub(crate) finish: FinishTurnResult,
    /// What the withdrawal returned; empty when it did not run.
    pub(crate) withdrawn: Vec<ChannelId>,
}

pub(super) struct Request {
    release: WitnessedEpisodeRelease,
    reply: oneshot::Sender<WitnessedReleaseResult>,
}

impl ChannelMailboxHandle {
    /// An actor that does not answer released nothing and ran no withdrawal.
    pub(crate) async fn release_witnessed_episode(
        &self,
        release: WitnessedEpisodeRelease,
    ) -> WitnessedReleaseResult {
        let request = |reply| {
            ChannelMailboxMsg::ReleaseWitnessedEpisode(Box::new(Request { release, reply }))
        };
        let offline = |_| WitnessedReleaseResult {
            finish: FinishTurnResult {
                removed_token: None,
                has_pending: false,
                mailbox_online: false,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
            withdrawn: Vec::new(),
        };
        self.request(request).await.unwrap_or_else(offline)
    }
}

/// The `FinishTurnIfMatches` finish with a non-empty exact nonce and a start cutoff, the
/// withdrawal running only on a match and before the token leaves.
pub(super) fn answer(state: &mut ChannelMailboxState, channel_id: ChannelId, request: Request) {
    let Request { release, reply } = request;
    let finish = episode_identity::FinishIfMatches {
        expected_actor: None,
        preserve_queue: false,
        expected_user_message_id: release.anchor,
        active_started_before: Some(release.started_before),
        turn_nonce_guard: episode_identity::TurnNonceGuard::exact(Some(release.nonce)),
        persistence: release.persistence,
    };
    let answer = |finished, withdrawn| {
        let _ = reply.send(WitnessedReleaseResult {
            finish: finished,
            withdrawn,
        });
    };
    let withdrawal = Some(release.withdrawal);
    episode_identity::finish_turn_if_matches(state, channel_id, finish, withdrawal, answer);
}

#[cfg(test)]
#[path = "witnessed_release_tests.rs"]
mod tests;
