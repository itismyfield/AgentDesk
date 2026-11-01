//! The one Claude TUI submission: a readiness wait where the entry needs one, one pre-paste read,
//! an optional C-s, a bracketed paste, a fixed settle and one Enter. No pane is read after a key.
use super::*;
#[cfg(unix)]
use crate::services::claude_tui::busy_inject::{self, Outcome, Timing, screen, stash};
#[cfg(unix)]
use crate::services::claude_tui::host_input::{InputRefusal, InputRun, StopCause};
#[cfg(unix)]
use crate::services::claude_tui::{composer_lock, submission_fence};
#[cfg(unix)]
use crate::services::tui_input::submission::Refusal;
#[cfg(unix)]
use crate::services::tui_input::submission_tmux::SubmissionTmux;
#[cfg(unix)]
use crate::services::tui_turn_state::{TuiTurnState, observe_claude_jsonl_turn_state};

pub(super) fn send(
    session: &str,
    prompt: &str,
    readiness: PromptReadinessKind,
    cancel: Option<&CancelToken>,
) -> Result<(), SubmissionError> {
    guarded_outcome::capture_preparation(|| {
        // An invalid prompt fails here, before any readiness wait or key.
        let text = payload_text(prompt)?;
        // A warm follow-up waited on its transcript turn before it got here.
        if !matches!(readiness, PromptReadinessKind::ProvenWarmFollowup) {
            wait_for_prompt_ready(session, readiness, cancel)
                .map_err(|error| preparation_hold::readiness_failure(session, error))?;
        }
        submit(session, prompt, &text, readiness, cancel)
    })
}

/// Steering has judged the pane from its own snapshot, so it skips the readiness wait.
pub(super) fn send_now(session: &str, prompt: &str) -> Result<(), SubmissionError> {
    guarded_outcome::capture_preparation(|| {
        let text = payload_text(prompt)?;
        submit(session, prompt, &text, PromptReadinessKind::Followup, None)
    })
}

fn payload_text(prompt: &str) -> Result<String, String> {
    let actions = plan_prompt_submit(prompt)?;
    let Some((TuiInputAction::Enter, payload)) = actions.split_last() else {
        return Err("claude tui prompt plan does not end with Enter".into());
    };
    payload
        .iter()
        .map(|action| match action {
            TuiInputAction::Literal(text) | TuiInputAction::PasteBuffer(text) => Ok(text.as_str()),
            _ => Err("claude tui prompt plan holds a non-text key".to_string()),
        })
        .collect()
}

/// A managed attempt claims its durable fence before the first key; any other caller only meets
/// the thread's fence, if one is installed.
#[cfg(unix)]
fn submit(
    session: &str,
    prompt: &str,
    text: &str,
    readiness: PromptReadinessKind,
    cancel: Option<&CancelToken>,
) -> Result<(), SubmissionError> {
    guarded_outcome::receipt(session, prompt);
    let request = Request {
        session,
        prompt,
        text,
        cancel,
        timing: &busy_inject::TIMING,
    };
    let modal = std::cell::Cell::new(false);
    let result = match submission_fence::idle_stash_scope() {
        Some((_, context)) if !context.available || context.target != session => {
            InputRun::Refused(InputRefusal::IdentityMismatch)
        }
        Some((fence, context)) => {
            let stash =
                fence.stash_any_channel() || busy_inject::stash_enabled(Some(context.channel));
            let authority = ManagedAuthority {
                fence,
                session: session.into(),
            };
            run(&authority, stash, (&request, &modal))
        }
        // No channel is known here, so a visible draft is never parked; a pane held for draft
        // recovery still takes no automatic write.
        None => {
            if composer_lock::draft_guarded(session) {
                composer_lock::with_composer_mutation_lock(session, || {
                    admit_automatic_write(session, readiness)
                })?;
            }
            run(&Unmanaged, false, (&request, &modal))
        }
    };
    guarded_outcome::finish(&result);
    SubmissionError::from_run(result)
        .map_err(|error| preparation_hold::idle_stash_stopped(session, error, modal.get()))
}

#[cfg(not(unix))]
fn submit(
    _session: &str,
    _prompt: &str,
    _text: &str,
    _readiness: PromptReadinessKind,
    _cancel: Option<&CancelToken>,
) -> Result<(), SubmissionError> {
    Err("claude tui input needs a unix tmux pane".to_string().into())
}

#[cfg(unix)]
trait StashAuthority {
    fn admits(&self, session: &str) -> bool;
    fn claim(&self, _session: &str) -> Result<(), String> {
        submission_fence::before_first_payload()
    }
}

#[cfg(unix)]
struct Request<'a> {
    session: &'a str,
    prompt: &'a str,
    text: &'a str,
    cancel: Option<&'a CancelToken>,
    timing: &'a Timing,
}

#[cfg(unix)]
fn refused(reason: Refusal) -> InputRun {
    InputRun::Refused(InputRefusal::Composer(reason))
}

#[cfg(unix)]
/// `modal` records whether the one capture before any key refused the pane for a modal.
fn run(
    authority: &dyn StashAuthority,
    stash_allowed: bool,
    (request, modal): (&Request<'_>, &std::cell::Cell<bool>),
) -> InputRun {
    let session = request.session;
    composer_lock::try_with_composer_mutation_lock(session, || {
        if !authority.admits(session) {
            return InputRun::Refused(InputRefusal::IdentityMismatch);
        }
        if cancel_requested(request.cancel) {
            return InputRun::Cancelled { confirmed: 0 };
        }
        let pane = busy_inject::Pane::with_program(session, "tmux".into());
        let park = match busy_inject::before_paste(&pane, stash_allowed) {
            Ok(park) => park,
            Err(veto) => {
                modal.set(veto == busy_inject::Veto::Modal);
                return refused(Refusal::NotReady);
            }
        };
        if authority.claim(session).is_err() {
            return refused(Refusal::SubmissionFence);
        }
        guarded_outcome::begin();
        crate::services::tui_prompt_dedupe::record_discord_originated_prompt(
            "claude",
            session,
            request.prompt,
        );
        let nonce = busy_inject::fresh_nonce();
        match stash::submit(&pane, &nonce, request.text, request.timing, park) {
            Outcome::Injected => InputRun::Applied,
            Outcome::NotSent(_) => refused(Refusal::CaptureUnavailable),
            Outcome::Failed(error) => InputRun::Indeterminate {
                confirmed: 0,
                cause: StopCause::Send(format!("managed idle input write failed: {error:?}")),
            },
        }
    })
    .unwrap_or_else(|| refused(Refusal::WriterContended))
}

#[cfg(unix)]
struct ManagedAuthority {
    fence: std::sync::Arc<dyn submission_fence::SubmissionFence>,
    session: String,
}
#[cfg(unix)]
impl StashAuthority for ManagedAuthority {
    fn admits(&self, session: &str) -> bool {
        self.session == session && self.fence.idle_stash_admits(session)
    }
    fn claim(&self, session: &str) -> Result<(), String> {
        self.fence
            .claim_idle_stash(session)
            .map_err(|error| error.to_string())
    }
}

#[cfg(unix)]
struct Unmanaged;
#[cfg(unix)]
impl StashAuthority for Unmanaged {
    fn admits(&self, _session: &str) -> bool {
        true
    }
}

// A watcher can suggest a wake; it neither claims an attempt nor removes draft protection.
#[cfg(unix)]
pub(crate) fn wake_hint(session: &str) -> Option<String> {
    wake_hint_observed(session).ok().flatten()
}

/// Failed observation cannot authorize skipping a capped head; confirmed non-targets can.
#[cfg(unix)]
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
            Err(InputRefusal::Composer(Refusal::UnpredictableRender)) => return Ok(None),
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
        let screen::Composer::Text(draft) = screen::input_box(&capture) else {
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

#[cfg(all(test, unix))]
#[path = "submission_tests.rs"]
pub(crate) mod tests;
