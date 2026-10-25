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
    Hop, INSPECT_DEADLINE, Idle, Operation, Outcome, Reply, Request, Termination, TransitionPort,
    Verdict,
};

pub(crate) mod termination;
use crate::services::tui_o::writer::actor::codex_retention;

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

fn codex_actor_error(reason: &'static str, suspended: bool) -> &'static str {
    #[cfg(test)]
    if std::env::var("AGENTDESK_BO_MUTANT").is_ok_and(|m| m == "codex_suspended") {
        return reason;
    }
    if suspended {
        "codex_suspended_sources"
    } else {
        reason
    }
}

async fn codex_send(
    ready: &Readiness,
    channel: u64,
    port: &TransitionPort,
    operation: Operation,
    context: Context,
) -> Result<Reply, &'static str> {
    let send = send(ready, channel, port, operation, context);
    #[cfg(test)]
    if std::env::var("AGENTDESK_BO_MUTANT").is_ok_and(|m| m == "codex_inspect_timeout") {
        return send.await;
    }
    let budget = INSPECT_DEADLINE
        + crate::services::tui_o::writer::rotation::RETIRE_QUIET
        + Duration::from_secs(10);
    let result = tokio::time::timeout(budget, send)
        .await
        .map_err(|_| "codex_inspect_timeout")?;
    result.map_err(|reason| codex_actor_error(reason, !port.sources_unsuspended()))
}

async fn channel_sweep(
    registry: &HealthRegistry,
    ready: &Readiness,
    provider: &ProviderKind,
    channel: u64,
    stats: &mut Stats,
) -> Result<(), &'static str> {
    let port = if *provider == ProviderKind::Codex {
        ready.observation_transition(channel)
    } else {
        ready.transition(channel)
    }
    .ok_or("actor_unavailable")?;
    let expected = match provider {
        ProviderKind::Claude => ShadowProvider::Claude,
        ProviderKind::Codex => ShadowProvider::Codex,
        _ => return Err("provider_unsupported"),
    };
    if port.provider() != Some(expected) {
        return Ok(());
    }
    if expected == ShadowProvider::Codex && !port.sources_unsuspended() {
        return Err(codex_actor_error("actor_reply_unavailable", true));
    }
    let shared = registry
        .shared_for_provider_on_channel(provider, ChannelId::new(channel))
        .await
        .ok_or("runtime_unavailable")?;
    if expected == ShadowProvider::Codex {
        let context = match resolve_codex_context(registry, Arc::clone(&shared), channel).await {
            Ok(context) => context,
            Err(unavailable) => {
                #[cfg(test)]
                if std::env::var("AGENTDESK_BO_MUTANT").is_ok_and(|m| m == "prepare_reason") {
                    stats.skip("codex_context_unavailable");
                    return Ok(());
                }
                let component = unavailable.component.as_str();
                let reason = unavailable.reason;
                tracing::info!(
                    channel,
                    component,
                    reason,
                    "[tui_o] Codex retention observation unknown"
                );
                stats.skip(format!("codex_context_unavailable:{component}"));
                return Ok(());
            }
        };
        let mut request = Request {
            channel,
            cutoff: u64::MAX,
            job_id: uuid::Uuid::new_v4().to_string(),
            operator: "automatic-retention".into(),
            at: Utc::now(),
            reason: "read-only Codex execution observation".into(),
            termination: None,
        };
        let reply = codex_send(
            ready,
            channel,
            &port,
            Operation::Inspect(request.clone()),
            context,
        )
        .await?;
        #[cfg(test)]
        test_support::record_codex_reply(reply.clone());
        if stats.actor_refused(&reply) {
            return Ok(());
        }
        tracing::info!(channel, diagnostics = ?reply.diagnostics, execution = ?reply.execution,
            "[tui_o] Codex retention observation");
        #[cfg(test)]
        if codex_retention::mutant("producer_unwired") {
            stats.skip("codex_inspect_only");
            return Ok(());
        }
        let diagnostics = reply
            .diagnostics
            .as_ref()
            .ok_or("codex_diagnostics_unavailable")?;
        let Some(crate::services::tui_o::writer::codex_snapshot::Observation::Complete(snapshot)) =
            &diagnostics.codex
        else {
            stats.skip("codex_observation_unknown");
            return Ok(());
        };
        if diagnostics.candidates.is_empty() {
            stats.skip("codex_inspect_only");
        }
        for candidate in &diagnostics.candidates {
            let Some(seq) = reply
                .hops
                .iter()
                .find(|hop| hop.judged.key == candidate.key)
                .and_then(|hop| hop.judged.hop_seq)
            else {
                stats.skip("codex_retention_hop_unknown");
                continue;
            };
            let Some(pane) = &candidate.pane else {
                stats.skip("codex_retention_pane_unknown");
                continue;
            };
            let (snapshot, key, source, pane) = (
                snapshot.clone(),
                candidate.key.clone(),
                candidate.source.clone(),
                pane.clone(),
            );
            let evidence = tokio::task::spawn_blocking(move || {
                codex_retention::for_hop(&snapshot, &key, &source, seq, &pane)
            })
            .await
            .map_err(|_| "codex_retention_worker_failed")?;
            let evidence = match evidence {
                Ok(evidence) => evidence,
                Err(reason) => {
                    stats.skip(reason);
                    continue;
                }
            };
            request.job_id = uuid::Uuid::new_v4().to_string();
            request.cutoff = evidence.hop_seq;
            request.reason =
                "non-current source with at least seven days since last dialogue".into();
            request.termination = Some(Termination::Retention(Box::new(evidence)));
            let context = resolve_codex_context(registry, Arc::clone(&shared), channel)
                .await
                .map_err(|_| "codex_inspect_context_unavailable")?;
            let inspected = codex_send(
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
            let idle = Idle(Arc::new(move || {
                let runtime = Arc::clone(&runtime);
                Box::pin(async move { super::automatic_idle::idle(&runtime, channel).await })
            }));
            let context = resolve_codex_context(registry, Arc::clone(&shared), channel)
                .await
                .map_err(|_| "codex_apply_context_unavailable")?;
            let applied = codex_send(
                ready,
                channel,
                &port,
                Operation::AutomaticApply(request.clone(), scope.digest, idle),
                context,
            )
            .await?;
            if !stats.actor_refused(&applied) {
                stats.applied(channel, &candidate.key, &applied);
            }
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

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    thread_local! {
        static CODEX_REPLIES: std::cell::RefCell<Vec<Reply>> = const { std::cell::RefCell::new(Vec::new()) };
        static CODEX_SKIPS: std::cell::RefCell<BTreeMap<String, usize>> = const { std::cell::RefCell::new(BTreeMap::new()) };
    }

    pub(crate) fn codex_skips_for_test() -> BTreeMap<String, usize> {
        CODEX_SKIPS.with(|slot| slot.borrow().clone())
    }

    pub(super) fn record_codex_reply(reply: Reply) {
        CODEX_REPLIES.with(|slot| slot.borrow_mut().push(reply));
    }

    pub(crate) async fn codex_sweep_for_test(
        registry: &HealthRegistry,
        ready: &Readiness,
        channel: u64,
    ) -> Result<Reply, &'static str> {
        CODEX_REPLIES.with(|slot| slot.borrow_mut().clear());
        let mut stats = Stats::default();
        let result =
            channel_sweep(registry, ready, &ProviderKind::Codex, channel, &mut stats).await;
        if let Err(reason) = result {
            stats.skip(reason);
        }
        CODEX_SKIPS.with(|slot| *slot.borrow_mut() = stats.skipped);
        result?;
        CODEX_REPLIES
            .with(|slot| slot.borrow_mut().pop())
            .ok_or("no Codex Inspect was sent")
    }

    #[cfg(unix)]
    #[test]
    fn provider_ticks_only_observe_the_matching_ready_actor() {
        use crate::services::discord::health::o_transition::codex_context::tests::Fixture;
        let fixture = Fixture::new(false);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let registry = HealthRegistry::new();
            for (provider, channel) in [("codex", 7), ("claude", 8)] {
                let shared = fixture.shared(None);
                shared.settings.write().await.allowed_channel_ids = vec![channel];
                registry.register(provider.into(), shared).await;
            }
            crate::services::tui_o::writer::tests::with_ready_codex_and_claude(
                fixture.source.clone(),
                |ready| async move {
                    let mut channels = ready.transition_channels();
                    channels.sort();
                    assert_eq!(
                        channels,
                        [7, 8],
                        "both providers are ready during both ticks"
                    );
                    CODEX_REPLIES.with(|slot| slot.borrow_mut().clear());
                    let mut claude = Stats::default();
                    for channel in &channels {
                        let result = channel_sweep(
                            &registry,
                            &ready,
                            &ProviderKind::Claude,
                            *channel,
                            &mut claude,
                        )
                        .await;
                        assert_eq!(
                            result,
                            if *channel == 8 {
                                Err("target_unavailable")
                            } else {
                                Ok(())
                            },
                            "Claude visits only its own ready actor, without requiring PG for Codex"
                        );
                    }
                    assert!(CODEX_REPLIES.with(|slot| slot.borrow().is_empty()));
                    let mut codex = Stats::default();
                    for channel in &channels {
                        assert_eq!(
                            channel_sweep(
                                &registry,
                                &ready,
                                &ProviderKind::Codex,
                                *channel,
                                &mut codex
                            )
                            .await,
                            Ok(())
                        );
                    }
                    let replies =
                        CODEX_REPLIES.with(|slot| std::mem::take(&mut *slot.borrow_mut()));
                    assert_eq!(
                        replies.len(),
                        1,
                        "the Codex tick must not target the Claude actor"
                    );
                    let reply = &replies[0];
                    assert!(reply.refused.is_none(), "{reply:?}");
                    let diagnostics =
                        serde_json::to_value(reply.diagnostics.as_ref().unwrap()).unwrap();
                    assert!(
                        diagnostics["codex"]["Complete"].is_object(),
                        "{diagnostics}"
                    );
                    assert_eq!(codex.skipped.get("codex_inspect_only"), Some(&1));
                    assert!(
                        codex.targets.is_empty() && claude.targets.is_empty(),
                        "observation grants no retirement authority"
                    );
                },
            )
            .await;
        });
    }
}
