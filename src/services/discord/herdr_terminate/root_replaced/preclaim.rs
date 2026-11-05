//! Prepares only a locally owned Bound execution already known to need root recovery.

use super::{Recovery, RootReplacedCandidate, candidate_for, current, local, remember_candidate};
use crate::db::dispatched_sessions::hosted_execution::{HostedRecord, HostedState};
use crate::services::discord::recovery_engine::{herdr_reader, herdr_server_replaced as replaced};
use crate::services::discord::{ChannelId, SharedData};
use crate::services::provider::ProviderKind;
use crate::services::session_host::{GrantedServer, herdr_endpoints};
use std::sync::Arc;

const HELD: &str = "NotSent(\"herdr_root_recovery_held\")";

/// Returns true only for a fresh exact match; legacy dequeue may omit its broad server hold.
pub(in crate::services::discord) async fn before_idle(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<bool, String> {
    if !matches!(provider, ProviderKind::Claude | ProviderKind::Codex) {
        return Ok(false);
    }
    let Some(endpoint) = crate::config::session_hosts::herdr_endpoint(channel.get()) else {
        return Ok(false);
    };
    if crate::config::session_hosts::local_node().as_deref()
        != Some(endpoint.execution_node.as_str())
    {
        return Ok(false);
    }
    let Some(pool) = shared.pg_pool.as_ref() else {
        return Ok(false);
    };
    let Some(row) = replaced::canonical_row(pool, &shared.token_hash, provider, channel).await
    else {
        return Ok(false);
    };
    let HostedRecord::Known(record) = &row.record else {
        return Ok(false);
    };
    if record.state != HostedState::Bound
        || local(shared, record).ok().as_ref() != Some(&(provider.clone(), channel))
        || current(pool, &row).await.is_err()
    {
        return Ok(false);
    }
    let previous = candidate_for(&row);
    let server_replaced = if previous.is_none() {
        match replaced::local_endpoint(channel.get()) {
            Some(launch) => matches!(
                replaced::granted_server(&launch, &record.execution_nonce).await,
                Some(GrantedServer::Replaced(_))
            ),
            None => false,
        }
    } else {
        false
    };
    if previous.is_none() && !server_replaced {
        return Ok(false);
    }
    let observed = match herdr_reader::prepare_root_replaced_candidate(&row) {
        Ok(probe) => tokio::task::spawn_blocking(move || probe.observe())
            .await
            .ok(),
        Err(_) => None,
    };
    let candidate = match observed {
        Some(Ok(None)) => {
            if let (Some(host), Some(at)) = (herdr_endpoints().launch_host(), &record.location) {
                let launch = crate::services::herdr_launch::HerdrLaunchEndpoint {
                    execution_node: at.execution_node.clone(),
                    config_key: at.endpoint_config_key.clone(),
                    socket_addr: at.socket_addr.clone(),
                    herdr_session: at.named_session.clone(),
                };
                host.prepare_existing_policy(&launch, &record.execution_nonce)
                    .await
                    .map_err(|_| HELD.to_string())?;
            }
            return Ok(true);
        }
        Some(Ok(Some(candidate))) => candidate,
        // A replaced server supplies only the candidate; strict recovery still proves its end.
        _ => previous.unwrap_or_else(|| RootReplacedCandidate(row.clone())),
    };
    remember_candidate(&candidate);
    match super::recover(shared, pool, &candidate, None).await {
        Recovery::Retired => Ok(false),
        Recovery::Held(_) | Recovery::RetiredCleanupPending(_) => Err(HELD.to_string()),
    }
}
