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
    let cleared = admitted_session(shared, provider, channel_id, session_name, reset_source);
    let Some(session) = cleared.await else {
        return false;
    };
    crate::services::platform::tmux::has_session(session.name())
        && kill_and_clean(&session, &format!("hard reset via {reset_source}"))
}

/// The channel's tmux session as the host guard admits it through its sessions row.
#[cfg(unix)]
async fn admitted_session(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    session_name: &str,
    reset_source: &str,
) -> Option<crate::services::session_host::ClearedHostSession> {
    let key = super::super::adk_session::build_namespaced_session_key(
        &shared.token_hash,
        provider,
        session_name,
    );
    super::super::inflight::clear_channel_session(
        shared.pg_pool.as_ref(),
        provider,
        channel_id.get(),
        Some(&key),
        session_name,
        reset_source,
    )
    .await
}

/// Kills an admitted session and, once it is gone, deletes its temp files so the next turn starts
/// from a clean slate in the canonical location.
#[cfg(unix)]
fn kill_and_clean(
    session: &crate::services::session_host::ClearedHostSession,
    reason: &str,
) -> bool {
    crate::services::tmux_diagnostics::record_tmux_exit_reason(session.name(), reason);
    let killed = crate::services::platform::tmux::kill_session(session.name(), reason);
    if killed {
        crate::services::tmux_common::cleanup_cleared_session_temp_files(session);
    }
    killed
}

/// What restarting a channel's TUI in place did. Its provider session id, DB selector and account
/// markers are never touched, so the next launch resumes the same session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum Relaunch {
    Relaunched,
    /// The TUI is busy with a turn of its own; nothing was ended.
    Deferred,
    /// The host guard keeps this session; nothing was ended.
    Refused(String),
    /// The kill failed and the pane still runs.
    KillFailed,
}

/// Ends a channel's tmux session the way a hard reset does, but puts its account markers back
/// after the cleanup, so the launch that follows keeps its session under the account it ran on.
#[cfg(unix)]
pub(in crate::services::discord) async fn relaunch_channel_tmux_keeping_session(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    session_name: &str,
    reset_source: &str,
) -> Relaunch {
    let busy = crate::services::discord::router::hosted_tui_promote_readiness_blocked;
    if busy(shared, provider, channel_id).await {
        return Relaunch::Deferred;
    }
    let refusal = crate::services::discord::admin_host_guard::managed_reset_refusal;
    if let Some(reason) = refusal(shared, provider, channel_id, false, true, None).await {
        return Relaunch::Refused(reason);
    }
    let cleared = admitted_session(shared, provider, channel_id, session_name, reset_source);
    let Some(session) = cleared.await else {
        return Relaunch::Refused(format!("host check kept `{session_name}`"));
    };
    if !crate::services::platform::tmux::has_session(session.name()) {
        return Relaunch::Relaunched;
    }
    let auth = crate::services::tmux_common::AuthMarkers::read(session.name());
    if !kill_and_clean(&session, &format!("hard reset via {reset_source}")) {
        return Relaunch::KillFailed;
    }
    if let Err(error) = auth.restore(session.name()) {
        tracing::warn!(tmux_session_name = session.name(), %error, "relaunch kept no account marker");
    }
    Relaunch::Relaunched
}

#[cfg(not(unix))]
pub(in crate::services::discord) async fn relaunch_channel_tmux_keeping_session(
    _: &std::sync::Arc<SharedData>,
    _: &ProviderKind,
    _: serenity::ChannelId,
    _: &str,
    _: &str,
) -> Relaunch {
    Relaunch::Refused("tmux is unix only".to_string())
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
