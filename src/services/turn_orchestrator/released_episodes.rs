//! Exact-episode releases the recovery re-mint must not re-open.

use std::collections::VecDeque;

use poise::serenity_prelude::MessageId;

/// Remembered releases per channel. Evicting the oldest only returns that
/// episode to recovery's pre-witness behaviour; it can never block a live one.
const RELEASED_EPISODE_CAPACITY: usize = 32;

/// Episodes this mailbox released on a finish naming their exact nonce, oldest
/// first. In-memory only, so it never speaks for a prior process's release.
#[derive(Clone, Debug, Default)]
pub(super) struct ReleasedEpisodes(VecDeque<(MessageId, String)>);

impl ReleasedEpisodes {
    pub(super) fn record(&mut self, user_message_id: MessageId, turn_nonce: &str) {
        if self.contains(user_message_id, Some(turn_nonce)) {
            return;
        }
        if self.0.len() == RELEASED_EPISODE_CAPACITY {
            self.0.pop_front();
        }
        self.0.push_back((user_message_id, turn_nonce.to_owned()));
    }

    /// A nonce-less claim names no episode, so it never matches.
    pub(super) fn contains(&self, user_message_id: MessageId, turn_nonce: Option<&str>) -> bool {
        turn_nonce.is_some_and(|nonce| {
            self.0
                .iter()
                .any(|(id, released)| *id == user_message_id && released == nonce)
        })
    }
}

#[cfg(test)]
mod released_episodes_tests {
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

    #[test]
    fn released_episodes_match_exactly_and_stay_bounded() {
        let mut released = ReleasedEpisodes::default();
        released.record(MessageId::new(7), "episode-a");
        assert!(released.contains(MessageId::new(7), Some("episode-a")));
        assert!(!released.contains(MessageId::new(7), None));
        assert!(!released.contains(MessageId::new(7), Some("episode-b")));
        assert!(!released.contains(MessageId::new(8), Some("episode-a")));

        for later in 1..RELEASED_EPISODE_CAPACITY {
            released.record(MessageId::new(100 + later as u64), "later");
        }
        assert!(
            released.contains(MessageId::new(7), Some("episode-a")),
            "a full history still holds its oldest release"
        );
        released.record(MessageId::new(99), "overflow");
        assert!(!released.contains(MessageId::new(7), Some("episode-a")));
        assert_eq!(released.0.len(), RELEASED_EPISODE_CAPACITY);
    }
}
