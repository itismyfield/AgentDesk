//! An independent, non-overlapping hourly sweep of ready Claude writers.

use std::collections::BTreeMap;
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
use crate::services::tui_o::writer::binding::{BindingEvents, ChannelBindingLog};
use crate::services::tui_o::writer::host::{Readiness, process_readiness};
use crate::services::tui_o::writer::transition_port::{
    Idle, Operation, Outcome, Reply, Request, Termination, TransitionPort, Verdict,
};

pub(crate) mod termination;

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
        match &hop.outcome {
            Outcome::Retired(_) => self.targets.push(format!("{channel}:{key}")),
            Outcome::Held(reason) => self.skip(format!("held:{reason:?}")),
            Outcome::Unchanged => match &hop.judged.verdict {
                Verdict::Refused(reason) => self.skip(format!("refused:{reason:?}")),
                Verdict::Unknown(reason) => self.skip(format!("unknown:{reason:?}")),
                Verdict::Eligible(_) => self.skip("policy_not_ready:age_or_idle_or_unknown"),
                Verdict::AlreadyDone => self.skip("already_done"),
                Verdict::NotTarget => self.skip("not_target"),
            },
            Outcome::AlreadyDone => self.skip("already_done"),
            Outcome::RetiredThenGrew => self.skip("retired_then_grew"),
            Outcome::Unknown => self.skip("write_unknown"),
        }
    }
}

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
    let channels = ready.transition_channels();
    let channel_count = channels.len();
    let mut stats = Stats::default();
    for channel in channels {
        if let Err(reason) = channel_sweep(registry, &ready, channel, &mut stats).await {
            stats.skip(reason);
        }
    }
    tracing::info!(channels = channel_count, count = stats.targets.len(), targets = ?stats.targets,
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
    channel: u64,
    stats: &mut Stats,
) -> Result<(), &'static str> {
    let shared = registry
        .shared_for_provider_on_channel(&ProviderKind::Claude, ChannelId::new(channel))
        .await
        .ok_or("runtime_unavailable")?;
    let target = target(&shared, channel).await.ok_or("target_unavailable")?;
    let pool = shared.pg_pool.as_ref().ok_or("pg_unavailable")?;
    let port = ready.transition(channel).ok_or("actor_unavailable")?;
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
    let hop_seqs: BTreeMap<_, _> = discovery
        .hops
        .iter()
        .filter_map(|hop| Some((hop.judged.key.clone(), hop.judged.hop_seq?)))
        .collect();
    let candidates = discovery
        .diagnostics
        .ok_or("discovery_diagnostics_unavailable")?
        .candidates;
    for candidate in candidates {
        let Some(seq) = hop_seqs.get(&candidate.key).copied() else {
            stats.skip("hop_seq_unknown");
            continue;
        };
        let Some(pane) = candidate.pane else {
            stats.skip("hop_pane_unknown");
            continue;
        };
        let evidence = {
            let (logical_key, nonce, pane, events) = (
                logical_key.clone(),
                target.nonce.clone(),
                pane.clone(),
                Arc::clone(&events),
            );
            tokio::task::spawn_blocking(move || {
                termination::for_hop(channel, &logical_key, &nonce, seq, &pane, &events)
            })
            .await
            .map_err(|_| "termination_worker_failed")?
        };
        let evidence = match evidence {
            Ok(ended) => ended,
            Err(reason) => {
                stats.skip(reason);
                continue;
            }
        };
        request.job_id = uuid::Uuid::new_v4().to_string();
        request.cutoff = seq;
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
            stats.applied(channel, &candidate.key, &applied);
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
