//! A managed attempt that provably wrote nothing ends on its own birth; the same request then
//! takes a distinct birth with a new one-shot capability. The A leaf and turn creator wire it.
use super::*;
use crate::services::claude_tui::busy_inject::Outcome;

const OUTCOME_CALLER: &str = "managed_submission_outcome";

/// A leaf result that reached no key: C-s, paste and Enter were all zero.
pub(in crate::services::discord) struct NoContentProof(());

impl NoContentProof {
    pub(in crate::services::discord) fn from_leaf(outcome: Outcome) -> Option<Self> {
        matches!(outcome, Outcome::NotSent(_)).then_some(Self(()))
    }
}

impl ManagedSubmission {
    /// Persists the not-submitted terminal on this exact birth, then closes this capability.
    pub(in crate::services::discord) fn conclude_not_submitted(
        &self,
        _proof: NoContentProof,
    ) -> Result<(), String> {
        let attempt = &self.0;
        let mut gate = attempt.gate.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(*gate, Gate::Refused(_)) {
            return Err("managed attempt already closed".into());
        }
        let mut row = super::super::episode_guard::lock_inflight_birth_episode(
            &attempt.provider,
            attempt.channel_id,
            &attempt.birth,
        )
        .map_err(|error| format!("inflight row unavailable: {error:?}"))?;
        let open = row
            .state()
            .managed_submission
            .as_ref()
            .is_some_and(|record| {
                record.protocol == MANAGED_SUBMISSION_PROTOCOL
                    && matches!(
                        record.phase,
                        SubmissionPhase::Waiting | SubmissionPhase::MayHaveSubmitted
                    )
            });
        if !open {
            let found = row.state().managed_submission.clone();
            return Err(format!("submission record cannot end here: {found:?}"));
        }
        let mut updated = row.state().clone();
        updated.managed_submission =
            Some(ManagedSubmissionRecord::at(SubmissionPhase::NotSubmitted));
        match row.persist_verified_under_guard(&updated, OUTCOME_CALLER) {
            GuardedSaveOutcome::Saved
                if row.state().managed_submission == updated.managed_submission => {}
            outcome => return Err(format!("terminal write not confirmed: {outcome:?}")),
        }
        *gate = Gate::Refused(PreSubmitPersistenceRefused {
            reason: "managed attempt ended: not submitted".into(),
        });
        Ok(())
    }

    /// Hands the same request to a distinct birth under the ended birth's lock; the row is
    /// replaced, never unlinked, and the new capability exists only after the read-back.
    pub(in crate::services::discord) fn successor(
        &self,
        turn_nonce: String,
    ) -> Result<ManagedSubmission, String> {
        let attempt = &self.0;
        let mut row = super::super::episode_guard::lock_inflight_birth_episode(
            &attempt.provider,
            attempt.channel_id,
            &attempt.birth,
        )
        .map_err(|error| format!("ended birth unavailable: {error:?}"))?;
        if row.state().managed_submission
            != Some(ManagedSubmissionRecord::at(SubmissionPhase::NotSubmitted))
        {
            return Err("only a not-submitted birth has a successor".into());
        }
        if row.state().turn_nonce.as_deref() == Some(turn_nonce.as_str()) {
            return Err("a successor needs a distinct birth".into());
        }
        let mut updated = row.state().clone();
        updated.turn_nonce = Some(turn_nonce);
        updated.managed_submission = Some(ManagedSubmissionRecord::at(SubmissionPhase::Waiting));
        match row.persist_verified_under_guard(&updated, OUTCOME_CALLER) {
            GuardedSaveOutcome::Saved
                if row.state().turn_nonce == updated.turn_nonce
                    && row.state().managed_submission == updated.managed_submission => {}
            outcome => return Err(format!("successor write not confirmed: {outcome:?}")),
        }
        let saved = row.state();
        Ok(ManagedSubmission(Arc::new(Attempt {
            provider: attempt.provider.clone(),
            channel_id: attempt.channel_id,
            birth: InflightEpisodePin::from_state(saved),
            target: attempt.target.clone(),
            native_session_id: saved.session_id.clone(),
            gate: Mutex::new(Gate::Open),
            guarded: Mutex::new(GuardedState::Dormant),
            evidence: Mutex::new(evidence::Evidence::default()),
        })))
    }
}

#[cfg(unix)]
#[path = "outcome_tests.rs"]
mod tests;
