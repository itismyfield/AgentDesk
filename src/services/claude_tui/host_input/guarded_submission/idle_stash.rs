//! Dormant queue-authorized stash adapter; the production capability arrives with queue wiring.

use super::*;
use crate::services::claude_tui::busy_inject::{
    self, Delivery, DraftState, Outcome, Report, Timing, Unconfirmed, Veto, screen,
    stash::{self, StashEffects},
};
use crate::services::tui_input::submission_tmux::guarded::{GuardedObserver, GuardedPane};
use crate::services::tui_turn_state::{TuiTurnState, observe_claude_jsonl_turn_state};
use std::path::Path;
use std::time::Instant;

mod receipt;

// The adapter never derives queue authority from a channel or a candidate capture.
trait QueueAuthority {
    fn admits(&self, pane: &GuardedPane) -> bool;
}

struct Request<'a> {
    target: &'a InputTarget,
    gate: &'a dyn MutationGate,
    prompt: &'a str,
    transcript: &'a Path,
    cancel: Option<&'a CancelToken>,
    timing: &'a Timing,
}

struct IdleStashCandidate(Vec<String>);

#[derive(Debug, PartialEq, Eq)]
struct IdleReport {
    delivery: Delivery,
    draft: DraftState,
}

fn stopped(draft: DraftState) -> IdleReport {
    IdleReport {
        delivery: Delivery::NotAttempted,
        draft,
    }
}

fn run(
    authority: Option<&dyn QueueAuthority>,
    allowed: bool,
    request: &Request<'_>,
) -> Option<IdleReport> {
    let authority = authority.filter(|_| allowed)?;
    let Ok(session) = request.target.tmux_session() else {
        return Some(stopped(DraftState::Unchanged));
    };
    let Ok(actions) = input::plan_prompt_submit(request.prompt) else {
        return Some(stopped(DraftState::Unchanged));
    };
    let Some((input::TuiInputAction::Enter, payload)) = actions.split_last() else {
        return Some(stopped(DraftState::Unchanged));
    };
    let Ok(text) = payload_frame(payload) else {
        return Some(stopped(DraftState::Unchanged));
    };
    let result = composer_lock::try_with_composer_permit(session, |permit| {
        let Ok(pane) = SubmissionTmux.pin_guarded(session) else {
            return stopped(DraftState::Unchanged);
        };
        let Some(before) = pane.capture_draft() else {
            return stopped(DraftState::Unchanged);
        };
        let admission = composer_lock::composer_admission(session, || Some(before.clone()));
        if admission == composer_lock::ComposerAdmission::Held
            || busy_inject::composer_owner(&before) != busy_inject::ComposerOwner::Person
            || judge_pane(ShadowProvider::Claude, &before) == PaneVerdict::Modal
            || observe_claude_jsonl_turn_state(request.transcript) != TuiTurnState::Idle
        {
            return stopped(DraftState::Unchanged);
        }
        let Some(draft) = screen::stashable(&before, &text, Some(pane.dimensions())) else {
            return stopped(DraftState::Unchanged);
        };
        let candidate = IdleStashCandidate(draft);
        let Some(receipt) = receipt::Receipt::before(request.transcript) else {
            return stopped(DraftState::Unchanged);
        };
        let mut effects = Effects {
            pane: Some(pane),
            observer: None,
            authority,
            request,
            text: &text,
            receipt,
            started: false,
            permit: &permit,
        };
        if !effects.generation_unchanged() || submission_fence::before_first_payload().is_err() {
            return stopped(DraftState::Unchanged);
        }
        crate::services::tui_prompt_dedupe::record_discord_originated_prompt(
            "claude",
            session,
            request.prompt,
        );
        let mut last = None;
        let report = stash::transact(&mut effects, &candidate.0, &mut last);
        match report.draft {
            DraftState::Unchanged => {}
            DraftState::RestoredObserved => {
                composer_lock::guard_draft(session, composer_lock::DraftGuard::DraftRestored)
            }
            DraftState::StashedVerified | DraftState::Unknown => {
                composer_lock::guard_draft(session, composer_lock::DraftGuard::RecoveryRequired)
            }
        }
        IdleReport {
            delivery: if report.outcome == Outcome::Injected {
                Delivery::Observed
            } else if effects.started {
                Delivery::AttemptedUnconfirmed
            } else {
                Delivery::NotAttempted
            },
            draft: report.draft,
        }
    });
    Some(result.unwrap_or_else(|| stopped(DraftState::Unchanged)))
}

struct Effects<'a, 'p> {
    pane: Option<GuardedPane>,
    observer: Option<GuardedObserver>,
    authority: &'a dyn QueueAuthority,
    request: &'a Request<'a>,
    text: &'a str,
    receipt: receipt::Receipt,
    started: bool,
    permit: &'a composer_lock::ComposerPermit<'p>,
}

fn failed(outcome: Outcome, draft: DraftState) -> Report {
    Report { outcome, draft }
}

impl StashEffects for Effects<'_, '_> {
    fn timing(&self) -> &Timing {
        self.request.timing
    }
    fn text(&self) -> &str {
        self.text
    }
    fn stash(&mut self) -> Result<(), Report> {
        if !self.generation_unchanged() {
            return Err(failed(
                Outcome::NotSent(Veto::HumanAttached),
                DraftState::Unchanged,
            ));
        }
        let effect = self.pane.as_mut().unwrap().stash();
        self.started = effect != EffectOutcome::Held;
        match effect {
            EffectOutcome::Applied => Ok(()),
            EffectOutcome::Held => Err(failed(
                Outcome::NotSent(Veto::HumanAttached),
                DraftState::Unchanged,
            )),
            EffectOutcome::Unknown(_) => Err(failed(
                Outcome::Unconfirmed(Unconfirmed::NotObserved),
                DraftState::Unknown,
            )),
        }
    }
    fn paste(&mut self) -> Result<(), Report> {
        if !self.generation_unchanged() {
            return Err(failed(
                Outcome::Unconfirmed(Unconfirmed::AttachedAfterPaste),
                DraftState::StashedVerified,
            ));
        }
        match self
            .pane
            .as_ref()
            .unwrap()
            .payload(&input::TuiInputAction::PasteBuffer(self.text.into()))
        {
            EffectOutcome::Applied => Ok(()),
            EffectOutcome::Held => Err(failed(
                Outcome::Unconfirmed(Unconfirmed::AttachedAfterPaste),
                DraftState::StashedVerified,
            )),
            EffectOutcome::Unknown(_) => Err(failed(
                Outcome::Unconfirmed(Unconfirmed::PasteFailed),
                DraftState::Unknown,
            )),
        }
    }
    fn drop_buffer(&self) {}
    fn generation_unchanged(&self) -> bool {
        !cancel_requested(self.request.cancel)
            && self.pane.as_ref().is_some_and(|pane| {
                pane.session_matches(self.permit.session())
                    && self.authority.admits(pane)
                    && self.request.gate.admit(self.permit.session()).is_ok()
                    && pane.geometry_matches()
                    && !cancel_requested(self.request.cancel)
            })
    }
    fn capture(&self) -> Option<String> {
        self.pane.as_ref().map_or_else(
            || self.observer.as_ref()?.capture_draft(),
            GuardedPane::capture_draft,
        )
    }
    fn await_own(&mut self, last: &mut Option<String>) -> Result<(), Unconfirmed> {
        std::thread::sleep(self.timing().settle);
        for check in 0..self.timing().rechecks {
            if check > 0 {
                std::thread::sleep(self.timing().recheck_interval);
            }
            if !self.generation_unchanged() {
                return Err(Unconfirmed::AttachedAfterPaste);
            }
            let after = self.capture().ok_or(Unconfirmed::CaptureFailed)?;
            let owned = screen::owns(&after, self.text());
            let modal = judge_pane(ShadowProvider::Claude, &after) == PaneVerdict::Modal;
            *last = Some(after);
            if modal {
                return Err(Unconfirmed::ModalAfterPaste);
            }
            if owned {
                return Ok(());
            }
        }
        Err(Unconfirmed::DraftNotOwned)
    }
    fn enter(&mut self) -> Outcome {
        if !self.generation_unchanged() {
            return Outcome::Unconfirmed(Unconfirmed::AttachedAfterPaste);
        }
        if !self.receipt.unchanged() {
            return Outcome::Unconfirmed(Unconfirmed::CursorChangedBeforeEnter);
        }
        let deadline = Instant::now() + self.timing().confirm_window;
        let pane = self.pane.take().unwrap();
        self.observer = Some(pane.observer());
        match pane.enter() {
            EffectOutcome::Applied => {}
            EffectOutcome::Held => return Outcome::Unconfirmed(Unconfirmed::AttachedAfterPaste),
            EffectOutcome::Unknown(_) => return Outcome::Unconfirmed(Unconfirmed::EnterFailed),
        }
        loop {
            if Instant::now() >= deadline {
                return Outcome::Unconfirmed(Unconfirmed::NotObserved);
            }
            if let Some(exact) = self.receipt.scan(self.text)
                && Instant::now() < deadline
            {
                return if exact {
                    Outcome::Injected
                } else {
                    Outcome::Unconfirmed(Unconfirmed::ContaminatedSubmit)
                };
            }
            std::thread::sleep(
                self.timing()
                    .confirm_poll
                    .min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
}

#[cfg(test)]
#[path = "idle_stash/idle_stash_tests.rs"]
mod tests;
