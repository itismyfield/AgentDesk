//! Boot-selected Claude executor: a locked PRE precedes the durable fence and one submission.

#[cfg(test)]
use super::LegacyTmuxGate;
use super::{InputRefusal, InputRun, InputTarget, MutationGate, StopCause};
use crate::services::claude_tui::{composer_lock, guarded_outcome, input, submission_fence};
use crate::services::provider::{CancelToken, cancel_requested};
use crate::services::tui_input::actor::gate::claude_guarded::{
    DraftEvidence, claude_should_fold, own_claude_guarded_draft,
};
use crate::services::tui_input::actor::gate::{PaneVerdict, judge_pane, submission_busy};
use crate::services::tui_input::actor::pane::MAX_PROMPT_BYTES;
use crate::services::tui_input::submission::{Refusal, SubmitContext};
use crate::services::tui_input::submission_tmux::{SubmissionTmux, guarded::EffectOutcome};
use crate::services::tui_o::shadow::ShadowProvider;
use std::rc::Rc;

pub(crate) fn folded_payload(frame: &str) -> Option<[input::TuiInputAction; 1]> {
    (!frame.contains('\n') && claude_should_fold(frame))
        .then(|| [input::TuiInputAction::PasteBuffer(frame.to_string())])
}

fn refused(reason: Refusal) -> InputRun {
    InputRun::Refused(InputRefusal::Composer(reason))
}

fn hold(confirmed: usize, reason: Refusal) -> InputRun {
    InputRun::Indeterminate {
        confirmed,
        cause: StopCause::Refused(InputRefusal::Composer(reason)),
    }
}

fn unknown(confirmed: usize, reason: String) -> InputRun {
    InputRun::Indeterminate {
        confirmed,
        cause: StopCause::Send(reason),
    }
}

pub(crate) fn run(
    target: &InputTarget,
    gate: &dyn MutationGate,
    prompt: &str,
    cancel: Option<&CancelToken>,
) -> InputRun {
    run_with_context(target, gate, prompt, cancel, SubmitContext::Idle)
}

fn run_with_context(
    target: &InputTarget,
    gate: &dyn MutationGate,
    prompt: &str,
    cancel: Option<&CancelToken>,
    context: SubmitContext,
) -> InputRun {
    let session = match target.tmux_session() {
        Ok(s) => s,
        Err(r) => return InputRun::Refused(r),
    };
    if let Some(held) = guarded_outcome::held(session) {
        return held;
    }
    let actions = match input::plan_prompt_submit(prompt) {
        Ok(a) => a,
        Err(_) => return refused(Refusal::InvalidPrompt),
    };
    let Some((input::TuiInputAction::Enter, payload)) = actions.split_last() else {
        return refused(Refusal::InvalidPrompt);
    };
    let frame = match payload_frame(payload) {
        Ok(frame) => frame,
        Err(reason) => return refused(reason),
    };
    if frame.len() > MAX_PROMPT_BYTES {
        return refused(Refusal::InvalidPrompt);
    }
    if cancel_requested(cancel) {
        return InputRun::Cancelled { confirmed: 0 };
    }
    let folded = folded_payload(&frame);
    let payload = folded.as_ref().map_or(payload, |p| &p[..]);
    composer_lock::try_with_composer_permit(session, |permit| {
        let stopped = (|| {
            if let Err(r) = gate.admit(session) {
                return InputRun::Refused(r);
            }
            let pane = match SubmissionTmux.pin_guarded(session) {
                Ok(p) => p,
                Err(r) => return InputRun::Refused(r),
            };
            if composer_lock::admit_composer_write(session, || pane.capture_draft()).is_err() {
                return refused(Refusal::NotReady);
            }
            let evidence =
                match DraftEvidence::observe(&frame, payload, &permit, &pane, gate, context) {
                    Ok(e) => e,
                    Err(r) => return InputRun::Refused(r),
                };
            if cancel_requested(cancel) {
                return InputRun::Cancelled { confirmed: 0 };
            }
            if let Err(r) = gate.admit(session) {
                return InputRun::Refused(r);
            }
            if !pane.geometry_matches() {
                return refused(Refusal::GeometryChanged);
            }
            if cancel_requested(cancel) {
                return InputRun::Cancelled { confirmed: 0 };
            }
            if submission_fence::before_first_payload().is_err() {
                return refused(Refusal::SubmissionFence);
            }
            crate::services::tui_prompt_dedupe::record_discord_originated_prompt(
                "claude", session, prompt,
            );
            let mut confirmed = 0;
            for action in payload {
                if cancel_requested(cancel) {
                    return unknown(confirmed, input::PROMPT_READY_CANCELLED_ERROR.into());
                }
                if let Err(r) = gate.admit(session) {
                    return InputRun::Indeterminate {
                        confirmed,
                        cause: StopCause::Refused(r),
                    };
                }
                if cancel_requested(cancel) {
                    return unknown(confirmed, input::PROMPT_READY_CANCELLED_ERROR.into());
                }
                match pane.payload(action) {
                    EffectOutcome::Applied => confirmed += 1,
                    EffectOutcome::Held => return hold(confirmed, Refusal::GeometryChanged),
                    EffectOutcome::Unknown(e) => return unknown(confirmed, e),
                }
            }
            std::thread::sleep(
                if matches!(payload.last(), Some(input::TuiInputAction::PasteBuffer(_))) {
                    input::POST_PASTE_BUFFER_SETTLE
                } else {
                    input::POST_LITERAL_SETTLE
                },
            );
            if cancel_requested(cancel) {
                return unknown(confirmed, input::PROMPT_READY_CANCELLED_ERROR.into());
            }
            let receipt = (context == SubmitContext::ClaudeSteering)
                .then(|| super::SteeringReceipt::before(session))
                .flatten();
            let Some(after) = pane.capture_draft() else {
                return hold(confirmed, Refusal::CaptureUnavailable);
            };
            let plain = crate::services::codex_tui::input::strip_ansi_escape_sequences(&after);
            if (submission_busy(ShadowProvider::Claude, &plain)
                && !context.permits_busy(ShadowProvider::Claude))
                || !own_claude_guarded_draft(&after, evidence)
            {
                return hold(confirmed, Refusal::OwnDraft);
            }
            if cancel_requested(cancel) {
                return unknown(confirmed, input::PROMPT_READY_CANCELLED_ERROR.into());
            }
            if let Err(r) = gate.admit(session) {
                return InputRun::Indeterminate {
                    confirmed,
                    cause: StopCause::Refused(r),
                };
            }
            if cancel_requested(cancel) {
                return unknown(confirmed, input::PROMPT_READY_CANCELLED_ERROR.into());
            }
            let observer = pane.observer();
            match pane.enter() {
                EffectOutcome::Applied => confirmed += 1,
                EffectOutcome::Held => return hold(confirmed, Refusal::GeometryChanged),
                EffectOutcome::Unknown(e) => return unknown(confirmed, e),
            }
            std::thread::sleep(input::POST_LITERAL_SETTLE);
            if cancel_requested(cancel) {
                return unknown(confirmed, input::PROMPT_READY_CANCELLED_ERROR.into());
            }
            let Some(after) = observer.capture_draft() else {
                return hold(confirmed, Refusal::CaptureUnavailable);
            };
            if cancel_requested(cancel) {
                return unknown(confirmed, input::PROMPT_READY_CANCELLED_ERROR.into());
            }
            let accepted = if context == SubmitContext::ClaudeSteering {
                crate::services::tui_input::actor::gate::submission_admits(
                    ShadowProvider::Claude,
                    &after,
                    context,
                ) && receipt.is_some_and(|receipt| receipt.confirmed(session, &frame))
            } else {
                passive_ready(&after)
            };
            if !accepted {
                return hold(confirmed, Refusal::Confirmation);
            }
            InputRun::Applied
        })();
        guarded_outcome::retain(session, &stopped);
        stopped
    })
    .unwrap_or_else(|| refused(Refusal::WriterContended))
}

fn payload_frame(payload: &[input::TuiInputAction]) -> Result<String, Refusal> {
    payload
        .iter()
        .map(|action| match action {
            input::TuiInputAction::Literal(text) | input::TuiInputAction::PasteBuffer(text) => {
                Ok(text.as_str())
            }
            _ => Err(Refusal::InvalidPrompt),
        })
        .collect()
}

fn passive_ready(capture: &str) -> bool {
    let plain = crate::services::codex_tui::input::strip_ansi_escape_sequences(capture);
    judge_pane(ShadowProvider::Claude, capture) != PaneVerdict::Modal
        && crate::services::tui_input::actor::gate::claude_composer_empty_when_present(capture)
        && !crate::services::tmux_common::tmux_capture_indicates_claude_tui_prompt_draft(&plain)
        && (judge_pane(ShadowProvider::Claude, capture) == PaneVerdict::Ready
            || crate::services::tmux_common::tmux_capture_indicates_claude_tui_busy(&plain))
}

pub(crate) struct Request {
    pub(crate) target: InputTarget,
    pub(crate) gate: Rc<dyn MutationGate>,
    pub(crate) steering: bool,
}

fn request(session: &str) -> Option<Rc<Request>> {
    #[cfg(test)]
    if let Some(request) = test_support::request(session) {
        return Some(request);
    }
    crate::services::claude_tui::submit_canary::request(session)
}

pub(crate) fn selection_refusal(session: &str) -> Option<InputRun> {
    let request = request(session)?;
    match request.target.tmux_session() {
        Ok(target) if target == session => {
            if crate::services::claude_tui::submit_canary::enabled() {
                request.gate.admit(session).err().map(InputRun::Refused)
            } else {
                None
            }
        }
        Ok(_) => Some(InputRun::Refused(InputRefusal::IdentityMismatch)),
        Err(refusal) => Some(InputRun::Refused(refusal)),
    }
}

pub(crate) fn capture_request(
    session: &str,
) -> Result<Option<Rc<Request>>, guarded_outcome::SubmissionError> {
    if let Some(run) = guarded_outcome::held(session) {
        return Err(guarded_outcome::SubmissionError::Guarded(run));
    }
    let request = request(session);
    if let Some(request) = &request {
        let target = request
            .target
            .tmux_session()
            .map_err(crate::services::claude_tui::submit_canary::stopped)?;
        if target != session {
            return Err(crate::services::claude_tui::submit_canary::stopped(
                InputRefusal::IdentityMismatch,
            ));
        }
    }
    Ok(request)
}

pub(crate) fn readiness_error(
    session: &str,
    error: String,
    request: &Option<Rc<Request>>,
) -> guarded_outcome::SubmissionError {
    if request.is_none() {
        return guarded_outcome::readiness_error(session, error);
    }
    let run = guarded_outcome::held(session).unwrap_or_else(|| {
        if input::is_prompt_ready_cancelled_error(&error) {
            InputRun::Cancelled { confirmed: 0 }
        } else {
            refused(Refusal::NotReady)
        }
    });
    guarded_outcome::finish(&run);
    guarded_outcome::SubmissionError::Guarded(run)
}

pub(crate) fn selected(
    session: &str,
    prompt: &str,
    cancel: Option<&CancelToken>,
    request: Option<Rc<Request>>,
) -> Option<InputRun> {
    selected_with_context(session, prompt, cancel, SubmitContext::Idle, request?)
}

fn selected_with_context(
    session: &str,
    prompt: &str,
    cancel: Option<&CancelToken>,
    context: SubmitContext,
    request: Rc<Request>,
) -> Option<InputRun> {
    if request.target.tmux_session().ok() != Some(session) {
        return Some(InputRun::Refused(InputRefusal::IdentityMismatch));
    }
    guarded_outcome::begin();
    let run = match context {
        SubmitContext::Idle => run(&request.target, &*request.gate, prompt, cancel),
        SubmitContext::ClaudeSteering => {
            run_with_context(&request.target, &*request.gate, prompt, cancel, context)
        }
    };
    guarded_outcome::finish(&run);
    #[cfg(test)]
    test_support::remember(&run);
    Some(run)
}

pub(crate) fn steering_selected(session: &str, prompt: &str) -> Option<InputRun> {
    let request = request(session)?;
    if request.steering {
        return selected_with_context(
            session,
            prompt,
            None,
            SubmitContext::ClaudeSteering,
            request,
        );
    }
    let run = refused(Refusal::NotReady);
    guarded_outcome::finish(&run);
    #[cfg(test)]
    test_support::remember(&run);
    Some(run)
}

#[cfg(test)]
pub(crate) fn confirm_prompt_submission_passively(
    session: &str,
    cancel: Option<&CancelToken>,
) -> Result<(), String> {
    if cancel_requested(cancel) {
        return Err(input::PROMPT_READY_CANCELLED_ERROR.into());
    }
    let capture = super::with_transport(|transport| {
        transport
            .capture_draft(session)
            .filter(|_| transport.pane_alive(session))
    });
    if cancel_requested(cancel) {
        return Err(input::PROMPT_READY_CANCELLED_ERROR.into());
    }
    if capture.as_deref().is_some_and(passive_ready) {
        Ok(())
    } else {
        Err("claude tui input held after mutation: confirmation ambiguous".into())
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::cell::RefCell;
    thread_local! {
        static REQUEST: RefCell<Option<Rc<Request>>> = const { RefCell::new(None) };
        static RESULT: RefCell<Option<InputRun>> = const { RefCell::new(None) };
    }
    pub(super) fn request(_session: &str) -> Option<Rc<Request>> {
        REQUEST.with(|r| r.borrow().as_ref().cloned())
    }
    pub(super) fn remember(run: &InputRun) {
        RESULT.with(|r| *r.borrow_mut() = Some(run.clone()));
    }
    pub(crate) fn with_request<R>(
        session: &str,
        gate: Rc<dyn MutationGate>,
        run: impl FnOnce() -> R,
    ) -> (R, Option<InputRun>) {
        with_request_mode(session, gate, false, run)
    }
    #[cfg(unix)]
    pub(crate) fn with_steering_request<R>(
        session: &str,
        gate: Rc<dyn MutationGate>,
        run: impl FnOnce() -> R,
    ) -> (R, Option<InputRun>) {
        with_request_mode(session, gate, true, run)
    }
    fn with_request_mode<R>(
        session: &str,
        gate: Rc<dyn MutationGate>,
        steering: bool,
        run: impl FnOnce() -> R,
    ) -> (R, Option<InputRun>) {
        struct Restore(Option<Rc<Request>>, Option<InputRun>);
        impl Drop for Restore {
            fn drop(&mut self) {
                REQUEST.with(|r| *r.borrow_mut() = self.0.take());
                RESULT.with(|r| *r.borrow_mut() = self.1.take());
            }
        }
        let old = REQUEST.with(|r| {
            r.replace(Some(Rc::new(Request {
                target: InputTarget::legacy_tmux(session),
                gate,
                steering,
            })))
        });
        let result = RESULT.with(|r| r.replace(None));
        let _restore = Restore(old, result);
        let output = run();
        (output, RESULT.with(|r| r.borrow().clone()))
    }
}

#[cfg(test)]
pub(crate) mod executor_tests;

#[cfg(unix)]
pub(crate) mod idle_stash;
