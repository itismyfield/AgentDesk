use std::time::Instant;

use poise::serenity_prelude::MessageId;

use super::*;

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
    pub(crate) async fn take_timeout_override(
        &self,
        expected_token: Arc<CancelToken>,
    ) -> Option<WatchdogDeadlineExtension> {
        self.request(
            |reply| ChannelMailboxMsg::TakeTimeoutOverride {
                expected_token,
                reply,
            },
            None,
        )
        .await
    }

    /// Operator recovery preserves queue payloads, ordering and pending claims.
    pub(crate) async fn release_turn_lease_if_matches(
        &self,
        expected_user_message_id: MessageId,
        expected_turn_nonce: String,
        active_started_before: Instant,
        persistence: QueuePersistenceContext,
    ) -> FinishTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::FinishTurnIfMatches {
                expected_user_message_id,
                active_started_before: Some(active_started_before),
                turn_nonce_guard: TurnNonceGuard::exact(Some(expected_turn_nonce)),
                preserve_queue: true,
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
                preserve_queue: false,
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
                preserve_queue: false,
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
                preserve_queue: false,
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

pub(super) fn persist_queue_or_restore(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
    previous_queue: Vec<Intervention>,
    operation: &str,
) -> Result<(), String> {
    match persist_queue(channel_id, &state.intervention_queue, persistence) {
        Ok(()) => Ok(()),
        Err(error) => {
            state.intervention_queue = previous_queue;
            log_queue_persistence_rollback(operation, channel_id, persistence, &error);
            Err(error)
        }
    }
}

/// Match an execution token; restoring the same nonce still creates a different Arc.
pub(super) fn matching_cancel_token(
    state: &ChannelMailboxState,
    expected: &Arc<CancelToken>,
) -> Option<Arc<CancelToken>> {
    state
        .cancel_token
        .clone()
        .filter(|token| Arc::ptr_eq(token, expected))
}

/// Called within one mailbox actor command: no await may separate this check and take.
pub(super) fn take_watchdog_override_if_current(
    state: &mut ChannelMailboxState,
    expected: &Arc<CancelToken>,
) -> Option<WatchdogDeadlineExtension> {
    matching_cancel_token(state, expected)?;
    state.watchdog_deadline_override.take()
}

pub(super) fn reset_watchdog_extension_state(state: &mut ChannelMailboxState) {
    state.watchdog_deadline_override = None;
    state.watchdog_extension_count = 0;
    state.watchdog_extension_total_secs = 0;
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

    use std::sync::atomic::Ordering::Relaxed;

    struct WatchdogOwnerFixture {
        channel: ChannelId,
        old: Arc<CancelToken>,
        current: Arc<CancelToken>,
        handle: ChannelMailboxHandle,
        accepted: WatchdogDeadlineExtension,
    }

    async fn watchdog_owner_take(
        channel: ChannelId,
        token: &Arc<CancelToken>,
    ) -> Option<WatchdogDeadlineExtension> {
        crate::services::discord::take_watchdog_deadline_override(channel.get(), token).await
    }

    async fn watchdog_owner_successor(case: u64, nonce: Option<&str>) -> WatchdogOwnerFixture {
        let channel = ChannelId::new(9_000_000_505_601_000 + case);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        let token = || {
            Arc::new(CancelToken::from_persisted_turn_nonce(
                nonce.map(str::to_owned),
            ))
        };
        let old = token();
        assert!(
            handle
                .try_start_turn(old.clone(), UserId::new(51), MessageId::new(51))
                .await
        );
        let captured = handle.cancel_token().await.unwrap();
        assert!(Arc::ptr_eq(&captured, &old));
        old.cancelled.store(true, Relaxed);
        assert!(Arc::ptr_eq(
            &handle.finish_cancelled_turn().await.removed_token.unwrap(),
            &old
        ));
        let current = token();
        assert!(!Arc::ptr_eq(&captured, &current));
        assert!(
            handle
                .try_start_turn(current.clone(), UserId::new(51), MessageId::new(51))
                .await
        );
        let accepted = handle.extend_timeout(86_400).await.unwrap();
        WatchdogOwnerFixture {
            channel,
            old: captured,
            current,
            handle,
            accepted,
        }
    }

    #[tokio::test]
    async fn watchdog_owner_stale_take_preserves_successor() {
        for (case, nonce) in [(0, Some("same-episode")), (1, None)] {
            let f = watchdog_owner_successor(case, nonce).await;
            assert!(watchdog_owner_take(f.channel, &f.old).await.is_none());
            let owned = watchdog_owner_take(f.channel, &f.current).await.unwrap();
            assert_eq!(owned.new_deadline_ms, f.accepted.new_deadline_ms);
            assert_eq!(
                f.current.watchdog_deadline_ms.load(Relaxed),
                f.accepted.new_deadline_ms
            );
            assert_eq!(
                f.current.watchdog_max_deadline_ms.load(Relaxed),
                f.accepted.max_deadline_ms
            );
            assert!(!f.current.cancelled.load(Relaxed));
        }
    }

    #[tokio::test]
    async fn watchdog_owner_stale_cleanup_discard_preserves_successor() {
        for (case, nonce) in [(2, Some("same-episode")), (3, None)] {
            let f = watchdog_owner_successor(case, nonce).await;
            let _ = watchdog_owner_take(f.channel, &f.old).await;
            assert_eq!(
                watchdog_owner_take(f.channel, &f.current)
                    .await
                    .unwrap()
                    .new_deadline_ms,
                f.accepted.new_deadline_ms
            );
            assert_eq!(
                f.current.watchdog_deadline_ms.load(Relaxed),
                f.accepted.new_deadline_ms
            );
        }
    }

    #[tokio::test]
    async fn watchdog_owner_matching_take_and_discard_consume_once() {
        let f = watchdog_owner_successor(4, Some("current")).await;
        assert_eq!(
            watchdog_owner_take(f.channel, &f.current)
                .await
                .unwrap()
                .new_deadline_ms,
            f.accepted.new_deadline_ms
        );
        assert!(watchdog_owner_take(f.channel, &f.current).await.is_none());
        let later = f.handle.extend_timeout(86_400).await.unwrap();
        let _ = watchdog_owner_take(f.channel, &f.current).await;
        assert!(watchdog_owner_take(f.channel, &f.current).await.is_none());
        assert_eq!(
            f.current.watchdog_deadline_ms.load(Relaxed),
            later.new_deadline_ms
        );
    }

    #[tokio::test]
    async fn watchdog_owner_missing_and_idle_mailboxes_are_noops() {
        let channel = ChannelId::new(9_000_000_505_601_005);
        let token = Arc::new(CancelToken::new());
        assert!(watchdog_owner_take(channel, &token).await.is_none());
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        assert!(watchdog_owner_take(channel, &token).await.is_none());
        assert!(handle.cancel_token().await.is_none());
    }

    #[tokio::test]
    async fn watchdog_owner_existing_cancel_guards_keep_pointer_identity() {
        for (case, reasoned) in [(6, false), (7, true)] {
            let f = watchdog_owner_successor(case, Some("same-episode")).await;
            let stale = if reasoned {
                f.handle
                    .cancel_active_turn_if_current_with_reason(
                        f.old.clone(),
                        "5056 test".to_owned(),
                    )
                    .await
            } else {
                f.handle.cancel_active_turn_if_current(f.old.clone()).await
            };
            assert!(stale.token.is_none());
            assert!(!f.current.cancelled.load(Relaxed));
            let current = if reasoned {
                f.handle
                    .cancel_active_turn_if_current_with_reason(
                        f.current.clone(),
                        "5056 test".to_owned(),
                    )
                    .await
            } else {
                f.handle
                    .cancel_active_turn_if_current(f.current.clone())
                    .await
            };
            assert!(Arc::ptr_eq(&current.token.unwrap(), &f.current));
            assert!(f.current.cancelled.load(Relaxed));
        }
    }

    #[test]
    fn watchdog_owner_call_sites_pass_captured_token() {
        let headless = include_str!("../discord/router/message_handler/watchdog.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        let text = include_str!("../discord/router/message_handler/intake_turn/turn_watchdog.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for (source, clears) in [(headless, 3), (text, 2)] {
            assert!(!source.contains("clear_watchdog_deadline_override("));
            assert_eq!(
                source
                    .matches("take_override(watchdog_channel_id_num, &watchdog_token)")
                    .count(),
                4
            );
            assert_eq!(source.matches("let _ = take_override(").count(), clears);
        }
        assert!(headless.contains("take_override(channel_id.get(), watchdog_token).await"));
    }

    fn persistence(label: &str) -> QueuePersistenceContext {
        QueuePersistenceContext::new(&ProviderKind::Claude, label, None)
    }

    fn pending_intervention(message_id: u64) -> Intervention {
        Intervention {
            author_id: UserId::new(99),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            queued_generation: crate::services::discord::runtime_store::process_generation(),
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
        let tmp = tempfile::tempdir().expect("isolated persistence root");
        let _root_guard = crate::config::set_agentdesk_root_for_test(tmp.path());
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
