use super::{CHANGED, Proof, ledger, observe};
use crate::services::provider::{CancelToken, cancel_requested};
use std::path::Path;
use std::time::{Duration, Instant};

pub(crate) fn wait(
    session: &str,
    path: Option<&Path>,
    cancel: Option<&CancelToken>,
    budget: Duration,
) -> Result<Proof, String> {
    let deadline = Instant::now() + budget;
    loop {
        if cancel_requested(cancel) {
            return Err(super::super::input::PROMPT_READY_CANCELLED_ERROR.into());
        }
        let revision = ledger().revision;
        if let Some(proof) = observe(session, path) {
            return Ok(proof);
        }
        if Instant::now() >= deadline {
            return Err("timeout waiting for claude tui current Idle/SessionStart evidence".into());
        }
        let state = ledger();
        if state.revision != revision {
            continue;
        }
        let _wait = CHANGED
            .wait_timeout(state, Duration::from_millis(100))
            .unwrap_or_else(|e| e.into_inner());
    }
}

pub(crate) fn prepare(
    session: &str,
    path: Option<&Path>,
    cancel: Option<&CancelToken>,
    budget: Duration,
    draft_required: bool,
) -> Result<Proof, String> {
    let deadline = Instant::now() + budget;
    loop {
        let proof = wait(
            session,
            path,
            cancel,
            deadline.saturating_duration_since(Instant::now()),
        )?;
        if !draft_required
            || super::super::host_input::observe_draft(session)
                .as_deref()
                .is_some_and(super::empty_draft)
        {
            return Ok(proof);
        }
        if Instant::now() >= deadline {
            return Err("timeout waiting for claude tui empty composer".into());
        }
        let state = ledger();
        let _wait = CHANGED
            .wait_timeout(
                state,
                Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
            )
            .unwrap_or_else(|e| e.into_inner());
    }
}
