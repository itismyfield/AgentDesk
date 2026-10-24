//! Which persisted episodes a recovery re-mint may still re-open, and which queue files a live
//! actor may still read after a clear could not remove them.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

use super::{ChannelMailboxRegistry, QueuePersistenceContext};
use crate::services::provider::{CancelToken, ProviderKind};
use poise::serenity_prelude::{ChannelId, MessageId};

/// The mailbox holds one episode at a time, so an episode that did not start
/// after its last exact-nonce release has already ended. Two fields, never a
/// history, so no proof is evicted; in-memory, so it never speaks for a prior
/// process's release.
#[derive(Clone, Debug, Default)]
pub(super) struct RemintFence {
    /// Raised by the first exact-nonce release and never lowered.
    raised: bool,
    /// The latest episode this mailbox started, until its own exact release.
    latest_started: Option<(Option<MessageId>, Option<String>)>,
    clear: ClearTrust,
    discarded: BTreeSet<MessageId>,
}

/// A queue-file namespace: the provider and the bot token hash.
pub(super) type Namespace = (ProviderKind, String);

pub(super) fn namespace(persistence: &QueuePersistenceContext) -> Namespace {
    (persistence.provider.clone(), persistence.token_hash.clone())
}

/// A clear distrusts every file until a write of its epoch trusts that namespace again; one slot
/// per file kind suffices because a registry serves one bot, so nothing is evicted.
#[derive(Clone, Debug, Default)]
struct ClearTrust {
    epoch: u64,
    queue: Option<(Namespace, u64)>,
    marker: Option<(Namespace, u64)>,
}

impl ClearTrust {
    fn trusts(&self, slot: &Option<(Namespace, u64)>, namespace: &Namespace) -> bool {
        self.epoch == 0 || slot.as_ref() == Some(&(namespace.clone(), self.epoch))
    }
}

impl RemintFence {
    pub(super) fn note_started(
        &mut self,
        user_message_id: Option<MessageId>,
        turn_nonce: Option<&str>,
    ) {
        self.latest_started = Some((user_message_id, turn_nonce.map(str::to_owned)));
    }

    /// A recovery kickoff that found a live token gained no ownership, so it
    /// must leave the latest started episode where it is.
    pub(super) fn note_kickoff(
        &mut self,
        claimed_empty_slot: bool,
        user_message_id: Option<MessageId>,
        token: &CancelToken,
    ) {
        if claimed_empty_slot {
            self.note_started(user_message_id, token.turn_nonce());
        }
    }

    pub(super) fn note_exact_release(&mut self, user_message_id: MessageId, turn_nonce: &str) {
        self.raised = true;
        if self.is_latest_started(user_message_id, Some(turn_nonce)) {
            self.latest_started = None;
        }
    }

    /// After a release, only the episode started since then is still recoverable.
    pub(super) fn refuses(&self, user_message_id: MessageId, turn_nonce: Option<&str>) -> bool {
        self.raised && !self.is_latest_started(user_message_id, turn_nonce)
    }

    fn is_latest_started(&self, user_message_id: MessageId, turn_nonce: Option<&str>) -> bool {
        self.latest_started.as_ref().is_some_and(|(id, nonce)| {
            *id == Some(user_message_id) && nonce.as_deref() == turn_nonce
        })
    }
}

/// A fence reading: raised, and the latest started episode.
#[cfg(test)]
pub(super) type FenceSnapshot = (bool, Option<(Option<MessageId>, Option<String>)>);

/// The channel's fence, shared by every actor incarnation the registry spawns
/// for it, so a purge that recreates the actor does not forget a release. Only
/// the live actor writes: a purged actor holds no token and refuses starts.
#[derive(Clone, Debug, Default)]
pub(super) struct FenceCell(Arc<Mutex<RemintFence>>);

impl FenceCell {
    fn with<T>(&self, f: impl FnOnce(&mut RemintFence) -> T) -> T {
        f(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub(super) fn note_discarded(&self, ids: &[MessageId]) {
        self.with(|fence| fence.discarded.extend(ids.iter().copied()));
    }

    pub(super) fn refuses_discarded(&self, ids: &[MessageId]) -> bool {
        self.with(|fence| ids.iter().any(|id| fence.discarded.contains(id)))
    }

    pub(super) fn note_started(
        &self,
        user_message_id: Option<MessageId>,
        turn_nonce: Option<&str>,
    ) {
        self.with(|fence| fence.note_started(user_message_id, turn_nonce));
    }

    pub(super) fn note_kickoff(
        &self,
        claimed_empty_slot: bool,
        user_message_id: Option<MessageId>,
        token: &CancelToken,
    ) {
        self.with(|fence| fence.note_kickoff(claimed_empty_slot, user_message_id, token));
    }

    pub(super) fn note_exact_release(&self, user_message_id: MessageId, turn_nonce: &str) {
        self.with(|fence| fence.note_exact_release(user_message_id, turn_nonce));
    }

    pub(super) fn refuses(&self, user_message_id: MessageId, turn_nonce: Option<&str>) -> bool {
        self.with(|fence| fence.refuses(user_message_id, turn_nonce))
    }

    /// A clear committed in memory: no file written before it is read back.
    pub(super) fn begin_clear(&self) {
        self.with(|fence| fence.clear.epoch += 1);
    }

    /// The namespace's queue file now holds what this actor wrote.
    pub(super) fn trust_queue(&self, namespace: &Namespace) {
        self.with(|fence| fence.clear.queue = Some((namespace.clone(), fence.clear.epoch)));
    }

    /// The namespace's dispatch marker is absent or one this actor wrote since the last clear.
    pub(super) fn trust_marker(&self, namespace: &Namespace) {
        self.with(|fence| fence.clear.marker = Some((namespace.clone(), fence.clear.epoch)));
    }

    pub(super) fn trusts_queue(&self, namespace: &Namespace) -> bool {
        self.with(|fence| fence.clear.trusts(&fence.clear.queue, namespace))
    }

    pub(super) fn trusts_marker(&self, namespace: &Namespace) -> bool {
        self.with(|fence| fence.clear.trusts(&fence.clear.marker, namespace))
    }

    /// Whether the fence is raised and which episode it last saw start.
    #[cfg(test)]
    pub(super) fn snapshot(&self) -> FenceSnapshot {
        self.with(|fence| (fence.raised, fence.latest_started.clone()))
    }

    #[cfg(test)]
    fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl ChannelMailboxRegistry {
    /// Cells are never removed: pruning one would let a successor start from a
    /// fresh fence while an older incarnation still holds the old cell.
    pub(super) fn fence_cell(&self, channel_id: ChannelId) -> FenceCell {
        self.remint_fences.entry(channel_id).or_default().clone()
    }

    /// Distinct channels this registry has served; grows for the process life.
    pub(crate) fn remint_fence_cells(&self) -> usize {
        self.remint_fences.len()
    }

    #[cfg(test)]
    fn fence_cell_for_test(&self, channel_id: ChannelId) -> Option<FenceCell> {
        self.remint_fences
            .get(&channel_id)
            .map(|cell| cell.value().clone())
    }
}

#[cfg(test)]
mod remint_fence_tests {
    use super::super::*;
    use super::*;

    fn episode_token(nonce: &str) -> Arc<CancelToken> {
        Arc::new(CancelToken::from_persisted_turn_nonce(Some(
            nonce.to_string(),
        )))
    }

    /// Only a finish that named the exact nonce of the token it took proves which
    /// episode ended; a message-id-only finish may have taken a successor.
    #[tokio::test]
    async fn only_a_finish_naming_the_exact_episode_fences_its_remint() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(5_242_001));
        let persistence = || QueuePersistenceContext::new(&ProviderKind::Claude, "l5242", None);
        let owner = UserId::new(5242);

        assert!(
            handle
                .try_start_turn(episode_token("episode-a"), owner, MessageId::new(7))
                .await
        );
        let exact = handle
            .finish_turn_if_matches_episode_started_before(
                MessageId::new(7),
                Some("episode-a".to_string()),
                std::time::Instant::now(),
                persistence(),
            )
            .await;
        assert!(exact.removed_token.is_some());
        let remint = handle
            .try_start_turn_unless_released(
                episode_token("episode-a"),
                owner,
                MessageId::new(7),
                persistence(),
            )
            .await;
        assert!(!remint.started && remint.refused_released_episode);

        assert!(
            handle
                .try_start_turn(episode_token("episode-b"), owner, MessageId::new(8))
                .await
        );
        let by_id = handle
            .finish_turn_if_matches(MessageId::new(8), persistence())
            .await;
        assert!(by_id.removed_token.is_some());
        let remint = handle
            .try_start_turn_unless_released(
                episode_token("episode-b"),
                owner,
                MessageId::new(8),
                persistence(),
            )
            .await;
        assert!(
            remint.started && !remint.refused_released_episode,
            "a message-id-only finish names no episode, so it witnesses none"
        );
    }

    fn persistence() -> QueuePersistenceContext {
        QueuePersistenceContext::new(&ProviderKind::Claude, "l5951", None)
    }

    async fn release_exactly(handle: &ChannelMailboxHandle, message: u64, nonce: &str) {
        let owner = UserId::new(5951);
        assert!(
            handle
                .try_start_turn(episode_token(nonce), owner, MessageId::new(message))
                .await
        );
        let exact = handle
            .finish_turn_if_matches_episode_started_before(
                MessageId::new(message),
                Some(nonce.to_string()),
                std::time::Instant::now(),
                persistence(),
            )
            .await;
        assert!(exact.removed_token.is_some());
    }

    async fn remint_refused(handle: &ChannelMailboxHandle, message: u64, nonce: &str) -> bool {
        let remint = handle
            .try_start_turn_unless_released(
                episode_token(nonce),
                UserId::new(5951),
                MessageId::new(message),
                persistence(),
            )
            .await;
        assert_eq!(remint.started, !remint.refused_released_episode);
        if remint.started {
            let _ = handle.hard_stop().await;
        }
        remint.refused_released_episode
    }

    fn forget_globals(channel: ChannelId) {
        GLOBAL_CHANNEL_MAILBOXES.remove(&channel);
        GLOBAL_RECOVERY_DONE_SIGNALS.remove(&channel);
        GLOBAL_TURN_FINISHED_SIGNALS.remove(&channel);
    }

    /// P7 (T-F1) — a registry purge recreates the channel's actor; the
    /// successor must still refuse an episode released before the purge.
    #[tokio::test]
    async fn a_recreated_actor_still_refuses_an_episode_released_before_the_purge() {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(5_951_001);
        let first = registry.handle(channel);
        release_exactly(&first, 7, "episode-a").await;
        assert!(remint_refused(&first, 7, "episode-a").await, "control");

        assert_eq!(
            registry.remove_idle_entry(channel).await,
            registry_purge::MailboxPurgeOutcome::Removed
        );
        let successor = registry.handle(channel);
        assert!(!successor.sender.same_channel(&first.sender));
        assert!(
            remint_refused(&successor, 7, "episode-a").await,
            "the purge that recreated the actor forgot the fence"
        );
        forget_globals(channel);
    }

    /// T-F3 — every incarnation of a channel shares one fence: a release on a
    /// successor stays refused on the next one, and a no-op finish through a
    /// purged handle never lowers it.
    #[tokio::test]
    async fn every_incarnation_of_a_channel_shares_one_fence() {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(5_951_002);
        let first = registry.handle(channel);
        let cell = registry
            .fence_cell_for_test(channel)
            .expect("spawn mints the cell");
        assert_eq!(
            registry.remove_idle_entry(channel).await,
            registry_purge::MailboxPurgeOutcome::Removed
        );
        let second = registry.handle(channel);
        assert!(
            registry
                .fence_cell_for_test(channel)
                .is_some_and(|c| c.ptr_eq(&cell)),
            "a pristine cell survives the purge and passes to the successor"
        );
        release_exactly(&second, 9, "episode-b").await;
        assert_eq!(
            registry.remove_idle_entry(channel).await,
            registry_purge::MailboxPurgeOutcome::Removed
        );
        let third = registry.handle(channel);
        assert!(
            registry
                .fence_cell_for_test(channel)
                .is_some_and(|c| c.ptr_eq(&cell))
        );
        assert!(remint_refused(&third, 9, "episode-b").await);

        let stale = first
            .finish_turn_if_matches_episode_started_before(
                MessageId::new(9),
                Some("episode-b".to_string()),
                std::time::Instant::now(),
                persistence(),
            )
            .await;
        assert!(stale.removed_token.is_none());
        assert!(remint_refused(&third, 9, "episode-b").await);
        forget_globals(channel);
    }

    /// T-F2 — the fence is per channel: a release on one channel never refuses
    /// the same episode identity on another.
    #[tokio::test]
    async fn a_release_on_one_channel_never_fences_another() {
        let registry = ChannelMailboxRegistry::default();
        let (x, y) = (ChannelId::new(5_951_003), ChannelId::new(5_951_004));
        let on_x = registry.handle(x);
        release_exactly(&on_x, 11, "episode-x").await;
        assert!(remint_refused(&on_x, 11, "episode-x").await);
        assert!(!remint_refused(&registry.handle(y), 11, "episode-x").await);
        forget_globals(x);
        forget_globals(y);
    }

    /// T-F4 — cells are never pruned, so the gauge counts distinct channels
    /// served, not live actors, and a successor reuses its channel's cell.
    #[tokio::test]
    async fn fence_cells_outlive_purges_and_pass_to_successors() {
        let registry = ChannelMailboxRegistry::default();
        let channels = [5_951_005, 5_951_006, 5_951_007].map(ChannelId::new);
        for channel in channels {
            let _ = registry.handle(channel);
        }
        assert_eq!(registry.remint_fence_cells(), 3);
        for channel in channels {
            assert_eq!(
                registry.remove_idle_entry(channel).await,
                registry_purge::MailboxPurgeOutcome::Removed
            );
        }
        assert_eq!(registry.remint_fence_cells(), 3, "a purge keeps the cell");
        for channel in channels {
            let _ = registry.handle(channel);
            forget_globals(channel);
        }
        assert_eq!(registry.remint_fence_cells(), 3, "successors reuse cells");
    }

    #[test]
    fn only_the_episode_started_since_the_last_release_is_recoverable() {
        let (a, live) = (MessageId::new(7), MessageId::new(200));
        let mut fence = RemintFence::default();
        fence.note_started(Some(a), Some("episode-a"));
        assert!(!fence.refuses(MessageId::new(8), Some("never-started")));
        fence.note_exact_release(a, "episode-a");
        assert!(fence.refuses(a, Some("episode-a")));
        assert!(fence.refuses(a, None));
        assert!(fence.refuses(a, Some("episode-b")));

        for later in 1..=100 {
            let id = MessageId::new(100 + later);
            fence.note_started(Some(id), Some("later"));
            fence.note_exact_release(id, "later");
        }
        assert!(
            fence.refuses(a, Some("episode-a")),
            "later releases never lower the fence under an ended episode"
        );

        fence.note_started(Some(live), Some("live"));
        assert!(!fence.refuses(live, Some("live")));
        assert!(fence.refuses(live, None));
        assert!(fence.refuses(MessageId::new(201), Some("live")));
        assert!(fence.refuses(a, Some("episode-a")));
    }
}
