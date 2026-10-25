//! Managed idle input uses the same single capture and paste → settle → Enter as busy input.
use super::*;
use crate::services::claude_tui::busy_inject::{self, Outcome, Timing, screen, stash};
use crate::services::claude_tui::host_input::LegacyTmuxGate;
use crate::services::tui_turn_state::{TuiTurnState, observe_claude_jsonl_turn_state};

trait StashAuthority {
    fn admits(&self, session: &str) -> bool;
    fn claim(&self, _session: &str) -> Result<(), String> {
        submission_fence::before_first_payload()
    }
}

struct Request<'a> {
    target: &'a InputTarget,
    gate: &'a dyn MutationGate,
    prompt: &'a str,
    cancel: Option<&'a CancelToken>,
    timing: &'a Timing,
}

fn run(
    authority: Option<&dyn StashAuthority>,
    stash_allowed: bool,
    request: &Request<'_>,
) -> Option<InputRun> {
    let authority = authority?;
    let session = request.target.tmux_session().ok()?;
    let actions = input::plan_prompt_submit(request.prompt).ok()?;
    let Some((input::TuiInputAction::Enter, payload)) = actions.split_last() else {
        return None;
    };
    let text = payload_frame(payload).ok()?;
    composer_lock::try_with_composer_mutation_lock(session, || {
        if !authority.admits(session) || request.gate.admit(session).is_err() {
            return Some(InputRun::Refused(InputRefusal::IdentityMismatch));
        }
        if cancel_requested(request.cancel) {
            return Some(InputRun::Cancelled { confirmed: 0 });
        }
        let pane = busy_inject::Pane::with_program(session, "tmux".into());
        let parkable = match busy_inject::before_paste(&pane) {
            Ok(parkable) => parkable,
            Err(_) => return Some(refused(Refusal::NotReady)),
        };
        if authority.claim(session).is_err() {
            return Some(refused(Refusal::SubmissionFence));
        }
        guarded_outcome::begin();
        crate::services::tui_prompt_dedupe::record_discord_originated_prompt(
            "claude",
            session,
            request.prompt,
        );
        let outcome = stash::submit(
            &pane,
            &busy_inject::fresh_nonce(),
            &text,
            request.timing,
            stash_allowed && parkable,
        );
        let result = match outcome {
            Outcome::Injected => InputRun::Applied,
            Outcome::NotSent(_) => refused(Refusal::CaptureUnavailable),
            Outcome::Failed(error) => InputRun::Indeterminate {
                confirmed: 0,
                cause: StopCause::Send(format!("managed idle input write failed: {error:?}")),
            },
        };
        Some(result)
    })
    .unwrap_or_else(|| Some(refused(Refusal::WriterContended)))
}

struct ManagedAuthority {
    fence: std::sync::Arc<dyn submission_fence::SubmissionFence>,
    session: String,
}
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

pub(crate) fn managed_enabled(session: &str) -> bool {
    submission_fence::idle_stash_scope()
        .is_some_and(|(_, context)| context.available && context.target == session)
}

pub(crate) fn submit(
    session: &str,
    prompt: &str,
    cancel: Option<&CancelToken>,
    selected: Option<&super::Request>,
) -> Option<Result<(), guarded_outcome::SubmissionError>> {
    let (fence, context) = submission_fence::idle_stash_scope()?;
    if !context.available || context.target != session {
        let held = InputRun::Refused(InputRefusal::IdentityMismatch);
        guarded_outcome::finish(&held);
        return Some(guarded_outcome::SubmissionError::from_run(held));
    }
    let target = InputTarget::legacy_tmux(session);
    let (target, gate): (&InputTarget, &dyn MutationGate) = selected
        .map_or((&target, &LegacyTmuxGate as &dyn MutationGate), |request| {
            (&request.target, &*request.gate)
        });
    let authority = ManagedAuthority {
        fence,
        session: session.into(),
    };
    let request = Request {
        target,
        gate,
        prompt,
        cancel,
        timing: &busy_inject::TIMING,
    };
    let result = run(
        Some(&authority),
        busy_inject::stash_enabled(Some(context.channel)),
        &request,
    )?;
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

#[cfg(test)]
#[path = "idle_stash/idle_stash_tests.rs"]
pub(crate) mod tests;
