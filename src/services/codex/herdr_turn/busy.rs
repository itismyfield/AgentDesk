//! A busy Codex turn on a Herdr pane: main's Codex busy transaction order on the execution's gate,
//! so one paste and one Enter reach the open model turn, or no key is sent.
use crate::services::claude_tui::busy_inject::{MAX_INPUT_BYTES, at_offsets, frame};
use crate::services::claude_tui::owner_input::{self, ReadyRefusal};
use crate::services::codex_tui::busy_inject::screen::{self, Composer};
use crate::services::codex_tui::busy_inject::{
    Ledger, Outcome, Request, TIMING, Unconfirmed, Veto, turn,
};
use crate::services::codex_tui::host_input::InputRun;
use crate::services::codex_tui::input::{TuiInputAction, try_with_composer_mutation_lock};
use crate::services::session_host::HerdrTarget;
use crate::services::tui_input::human_frame::history;

/// One paste and one Enter of `request`'s frame through `target`'s gate, under the composer lock
/// of the logical session the history and ledger name.
pub(crate) fn inject(target: &HerdrTarget, request: &Request<'_>, ledger: &dyn Ledger) -> Outcome {
    let text = frame(
        request.source,
        request.author,
        request.name,
        request.nonce,
        request.text,
    );
    if request.text.trim().is_empty()
        || text.len() > MAX_INPUT_BYTES
        || request.nonce.is_empty()
        || (!owner_input::active() && request.target_turn.trim().is_empty())
    {
        return Outcome::NotSent(Veto::InvalidInput);
    }
    let started = std::time::Instant::now();
    let locked = at_offsets(
        TIMING.lock_retries,
        || started.elapsed(),
        std::thread::sleep,
        || {
            try_with_composer_mutation_lock(request.session, || {
                locked(target, request, &text, ledger)
            })
        },
    );
    locked.unwrap_or(Outcome::NotSent(Veto::LockContended))
}

/// Both hosts share the owned screen policy; legacy steering still needs an empty editor.
fn composer(target: &HerdrTarget) -> Result<bool, Veto> {
    let capture = target.capture_ansi(-80).ok_or(Veto::PaneUnavailable)?;
    if owner_input::active() {
        if capture.trim().is_empty() {
            return Err(Veto::PaneUnavailable);
        }
        if let Some(veto) = screen::delivery_veto(&capture) {
            return Err(veto);
        }
        return Ok(screen::draft_may_be_present(&capture));
    }
    let shown = screen::read(&capture);
    if shown.modal {
        return Err(Veto::Modal);
    }
    if shown.composer != Composer::Empty || shown.attachments {
        return Err(Veto::ComposerUnread);
    }
    screen::append_veto(&capture, &shown).map_or(Ok(false), Err)
}

fn locked(target: &HerdrTarget, request: &Request<'_>, text: &str, ledger: &dyn Ledger) -> Outcome {
    let not_sent = Outcome::NotSent;
    let mut draft = match composer(target) {
        Ok(draft) => draft,
        Err(veto) => return not_sent(veto),
    };
    let Ok(offset) = std::fs::metadata(request.rollout).map(|meta| meta.len()) else {
        return not_sent(Veto::TranscriptUnavailable);
    };
    if let Err(veto) = turn(request) {
        return not_sent(veto);
    }
    if !ledger.register() {
        return not_sent(Veto::LedgerFull);
    }
    if let Err(error) = history::append::register(
        (request.session, request.nonce, request.author),
        request.rollout,
        offset,
        text,
        request.target_turn,
        ledger.message_id(),
        false,
    ) {
        tracing::warn!(%error, "busy Herdr frame ledger registration failed");
        ledger.withdraw();
        return not_sent(if error == "identical frame still unconfirmed" {
            Veto::AmbiguousIdenticalPending
        } else {
            Veto::LedgerFull
        });
    }
    let withdraw = |veto| {
        ledger.withdraw();
        history::withdraw(request.nonce, request.rollout);
        not_sent(veto)
    };
    // The turn and the composer are read again just before the owner's ready, as the tmux path
    // does; a failed read withdraws before the owner is asked at all.
    match turn(request).and_then(|()| composer(target)) {
        Ok(current) => draft |= current,
        Err(veto) => return withdraw(veto),
    }
    let ready = owner_input::ready_payload().map_err(|refusal| match refusal {
        ReadyRefusal::Target(_) => Veto::PaneUnavailable,
        ReadyRefusal::Lease(_) => Veto::LockContended,
    });
    if let Err(veto) = ready {
        return withdraw(veto);
    }
    use history::append::{self, Admission};
    if append::mark(request.nonce, request.rollout, Admission::EffectPossible).is_err() {
        return withdraw(Veto::LedgerFull);
    }
    if owner_input::start_payload().is_err() {
        return withdraw(Veto::LockContended);
    }
    owner_input::keyed();
    let plan = [
        TuiInputAction::PasteBuffer(text.into()),
        TuiInputAction::Enter,
    ];
    let run = crate::services::codex_tui::host_input::run_herdr(target, &plan, None);
    let unconfirmed = |reason| {
        let _ = append::mark(request.nonce, request.rollout, Admission::Uncertain);
        Outcome::Unconfirmed(reason)
    };
    match run.run {
        InputRun::Applied => {}
        // The gate refused before its first write: nothing reached the pane.
        InputRun::Refused(_) => {
            owner_input::first_key_held("herdr_gate_refused");
            return withdraw(Veto::PaneUnavailable);
        }
        InputRun::Indeterminate { confirmed: 0, .. } | InputRun::Cancelled { confirmed: 0 } => {
            return unconfirmed(Unconfirmed::PasteFailed);
        }
        _ => return unconfirmed(Unconfirmed::EnterFailed),
    }
    if append::mark(request.nonce, request.rollout, Admission::Admitted).is_ok() {
        ledger.admitted();
    }
    if owner_input::active() && draft {
        ledger.draft_delivered();
    }
    Outcome::Injected
}

#[cfg(test)]
#[path = "busy_tests.rs"]
mod tests;
