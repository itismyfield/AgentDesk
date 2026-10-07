//! Verified incarnations stay intact until turn-scoped admission is available.

use crate::services::{
    provider::{CancelToken, cancel_requested},
    tui_prompt_dedupe as dedupe,
};
use std::{sync::Arc, time::Duration};

pub(crate) fn existing_incarnation(tmux: &str) -> bool {
    dedupe::codex_verified_requires_proof(tmux)
}

pub(crate) fn wait_for_cancel(tmux: &str, cancel: Option<&Arc<CancelToken>>) -> Result<(), String> {
    tracing::info!(
        tmux_session = tmux,
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
