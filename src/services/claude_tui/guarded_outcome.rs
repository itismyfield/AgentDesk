//! Typed guarded stops survive owner decisions and protect the composer before its lock returns.

use super::host_input::InputRun;
#[cfg(test)]
use super::input;
#[cfg(test)]
use crate::services::provider::CancelToken;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SubmissionError {
    Legacy(String),
    Guarded(InputRun),
}

impl From<String> for SubmissionError {
    fn from(error: String) -> Self {
        Self::Legacy(error)
    }
}

impl SubmissionError {
    pub(crate) fn from_run(run: InputRun) -> Result<(), Self> {
        match run {
            InputRun::Applied => Ok(()),
            stopped => Err(Self::Guarded(stopped)),
        }
    }
    pub(crate) fn into_legacy(self) -> String {
        match self {
            Self::Legacy(error) => error,
            Self::Guarded(run) => {
                finish(&run);
                format!("claude tui guarded submission held: {run:?}")
            }
        }
    }
}

pub(crate) trait Owner: Send + Sync {
    fn started(&self);
    fn finished(&self, run: &InputRun);
}

struct Scope {
    owner: Option<Arc<dyn Owner>>,
    result: Option<InputRun>,
}
thread_local! { static OWNER: RefCell<Option<Scope>> = const { RefCell::new(None) }; }
struct Restore(Option<Scope>);
impl Drop for Restore {
    fn drop(&mut self) {
        OWNER.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

pub(crate) fn with_owner<R>(owner: Option<Arc<dyn Owner>>, run: impl FnOnce() -> R) -> R {
    scoped(owner, run).0
}
#[cfg(unix)]
pub(crate) fn capture<R>(run: impl FnOnce() -> R) -> (R, Option<InputRun>) {
    let owner = OWNER.with(|slot| slot.borrow().as_ref().and_then(|scope| scope.owner.clone()));
    scoped(owner, run)
}
fn scoped<R>(owner: Option<Arc<dyn Owner>>, run: impl FnOnce() -> R) -> (R, Option<InputRun>) {
    let previous = OWNER.with(|slot| {
        slot.replace(Some(Scope {
            owner,
            result: None,
        }))
    });
    let _restore = Restore(previous);
    let result = run();
    let stopped = OWNER.with(|slot| {
        slot.borrow_mut()
            .as_mut()
            .and_then(|scope| scope.result.take())
    });
    (result, stopped)
}
pub(crate) fn begin() {
    let owner = OWNER.with(|slot| slot.borrow().as_ref().and_then(|scope| scope.owner.clone()));
    if let Some(owner) = owner {
        owner.started();
    }
}
pub(crate) fn finish(run: &InputRun) {
    let owner = OWNER.with(|slot| {
        slot.borrow_mut().as_mut().and_then(|scope| {
            scope.result = Some(run.clone());
            scope.owner.clone()
        })
    });
    if let Some(owner) = owner {
        owner.finished(run);
    }
}

static HELD: LazyLock<Mutex<HashMap<String, InputRun>>> = LazyLock::new(Default::default);
pub(crate) fn held(session: &str) -> Option<InputRun> {
    HELD.lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(session)
        .cloned()
}
pub(crate) fn held_result(session: &str) -> Result<(), SubmissionError> {
    held(session)
        .or_else(|| super::host_input::guarded_submission::selection_refusal(session))
        .map_or(Ok(()), |run| Err(SubmissionError::Guarded(run)))
}
pub(crate) fn readiness_error(session: &str, error: String) -> SubmissionError {
    held(session).map_or(SubmissionError::Legacy(error), SubmissionError::Guarded)
}
/// The composer permit is still held; no automatic path releases this submission.
pub(crate) fn retain(session: &str, run: &InputRun) {
    if matches!(run, InputRun::Indeterminate { .. }) {
        HELD.lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .entry(session.to_owned())
            .or_insert_with(|| run.clone());
    }
    finish(run);
}
#[cfg(test)]
pub(crate) fn forget_test_hold(session: &str) {
    HELD.lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .remove(session);
}

#[cfg(test)]
pub fn send_fresh_prompt(
    session: &str,
    prompt: &str,
    cancel: Option<&CancelToken>,
) -> Result<(), String> {
    input::send_prompt_with_readiness(
        session,
        prompt,
        input::PromptReadinessKind::FreshTurn,
        cancel,
    )
    .map_err(SubmissionError::into_legacy)
}
#[cfg(test)]
pub fn send_followup_prompt_or_idle_transcript(
    session: &str,
    prompt: &str,
    cancel: Option<&CancelToken>,
    transcript: &std::path::Path,
) -> Result<(), String> {
    input::send_followup_prompt_or_idle_transcript_owned(session, prompt, cancel, transcript)
        .map_err(SubmissionError::into_legacy)
}

#[cfg(unix)]
pub(crate) enum OwnedWarmOutcome {
    Legacy(super::hosting::ClaudeTuiWarmFollowupOutcome),
    Stopped(InputRun),
}

#[cfg(unix)]
pub(crate) fn warm_owned(
    run: impl FnOnce() -> super::hosting::ClaudeTuiWarmFollowupOutcome,
) -> OwnedWarmOutcome {
    let (legacy, input) = capture(run);
    match input {
        Some(stopped) if stopped != InputRun::Applied => OwnedWarmOutcome::Stopped(stopped),
        _ => OwnedWarmOutcome::Legacy(legacy),
    }
}
