//! A thread turn about to cold-resume a Claude session no hook verified starts fresh instead.

use super::super::super::SharedData;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;
use std::sync::Arc;

/// The session strategy code of a turn that replaced its unverified stored session.
pub(in crate::services::discord) const REASON: &str = "thread_unverified_provider_session";

/// The session a turn selected, as its caller holds it.
pub(in crate::services::discord) struct Selected<'a> {
    pub session_id: &'a mut Option<String>,
    pub memento_context_loaded: &'a mut bool,
    pub reason: &'a mut &'static str,
}

/// Whether this turn would cold-launch a Claude pane resuming `native` only to be refused: the turn
/// runs Claude, no verified binding names `native` in the thread, and its pane is observed gone.
#[cfg(unix)]
async fn replaces(provider: &ProviderKind, channel: u64, tmux: Option<&str>, native: &str) -> bool {
    use crate::services::platform::tmux::PaneLiveness;
    use crate::services::tmux_diagnostics::probe_tmux_session_pane_liveness as pane;
    use crate::services::tui_o::cutover::claude_thread::cold_resume_unverified;
    let Some(tmux) = tmux.filter(|_| matches!(provider, ProviderKind::Claude)) else {
        return false;
    };
    cold_resume_unverified(channel, native) && pane(tmux).await == PaneLiveness::DeadOrAbsent
}

#[cfg(not(unix))]
async fn replaces(_: &ProviderKind, _: u64, _: Option<&str>, _: &str) -> bool {
    false
}

/// Drops the selected session before the prompt is built, so the turn carries the thread's recent
/// messages and launches without a resume; the channel is told once per replaced session.
pub(in crate::services::discord) async fn apply(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux: Option<&str>,
    selected: Selected<'_>,
) {
    let Some(native) = selected.session_id.as_deref() else {
        return;
    };
    if !replaces(provider, channel_id.get(), tmux, native).await {
        return;
    }
    let replaced = selected.session_id.take().unwrap_or_default();
    if let Some(session) = shared.core.lock().await.sessions.get_mut(&channel_id) {
        session.restore_provider_session(None);
    }
    *selected.memento_context_loaded = false;
    *selected.reason = REASON;
    let pool = shared.pg_pool.as_ref();
    crate::services::claude::fresh_notice::announce_unverified(pool, channel_id.get(), &replaced)
        .await;
}

#[cfg(all(test, unix))]
#[path = "unverified_thread_resume_tests.rs"]
pub(super) mod tests;
