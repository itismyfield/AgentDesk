//! Verified incarnations stay intact until turn-scoped admission is available.

use crate::services::{
    provider::{CancelToken, cancel_requested},
    tui_prompt_dedupe as dedupe,
};
use std::{sync::Arc, time::Duration};

#[cfg(test)]
pub(crate) static HOLD_ENTERED_FOR_TESTS: std::sync::Mutex<
    Option<std::sync::mpsc::Sender<String>>,
> = std::sync::Mutex::new(None);

pub(crate) fn existing_incarnation(tmux: &str) -> bool {
    dedupe::codex_verified_requires_proof(tmux) && !dedupe::clear_relaunch::relaunch_open(tmux)
}

pub(crate) fn existing_incarnation_observed(
    tmux: &str,
    observation: &crate::services::discord::codex_absence::InflightObs,
) -> bool {
    dedupe::clear_relaunch::absent_capture(tmux, observation).is_none()
        && existing_incarnation(tmux)
}

pub(crate) fn wait_for_cancel(tmux: &str, cancel: Option<&Arc<CancelToken>>) -> Result<(), String> {
    #[cfg(test)]
    if let Some(sender) = HOLD_ENTERED_FOR_TESTS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
    {
        let _ = sender.send(tmux.to_owned());
    }
    tracing::info!(
        tmux_session = tmux,
        reason = dedupe::codex_verified_hold_detail(tmux),
        "Codex verified turn is waiting for admission; preserving its source and pane"
    );
    while !cancel_requested(cancel.map(Arc::as_ref)) {
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

pub(crate) fn preflight(
    tmux: &str,
    cancel: Option<&Arc<CancelToken>>,
) -> Option<Result<(), String>> {
    existing_incarnation(tmux).then(|| wait_for_cancel(tmux, cancel))
}
