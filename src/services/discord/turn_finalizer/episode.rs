//! Producer-owned terminal evidence. This slice transports captured identity;
//! it does not change terminal admission or finalize policy.
use super::*;

pub(super) struct TerminalEvidence {
    /// Observed legacy None is distinct from an uncaptured episode.
    pub(super) episode_captured: bool,
    pub(super) turn_nonce: Option<String>,
    pub(super) claim_snapshot: Option<SyntheticClaimSnapshot>,
}

#[cfg(test)]
mod tests {
    use super::*;

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
        if matches!(out, FinalizeOutcome::AlreadyFinalized) {
            cleanup::already_finalized_active_state(key, &provider, &event, ctx, &shared).await;
        }
        out
    }
}
