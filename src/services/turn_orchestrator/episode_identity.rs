use std::time::Instant;

use poise::serenity_prelude::MessageId;

use super::{ChannelMailboxHandle, ChannelMailboxMsg, FinishTurnResult, QueuePersistenceContext};

#[derive(Clone, Debug)]
pub(super) enum TurnNonceGuard {
    Ignore,
    Exact(Option<String>),
}

impl TurnNonceGuard {
    pub(super) fn exact(turn_nonce: Option<String>) -> Self {
        Self::Exact(turn_nonce.filter(|nonce| !nonce.is_empty()))
    }
}

pub(super) fn turn_nonce_guard_matches(
    guard: &TurnNonceGuard,
    active_turn_nonce: Option<&str>,
) -> bool {
    match guard {
        TurnNonceGuard::Ignore => true,
        TurnNonceGuard::Exact(expected) => expected.as_deref() == active_turn_nonce,
    }
}

impl ChannelMailboxHandle {
    /// Episode-identity + monotonic-start guarded finish for durable repair.
    /// The actor compares both axes before taking the active token, so a stale
    /// row cannot release a same-message-id successor admitted before the sweep.
    pub(crate) async fn finish_turn_if_matches_episode_started_before(
        &self,
        expected_user_message_id: MessageId,
        expected_turn_nonce: Option<String>,
        active_started_before: Instant,
        persistence: QueuePersistenceContext,
    ) -> FinishTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::FinishTurnIfMatches {
                expected_user_message_id,
                active_started_before: Some(active_started_before),
                turn_nonce_guard: TurnNonceGuard::exact(expected_turn_nonce),
                persistence,
                reply,
            },
            FinishTurnResult {
                removed_token: None,
                has_pending: false,
                mailbox_online: false,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    /// #3016 — identity-guarded finish. Finalizes the active turn ONLY when
    /// the mailbox's current `active_user_message_id` matches
    /// `expected_user_message_id`; otherwise it is a no-op that returns
    /// `removed_token = None` (so the caller's counter decrement is skipped)
    /// and leaves the possibly-newer live turn untouched.
    pub(crate) async fn finish_turn_if_matches(
        &self,
        expected_user_message_id: MessageId,
        persistence: QueuePersistenceContext,
    ) -> FinishTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::FinishTurnIfMatches {
                expected_user_message_id,
                active_started_before: None,
                turn_nonce_guard: TurnNonceGuard::Ignore,
                persistence,
                reply,
            },
            FinishTurnResult {
                removed_token: None,
                has_pending: false,
                mailbox_online: false,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }

    /// Identity + monotonic-start guarded finish (nonce-agnostic base predicate).
    /// A fresh same-id turn that starts after `active_started_before` must survive
    /// as a no-op. Production durable-repair now goes through the episode-guarded
    /// `finish_turn_if_matches_episode_started_before` (#4595); this nonce-agnostic
    /// entry is retained only to exercise the shared `FinishTurnIfMatches` handler's
    /// start-cutoff branch in tests.
    #[cfg(test)]
    pub(crate) async fn finish_turn_if_matches_started_before(
        &self,
        expected_user_message_id: MessageId,
        active_started_before: Instant,
        persistence: QueuePersistenceContext,
    ) -> FinishTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::FinishTurnIfMatches {
                expected_user_message_id,
                active_started_before: Some(active_started_before),
                turn_nonce_guard: TurnNonceGuard::Ignore,
                persistence,
                reply,
            },
            FinishTurnResult {
                removed_token: None,
                has_pending: false,
                mailbox_online: false,
                queue_exit_events: Vec::new(),
                persistence_error: None,
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use poise::serenity_prelude::{ChannelId, UserId};

    use super::*;
    use crate::services::provider::{CancelToken, ProviderKind};
    use crate::services::turn_orchestrator::{
        ChannelMailboxRegistry, Intervention, InterventionMode,
    };

    fn persistence(label: &str) -> QueuePersistenceContext {
        QueuePersistenceContext::new(&ProviderKind::Claude, label, None)
    }

    fn pending_intervention(message_id: u64) -> Intervention {
        Intervention {
            author_id: UserId::new(99),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            queued_generation: crate::services::discord::runtime_store::load_generation(),
            source_message_ids: vec![MessageId::new(message_id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: "queued successor work".to_string(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    #[test]
    fn exact_nonce_guard_has_explicit_legacy_boundary() {
        assert!(turn_nonce_guard_matches(&TurnNonceGuard::exact(None), None,));
        assert!(turn_nonce_guard_matches(
            &TurnNonceGuard::exact(Some(String::new())),
            None,
        ));
        assert!(!turn_nonce_guard_matches(
            &TurnNonceGuard::exact(None),
            Some("modern"),
        ));
        assert!(!turn_nonce_guard_matches(
            &TurnNonceGuard::exact(Some("modern".to_string())),
            None,
        ));
        assert!(turn_nonce_guard_matches(
            &TurnNonceGuard::exact(Some("modern".to_string())),
            Some("modern"),
        ));
        assert!(!turn_nonce_guard_matches(
            &TurnNonceGuard::exact(Some("episode-a".to_string())),
            Some("episode-b"),
        ));
    }

    #[tokio::test]
    async fn stale_episode_cannot_release_pre_cutoff_same_id_successor() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(4_595_001));
        let user_msg_id = MessageId::new(9_595);
        let stale_token = Arc::new(CancelToken::new());
        let stale_nonce = stale_token.turn_nonce().map(str::to_owned);

        assert!(
            handle
                .try_start_turn(stale_token, UserId::new(7), user_msg_id)
                .await
        );
        let released = handle.finish_turn(persistence("episode-a-release")).await;
        assert!(released.removed_token.is_some());

        let successor_token = Arc::new(CancelToken::new());
        assert!(
            handle
                .try_start_turn(successor_token.clone(), UserId::new(8), user_msg_id)
                .await
        );
        handle
            .replace_queue(
                vec![pending_intervention(9_596)],
                persistence("successor-queue"),
            )
            .await;
        let sweep_started_before = Instant::now();
        let stale_cleanup = handle
            .finish_turn_if_matches_episode_started_before(
                user_msg_id,
                stale_nonce,
                sweep_started_before,
                persistence("stale-episode-a-cleanup"),
            )
            .await;

        assert!(stale_cleanup.removed_token.is_none());
        let snapshot = handle.snapshot().await;
        assert_eq!(snapshot.active_request_owner, Some(UserId::new(8)));
        assert_eq!(snapshot.active_user_message_id, Some(user_msg_id));
        assert_eq!(
            snapshot.active_turn_nonce.as_deref(),
            successor_token.turn_nonce()
        );
        assert!(
            snapshot
                .cancel_token
                .as_ref()
                .is_some_and(|token| Arc::ptr_eq(token, &successor_token))
        );
        assert_eq!(snapshot.intervention_queue.len(), 1);
        assert_eq!(
            snapshot.intervention_queue[0].message_id,
            MessageId::new(9_596)
        );

        let matching_cleanup = handle
            .finish_turn_if_matches_episode_started_before(
                user_msg_id,
                successor_token.turn_nonce().map(str::to_owned),
                sweep_started_before,
                persistence("matching-episode-b-cleanup"),
            )
            .await;
        assert!(matching_cleanup.removed_token.is_some());
    }

    #[tokio::test]
    async fn legacy_episode_only_matches_legacy_active_anchor() {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(4_595_002));
        let user_msg_id = MessageId::new(9_596);
        let legacy_token = Arc::new(CancelToken::from_persisted_turn_nonce(None));
        assert!(
            handle
                .try_start_turn(legacy_token, UserId::new(7), user_msg_id)
                .await
        );

        let result = handle
            .finish_turn_if_matches_episode_started_before(
                user_msg_id,
                None,
                Instant::now(),
                persistence("legacy-episode-cleanup"),
            )
            .await;
        assert!(result.removed_token.is_some());
    }
}
