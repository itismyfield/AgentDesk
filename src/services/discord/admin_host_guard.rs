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

/// A managed reset's outcome; a refusal is never read as a channel with nothing to reset.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub(crate) enum ManagedReset {
    /// The reset ran; the channel's tmux name, when it has one.
    Applied(Option<String>),
    /// Nothing changed: the session's host is not a confirmed legacy tmux one.
    Refused(String),
}

impl ManagedReset {
    /// Tells the channel why `command` was not applied; `true` when it was refused.
    pub(crate) async fn report(
        &self,
        http: &poise::serenity_prelude::Http,
        channel_id: ChannelId,
        command: &str,
    ) -> bool {
        let Self::Refused(reason) = self else {
            return false;
        };
        let notice = format!("{command}을(를) 적용하지 않았어요: {reason}");
        if let Err(error) = channel_id.say(http, notice).await {
            tracing::warn!(channel_id = channel_id.get(), %error, "refusal notice failed");
        }
        true
    }
}

/// Puts back the input an intake took from `channel_id` when a refused reset stops it before
/// its turn: the uploads lead the channel's pending list again and its clear flag returns.
pub(crate) async fn return_intake_input(
    shared: &SharedData,
    channel_id: ChannelId,
    (uploads, was_cleared): (
        crate::services::cluster::attachment_transfer::uploads::PendingUploads,
        Option<bool>,
    ),
) {
    let mut data = shared.core.lock().await;
    let Some(session) = data.sessions.get_mut(&channel_id) else {
        let (channel_id, lost) = (channel_id.get(), uploads.len());
        tracing::warn!(
            channel_id,
            lost,
            "no session to return a refused intake's input to"
        );
        return;
    };
    session.pending_uploads.splice(0..0, uploads);
    if let Some(cleared) = was_cleared {
        session.cleared = cleared;
    }
}

/// Why a reset that kills or recreates the channel's managed session may not touch it. A
/// clear naming its target's own key is judged on that key before the channel's session.
pub(super) async fn managed_reset_refusal(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    reset_provider_state: bool,
    recreate_tmux: bool,
    explicit_session_key: Option<&str>,
) -> Option<String> {
    let kills = reset_provider_state && provider.uses_managed_tmux_backend();
    if !(kills || recreate_tmux) {
        return None;
    }
    let reason = reset_refusal(shared, provider, channel_id, explicit_session_key).await?;
    let channel_id = channel_id.get();
    tracing::warn!(channel_id, %reason, "managed session reset refused");
    Some(reason)
}

/// The in-memory name first, then the registered fallback name; with neither, only the
/// channel's own row admits the reset.
async fn reset_refusal(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    explicit_session_key: Option<&str>,
) -> Option<String> {
    // Test runtimes built without a pool predate the guard; production requires PostgreSQL.
    #[cfg(test)]
    if shared.pg_pool.is_none() {
        return None;
    }
    if let Some(key) = explicit_session_key {
        let Some(pool) = shared.pg_pool.as_ref() else {
            return Some("no postgres pool to read the session's host".to_string());
        };
        let Some(identity) = super::session_identity::SessionIdentity::parse(key) else {
            return Some(format!("`{key}` names no tmux session"));
        };
        let name = identity.tmux_name.as_str();
        let refused = session_key_refusal(pool, Some(provider), Some(channel_id.get()), key, name);
        if let Some(reason) = refused.await {
            return Some(format!("`{name}`: {reason}"));
        }
    }
    let channel_name = {
        let data = shared.core.lock().await;
        let session = data.sessions.get(&channel_id);
        session.and_then(|session| session.channel_name.clone())
    };
    if let Some(name) = channel_name {
        let tmux_name = provider.build_tmux_session_name(&name);
        return channel_refusal(shared, provider, channel_id.get(), &tmux_name).await;
    }
    let deferred = super::host_defer_gate::nameless_channel_deferred;
    deferred(shared, provider, channel_id.get())
        .await
        .then(|| "채널 이름이 없어 세션 호스트를 legacy tmux로 확인하지 못했어요".to_string())
}

/// Why a stale or orphan turn may not release its mailbox and counters: its session, named by
/// the watcher, the inflight row or the channel in that order, is not a confirmed legacy one.
pub(super) async fn turn_release_refusal(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Option<String> {
    // Test runtimes built without a pool predate the guard; production requires PostgreSQL.
    #[cfg(test)]
    if shared.pg_pool.is_none() {
        return None;
    }
    let name = match shared.tmux_watchers.channel_binding(&channel_id) {
        Some(binding) => Some(binding.tmux_session_name),
        None => {
            let row = super::inflight::load_inflight_state_read_only_result;
            match row(provider, channel_id.get()) {
                Ok(row) => row.and_then(|row| row.tmux_session_name),
                Err(error) => {
                    return Some(format!("the turn's inflight row is unreadable: {error}"));
                }
            }
        }
    };
    let name = match name.filter(|name| !name.trim().is_empty()) {
        Some(name) => Some(name),
        None if provider.uses_managed_tmux_backend() => {
            let data = shared.core.lock().await;
            let session = data.sessions.get(&channel_id);
            let channel_name = session.and_then(|session| session.channel_name.as_deref());
            channel_name.map(|name| provider.build_tmux_session_name(name))
        }
        None => None,
    };
    if let Some(name) = name {
        return channel_refusal(shared, provider, channel_id.get(), &name).await;
    }
    let deferred = super::host_defer_gate::nameless_channel_deferred;
    let held = deferred(shared, provider, channel_id.get()).await;
    held.then(|| "the turn's channel names no session to confirm as legacy tmux".to_string())
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

/// [`session_key_refusal`] on a stored row's provider and channel columns.
pub(crate) async fn row_refusal(
    pool: &PgPool,
    provider: Option<&str>,
    channel_id: Option<&str>,
    session_key: &str,
    tmux_name: &str,
) -> Option<String> {
    let provider = provider.and_then(ProviderKind::from_str);
    let channel = channel_id.and_then(|raw| raw.trim().parse::<u64>().ok());
    session_key_refusal(pool, provider.as_ref(), channel, session_key, tmux_name).await
}

/// What a diagnostic of a stored row shows in place of the tmux observations its host check
/// refuses; `None` keeps main's tmux readings.
pub(crate) async fn row_unsupported(
    pool: &PgPool,
    provider: Option<&str>,
    channel_id: Option<&str>,
    session_key: &str,
    tmux_name: Option<&str>,
) -> Option<Value> {
    let tmux_name = tmux_name?;
    let reason = row_refusal(pool, provider, channel_id, session_key, tmux_name).await?;
    Some(json!({"state": "host_unsupported", "tmux_session": tmux_name, "reason": reason}))
}

#[cfg(all(test, unix))]
#[path = "admin_host_guard_tests.rs"]
pub(crate) mod tests;
