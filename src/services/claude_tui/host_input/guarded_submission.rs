//! Dormant Claude executor: a locked PRE precedes the durable fence and one passive submission.

#[cfg(test)]
use super::LegacyTmuxGate;
use super::{InputRefusal, InputRun, InputTarget, MutationGate, StopCause};
use crate::services::claude_tui::{composer_lock, input, submission_fence};
use crate::services::provider::{CancelToken, cancel_requested};
use crate::services::tui_input::actor::gate::claude_guarded::{
    DraftEvidence, claude_should_fold, own_claude_guarded_draft,
};
use crate::services::tui_input::actor::gate::{PaneVerdict, judge_pane, submission_busy};
use crate::services::tui_input::actor::pane::MAX_PROMPT_BYTES;
use crate::services::tui_input::submission::Refusal;
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
    let session = match target.tmux_session() {
        Ok(s) => s,
        Err(r) => return InputRun::Refused(r),
    };
    let actions = match input::plan_prompt_submit(prompt) {
        Ok(a) => a,
        Err(_) => return refused(Refusal::InvalidPrompt),
    };
    let Some((input::TuiInputAction::Enter, payload)) = actions.split_last() else {
        return refused(Refusal::InvalidPrompt);
    };
    let frame: String = payload
        .iter()
        .filter_map(|a| match a {
            input::TuiInputAction::Literal(s) | input::TuiInputAction::PasteBuffer(s) => {
                Some(s.as_str())
            }
            _ => None,
        })
        .collect();
    if frame.len() > MAX_PROMPT_BYTES {
        return refused(Refusal::InvalidPrompt);
    }
    if cancel_requested(cancel) {
        return InputRun::Cancelled { confirmed: 0 };
    }
    let folded = folded_payload(&frame);
    let payload = folded.as_ref().map_or(payload, |p| &p[..]);
    composer_lock::try_with_composer_permit(session, |permit| {
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
        let evidence = match DraftEvidence::observe(&frame, payload, &permit, &pane, gate) {
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
        let Some(after) = pane.capture_draft() else {
            return hold(confirmed, Refusal::CaptureUnavailable);
        };
        let plain = crate::services::codex_tui::input::strip_ansi_escape_sequences(&after);
        if submission_busy(ShadowProvider::Claude, &plain)
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
        if !passive_ready(&after) {
            return hold(confirmed, Refusal::Confirmation);
        }
        InputRun::Applied
    })
    .unwrap_or_else(|| refused(Refusal::WriterContended))
}

fn passive_ready(capture: &str) -> bool {
    let plain = crate::services::codex_tui::input::strip_ansi_escape_sequences(capture);
    judge_pane(ShadowProvider::Claude, capture) != PaneVerdict::Modal
        && crate::services::tui_input::actor::gate::claude_composer_empty_when_present(capture)
        && !crate::services::tmux_common::tmux_capture_indicates_claude_tui_prompt_draft(&plain)
        && (judge_pane(ShadowProvider::Claude, capture) == PaneVerdict::Ready
            || crate::services::tmux_common::tmux_capture_indicates_claude_tui_busy(&plain))
}

struct Request {
    target: InputTarget,
    gate: Rc<dyn MutationGate>,
}

fn request(_session: &str) -> Option<Rc<Request>> {
    #[cfg(test)]
    {
        test_support::request(_session)
    }
    #[cfg(not(test))]
    {
        None
    }
}

pub(crate) fn selected(
    session: &str,
    prompt: &str,
    cancel: Option<&CancelToken>,
) -> Option<InputRun> {
    let request = request(session)?;
    let run = run(&request.target, &*request.gate, prompt, cancel);
    #[cfg(test)]
    test_support::remember(&run);
    Some(run)
}

pub(crate) fn steering_selected(session: &str) -> Option<InputRun> {
    request(session)?;
    let run = refused(Refusal::NotReady);
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
    pub(super) fn request(session: &str) -> Option<Rc<Request>> {
        REQUEST.with(|r| {
            r.borrow()
                .as_ref()
                .filter(|r| r.target.tmux_session().ok() == Some(session))
                .cloned()
        })
    }
    pub(super) fn remember(run: &InputRun) {
        RESULT.with(|r| *r.borrow_mut() = Some(run.clone()));
    }
    pub(crate) fn with_request<R>(
        session: &str,
        gate: Rc<dyn MutationGate>,
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
            })))
        });
        let result = RESULT.with(|r| r.replace(None));
        let _restore = Restore(old, result);
        let output = run();
        (output, RESULT.with(|r| r.borrow().clone()))
    }
}

#[cfg(test)]
mod executor_tests;
