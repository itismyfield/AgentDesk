//! Readiness waits keep their typed evidence through legacy string adapters.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EffectPhase {
    BeforePayload,
    PayloadUncertain,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreparationHold {
    pub(crate) target: String,
    pub(crate) reason: &'static str,
    pub(crate) phase: EffectPhase,
    pub(crate) detail: String,
}

impl PreparationHold {
    pub(crate) fn before(session: &str, reason: &'static str, detail: String) -> Self {
        crate::services::claude_tui::composer_lock::guard_draft(
            session,
            crate::services::claude_tui::composer_lock::DraftGuard::RecoveryRequired,
        );
        Self {
            target: session.into(),
            reason,
            phase: EffectPhase::BeforePayload,
            detail,
        }
    }

    pub(crate) fn uncertain(session: &str, detail: String) -> Self {
        crate::services::claude_tui::composer_lock::guard_draft(
            session,
            crate::services::claude_tui::composer_lock::DraftGuard::RecoveryRequired,
        );
        Self {
            target: session.into(),
            reason: "submission_uncertain",
            phase: EffectPhase::PayloadUncertain,
            detail,
        }
    }

    pub(crate) fn may_requeue(&self) -> bool {
        self.phase == EffectPhase::BeforePayload
    }
}

pub(crate) fn readiness_failure(session: &str, error: String) -> SubmissionError {
    if !is_prompt_ready_timeout_error(&error) {
        return guarded_outcome::readiness_error(session, error);
    }
    let capture = host_input::observe_draft(session);
    if let Some(reason) = composer_refusal(session, capture.as_deref()) {
        return SubmissionError::Held(PreparationHold::before(session, reason, error));
    }
    guarded_outcome::readiness_error(session, error)
}

pub(super) fn changed(session: &str) -> String {
    SubmissionError::Held(PreparationHold::before(
        session,
        "composer_changed",
        "claude tui composer changed before follow-up mutation".into(),
    ))
    .into_legacy()
}

pub(super) fn stopped(session: &str, error: SubmissionError) -> SubmissionError {
    use crate::services::tui_input::submission::Refusal;
    use host_input::{InputRefusal, InputRun};
    match error {
        SubmissionError::Guarded(InputRun::Refused(InputRefusal::Composer(
            Refusal::NotReady
            | Refusal::CaptureUnavailable
            | Refusal::OwnDraft
            | Refusal::GeometryChanged
            | Refusal::WriterContended
            | Refusal::UnpredictableRender,
        ))) => SubmissionError::Held(PreparationHold::before(
            session,
            "composer_not_ready",
            "claude tui composer preparation held before payload".into(),
        )),
        SubmissionError::Guarded(run @ InputRun::Indeterminate { .. }) => {
            SubmissionError::Held(PreparationHold::uncertain(
                session,
                format!("claude tui guarded submission held: {run:?}"),
            ))
        }
        other => other,
    }
}

#[cfg(unix)]
pub(super) fn idle_stash_stopped(session: &str, error: SubmissionError) -> SubmissionError {
    let SubmissionError::Legacy(detail) = error else {
        return stopped(session, error);
    };
    // The live fence, rather than timeout text, proves whether this adapter preceded payload.
    let before =
        super::super::submission_fence::idle_stash_scope().is_some_and(|(fence, context)| {
            context.available && context.target == session && fence.idle_stash_admits(session)
        });
    SubmissionError::Held(if before {
        PreparationHold::before(session, "draft_recovery_hold", detail)
    } else {
        PreparationHold::uncertain(session, detail)
    })
}
