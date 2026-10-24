//! Observes the clear's existing effect; it grants no teardown or launch permission.

use tokio::sync::OwnedMutexGuard;

use crate::services::codex_tui::execution_end::{self, capture::CapturedOldExecution};
use crate::services::provider::ProviderKind;

pub(super) fn capture_for(
    provider: &ProviderKind,
    channel: u64,
    pane: &str,
    transition: &Option<OwnedMutexGuard<()>>,
) -> Option<CapturedOldExecution> {
    if !matches!(provider, ProviderKind::Codex)
        || crate::config::session_hosts::herdr_endpoint(channel).is_some()
        || execution_end::mutant("observer_off")
    {
        return None;
    }
    if transition.is_none() {
        tracing::warn!(channel, pane, pid = ?None::<u32>, pgid = ?None::<u32>,
            reason = "transition guard unavailable", "Codex clear end not captured");
        return None;
    }
    execution_end::capture::capture(channel, pane)
        .inspect_err(|reason| tracing::warn!(channel, pane, pid = ?None::<u32>, pgid = ?None::<u32>, reason, "Codex clear end not captured"))
        .ok()
}

pub(super) async fn commit_confirmed(
    captured: Option<CapturedOldExecution>,
    (reset, unconfirmed): (bool, bool),
) {
    if let Some(captured) = captured {
        execution_end::confirm::record_clear(captured, reset, unconfirmed).await;
    }
}
