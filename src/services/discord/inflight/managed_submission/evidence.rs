use super::*;
use crate::services::claude_tui::input::{PreparationHold, UserReceipt};

#[derive(Default)]
pub(super) struct Evidence {
    hold: Option<PreparationHold>,
    ended: bool,
    receipt: Option<UserReceipt>,
}

impl InflightTurnState {
    pub(in crate::services::discord) fn with_live_submission_from(
        mut self,
        creator: &Self,
    ) -> Self {
        if let Some(owner) = creator.managed_submission_capability()
            && owner.matches_attempt(&self)
        {
            self.bind_managed_submission(owner.clone());
        }
        self
    }

    pub(in crate::services::discord) fn has_native_herdr_input(&self) -> bool {
        matches!(&self.host_locator, Some(super::super::host_locator::PersistedHostLocator::Known(locator))
            if locator.host_kind == crate::services::session_host::HostKind::Herdr)
    }
}

impl ManagedSubmission {
    pub(in crate::services::discord) fn matches_attempt(&self, state: &InflightTurnState) -> bool {
        state.managed_submission.as_ref().is_some_and(|record| {
            record.protocol == MANAGED_SUBMISSION_PROTOCOL
                && matches!(
                    record.phase,
                    SubmissionPhase::Waiting | SubmissionPhase::MayHaveSubmitted
                )
        }) && self.0.channel_id == state.channel_id
            && state.provider_kind().as_ref() == Some(&self.0.provider)
            && state.tmux_session_name.as_deref() == Some(self.0.target.as_str())
            && self
                .0
                .birth
                .is_same_episode_as(&InflightEpisodePin::from_state(state))
    }
    pub(in crate::services::discord) fn preparation_hold(&self) -> Option<PreparationHold> {
        self.0
            .evidence
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .hold
            .clone()
    }

    pub(in crate::services::discord) fn exact_user_receipt(&self) -> bool {
        if !matches!(
            *self.0.gate.lock().unwrap_or_else(PoisonError::into_inner),
            Gate::Fenced | Gate::IdleStashFenced
        ) {
            return false;
        }
        let mut evidence = self
            .0
            .evidence
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        evidence
            .receipt
            .as_mut()
            .is_some_and(UserReceipt::confirmed)
    }
}

pub(super) fn hold(attempt: &Attempt, hold: &PreparationHold) {
    if hold.target == attempt.target {
        let mut evidence = attempt
            .evidence
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if evidence
            .hold
            .as_ref()
            .is_none_or(|old| old.may_requeue() && !hold.may_requeue())
        {
            evidence.hold = Some(hold.clone());
        }
    }
}

pub(super) fn receipt(attempt: &Attempt, receipt: UserReceipt) {
    let mut evidence = attempt
        .evidence
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if evidence
        .hold
        .as_ref()
        .is_none_or(PreparationHold::may_requeue)
        && !evidence.ended
    {
        evidence.receipt = Some(receipt);
    }
}

pub(super) fn finish_receipt(attempt: &Attempt, session: &str) {
    if attempt.target != session {
        return;
    }
    let mut evidence = attempt
        .evidence
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    evidence.ended = true;
    if let Some(receipt) = evidence.receipt.as_mut() {
        receipt.seal();
    }
}

/// An accepted payload supersedes a transient admission refusal, never an uncertain send.
pub(super) fn admit_payload(attempt: &Attempt) {
    let mut evidence = attempt
        .evidence
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if evidence
        .hold
        .as_ref()
        .is_some_and(PreparationHold::may_requeue)
    {
        evidence.hold = None;
    }
}
