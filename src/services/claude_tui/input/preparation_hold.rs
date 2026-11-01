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
    /// A modal refused the pane at the capture before any key; legacy callers read the reason.
    pub(crate) modal: bool,
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
            modal: false,
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
            modal: false,
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

#[cfg(unix)]
pub(super) fn idle_stash_stopped(
    session: &str,
    error: SubmissionError,
    modal: bool,
) -> SubmissionError {
    use crate::services::tui_input::submission::Refusal;
    use host_input::{InputRefusal, InputRun};
    match error {
        SubmissionError::Guarded(InputRun::Refused(InputRefusal::Composer(
            Refusal::NotReady | Refusal::CaptureUnavailable | Refusal::WriterContended,
        ))) => SubmissionError::Held(PreparationHold {
            target: session.into(),
            reason: "composer_not_ready",
            phase: EffectPhase::BeforePayload,
            detail: "claude tui idle preparation held before payload".into(),
            modal,
        }),
        other => other,
    }
}

#[cfg(test)]
#[cfg(unix)]
mod idle_injection_tests {
    use super::*;
    use crate::services::claude_tui::host_input::{InputRefusal, InputRun, StopCause};
    use crate::services::tui_input::submission::Refusal;

    #[test]
    fn idle_menu_requeues_without_locking_and_write_failures_do_not_lock() {
        let session = format!("idle-preparation-{}", uuid::Uuid::new_v4());
        for refusal in [Refusal::NotReady, Refusal::WriterContended] {
            let held = idle_stash_stopped(
                &session,
                SubmissionError::Guarded(InputRun::Refused(InputRefusal::Composer(refusal))),
                false,
            );
            assert!(matches!(held, SubmissionError::Held(ref hold) if hold.may_requeue()));
            assert!(!crate::services::claude_tui::composer_lock::draft_guarded(
                &session
            ));
        }
        let failed = idle_stash_stopped(
            &session,
            SubmissionError::Guarded(InputRun::Indeterminate {
                confirmed: 0,
                cause: StopCause::Send("tmux write failed".into()),
            }),
            false,
        );
        assert!(matches!(
            failed,
            SubmissionError::Guarded(InputRun::Indeterminate { .. })
        ));
        assert!(!crate::services::claude_tui::composer_lock::draft_guarded(
            &session
        ));
    }
}
