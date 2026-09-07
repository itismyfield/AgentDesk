//! Captured episode identity shared by normal finalize and operator recovery.
use super::*;

pub(super) fn episode_fingerprint(nonce: Option<&str>) -> [u8; 32] {
    nonce
        .filter(|nonce| !nonce.is_empty())
        .map_or([0; 32], |nonce| *blake3::hash(nonce.as_bytes()).as_bytes())
}

pub(in crate::services::discord) struct CapturedFinish {
    pub(in crate::services::discord) finish: crate::services::turn_orchestrator::FinishTurnResult,
    pub(super) snapshot: Option<SyntheticClaimSnapshot>,
}

impl CapturedFinish {
    pub(in crate::services::discord) fn publish_release(&self, shared: &SharedData, key: TurnKey) {
        if self.finish.removed_token.is_none() {
            return;
        }
        super::super::turn_completion_events::publish_turn_completion_event(
            shared,
            super::super::turn_completion_events::TurnCompletionEvent::mailbox_released(
                key.channel_id,
                Some(key.user_msg_id),
            ),
        );
    }
}

/// The digest authenticates the observed nonce; the existing mailbox actor
/// then compares the full nonce and start cutoff at the actual mutation.
pub(in crate::services::discord) async fn claim_normal_episode(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    key: TurnKey,
    clear_inflight: bool,
) -> Result<Option<CapturedFinish>, ()> {
    if key.episode.is_none() {
        return Ok(None);
    }
    let observed_before = std::time::Instant::now();
    let observed = match shared.mailbox_peek(key.channel_id) {
        Some(mailbox) => Some(mailbox.snapshot().await),
        None => None,
    };
    if key.user_msg_id == 0
        || observed.as_ref().is_some_and(|snapshot| {
            snapshot.cancel_token.is_some()
                && !key.matches_episode_nonce(snapshot.active_turn_nonce.as_deref())
        })
    {
        return Err(());
    }
    let row =
        super::super::inflight::load_inflight_state(provider, key.channel_id.get()).filter(|row| {
            row.effective_finalizer_turn_id() == key.user_msg_id
                && key.matches_episode_nonce(row.turn_nonce.as_deref())
        });
    let finish = if let Some(active) = observed.as_ref().filter(|s| s.cancel_token.is_some()) {
        super::super::mailbox_finish::mailbox_finish_turn_if_matches_episode_started_before_without_completion(
            shared,
            provider,
            key.channel_id,
            serenity::model::id::MessageId::new(key.user_msg_id),
            active.active_turn_nonce.clone(),
            observed_before,
        )
        .await
    } else {
        crate::services::turn_orchestrator::FinishTurnResult {
            removed_token: None,
            has_pending: false,
            mailbox_online: observed.is_some(),
            queue_exit_events: Vec::new(),
            persistence_error: None,
        }
    };
    // Same-episode ID misses retain the ordinary guarded-miss recovery owner.
    // Only the separately gated reconciler may release that residual anchor.
    // Row cleanup is independently authorized by the captured row identity;
    // the lock-held recheck still preserves any successor that replaced it.
    if clear_inflight && let Some(row) = row.as_ref() {
        let _ = super::super::inflight::clear_inflight_state_for_captured_episode(
            provider,
            key.channel_id.get(),
            &super::super::inflight::InflightTurnIdentity::from_state(row),
            row.turn_nonce.as_deref(),
        );
    }
    Ok(Some(CapturedFinish {
        snapshot: row.as_ref().map(SyntheticClaimSnapshot::from_row),
        finish,
    }))
}

pub(super) struct TerminalEvidence {
    /// Observed legacy None is distinct from an uncaptured episode.
    pub(super) episode_captured: bool,
    pub(super) turn_nonce: Option<String>,
    pub(super) claim_snapshot: Option<SyntheticClaimSnapshot>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serenity::model::id::{MessageId, UserId};
    use std::sync::atomic::Ordering;

    async fn start_episode(
        shared: &Arc<SharedData>,
        channel: ChannelId,
        nonce: Option<&str>,
        plan: CompletionAdmissionPlan,
    ) -> (Arc<CancelToken>, TurnKey) {
        let token = Arc::new(CancelToken::from_persisted_turn_nonce(
            nonce.map(str::to_owned),
        ));
        shared
            .mailbox(channel)
            .restore_active_turn(token.clone(), UserId::new(7), MessageId::new(123))
            .await;
        shared.restart.global_active.fetch_add(1, Ordering::Relaxed);
        let key =
            TurnKey::new(channel, 123, shared.restart.current_generation).with_episode_nonce(nonce);
        shared
            .turn_finalizer
            .register_start_with_completion_admission(
                key,
                ProviderKind::Codex,
                RelayOwnerKind::Watcher,
                plan,
                shared,
            );
        (token, key)
    }

    #[tokio::test]
    async fn normal_successor_keeps_admission_and_cleanup_for_modern_and_legacy_episode() {
        super::super::tests::with_isolated_runtime_root(|| async {
            for successor_nonce in [Some("episode-b"), None] {
                let shared = super::super::super::make_shared_data_for_tests_with_storage(None);
                let channel = ChannelId::new(575407);
                let (_, original) = start_episode(
                    &shared,
                    channel,
                    Some("episode-a"),
                    CompletionAdmissionPlan::Immediate,
                )
                .await;
                shared
                    .turn_finalizer
                    .submit_terminal(
                        original,
                        ProviderKind::Codex,
                        TerminalEvent::Complete,
                        FinalizeContext::bridge(),
                        shared.clone(),
                    )
                    .await;
                let (token, successor) = start_episode(
                    &shared,
                    channel,
                    successor_nonce,
                    CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled,
                )
                .await;
                shared.turn_finalizer.note_terminal_projection_settled(
                    successor,
                    false,
                    shared.clone(),
                );
                shared.turn_finalizer.note_terminal_disposition_settled(
                    successor,
                    false,
                    shared.clone(),
                );
                shared
                    .dispatch
                    .role_overrides
                    .insert(channel, ChannelId::new(575408));
                let recovery = shared.mailboxes.recovery_done(channel);
                recovery.reset();
                let mut events =
                    super::super::super::turn_completion_events::subscribe_turn_completion_events(
                        &shared,
                    );

                shared
                    .turn_finalizer
                    .submit_terminal(
                        original,
                        ProviderKind::Codex,
                        TerminalEvent::Complete,
                        FinalizeContext::bridge(),
                        shared.clone(),
                    )
                    .await;
                assert!(
                    !token.cancelled.load(Ordering::Acquire),
                    "late A must not cancel B"
                );
                assert!(events.try_recv().is_err());

                let outcome = shared
                    .turn_finalizer
                    .submit_terminal_with_episode_nonce(
                        successor,
                        ProviderKind::Codex,
                        TerminalEvent::Complete,
                        FinalizeContext::bridge(),
                        successor_nonce.map(str::to_owned),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(
                    outcome,
                    FinalizeOutcome::Finalized {
                        removed_token: Some(_),
                        ..
                    }
                ));
                assert!(
                    !shared.dispatch.role_overrides.contains_key(&channel),
                    "normal cleanup must run"
                );
                assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
                tokio::time::timeout(Duration::from_millis(50), recovery.wait())
                    .await
                    .expect("normal release wakes recovery");
                assert!(!events.try_recv().unwrap().queue_is_eligible());
                assert!(
                    events.try_recv().is_err(),
                    "negative B evidence must block QueueEligible"
                );
            }
        })
        .await;
    }

    #[tokio::test]
    async fn rowless_submission_transports_original_episode_without_snapshot() {
        let shared = super::super::super::make_shared_data_for_tests_with_storage(None);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let finalizer = TurnFinalizer {
            tx,
            guarded_finish_residues: Default::default(),
        };
        let mut source_nonce = "episode-a".to_string();
        let request = finalizer.submit_terminal_with_episode_nonce(
            TurnKey::new(ChannelId::new(575406), 123, 0),
            ProviderKind::Codex,
            TerminalEvent::Complete,
            FinalizeContext::bridge(),
            Some(source_nonce.clone()),
            shared,
        );
        source_nonce = "episode-b".to_string();
        let receiver = async {
            let FinalizeMsg::Terminal { evidence, ack, .. } = rx.recv().await.unwrap() else {
                panic!("expected terminal evidence");
            };
            assert_eq!(source_nonce, "episode-b");
            assert_eq!(evidence.turn_nonce.as_deref(), Some("episode-a"));
            assert!(evidence.episode_captured);
            assert!(
                evidence.claim_snapshot.is_none(),
                "rowless proof must not manufacture output metadata"
            );
            assert!(ack.send(FinalizeOutcome::Deferred).is_ok());
        };
        let (outcome, ()) = tokio::join!(request, receiver);
        assert!(matches!(outcome, FinalizeOutcome::Deferred));
    }

    #[tokio::test]
    async fn unique_generation_producer_keeps_successor_safe_after_external_release() {
        super::super::tests::with_isolated_runtime_root(|| async {
            let shared = super::super::super::make_shared_data_for_tests_with_storage(None);
            let channel = ChannelId::new(575410);
            let mailbox = shared.mailbox(channel);
            let original = Arc::new(CancelToken::new());
            let first =
                TurnKey::new(channel, 123, 1 << 48).with_episode_nonce(original.turn_nonce());
            mailbox
                .restore_active_turn(original.clone(), UserId::new(7), MessageId::new(123))
                .await;
            shared.turn_finalizer.register_start(
                first,
                ProviderKind::Codex,
                RelayOwnerKind::Watcher,
                &shared,
            );
            // Model an external lease release that leaves the old producer pending.
            let released = mailbox
                .finish_turn_if_matches_episode_started_before(
                    MessageId::new(123),
                    original.turn_nonce().map(str::to_owned),
                    std::time::Instant::now(),
                    super::super::super::queue_persistence_context(
                        &shared,
                        &ProviderKind::Codex,
                        channel,
                    ),
                )
                .await;
            assert!(released.removed_token.is_some());
            let successor = Arc::new(CancelToken::new());
            let second = TurnKey::new(channel, 123, (1 << 48) + 1)
                .with_episode_nonce(successor.turn_nonce());
            mailbox
                .restore_active_turn(successor.clone(), UserId::new(7), MessageId::new(123))
                .await;
            shared.restart.global_active.store(1, Ordering::Relaxed);
            shared.turn_finalizer.register_start(
                second,
                ProviderKind::Codex,
                RelayOwnerKind::Watcher,
                &shared,
            );
            for generation in [first.generation, (1 << 48) + 99] {
                shared
                    .turn_finalizer
                    .submit_terminal(
                        TurnKey::new(channel, 123, generation),
                        ProviderKind::Codex,
                        TerminalEvent::Complete,
                        FinalizeContext::bridge(),
                        shared.clone(),
                    )
                    .await;
                assert!(!successor.cancelled.load(Ordering::Acquire));
                assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
            }
            let completed = shared
                .turn_finalizer
                .submit_terminal(
                    TurnKey::new(channel, 123, second.generation),
                    ProviderKind::Codex,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(
                completed,
                FinalizeOutcome::Finalized {
                    removed_token: Some(_),
                    ..
                }
            ));
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        })
        .await;
    }

    #[tokio::test]
    async fn recovery_retains_captured_nonce_and_refuses_same_id_successor() {
        super::super::tests::with_isolated_runtime_root(|| async {
            for nonce in [Some("restored-a"), None] {
                let shared = super::super::super::make_shared_data_for_tests_with_storage(None);
                let channel = ChannelId::new(575409);
                // No tmux/process target: this is an isolated mailbox identity check.
                let mut row = super::super::super::inflight::InflightTurnState::new(
                    ProviderKind::Codex,
                    channel.get(),
                    None,
                    7,
                    123,
                    124,
                    "restore".to_string(),
                    None,
                    None,
                    None,
                    None,
                    0,
                );
                row.turn_nonce = nonce.map(str::to_owned);
                assert!(
                    crate::services::discord::recovery::reregister_active_turn_from_inflight(
                        &shared, &row
                    )
                    .await
                );
                assert_eq!(
                    shared
                        .mailbox(channel)
                        .snapshot()
                        .await
                        .active_turn_nonce
                        .as_deref(),
                    nonce
                );
                let successor = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
                    "successor".to_string(),
                )));
                shared
                    .mailbox(channel)
                    .restore_active_turn(successor.clone(), UserId::new(7), MessageId::new(123))
                    .await;
                assert!(
                    !crate::services::discord::recovery::reregister_active_turn_from_inflight(
                        &shared, &row
                    )
                    .await
                );
                let live = shared
                    .mailbox(channel)
                    .snapshot()
                    .await
                    .cancel_token
                    .unwrap();
                assert!(Arc::ptr_eq(&live, &successor));
                assert!(!successor.cancelled.load(Ordering::Acquire));
            }
        })
        .await;
    }

    #[test]
    fn missing_snapshot_is_not_an_observed_legacy_episode() {
        let evidence = TerminalEvidence::from_snapshot(None);
        assert!(!evidence.episode_captured);
        assert!(evidence.turn_nonce.is_none());
    }
}

impl TerminalEvidence {
    pub(super) fn from_snapshot(claim_snapshot: Option<SyntheticClaimSnapshot>) -> Self {
        Self {
            episode_captured: claim_snapshot.is_some(),
            turn_nonce: claim_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.turn_nonce.clone()),
            claim_snapshot,
        }
    }
}

impl TurnFinalizer {
    /// A rowless recovery may still carry the mailbox episode it observed
    /// before proving eligibility. It must not manufacture a row snapshot.
    pub(in crate::services::discord) async fn submit_terminal_with_episode_nonce(
        &self,
        key: TurnKey,
        provider: ProviderKind,
        event: TerminalEvent,
        ctx: FinalizeContext,
        turn_nonce: Option<String>,
        shared: Arc<SharedData>,
    ) -> FinalizeOutcome {
        self.submit_terminal_evidence(
            key,
            provider,
            event,
            ctx,
            TerminalEvidence {
                episode_captured: true,
                turn_nonce,
                claim_snapshot: None,
            },
            shared,
        )
        .await
    }

    pub(super) async fn submit_terminal_evidence(
        &self,
        key: TurnKey,
        provider: ProviderKind,
        event: TerminalEvent,
        ctx: FinalizeContext,
        evidence: TerminalEvidence,
        shared: Arc<SharedData>,
    ) -> FinalizeOutcome {
        let key = if evidence.episode_captured {
            if !key.matches_episode_nonce(evidence.turn_nonce.as_deref()) {
                return FinalizeOutcome::Deferred;
            }
            key.with_episode_nonce(evidence.turn_nonce.as_deref())
        } else {
            key
        };
        if let Some(snapshot) = evidence.claim_snapshot.as_ref() {
            cleanup::ensure_synthetic_claim_marker_before_clear(key, &provider, Some(snapshot));
        }
        let (ack, rx) = oneshot::channel();
        if self
            .tx
            .send(FinalizeMsg::Terminal {
                key,
                provider: provider.clone(),
                event: event.clone(),
                ctx,
                evidence,
                shared: shared.clone(),
                ack,
            })
            .is_err()
        {
            return FinalizeOutcome::AlreadyFinalized;
        }
        let Ok(out) = rx.await else {
            return FinalizeOutcome::AlreadyFinalized;
        };
        if matches!(out, FinalizeOutcome::AlreadyFinalized)
            && !(key.episode.is_none() && key.generation != shared.restart.current_generation)
        {
            cleanup::already_finalized_active_state(key, &provider, &event, ctx, &shared).await;
        }
        out
    }
}
