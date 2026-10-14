//! A managed idle attempt parks a flat person draft, submits once, and watches it return.

use super::*;
use crate::services::claude_tui::busy_inject::{
    self, Delivery, DraftState, Outcome, Report, Timing, Unconfirmed, Veto, screen,
    stash::{self, StashEffects},
};
use crate::services::claude_tui::host_input::LegacyTmuxGate;
use crate::services::tui_input::submission_tmux::guarded::{GuardedObserver, GuardedPane};
use crate::services::tui_turn_state::{TuiTurnState, observe_claude_jsonl_turn_state};
use std::path::Path;
use std::time::Instant;

mod receipt;

// A channel selects policy; only the managed attempt can authorize a mutation.
trait StashAuthority {
    fn admits(&self, pane: &GuardedPane) -> bool;
    fn claim(&self, _pane: &GuardedPane) -> Result<(), String> {
        submission_fence::before_first_payload()
    }
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
    entered: bool,
    retryable: bool,
    refusal: Option<InputRefusal>,
}

fn stopped(draft: DraftState) -> IdleReport {
    IdleReport {
        delivery: Delivery::NotAttempted,
        draft,
        entered: false,
        retryable: false,
        refusal: None,
    }
}

fn run(
    authority: Option<&dyn StashAuthority>,
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
            return Some(stopped(DraftState::Unchanged));
        };
        let Some(before) = pane.capture_draft() else {
            return Some(stopped(DraftState::Unchanged));
        };
        if !authority.admits(&pane)
            || request.gate.admit(session).is_err()
            || cancel_requested(request.cancel)
        {
            let mut held = stopped(DraftState::Unchanged);
            held.refusal = Some(InputRefusal::IdentityMismatch);
            return Some(held);
        }
        if observe_claude_jsonl_turn_state(request.transcript).is_busy() {
            return None;
        }
        let admission = composer_lock::composer_admission(session, || Some(before.clone()));
        if admission == composer_lock::ComposerAdmission::Any
            && busy_inject::composer_owner(&before) == busy_inject::ComposerOwner::Empty
        {
            return None;
        }
        if admission == composer_lock::ComposerAdmission::Held
            || busy_inject::composer_owner(&before) != busy_inject::ComposerOwner::Person
            || judge_pane(ShadowProvider::Claude, &before) == PaneVerdict::Modal
            || observe_claude_jsonl_turn_state(request.transcript) != TuiTurnState::Idle
        {
            return Some(stopped(DraftState::Unchanged));
        }
        let Some(draft) = screen::stashable(&before, &text, Some(pane.dimensions())) else {
            composer_lock::guard_draft(session, composer_lock::DraftGuard::DraftRestored);
            let mut held = stopped(DraftState::Unchanged);
            held.retryable = true;
            return Some(held);
        };
        let candidate = IdleStashCandidate(draft);
        let Some(receipt) = receipt::Receipt::before(request.transcript) else {
            return Some(stopped(DraftState::Unchanged));
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
            draft: &candidate.0,
        };
        if !effects.generation_unchanged() {
            return Some(stopped(DraftState::Unchanged));
        }
        guarded_outcome::begin();
        if authority.claim(effects.pane.as_ref().unwrap()).is_err() {
            return Some(stopped(DraftState::Unchanged));
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
        let result = IdleReport {
            delivery: if report.outcome == Outcome::Injected {
                Delivery::Observed
            } else if effects.started {
                Delivery::AttemptedUnconfirmed
            } else {
                Delivery::NotAttempted
            },
            draft: report.draft,
            entered: true,
            retryable: false,
            refusal: None,
        };
        guarded_outcome::retain(session, &result.input_run(request.cancel));
        Some(result)
    });
    result.unwrap_or_else(|| Some(stopped(DraftState::Unchanged)))
}

struct Effects<'a, 'p> {
    pane: Option<GuardedPane>,
    observer: Option<GuardedObserver>,
    authority: &'a dyn StashAuthority,
    request: &'a Request<'a>,
    text: &'a str,
    receipt: receipt::Receipt,
    started: bool,
    permit: &'a composer_lock::ComposerPermit<'p>,
    draft: &'a [String],
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
        let fresh = self.capture().and_then(|capture| {
            screen::stashable(
                &capture,
                self.text,
                Some(self.pane.as_ref().unwrap().dimensions()),
            )
        });
        if fresh.as_deref() != Some(self.draft) {
            return Err(failed(Outcome::NotSent(Veto::Draft), DraftState::Unchanged));
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
        if !self.capture().is_some_and(|capture| {
            screen::stashed(&capture)
                && judge_pane(ShadowProvider::Claude, &capture) != PaneVerdict::Modal
        }) {
            return Err(failed(
                Outcome::Unconfirmed(Unconfirmed::DraftNotOwned),
                DraftState::Unknown,
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
        if !self.capture().is_some_and(|capture| {
            screen::owns(&capture, self.text)
                && judge_pane(ShadowProvider::Claude, &capture) != PaneVerdict::Modal
        }) {
            return Outcome::Unconfirmed(Unconfirmed::DraftNotOwned);
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

impl IdleReport {
    fn input_run(&self, cancel: Option<&CancelToken>) -> InputRun {
        if self.entered {
            if self.delivery == Delivery::Observed {
                InputRun::Applied
            } else {
                InputRun::Indeterminate {
                    confirmed: 0,
                    cause: StopCause::Send("managed idle stash delivery unconfirmed".into()),
                }
            }
        } else if cancel_requested(cancel) {
            InputRun::Cancelled { confirmed: 0 }
        } else {
            InputRun::Refused(
                self.refusal
                    .clone()
                    .unwrap_or(InputRefusal::Composer(Refusal::NotReady)),
            )
        }
    }
}

struct ManagedAuthority {
    fence: std::sync::Arc<dyn submission_fence::SubmissionFence>,
    session: String,
    pin: std::cell::RefCell<
        Option<crate::services::tui_input::submission_tmux::guarded::PaneIdentity>,
    >,
}
impl StashAuthority for ManagedAuthority {
    fn admits(&self, pane: &GuardedPane) -> bool {
        pane.session_matches(&self.session)
            && self.fence.idle_stash_admits(&self.session)
            && self
                .pin
                .borrow()
                .as_ref()
                .is_none_or(|pin| *pin == pane.identity())
    }
    fn claim(&self, pane: &GuardedPane) -> Result<(), String> {
        self.fence
            .claim_idle_stash(&self.session)
            .map_err(|error| error.to_string())?;
        *self.pin.borrow_mut() = Some(pane.identity());
        Ok(())
    }
}

pub(crate) fn managed_enabled(session: &str) -> bool {
    submission_fence::idle_stash_scope().is_some_and(|(_, context)| {
        context.available
            && context.target == session
            && busy_inject::stash_enabled(Some(context.channel))
    })
}

pub(crate) fn submit(
    session: &str,
    prompt: &str,
    cancel: Option<&CancelToken>,
    transcript: Option<&Path>,
    selected: Option<&super::Request>,
) -> Option<Result<(), guarded_outcome::SubmissionError>> {
    let (fence, context) = submission_fence::idle_stash_scope()?;
    if !busy_inject::stash_enabled(Some(context.channel)) {
        return None;
    }
    if !context.available || context.target != session {
        let held = InputRun::Refused(InputRefusal::IdentityMismatch);
        guarded_outcome::finish(&held);
        return Some(guarded_outcome::SubmissionError::from_run(held));
    }
    let path = transcript
        .map(Path::to_path_buf)
        .or_else(|| {
            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(session)
                .filter(|binding| {
                    binding.runtime_kind
                        == crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
                })
                .map(|binding| binding.output_path.into())
        })
        .unwrap_or_default();
    let target = InputTarget::legacy_tmux(session);
    let (target, gate): (&InputTarget, &dyn MutationGate) = selected
        .map_or((&target, &LegacyTmuxGate as &dyn MutationGate), |request| {
            (&request.target, &*request.gate)
        });
    let authority = ManagedAuthority {
        fence,
        session: session.into(),
        pin: Default::default(),
    };
    let request = Request {
        target,
        gate,
        prompt,
        transcript: &path,
        cancel,
        timing: &busy_inject::TIMING,
    };
    let report = run(Some(&authority), true, &request)?;
    if !report.entered && report.retryable {
        return Some(Err(guarded_outcome::SubmissionError::Legacy(
            "timeout waiting for claude tui follow-up prompt input readiness held; reason=draft_recovery_hold; previous_tui_turn_still_running=false; prompt_marker_detected=true".into())));
    }
    let result = report.input_run(cancel);
    guarded_outcome::finish(&result);
    Some(guarded_outcome::SubmissionError::from_run(result))
}

// A watcher can suggest a wake; it neither claims an attempt nor removes draft protection.
pub(crate) fn wake_hint(session: &str) -> Option<String> {
    wake_hint_observed(session).ok().flatten()
}

/// Failed observation cannot authorize skipping a capped head; confirmed non-targets can.
pub(crate) fn wake_hint_observed(session: &str) -> Result<Option<String>, ()> {
    let Some(binding) =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(session)
    else {
        return Ok(None);
    };
    let path = std::path::PathBuf::from(binding.output_path);
    if binding.runtime_kind != crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui {
        return Ok(None);
    }
    match observe_claude_jsonl_turn_state(&path) {
        TuiTurnState::Idle => {}
        TuiTurnState::Unknown => return Err(()),
        _ => return Ok(None),
    }
    composer_lock::try_with_composer_permit(session, |_| -> Result<Option<String>, ()> {
        let pane = match SubmissionTmux.pin_guarded(session) {
            Ok(pane) => pane,
            Err(InputRefusal::Composer(
                crate::services::tui_input::submission::Refusal::UnpredictableRender,
            )) => return Ok(None),
            Err(_) => return Err(()),
        };
        let capture = pane.capture_draft().ok_or(())?;
        if composer_lock::composer_admission(session, || Some(capture.clone()))
            != composer_lock::ComposerAdmission::StashOnly
        {
            return Ok(None);
        }
        match observe_claude_jsonl_turn_state(&path) {
            TuiTurnState::Idle => {}
            TuiTurnState::Unknown => return Err(()),
            _ => return Ok(None),
        }
        let Some(draft) = screen::stashable(&capture, "wake", Some(pane.dimensions())) else {
            return Ok(None);
        };
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path).map_err(|_| ())?;
        Ok(Some(format!(
            "{:?}/{draft:?}/{}/{}/{}",
            pane.identity(),
            metadata.dev(),
            metadata.ino(),
            metadata.len()
        )))
    })
    .ok_or(())?
}

#[cfg(test)]
#[path = "idle_stash/idle_stash_tests.rs"]
pub(crate) mod tests;
