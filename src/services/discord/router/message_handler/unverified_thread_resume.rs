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

/// The turn's pane name and session key, as its caller resolved them.
pub(in crate::services::discord) type Target<'a> = (Option<&'a str>, Option<&'a str>);

/// Whether this Claude turn would cold-launch a tmux pane resuming `native` only to be refused:
/// no verified binding names `native`, the host reads as tmux and the pane is observed gone.
#[cfg(unix)]
async fn replaces(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: u64,
    (tmux, session_key): Target<'_>,
    native: &str,
) -> bool {
    use crate::services::platform::tmux::PaneLiveness;
    use crate::services::tmux_diagnostics::probe_tmux_session_pane_liveness as pane;
    use crate::services::tui_o::cutover::claude_thread::cold_resume_unverified;
    use crate::services::turn_host::{TurnHost, for_observation};
    let Some(tmux) = tmux.filter(|_| matches!(provider, ProviderKind::Claude)) else {
        return false;
    };
    if !cold_resume_unverified(channel, native) {
        return false;
    }
    let pool = shared.pg_pool.as_ref();
    matches!(
        for_observation(pool, provider, channel, session_key).await,
        TurnHost::Tmux
    ) && pane(tmux).await == PaneLiveness::DeadOrAbsent
}

#[cfg(not(unix))]
async fn replaces(_: &SharedData, _: &ProviderKind, _: u64, _: Target<'_>, _: &str) -> bool {
    false
}

/// Drops the selected session before the prompt is built, so the turn carries the thread's recent
/// messages and launches without a resume; the channel is told once per replaced session.
pub(in crate::services::discord) async fn apply(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    target: Target<'_>,
    selected: Selected<'_>,
) {
    let Some(native) = selected.session_id.as_deref() else {
        return;
    };
    if !replaces(shared, provider, channel_id.get(), target, native).await {
        return;
    }
    let replaced = selected.session_id.take().unwrap_or_default();
    if let Some(session) = shared.core.lock().await.sessions.get_mut(&channel_id) {
        session.restore_provider_session(None);
    }
    *selected.memento_context_loaded = false;
    *selected.reason = REASON;
    // The notice module exists where the judgment can be true.
    #[cfg(unix)]
    crate::services::claude::fresh_notice::announce_unverified(
        shared.pg_pool.as_ref(),
        channel_id.get(),
        &replaced,
    )
    .await;
    #[cfg(not(unix))]
    drop(replaced);
}

#[cfg(all(test, unix))]
#[path = "unverified_thread_resume_tests.rs"]
pub(super) mod tests;
