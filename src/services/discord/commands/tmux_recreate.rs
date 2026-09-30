use poise::serenity_prelude as serenity;

use super::super::SharedData;
use crate::services::provider::ProviderKind;

/// Hard reset of a channel's tmux session: kill it and clear its temp files, but only
/// once the host guard admits it through the channel's sessions row.
#[cfg(unix)]
pub(super) async fn recreate_channel_tmux(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    session_name: &str,
    reset_source: &str,
) -> bool {
    let key = super::super::adk_session::build_namespaced_session_key(
        &shared.token_hash,
        provider,
        session_name,
    );
    let Some(session) = super::super::inflight::clear_channel_session(
        shared.pg_pool.as_ref(),
        provider,
        channel_id.get(),
        Some(&key),
        session_name,
        reset_source,
    )
    .await
    else {
        return false;
    };
    if !crate::services::platform::tmux::has_session(session.name()) {
        return false;
    }
    crate::services::tmux_diagnostics::record_tmux_exit_reason(
        session.name(),
        &format!("hard reset via {reset_source}"),
    );
    let killed = crate::services::platform::tmux::kill_session(
        session.name(),
        &format!("hard reset via {reset_source}"),
    );
    if killed {
        // #892: delete persistent + legacy session temp files so the next
        // turn starts from a clean slate in the canonical location.
        crate::services::tmux_common::cleanup_cleared_session_temp_files(&session);
    }
    killed
}

#[cfg(not(unix))]
pub(super) async fn recreate_channel_tmux(
    _: &SharedData,
    _: &ProviderKind,
    _: serenity::ChannelId,
    _: &str,
    _: &str,
) -> bool {
    false
}
