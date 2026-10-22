//! An independent, non-overlapping hourly sweep of ready Claude writers.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use poise::serenity_prelude::ChannelId;

use super::*;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind, resolve_session_row_pg,
};
use crate::services::discord::SharedData;
use crate::services::session_host::{HostKind, HostLiveness, HostSessionRef, host_for};
use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};
use crate::services::tui_o::store::spool::ExecutionEnd;
use crate::services::tui_o::writer::host::{Readiness, process_readiness};
use crate::services::tui_o::writer::transition_port::{
    Idle, Operation, Reply, Request, Termination, TransitionPort,
};

pub(crate) fn spawn(registry: Arc<HealthRegistry>, provider: &ProviderKind) {
    if *provider != ProviderKind::Claude {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            sweep(&registry).await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}

async fn sweep(registry: &HealthRegistry) {
    let ready = process_readiness();
    for channel in ready.transition_channels() {
        let work = channel_sweep(registry, &ready, channel);
        // An unavailable actor or host never delays the other channels indefinitely.
        let _ = tokio::time::timeout(Duration::from_secs(240), work).await;
    }
}

async fn target(shared: &SharedData, channel: u64) -> Option<Target> {
    let pool = shared.pg_pool.as_ref()?;
    let id = channel.to_string();
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: &shared.token_hash,
        channel_id: &id,
    };
    let (_, session_key) = resolve_session_row_pg(pool, None, Some("claude"), Some(identity))
        .await
        .ok()??;
    let key = HostedLookupKey::SessionKey(&session_key);
    let HostedLookup::Found(row) = load_hosted_execution_pg(pool, key).await else {
        return None;
    };
    let HostedRecord::Known(record) = row.record else {
        return None;
    };
    Some(Target {
        channel,
        session_key,
        nonce: record.execution_nonce,
    })
}

async fn send(
    ready: &Readiness,
    channel: u64,
    port: &TransitionPort,
    operation: Operation,
    context: Context,
) -> Option<Reply> {
    let reply = port.request(operation, context).await.ok()?;
    ready
        .with_transition_reply(channel, port, reply, Clone::clone)
        .ok()
        .filter(|reply| reply.refused.is_none())
}

async fn channel_sweep(registry: &HealthRegistry, ready: &Readiness, channel: u64) -> Option<()> {
    let shared = registry
        .shared_for_provider_on_channel(&ProviderKind::Claude, ChannelId::new(channel))
        .await?;
    let target = target(&shared, channel).await?;
    let pool = shared.pg_pool.as_ref()?;
    let port = ready.transition(channel)?;
    let mut request = Request {
        channel,
        cutoff: u64::MAX,
        job_id: uuid::Uuid::new_v4().to_string(),
        operator: "automatic-retention".into(),
        at: Utc::now(),
        reason: "non-current source with at least seven days since last dialogue".into(),
        termination: None,
    };
    let context = resolve_context(registry, pool, &target).await.ok()?;
    let discovery = send(
        ready,
        channel,
        &port,
        Operation::Inspect(request.clone()),
        context,
    )
    .await?;
    let panes: BTreeSet<String> = discovery
        .diagnostics?
        .candidates
        .into_iter()
        .filter_map(|candidate| candidate.pane)
        .collect();
    for pane in panes {
        if !ended(pane.clone()).await {
            continue;
        }
        request.job_id = uuid::Uuid::new_v4().to_string();
        request.termination = Some(Termination::Confirmed(ExecutionEnd {
            host: crate::config::session_hosts::local_node()?,
            pane: pane.clone(),
            first_seq: 0,
            last_seq: request.cutoff,
            basis: "known historical host reports execution dead or absent".into(),
        }));
        let context = resolve_context(registry, pool, &target).await.ok()?;
        let inspected = send(
            ready,
            channel,
            &port,
            Operation::Inspect(request.clone()),
            context,
        )
        .await?;
        let scope = inspected.scope?.ok()?;
        let runtime = Arc::clone(&shared);
        let idle = Idle(Arc::new(move || {
            let (shared, pane) = (Arc::clone(&runtime), pane.clone());
            Box::pin(async move {
                super::automatic_idle::idle(&shared, channel).await && ended(pane).await
            })
        }));
        let context = resolve_context(registry, pool, &target).await.ok()?;
        let operation = Operation::AutomaticApply(request.clone(), scope.digest, idle);
        send(ready, channel, &port, operation, context).await?;
    }
    Some(())
}

async fn ended(pane: String) -> bool {
    tokio::task::spawn_blocking(move || {
        // Unknown historical hosts are retained; a Herdr logical name is never a tmux pane.
        let HostKindMarker::Known(kind @ (HostKind::Tmux | HostKind::Process)) =
            read_host_kind_marker(&pane)
        else {
            return false;
        };
        host_for(kind).liveness(HostSessionRef { kind, name: &pane }) == HostLiveness::DeadOrAbsent
    })
    .await
    .unwrap_or(false)
}
