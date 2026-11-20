use super::super::input::{PreparationHold, PromptReadinessKind};
use super::{CHANGED, Expiry, Proof, Unproven, ledger, observe};
use crate::services::provider::{CancelToken, cancel_requested};
use std::path::Path;
use std::time::{Duration, Instant};

/// Why a wait ended without a proof; `unproven` when no later look can supply one.
#[derive(Debug)]
pub(crate) struct Stopped {
    pub(crate) error: String,
    pub(crate) unproven: Option<Unproven>,
}
impl From<String> for Stopped {
    fn from(error: String) -> Self {
        Self {
            error,
            unproven: None,
        }
    }
}

/// A budget that ends on a transcript that only moved runs again in short, counted windows.
pub(crate) fn prepare(
    session: &str,
    path: Option<&Path>,
    cancel: Option<&CancelToken>,
    budget: Duration,
    readiness: PromptReadinessKind,
) -> Result<Proof, Stopped> {
    let owner_deadline = (readiness == PromptReadinessKind::ProvenWarmFollowup)
        .then(super::super::owner_input::readiness_deadline)
        .flatten();
    let bounded = |window| owner_deadline.map_or(window, |due| due.min(window));
    let mut watched = super::watched(session, path);
    let mut deadline = bounded(Instant::now() + budget);
    let mut dialogs = super::dialog::DialogWait::new(readiness);
    loop {
        if cancel_requested(cancel) {
            return Err(String::from(super::super::input::PROMPT_READY_CANCELLED_ERROR).into());
        }
        if let Some(refused) = crate::services::claude_tui::submission_fence::refusal() {
            return Err(refused.to_string().into());
        }
        if owner_deadline.is_some_and(|due| Instant::now() >= due) {
            return Err(PreparationHold::OWNER_READINESS_DEADLINE.to_string().into());
        }
        let revision = ledger().revision;
        let dialog_pending = dialogs.poll(session, cancel)?;
        if !dialog_pending
            && let Some(proof) = observe(session, path)
            && (readiness == PromptReadinessKind::ProvenWarmFollowup
                || super::super::host_input::observe_draft(session)
                    .as_deref()
                    .is_some_and(|capture| composer_prepared(session, capture)))
        {
            return Ok(proof);
        }
        if Instant::now() >= deadline {
            if owner_deadline.is_some_and(|due| Instant::now() >= due) {
                return Err(PreparationHold::OWNER_READINESS_DEADLINE.to_string().into());
            }
            let unproven =
                match watched.expired(|| super::super::host_input::observe_draft(session)) {
                    Expiry::Requiet => {
                        deadline = bounded(Instant::now() + budget.min(super::WINDOW));
                        continue;
                    }
                    Expiry::Unproven(end) => Some(end),
                    Expiry::Waits(_) => None,
                };
            return Err(Stopped {
                error: "timeout waiting for claude tui current Idle/SessionStart evidence and empty composer".into(),
                unproven,
            });
        }
        let state = ledger();
        if state.revision != revision {
            continue;
        }
        let _wait = CHANGED
            .wait_timeout(
                state,
                Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
            )
            .unwrap_or_else(|e| e.into_inner());
    }
}

// Existing managed stash authority may park a draft; source readiness is still required first.
fn composer_prepared(session: &str, capture: &str) -> bool {
    if super::empty_draft(capture) {
        return true;
    }
    #[cfg(unix)]
    if let Some((fence, context)) = super::super::submission_fence::idle_stash_scope()
        && context.available
        && context.target == session
        && fence.idle_stash_admits(session)
        && (fence.stash_any_channel()
            || super::super::busy_inject::stash_enabled(Some(context.channel)))
    {
        return super::super::busy_inject::screen::parkable(capture);
    }
    #[cfg(not(unix))]
    let _ = session;
    false
}
