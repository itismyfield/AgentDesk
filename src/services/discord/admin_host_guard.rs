//! Host check for admin commands, HTTP routes and diagnostics that reach a session by its
//! tmux name: another or unknown host is reported as such, never probed or changed as tmux.

use std::sync::Arc;

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
/// `false` when the channel has no session to take it back.
pub(crate) async fn return_intake_input(
    shared: &SharedData,
    channel_id: ChannelId,
    (uploads, was_cleared): (
        crate::services::cluster::attachment_transfer::uploads::PendingUploads,
        Option<bool>,
    ),
) -> bool {
    let mut data = shared.core.lock().await;
    let Some(session) = data.sessions.get_mut(&channel_id) else {
        let (channel_id, lost) = (channel_id.get(), uploads.len());
        tracing::warn!(
            channel_id,
            lost,
            "no session to return a refused intake's input to"
        );
        return false;
    };
    session.pending_uploads.splice(0..0, uploads);
    if let Some(cleared) = was_cleared {
        session.cleared = cleared;
    }
    true
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
        // A Herdr-configured channel keeps its provider state whatever the runtime's backend.
        return reset_provider_state.then(|| configured_refusal(channel_id.get()))?;
    }
    let name = session_channel_name(shared, channel_id).await;
    let reason = reset_refusal(shared, provider, channel_id, explicit_session_key, name).await?;
    let channel_id = channel_id.get();
    tracing::warn!(channel_id, %reason, "managed session reset refused");
    Some(reason)
}

/// Where a channel clear goes, judged before its first change; the clear command consumes it.
#[allow(dead_code)]
pub(crate) enum ResetTarget {
    /// Main's managed tmux reset.
    LegacyTmux,
    /// The provider's own `/clear` through its host's adapter; the pane is kept.
    NativeClear(HostAdapter),
    Refused(String),
}

#[allow(dead_code)]
pub(crate) enum HostAdapter {
    #[cfg(unix)]
    Herdr(Box<crate::services::session_host::HerdrClearPlan>),
}

/// A channel clear's target. With the Herdr turn switch off or no Herdr endpoint for the channel
/// it is exactly [`managed_reset_refusal`]'s verdict.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) async fn clear_reset_target(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    explicit_session_key: Option<&str>,
    session_key: Option<&str>,
) -> ResetTarget {
    #[cfg(unix)]
    if crate::services::turn_host::herdr_turn_switched_on()
        && crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_some()
    {
        let pool = shared.pg_pool.as_ref();
        let read = read_herdr_clear(pool, provider, channel_id.get(), session_key).await;
        let read = match read {
            Ok(read) => read,
            Err(reason) => return ResetTarget::Refused(reason),
        };
        let judged = tokio::task::spawn_blocking(move || read.target()).await;
        return judged.unwrap_or_else(|error| ResetTarget::Refused(error.to_string()));
    }
    #[cfg(not(unix))]
    let _ = session_key;
    let refusal = managed_reset_refusal;
    match refusal(
        shared,
        provider,
        channel_id,
        true,
        false,
        explicit_session_key,
    )
    .await
    {
        Some(reason) => ResetTarget::Refused(reason),
        None => ResetTarget::LegacyTmux,
    }
}

/// What a configured channel's clear is judged on: its row as the turn host reads it, and O's
/// rotation projection, both read before any Herdr I/O.
#[cfg(unix)]
pub(crate) struct HerdrClearRead {
    channel: u64,
    row: Option<crate::db::dispatched_sessions::hosted_execution::HostedRecord>,
    unsettled: Option<usize>,
}

#[cfg(unix)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn read_herdr_clear(
    pool: Option<&PgPool>,
    provider: &ProviderKind,
    channel: u64,
    session_key: Option<&str>,
) -> Result<HerdrClearRead, String> {
    use crate::services::turn_host::{TurnHost, for_turn};
    let plan = match for_turn(pool, provider, channel, session_key).await {
        TurnHost::Herdr(plan) => plan,
        TurnHost::Refused(refusal) => return Err(refusal.to_string()),
        TurnHost::Tmux => return Err("herdr clear: the channel resolved to tmux".into()),
    };
    let row = plan.row.map(|observed| observed.record);
    let unsettled = crate::services::tui_o::writer::host::rotation_unsettled(channel);
    Ok(HerdrClearRead {
        channel,
        row,
        unsettled,
    })
}

#[cfg(unix)]
#[cfg_attr(not(test), allow(dead_code))]
impl HerdrClearRead {
    /// Plans the clear on the pane; it blocks on the socket.
    pub(crate) fn target(self) -> ResetTarget {
        let plan = crate::services::session_host::plan_clear;
        match plan(self.channel, self.row.as_ref(), self.unsettled) {
            Ok(plan) => ResetTarget::NativeClear(HostAdapter::Herdr(Box::new(plan))),
            Err(refusal) => ResetTarget::Refused(refusal.to_string()),
        }
    }
}

/// Why a Herdr-configured channel's tmux session is left as it is, whatever its stored rows say.
pub(crate) fn configured_refusal(channel_id: u64) -> Option<String> {
    let endpoint = crate::config::session_hosts::herdr_endpoint(channel_id)?;
    let key = endpoint.key;
    Some(format!(
        "Herdr 설정 채널(endpoint `{key}`)이라 tmux 세션을 바꾸지 않아요"
    ))
}

/// The in-memory `channel_name` first, then the registered fallback name; with neither, only
/// the channel's own row admits the reset.
async fn reset_refusal(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    explicit_session_key: Option<&str>,
    channel_name: Option<String>,
) -> Option<String> {
    if let Some(reason) = configured_refusal(channel_id.get()) {
        return Some(reason);
    }
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
    if let Some(name) = channel_name {
        let tmux_name = provider.build_tmux_session_name(&name);
        return channel_refusal(shared, provider, channel_id.get(), &tmux_name).await;
    }
    let deferred = super::host_defer_gate::nameless_channel_deferred;
    deferred(shared, provider, channel_id.get())
        .await
        .then(|| "채널 이름이 없어 세션 호스트를 legacy tmux로 확인하지 못했어요".to_string())
}

async fn session_channel_name(shared: &SharedData, channel_id: ChannelId) -> Option<String> {
    let data = shared.core.lock().await;
    let session = data.sessions.get(&channel_id);
    session.and_then(|session| session.channel_name.clone())
}

/// A provider-state reset of one channel judged once, read only, on the runtime, channel name,
/// key and turn it acts on; [`ManagedResetVerdict::apply`] runs on that target without judging again.
#[must_use]
pub(crate) struct ManagedResetVerdict {
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    channel_name: Option<String>,
    tmux_name: Option<String>,
    refusal: Option<String>,
}

/// The verdict a runtime clear reaches for these inputs, taken before the caller's first write;
/// `None` when no runtime is registered for the provider, so there is nothing to clear.
pub(crate) async fn managed_reset_verdict(
    registry: &super::health::HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
    session_key: Option<&str>,
) -> Option<ManagedResetVerdict> {
    let provider = ProviderKind::from_str(provider_name)?;
    let shared = registry.shared_for_provider(&provider).await?;
    let channel_name = session_channel_name(&shared, channel_id).await;
    let by_key = || session_key.and_then(super::session_identity::tmux_name_from_session_key);
    let named = channel_name.as_deref();
    let tmux_name = named.map(|name| provider.build_tmux_session_name(name));
    let tmux_name = tmux_name.or_else(by_key);
    let name = channel_name.clone();
    let mut refusal = reset_refusal(&shared, &provider, channel_id, session_key, name).await;
    // The turn this reset stops must run the judged session: its token, watcher and inflight row.
    let holds = super::host_teardown_gate::runtime_target_holds;
    if refusal.is_none() && !holds(&shared, &provider, channel_id, tmux_name.as_deref()).await {
        refusal = Some("the channel's runtime holds a session other than the judged one".into());
    }
    Some(ManagedResetVerdict {
        shared,
        provider,
        channel_id,
        channel_name,
        tmux_name,
        refusal,
    })
}

impl ManagedResetVerdict {
    pub(crate) fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    pub(crate) fn refusal(&self) -> Option<&str> {
        self.refusal.as_deref()
    }

    /// Clears the approved channel's turn, provider session and managed process. A refusal, or a
    /// runtime whose session changed since the verdict, changes nothing.
    pub(crate) async fn apply(self) -> ManagedReset {
        if let Some(reason) = self.refusal {
            return ManagedReset::Refused(reason);
        }
        let (shared, provider, channel_id) = (self.shared, self.provider, self.channel_id);
        let approved = self.tmux_name.as_deref();
        // An identity fence on the approved names only; the host is not judged again.
        let holds = super::host_teardown_gate::runtime_target_holds;
        if session_channel_name(&shared, channel_id).await != self.channel_name
            || !holds(&shared, &provider, channel_id, approved).await
        {
            let reason = "the channel's session changed after its reset was approved";
            return ManagedReset::Refused(reason.to_string());
        }
        let cleared = super::mailbox_clear_channel(&shared, &provider, channel_id).await;
        if let Some(token) = cleared.removed_token {
            let policy = super::TmuxCleanupPolicy::PreserveSession;
            let stop = super::turn_bridge::stop_approved_turn;
            stop(
                &provider,
                &token,
                Some(approved),
                policy,
                "auto-queue slot clear",
            )
            .await;
            super::saturating_decrement_global_active(&shared);
        }
        {
            let mut data = shared.core.lock().await;
            if let Some(session) = data.sessions.get_mut(&channel_id) {
                super::settings::cleanup_channel_uploads(channel_id);
                session.clear_provider_session();
                session.history.clear();
                session.pending_uploads.clear();
                session.cleared = true;
            }
        }
        #[cfg(unix)]
        if let Some(name) = self.tmux_name.as_deref() {
            if provider.uses_managed_tmux_backend() {
                super::commands::reset_managed_process_session(name);
            }
        }
        ManagedReset::Applied(self.tmux_name)
    }
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
