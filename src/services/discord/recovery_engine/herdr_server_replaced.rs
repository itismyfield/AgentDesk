//! A row whose granted herdr server is gone or replaced: start the server, observe its policy,
//! retire the old execution through the existing proofs, then wake the held queue.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

use super::*;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    self as hosted, HostedCasOutcome, HostedExecution, HostedLocation, HostedLookup,
    HostedLookupKey, HostedObservation, HostedRecord, HostedState,
};
use crate::services::discord::herdr_terminate::root_replaced::{
    self, Recovery, RootReplacedCandidate,
};
use crate::services::herdr_launch::HerdrLaunchEndpoint;
use crate::services::herdr_launch::unplaced::{
    Resolution, UNPLACED_QUIET, Unplaced, resolve_unplaced,
};
use crate::services::session_host::autostart::spawn::{self as autostart, Ensured};
use crate::services::session_host::{GrantedServer, PaneProvider, PaneReading, herdr_endpoints};

/// The channel's user_default endpoint, only when this node executes it.
pub(in crate::services::discord) fn local_endpoint(channel: u64) -> Option<HerdrLaunchEndpoint> {
    let launch = crate::services::discord::router::herdr_consent::endpoint(channel)?;
    let local = crate::config::session_hosts::local_node();
    (local.as_deref() == Some(launch.execution_node.as_str())).then_some(launch)
}

/// Starts the channel's server if it is absent, without waiting.
pub(in crate::services::discord) fn demand(channel: u64) {
    if let Some(launch) = local_endpoint(channel) {
        autostart::demand(&launch);
    }
}

/// Starts the channel's server if it is absent and waits for the start to settle.
pub(in crate::services::discord) async fn ensure_server(channel: u64) {
    if let Some(launch) = local_endpoint(channel) {
        autostart::ensure(&launch).await;
    }
}

pub(in crate::services::discord) async fn canonical_row(
    pool: &sqlx::PgPool,
    token_hash: &str,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Option<HostedObservation> {
    let channel = channel.get().to_string();
    let key = HostedLookupKey::Canonical {
        provider: provider.as_str(),
        identity: CanonicalSessionIdentity {
            kind: SessionIdentityKind::DiscordChannel,
            discord_token_hash: token_hash,
            channel_id: &channel,
        },
    };
    match hosted::load_hosted_execution_pg(pool, key).await {
        HostedLookup::Found(row) => Some(row),
        _ => None,
    }
}

/// The nonce of a Bound row on `launch`, or of a Pending row there or with no pane yet.
pub(in crate::services::discord) fn on_endpoint(
    row: &HostedObservation,
    launch: &HerdrLaunchEndpoint,
) -> Option<String> {
    let HostedRecord::Known(record) = &row.record else {
        return None;
    };
    let here = |at: &HostedLocation| {
        (
            &at.execution_node,
            &at.endpoint_config_key,
            &at.named_session,
        ) == (
            &launch.execution_node,
            &launch.config_key,
            &launch.herdr_session,
        ) && at.socket_addr == launch.socket_addr
    };
    let held = match (record.state, &record.location) {
        (HostedState::Bound, Some(at)) => here(at),
        (HostedState::Pending, None) => true,
        (HostedState::Pending, Some(at)) => here(at) && record.expected.is_none(),
        _ => false,
    };
    held.then(|| record.execution_nonce.clone())
}

pub(in crate::services::discord) async fn granted_server(
    launch: &HerdrLaunchEndpoint,
    nonce: &str,
) -> Option<GrantedServer> {
    let registry = herdr_endpoints();
    registry.launch_host()?;
    let (launch, nonce) = (launch.clone(), nonce.to_owned());
    let read = move || {
        registry
            .launch_host()
            .map(|h| h.granted_server(&launch, &nonce))
    };
    tokio::task::spawn_blocking(read).await.ok().flatten()
}

/// The endpoint's socket has no server and nothing that may start one.
pub(in crate::services::discord) async fn absent(launch: &HerdrLaunchEndpoint) -> bool {
    use crate::services::session_host::autostart::presence::{self, Presence};
    let socket = std::path::PathBuf::from(&launch.socket_addr);
    let read = move || presence::classify(&socket, &presence::HostOs);
    let seen = tokio::task::spawn_blocking(read).await;
    matches!(seen, Ok(Presence::Absent(_)))
}

static RECOVERING: LazyLock<Mutex<HashSet<u64>>> = LazyLock::new(Default::default);

struct Claim(u64);

impl Claim {
    fn take(channel: u64) -> Option<Self> {
        let mut claimed = RECOVERING.lock().unwrap_or_else(PoisonError::into_inner);
        claimed.insert(channel).then_some(Self(channel))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut claimed = RECOVERING.lock().unwrap_or_else(PoisonError::into_inner);
        claimed.remove(&self.0);
    }
}

/// One recovery per channel; a request while one runs is the same request. Held leaves the row,
/// queue and hold file as they are; the queue's backstop asks again.
pub(in crate::services::discord) fn request(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) {
    let Some(claim) = Claim::take(channel.get()) else {
        return;
    };
    let (shared, provider) = (shared.clone(), provider.clone());
    super::task_supervisor::spawn_observed("herdr_server_replaced", async move {
        let _claim = claim;
        let outcome = recover(&shared, &provider, channel).await;
        tracing::info!(
            provider = provider.as_str(),
            channel_id = channel.get(),
            ?outcome,
            "herdr_server_replaced"
        );
        if outcome.is_ok() {
            super::schedule_deferred_idle_queue_kickoff_immediate(
                shared.clone(),
                provider.clone(),
                channel,
                "herdr_server_replaced",
            );
        }
    });
}

async fn recover(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<(), String> {
    let launch = local_endpoint(channel.get()).ok_or("not a local user_default channel")?;
    if let Ensured::Refused(why) = autostart::ensure(&launch).await {
        return Err(format!("server_unavailable: {why}"));
    }
    let host = herdr_endpoints()
        .launch_host()
        .ok_or("launch host unavailable")?;
    host.observe_policy(&launch)
        .await
        .map_err(|why| format!("policy: {why}"))?;
    let pool = shared.pg_pool.as_ref().ok_or("pool unavailable")?;
    let row = canonical_row(pool, &shared.token_hash, provider, channel)
        .await
        .ok_or("row unavailable")?;
    let nonce = on_endpoint(&row, &launch).ok_or("row is not held on this endpoint")?;
    let server = match granted_server(&launch, &nonce).await {
        Some(GrantedServer::Replaced(server)) if server.startup => server,
        other => return Err(format!("no replaced server with startup proof: {other:?}")),
    };
    let HostedRecord::Known(record) = &row.record else {
        return Err("unknown row".into());
    };
    match (record.state, &record.location) {
        (HostedState::Bound, _) => {
            let candidate = RootReplacedCandidate::from_replaced_server(&row, &server)
                .ok_or("not a Bound execution")?;
            match root_replaced::recover(shared, pool, &candidate, None).await {
                Recovery::Retired => Ok(()),
                other => Err(format!("{other:?}")),
            }
        }
        (_, None) => unplaced(shared, pool, record, &launch, channel).await,
        (_, Some(_)) => rebooted_pending(shared, pool, &row, record).await,
    }
}

/// The existing resolver for a Pending that never recorded a pane, under its own lease.
async fn unplaced(
    shared: &SharedData,
    pool: &sqlx::PgPool,
    record: &HostedExecution,
    launch: &HerdrLaunchEndpoint,
    channel: ChannelId,
) -> Result<(), String> {
    let lease = crate::services::discord::UnplacedLease::try_acquire(shared, channel)
        .map_err(|_| "transition busy")?;
    let host = crate::services::claude::herdr_turn::boot_launch_host().ok_or("no launch host")?;
    let target = Unplaced {
        pool,
        owner: &record.owner,
        nonce: &record.execution_nonce,
        endpoint: launch,
        host,
        quiet: UNPLACED_QUIET,
    };
    match resolve_unplaced(&lease, &target).await {
        Resolution::Retired => Ok(()),
        other => Err(format!("unplaced: {other:?}")),
    }
}

/// A Pending whose pane was placed but never confirmed: it has no process to prove absent, so
/// only a reboot after its launch context was made ends it, with its pane gone or a lone shell.
async fn rebooted_pending(
    shared: &SharedData,
    pool: &sqlx::PgPool,
    row: &HostedObservation,
    record: &HostedExecution,
) -> Result<(), String> {
    use crate::services::session_host::{StartIdentity, boot_time};
    let context = crate::services::tui_prompt_dedupe::binding_context::execution_context(
        &record.owner.provider,
        &record.execution_nonce,
    )?;
    let Some(StartIdentity::Darwin { seconds, .. }) = boot_time() else {
        return Err("boot time unread".into());
    };
    let booted = UNIX_EPOCH + Duration::from_secs(seconds);
    if booted <= std::time::SystemTime::from(context.created_at) {
        return Err("pending_server_replaced_without_reboot".into());
    }
    let view = herdr_endpoints()
        .view(record)
        .ok_or("pane view unavailable")?;
    let reading = tokio::task::spawn_blocking(move || view.read_execution())
        .await
        .map_err(|e| e.to_string())?;
    let lone = |reading: &PaneReading| match reading {
        PaneReading::Missing => true,
        PaneReading::Present { provider, .. } => matches!(provider, PaneProvider::Exited),
        PaneReading::Unreadable(_) => false,
    };
    if !lone(&reading) {
        return Err(format!("placed pane still runs: {reading:?}"));
    }
    let channel = ChannelId::new(record.owner.channel_id.parse().map_err(|_| "channel")?);
    let _lease = crate::services::discord::session_transition::SessionTransitionLease::try_acquire(
        shared, channel,
    )
    .map_err(|_| "transition busy")?;
    let provider = ProviderKind::from_str(&record.owner.provider).ok_or("provider")?;
    let now = canonical_row(pool, &shared.token_hash, &provider, channel).await;
    if now.as_ref() != Some(row) {
        return Err("row changed".into());
    }
    let (owner, nonce) = (&record.owner, &record.execution_nonce);
    let placed = record.location.as_ref();
    match crate::services::herdr_launch::retire_own_pending(pool, owner, nonce, placed).await {
        Ok(HostedCasOutcome::Written) => Ok(()),
        other => Err(format!("retire: {other:?}")),
    }
}

#[cfg(test)]
#[path = "herdr_server_replaced_tests.rs"]
mod tests;
