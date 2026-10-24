//! The stash path: park a person's draft in Claude's single stash slot, submit the frame, and
//! watch Claude hand the draft back on the submit. No key is ever retried.

use std::time::{Duration, Instant};

use super::super::composer_lock::DraftGuard;
use super::screen::{self, Composer, Stash};
use super::{Attempt, Delivery, DraftState, Guard, Outcome, Report, Unconfirmed, Veto, modal};

/// One stash transaction; recovery is decided before the caller releases the composer lock.
pub(super) fn run(mut attempt: &Attempt<'_>, draft: &[String]) -> Report {
    let mut last = None;
    let report = transact(&mut attempt, draft, &mut last);
    protect(attempt, &report, last.as_deref());
    report
}

pub(super) fn protect(attempt: &Attempt<'_>, report: &Report, last: Option<&str>) {
    let guard = |state| super::super::composer_lock::guard_draft(attempt.request.session, state);
    match report.draft {
        DraftState::Unchanged => {}
        // Handed back is not yet sent or cleared by the person: automatic writes stay off it.
        DraftState::RestoredObserved => guard(DraftGuard::DraftRestored),
        DraftState::StashedVerified | DraftState::Unknown => {
            guard(DraftGuard::RecoveryRequired);
            alert(attempt, report, last);
        }
    }
}

pub(in crate::services::claude_tui) trait StashEffects {
    fn timing(&self) -> &super::Timing;
    fn text(&self) -> &str;
    fn stash(&mut self) -> Result<(), Report>;
    fn paste(&mut self) -> Result<(), Report>;
    fn drop_buffer(&self);
    fn generation_unchanged(&self) -> bool;
    fn capture(&self) -> Option<String>;
    fn await_own(&mut self, last: &mut Option<String>) -> Result<(), Unconfirmed>;
    fn enter(&mut self) -> Outcome;
}

impl StashEffects for &Attempt<'_> {
    fn timing(&self) -> &super::Timing {
        self.timing
    }
    fn text(&self) -> &str {
        self.text
    }
    fn stash(&mut self) -> Result<(), Report> {
        let report = match self.key("C-s") {
            Guard::Applied => return Ok(()),
            Guard::Vetoed | Guard::Resized => {
                Report::new(Outcome::NotSent(Veto::HumanAttached), DraftState::Unchanged)
            }
            Guard::Gone => Report::new(
                Outcome::NotSent(Veto::PaneUnavailable),
                DraftState::Unchanged,
            ),
            Guard::Failed => {
                Report::new(Outcome::NotSent(Veto::PaneUnavailable), DraftState::Unknown)
            }
        };
        Attempt::drop_buffer(self);
        Err(report)
    }
    fn paste(&mut self) -> Result<(), Report> {
        let report = match Attempt::paste(self, None) {
            Guard::Applied => return Ok(()),
            Guard::Vetoed | Guard::Resized => Report::new(
                Outcome::NotSent(Veto::HumanAttached),
                DraftState::StashedVerified,
            ),
            Guard::Gone => Report::new(
                Outcome::NotSent(Veto::PaneUnavailable),
                DraftState::StashedVerified,
            ),
            Guard::Failed => {
                return Err(Report::new(
                    Outcome::Unconfirmed(Unconfirmed::PasteFailed),
                    DraftState::Unknown,
                ));
            }
        };
        Attempt::drop_buffer(self);
        Err(report)
    }
    fn drop_buffer(&self) {
        Attempt::drop_buffer(self);
    }
    fn generation_unchanged(&self) -> bool {
        Attempt::generation_unchanged(self)
    }
    fn capture(&self) -> Option<String> {
        self.pane.capture()
    }
    fn await_own(&mut self, last: &mut Option<String>) -> Result<(), Unconfirmed> {
        Attempt::await_own(self, |after| screen::owns(after, self.text()), last)
    }
    fn enter(&mut self) -> Outcome {
        Attempt::enter(self)
    }
}

pub(in crate::services::claude_tui) fn transact(
    attempt: &mut impl StashEffects,
    draft: &[String],
    last: &mut Option<String>,
) -> Report {
    if let Err(report) = attempt.stash() {
        return report;
    }
    if let Err(veto) = await_stash(attempt, last) {
        attempt.drop_buffer();
        return Report::new(Outcome::NotSent(veto), DraftState::Unknown);
    }
    if let Err(report) = attempt.paste() {
        return report;
    }
    let outcome = match attempt.await_own(last) {
        Ok(()) => attempt.enter(),
        Err(detail) => return Report::new(Outcome::Unconfirmed(detail), DraftState::Unknown),
    };
    Report::new(outcome, await_restore(attempt, draft, last))
}

/// Rechecks after the C-s until Claude shows an empty composer under its stash marker. Only the
/// composer, stash, modal and generation count: the turn may end meanwhile.
fn await_stash(attempt: &impl StashEffects, last: &mut Option<String>) -> Result<(), Veto> {
    std::thread::sleep(attempt.timing().settle);
    for check in 0..attempt.timing().rechecks {
        if check > 0 {
            std::thread::sleep(attempt.timing().recheck_interval);
        }
        if !attempt.generation_unchanged() {
            return Err(Veto::HumanAttached);
        }
        let Some(after) = attempt.capture() else {
            return Err(Veto::PaneUnavailable);
        };
        let (shown_modal, stashed) = (modal(&after), screen::stashed(&after));
        *last = Some(after);
        if shown_modal {
            return Err(Veto::Modal);
        }
        if stashed {
            return Ok(());
        }
    }
    Err(Veto::Draft)
}

/// Read-only watch for the draft coming back after the submit.
fn await_restore(
    attempt: &impl StashEffects,
    draft: &[String],
    last: &mut Option<String>,
) -> DraftState {
    let started = Instant::now();
    let mut deadline = started + attempt.timing().restore_window;
    // Measured on Claude Code 2.1.309: the folded-paste hint can last 8.09 seconds.
    let hint_deadline = started + attempt.timing().restore_window.max(Duration::from_secs(9));
    loop {
        if let Some(capture) = attempt.capture() {
            let back = screen::restored(&capture, draft);
            let observed = screen::read(&capture);
            let pending_hint = observed.footer == screen::Footer::PasteExpand
                && observed.stash == Stash::AbsentInRecognizedLayout
                && matches!(&observed.composer, Composer::Text(rows) if rows == draft);
            if pending_hint {
                deadline = hint_deadline;
            }
            *last = Some(capture);
            if back {
                return DraftState::RestoredObserved;
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return DraftState::Unknown;
        }
        std::thread::sleep(attempt.timing().confirm_poll.min(left));
    }
}

/// Names what the last capture showed and the one safe manual step for that state.
fn alert(attempt: &Attempt<'_>, report: &Report, last: Option<&str>) {
    let screen = last.map(screen::read);
    let composer = match screen.as_ref().map(|screen| &screen.composer) {
        Some(Composer::Empty) => "empty",
        Some(Composer::Text(rows))
            if rows.iter().map(String::as_str).eq(attempt.text.split('\n')) =>
        {
            "external_input"
        }
        Some(Composer::Text(_)) => "text",
        _ => "unreadable",
    };
    let stash = match screen.map(|screen| screen.stash) {
        Some(Stash::Present) => "present",
        Some(Stash::AbsentInRecognizedLayout) => "absent",
        _ => "unknown",
    };
    let guidance = match (composer, stash) {
        ("empty", "present") => {
            "The draft is in Claude's stash and the composer is empty: press Ctrl+S once to bring it back."
        }
        (_, "present") => {
            "The draft is in Claude's stash but the composer holds other text: send or clear that text first, because Ctrl+S now would overwrite the stashed draft."
        }
        (_, "absent") => "Claude shows no stash: look for the draft in the composer before typing.",
        _ => {
            "The composer or stash could not be read: look at the pane before pressing Ctrl+S, which overwrites the stash whenever the composer has text."
        }
    };
    let delivery = match report.delivery() {
        Delivery::NotAttempted => "not_attempted",
        Delivery::AttemptedUnconfirmed => "attempted_unconfirmed",
        Delivery::Observed => "observed",
    };
    let payload = serde_json::json!({
        "tmux_session": attempt.request.session,
        "nonce": attempt.request.nonce,
        "delivery": delivery,
        "draft": if report.draft == DraftState::StashedVerified { "stashed" } else { "unknown" },
        "composer": composer,
        "stash": stash,
        "automatic_writes_held": true,
        "guidance": guidance,
    });
    tracing::warn!(
        %payload,
        "busy inject left a person's draft unaccounted for; automatic writes to the pane are held"
    );
    crate::services::observability::events::record_simple(
        "busy_inject_draft_unrecovered",
        None,
        Some("claude"),
        payload,
    );
}
