//! Host check for admin commands, HTTP routes and diagnostics that reach a session by its
//! tmux name: another or unknown host is reported as such, never probed or changed as tmux.

use poise::serenity_prelude::ChannelId;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::SharedData;
use crate::services::provider::ProviderKind;

/// Why a channel's session may not be changed or observed by its tmux name. A found legacy
/// row, or no row yet with no marker or inflight trace of another host, keeps main's path.
pub(super) async fn channel_refusal(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: u64,
    tmux_name: &str,
) -> Option<String> {
    // Test runtimes built without a pool predate the guard; production requires PostgreSQL.
    #[cfg(test)]
    if shared.pg_pool.is_none() {
        return None;
    }
    let deferred = super::host_defer_gate::channel_session_deferred;
    deferred(shared, provider, channel_id, tmux_name)
        .await
        .then(|| {
            format!("`{tmux_name}`의 호스트가 legacy tmux로 확인되지 않아요 (Herdr·미확인·충돌)")
        })
}

/// Why a reset that kills or recreates the channel's managed session may not touch it.
pub(super) async fn managed_reset_refusal(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    reset_provider_state: bool,
    recreate_tmux: bool,
) -> Option<String> {
    let kills = reset_provider_state && provider.uses_managed_tmux_backend();
    if !(kills || recreate_tmux) {
        return None;
    }
    let tmux_name = {
        let data = shared.core.lock().await;
        let session = data.sessions.get(&channel_id)?;
        provider.build_tmux_session_name(session.channel_name.as_ref()?)
    };
    let reason = channel_refusal(shared, provider, channel_id.get(), &tmux_name).await?;
    let channel_id = channel_id.get();
    tracing::warn!(channel_id, tmux_name, %reason, "managed session reset refused");
    Some(reason)
}

/// [`channel_refusal`] for a caller holding the session's own key: only a found legacy row
/// with no host trace admits it.
pub(crate) async fn session_key_refusal(
    pool: &PgPool,
    provider: Option<&ProviderKind>,
    channel_id: Option<u64>,
    session_key: &str,
    tmux_name: &str,
) -> Option<String> {
    let channel = channel_id.filter(|id| *id != 0).map(ChannelId::new);
    let refusal = super::host_defer_gate::resume_host_refusal;
    refusal(pool, provider, channel, session_key, tmux_name).await
}

/// The `.host_kind` marker check alone, for a path holding only a tmux name.
pub(crate) fn marker_refusal(tmux_name: &str) -> Option<String> {
    let tmux = super::host_liveness::local_tmux(tmux_name, None);
    (!tmux).then(|| format!("the host marker of {tmux_name} is not tmux"))
}

/// A provider-auth login target: only the auth namespace with no other host's marker.
pub(crate) fn auth_login_refusal(tmux_name: &str) -> Option<String> {
    if !tmux_name.starts_with("adk-login-") {
        return Some(format!("{tmux_name} is not a provider-auth login session"));
    }
    marker_refusal(tmux_name)
}

/// What a diagnostic shows in place of the tmux observation the host check refused.
pub(crate) fn unsupported_observation(tmux_name: &str, reason: &str) -> Value {
    json!({"state": "host_unsupported", "tmux_session": tmux_name, "reason": reason})
}

#[cfg(all(test, unix))]
#[path = "admin_host_guard_tests.rs"]
mod tests;
