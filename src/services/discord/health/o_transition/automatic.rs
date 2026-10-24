//! An independent, non-overlapping hourly sweep, scoped to each ready writer's provider.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use poise::serenity_prelude::ChannelId;

use super::*;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind, resolve_session_row_pg,
};
use crate::services::discord::SharedData;
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::store::spool::ExecutionEnd;
use crate::services::tui_o::writer::binding::{BindingEvent, BindingEvents, ChannelBindingLog};
use crate::services::tui_o::writer::host::{Readiness, process_readiness};
use crate::services::tui_o::writer::transition_port::{
    Hop, Idle, Operation, Outcome, Reply, Request, Termination, TransitionPort, Verdict,
};

pub(crate) mod termination;

pub(crate) type Decision = (String, Result<ExecutionEnd, &'static str>);

pub(crate) fn plan(
    discovery: &Reply,
    events: &[BindingEvent],
    execution: &termination::Execution<'_>,
) -> Vec<Decision> {
    let Some(diagnostics) = &discovery.diagnostics else {
        return Vec::new();
    };
    let hop_seqs: BTreeMap<_, _> = discovery
        .hops
        .iter()
        .filter_map(|hop| Some((hop.judged.key.clone(), hop.judged.hop_seq?)))
        .collect();
    #[cfg(test)]
    let hop_seqs =
        if std::env::var("AGENTDESK_OTRANSITION_MUTANT").is_ok_and(|m| m == "candidate_index") {
            diagnostics
                .candidates
                .iter()
                .enumerate()
                .map(|(index, candidate)| (candidate.key.clone(), index as u64))
                .collect()
        } else {
            hop_seqs
        };
    diagnostics
        .candidates
        .iter()
        .map(|candidate| {
            let decision = (|| {
                let seq = hop_seqs
                    .get(&candidate.key)
                    .copied()
                    .ok_or("hop_seq_unknown")?;
                let pane = candidate.pane.as_deref().ok_or("hop_pane_unknown")?;
                termination::for_hop(execution, seq, pane, events)
            })();
            (candidate.key.clone(), decision)
        })
        .collect()
}

pub(crate) fn skip_key(hop: &Hop) -> Option<String> {
    Some(match &hop.outcome {
        Outcome::Retired(_) => return None,
        Outcome::Held(reason) => format!("held:{reason:?}"),
        Outcome::Unchanged => match &hop.judged.verdict {
            Verdict::Refused(reason) => format!("refused:{reason:?}"),
            Verdict::Unknown(reason) => format!("unknown:{reason:?}"),
            Verdict::Eligible(_) => "unchanged".into(),
            Verdict::AlreadyDone => "already_done".into(),
            Verdict::NotTarget => "not_target".into(),
        },
        Outcome::AlreadyDone => "already_done".into(),
        Outcome::RetiredThenGrew => "retired_then_grew".into(),
        Outcome::Unknown => "write_unknown".into(),
    })
}

#[derive(Default)]
struct Stats {
    targets: Vec<String>,
    skipped: BTreeMap<String, usize>,
}

impl Stats {
    fn skip(&mut self, reason: impl Into<String>) {
        *self.skipped.entry(reason.into()).or_default() += 1;
    }

    fn actor_refused(&mut self, reply: &Reply) -> bool {
        if let Some(reason) = &reply.refused {
            self.skip(format!("actor_refused:{reason:?}"));
            return true;
        }
        false
    }

    fn applied(&mut self, channel: u64, key: &str, reply: &Reply) {
        let Some(hop) = reply.hops.iter().find(|hop| hop.judged.key == key) else {
            self.skip("candidate_not_reported");
            return;
        };
        if let Some(reason) = skip_key(hop) {
            self.skip(reason);
        } else {
            self.targets.push(format!("{channel}:{key}"));
        }
    }
}

pub(crate) fn spawn(registry: Arc<HealthRegistry>, provider: &ProviderKind) {
    if !matches!(provider, ProviderKind::Claude | ProviderKind::Codex) {
        return;
    }
    let provider = provider.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            sweep(&registry, &provider).await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}

async fn sweep(registry: &HealthRegistry, provider: &ProviderKind) {
    let ready = process_readiness();
    let channels = ready.transition_channels();
    let channel_count = channels.len();
    let mut stats = Stats::default();
    for channel in channels {
        if let Err(reason) = channel_sweep(registry, &ready, provider, channel, &mut stats).await {
            stats.skip(reason);
        }
    }
    tracing::info!(provider = provider.as_str(), channels = channel_count, count = stats.targets.len(), targets = ?stats.targets,
        skip_reasons = ?stats.skipped, "[tui_o] automatic retention sweep");
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
) -> Result<Reply, &'static str> {
    let reply = port
        .request(operation, context)
        .await
        .map_err(|_| "actor_reply_unavailable")?;
    ready
        .with_transition_reply(channel, port, reply, Clone::clone)
        .map_err(|_| "stale_actor_reply")
}

async fn channel_sweep(
    registry: &HealthRegistry,
    ready: &Readiness,
    provider: &ProviderKind,
    channel: u64,
    stats: &mut Stats,
) -> Result<(), &'static str> {
    let port = ready.transition(channel).ok_or("actor_unavailable")?;
    let expected = match provider {
        ProviderKind::Claude => ShadowProvider::Claude,
        ProviderKind::Codex => ShadowProvider::Codex,
        _ => return Err("provider_unsupported"),
    };
    if port.provider() != Some(expected) {
        return Ok(());
    }
    let shared = registry
        .shared_for_provider_on_channel(provider, ChannelId::new(channel))
        .await
        .ok_or("runtime_unavailable")?;
    if expected == ShadowProvider::Codex {
        let context = resolve_codex_context(registry, shared, channel)
            .await
            .map_err(|_| "codex_context_unavailable")?;
        let request = Request {
            channel,
            cutoff: u64::MAX,
            job_id: uuid::Uuid::new_v4().to_string(),
            operator: "automatic-retention".into(),
            at: Utc::now(),
            reason: "read-only Codex execution observation".into(),
            termination: None,
        };
        let reply = send(ready, channel, &port, Operation::Inspect(request), context).await?;
        if !stats.actor_refused(&reply) {
            tracing::info!(channel, diagnostics = ?reply.diagnostics, execution = ?reply.execution,
                "[tui_o] Codex retention observation");
            stats.skip("codex_inspect_only");
        }
        return Ok(());
    }
    let target = target(&shared, channel).await.ok_or("target_unavailable")?;
    let pool = shared.pg_pool.as_ref().ok_or("pg_unavailable")?;
    let events = tokio::task::spawn_blocking(move || {
        ChannelBindingLog::new(channel, ShadowProvider::Claude).binding_events_since(channel, 0)
    })
    .await
    .map_err(|_| "binding_log_worker_failed")?
    .map_err(|_| "binding_log_unreadable")?;
    let events = Arc::new(events);
    let mut request = Request {
        channel,
        cutoff: u64::MAX,
        job_id: uuid::Uuid::new_v4().to_string(),
        operator: "automatic-retention".into(),
        at: Utc::now(),
        reason: "non-current source with at least seven days since last dialogue".into(),
        termination: None,
    };
    let context = resolve_context(registry, pool, &target)
        .await
        .map_err(|_| "discovery_context_unavailable")?;
    let logical_key = context.tmux.clone();
    let discovery = send(
        ready,
        channel,
        &port,
        Operation::Inspect(request.clone()),
        context,
    )
    .await?;
    if stats.actor_refused(&discovery) {
        return Ok(());
    }
    let panes: BTreeSet<_> = discovery
        .diagnostics
        .as_ref()
        .ok_or("discovery_diagnostics_unavailable")?
        .candidates
        .iter()
        .filter_map(|candidate| candidate.pane.clone())
        .filter(|pane| pane != &logical_key)
        .collect();
    let mut ended_panes = BTreeSet::new();
    for pane in panes {
        if ended(pane.clone()).await {
            ended_panes.insert(pane);
        }
    }
    let host = crate::config::session_hosts::local_node().ok_or("local_node_unknown")?;
    let decisions = {
        let (logical_key, nonce) = (logical_key.clone(), target.nonce.clone());
        tokio::task::spawn_blocking(move || {
            let execution = termination::Execution {
                channel,
                logical_key: &logical_key,
                nonce: &nonce,
                host: &host,
                ended_panes: &ended_panes,
            };
            plan(&discovery, &events, &execution)
        })
        .await
        .map_err(|_| "termination_worker_failed")?
    };
    for (key, evidence) in decisions {
        let evidence = match evidence {
            Ok(ended) => ended,
            Err(reason) => {
                stats.skip(reason);
                continue;
            }
        };
        let pane = evidence.pane.clone();
        request.job_id = uuid::Uuid::new_v4().to_string();
        request.cutoff = evidence.last_seq;
        request.termination = Some(Termination::Confirmed(evidence));
        let context = resolve_context(registry, pool, &target)
            .await
            .map_err(|_| "inspect_context_unavailable")?;
        let inspected = send(
            ready,
            channel,
            &port,
            Operation::Inspect(request.clone()),
            context,
        )
        .await?;
        if stats.actor_refused(&inspected) {
            continue;
        }
        let scope = match inspected.scope {
            Some(Ok(scope)) => scope,
            Some(Err(reason)) => {
                stats.skip(format!("scope_refused:{reason}"));
                continue;
            }
            None => {
                stats.skip("scope_unavailable");
                continue;
            }
        };
        let runtime = Arc::clone(&shared);
        let historical_pane = pane != logical_key;
        let idle = Idle(Arc::new(move || {
            let (shared, pane) = (Arc::clone(&runtime), pane.clone());
            Box::pin(async move {
                super::automatic_idle::idle(&shared, channel).await
                    && (!historical_pane || ended(pane).await)
            })
        }));
        let context = resolve_context(registry, pool, &target)
            .await
            .map_err(|_| "apply_context_unavailable")?;
        let operation = Operation::AutomaticApply(request.clone(), scope.digest, idle);
        let applied = send(ready, channel, &port, operation, context).await?;
        if !stats.actor_refused(&applied) {
            stats.applied(channel, &key, &applied);
        }
    }
    Ok(())
}

async fn ended(pane: String) -> bool {
    tokio::task::spawn_blocking(move || {
        crate::services::discord::host_liveness::historical_execution_ended(&pane)
    })
    .await
    .unwrap_or(false)
}
