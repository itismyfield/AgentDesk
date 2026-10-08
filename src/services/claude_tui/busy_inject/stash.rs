//! The stash path: park a person's draft in Claude's single stash slot, submit the frame, and
//! watch Claude hand the draft back on the submit. No key is ever retried.

use std::time::Instant;

use super::super::composer_lock::DraftGuard;
use super::screen::{self, Composer, Stash};
use super::{Attempt, Delivery, DraftState, Guard, Outcome, Report, Unconfirmed, Veto, modal};

/// One stash transaction; recovery is decided before the caller releases the composer lock.
pub(super) fn run(attempt: &Attempt<'_>, draft: &[String]) -> Report {
    let mut last = None;
    let report = transact(attempt, draft, &mut last);
    let guard = |state| super::super::composer_lock::guard_draft(attempt.request.session, state);
    match report.draft {
        DraftState::Unchanged => {}
        // Handed back is not yet sent or cleared by the person: automatic writes stay off it.
        DraftState::RestoredObserved => guard(DraftGuard::DraftRestored),
        DraftState::StashedVerified | DraftState::Unknown => {
            guard(DraftGuard::RecoveryRequired);
            alert(attempt, &report, last.as_deref());
        }
    }
    report
}

fn transact(attempt: &Attempt<'_>, draft: &[String], last: &mut Option<String>) -> Report {
    let stop = |veto, state| {
        attempt.drop_buffer();
        Report::new(Outcome::NotSent(veto), state)
    };
    match attempt.key("C-s") {
        Guard::Applied => {}
        Guard::Vetoed => return stop(Veto::HumanAttached, DraftState::Unchanged),
        Guard::Gone => return stop(Veto::PaneUnavailable, DraftState::Unchanged),
        // The key may still have landed.
        Guard::Failed => return stop(Veto::PaneUnavailable, DraftState::Unknown),
    }
    if let Err(veto) = await_stash(attempt, last) {
        return stop(veto, DraftState::Unknown);
    }
    match attempt.paste(None) {
        Guard::Applied => {}
        Guard::Vetoed => return stop(Veto::HumanAttached, DraftState::StashedVerified),
        Guard::Gone => return stop(Veto::PaneUnavailable, DraftState::StashedVerified),
        Guard::Failed => {
            let outcome = Outcome::Unconfirmed(Unconfirmed::PasteFailed);
            return Report::new(outcome, DraftState::Unknown);
        }
    }
    // Only the exact rows prove the bytes are ours; a folded placeholder never does here.
    let outcome = match attempt.await_own(|after| screen::owns(after, attempt.text), last) {
        Ok(()) => attempt.enter(),
        Err(detail) => return Report::new(Outcome::Unconfirmed(detail), DraftState::Unknown),
    };
    Report::new(outcome, await_restore(attempt, draft, last))
}

/// Rechecks after the C-s until Claude shows an empty composer under its stash marker. Only the
/// composer, stash, modal and generation count: the turn may end meanwhile.
fn await_stash(attempt: &Attempt<'_>, last: &mut Option<String>) -> Result<(), Veto> {
    std::thread::sleep(attempt.timing.settle);
    for check in 0..attempt.timing.rechecks {
        if check > 0 {
            std::thread::sleep(attempt.timing.recheck_interval);
        }
        if !attempt.unattended() {
            return Err(Veto::HumanAttached);
        }
        let Some(after) = attempt.pane.capture() else {
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
fn await_restore(attempt: &Attempt<'_>, draft: &[String], last: &mut Option<String>) -> DraftState {
    let deadline = Instant::now() + attempt.timing.restore_window;
    loop {
        if let Some(capture) = attempt.pane.capture() {
            let back = screen::restored(&capture, draft);
            *last = Some(capture);
            if back {
                return DraftState::RestoredObserved;
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return DraftState::Unknown;
        }
        std::thread::sleep(attempt.timing.confirm_poll.min(left));
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
