use sqlx::PgPool;

use super::herdr_reader::SocketHerdrReader;
use super::host_reconcile::{
    HerdrEndpointId, HerdrExecutionReader, HerdrPaneReading, HostReconcile, NoHerdrEndpoint,
    reconcile_hosted,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLookup, HostedObservation, HostedRecord, HostedState,
    refresh_bound_pane_pg,
};
use crate::services::discord::herdr_terminate::root_replaced;
use crate::services::discord::session_transition::SessionTransitionLease;
use crate::services::discord::tui_prompt_relay::herdr_source::{
    HerdrSourceAttach, attach_restarted_herdr_source, awaits_own_clear, nonce_baseline,
    prepare_restarted_binding, publish_restarted_binding,
};
use crate::services::discord::{ChannelId, SharedData};
use crate::services::herdr_launch::clear_epoch;
use crate::services::provider::ProviderKind;
use crate::services::session_host::{
    HerdrPaneView, HostKind, PaneReading, ServerWitness, herdr_endpoints, restore_execution_fence,
};
use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};
use crate::services::tmux_common::with_tmux_source_authority;
use crate::services::tui_prompt_dedupe::binding_context::{
    SpawnNonceMarker, observe_spawn_nonce_marker,
};
use crate::services::tui_prompt_dedupe::{
    Persisted, admit_herdr_execution, runtime_binding_for_tmux_session_under_source_authority,
};

pub(super) struct VerifiedTarget<'a> {
    pub(super) shared: &'a SharedData,
    pub(super) transition: &'a SessionTransitionLease<'a>,
}

pub(super) enum RestartAttachment {
    Legacy(HerdrSourceAttach),
    Verified(VerifiedReattach),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum VerifiedReattach {
    Live,
    AwaitingClear,
    Ended,
    Held,
}

pub(in crate::services::discord) async fn continue_channel(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    transition: &SessionTransitionLease<'_>,
) -> Option<VerifiedReattach> {
    use super::herdr_server_replaced::canonical_row;
    if !crate::config::session_hosts::has_herdr_endpoint(channel.get()) {
        return None;
    }
    let pool = shared.pg_pool.as_ref()?;
    let row = canonical_row(pool, &shared.token_hash, provider, channel).await?;
    let HostedRecord::Known(record) = &row.record else {
        return None;
    };
    if record.state != HostedState::Bound || root_replaced::local(shared, record).is_err() {
        return None;
    }
    if already_admitted(shared, record, channel) {
        return Some(VerifiedReattach::Live);
    }
    Some(continue_owner(shared, pool, transition, &row).await.0)
}

pub(in crate::services::discord) fn already_admitted(
    shared: &SharedData,
    record: &HostedExecution,
    channel: ChannelId,
) -> bool {
    use crate::services::tui_prompt_dedupe::{herdr_listing, runtime_binding_for_tmux_session};
    let key = &record.owner.logical_key;
    root_replaced::local(shared, record)
        .is_ok_and(|(provider, owner)| provider == shared.provider && owner == channel)
        && herdr_listing(key)
            .is_some_and(|(nonce, admitted)| admitted && nonce == record.execution_nonce)
        && runtime_binding_for_tmux_session(key).is_some()
        && !awaits_own_clear(channel.get(), key, &record.execution_nonce)
        && super::inflight::load_inflight_state_read_only(&shared.provider, channel.get())
            .is_none_or(|row| {
                super::inflight::row_is_current_generation(&row, shared.restart.current_generation)
            })
}

/// The owner's channel lease is the same authority the boot path uses.
pub(in crate::services::discord) async fn verify_owner(
    shared: &SharedData,
    pool: &PgPool,
    transition: &SessionTransitionLease<'_>,
    observed: &HostedObservation,
) -> VerifiedReattach {
    verify_with(shared, pool, transition, observed, Verification::default()).await
}

/// Resolve the usual name before consulting the previous placement. All paths still publish
/// under the channel transition and the current clear epoch.
pub(in crate::services::discord) async fn continue_owner(
    shared: &std::sync::Arc<SharedData>,
    pool: &PgPool,
    transition: &SessionTransitionLease<'_>,
    observed: &HostedObservation,
) -> (VerifiedReattach, HostedObservation) {
    let held = || (VerifiedReattach::Held, observed.clone());
    let HostedRecord::Known(record) = &observed.record else {
        return held();
    };
    let Ok((provider, channel)) = root_replaced::local(shared, record) else {
        return held();
    };
    if record.state != HostedState::Bound
        || provider != shared.provider
        || !transition.matches(shared, channel)
    {
        return held();
    }
    let registry = herdr_endpoints();
    let Some(host) = registry.launch_host() else {
        let result = verify_owner(shared, pool, transition, observed).await;
        return finish_continuation(shared, observed.clone(), result).await;
    };
    let stored = record.clone();
    let Ok(Ok(named)) = tokio::task::spawn_blocking(move || {
        let Some((witness, panes)) = host.named_panes(&stored)? else {
            return Ok(None);
        };
        let mut live = panes.iter().filter(|pane| {
            let mut candidate = stored.clone();
            if let Some(at) = candidate.location.as_mut() {
                at.pane_id.clone_from(pane);
            }
            registry
                .view(&candidate)
                .and_then(|view| view.observe_execution())
                .is_some_and(|(server, reading)| {
                    server == witness
                        && matches!(
                            reading,
                            PaneReading::Present {
                                provider: crate::services::session_host::PaneProvider::Execution(_),
                                ..
                            }
                        )
                })
        });
        let selected = live.next();
        if live.next().is_some() {
            return Err("multiple live agent panes in the named workspace".to_string());
        }
        match selected {
            Some(pane) => Ok(Some((witness, pane.clone()))),
            None if panes
                .iter()
                .any(|pane| Some(pane) == stored.location.as_ref().map(|at| &at.pane_id)) =>
            {
                Ok(None)
            }
            None => Err("named workspace is present without a confirmed live pane".into()),
        }
    })
    .await
    else {
        return held();
    };
    let Some((witness, pane)) = named else {
        let result = verify_owner(shared, pool, transition, observed).await;
        return finish_continuation(shared, observed.clone(), result).await;
    };
    if root_replaced::current(pool, observed).await.is_err() {
        return held();
    }
    let Ok(Some(row)) = refresh_bound_pane_pg(pool, observed, pane).await else {
        return held();
    };
    let result = verify_selected(
        shared,
        pool,
        transition,
        &row,
        Verification::default(),
        Some(witness),
    )
    .await;
    finish_continuation(shared, row, result).await
}

async fn finish_continuation(
    shared: &std::sync::Arc<SharedData>,
    row: HostedObservation,
    result: VerifiedReattach,
) -> (VerifiedReattach, HostedObservation) {
    let live = matches!(
        result,
        VerifiedReattach::Live | VerifiedReattach::AwaitingClear
    );
    let result = if live && !super::restore_inflight::reattach_live_herdr(shared, &row).await {
        VerifiedReattach::Held
    } else {
        result
    };
    (result, row)
}

struct CapturedPane {
    endpoint: HerdrEndpointId,
    pane_id: String,
    reading: HerdrPaneReading,
}

impl HerdrExecutionReader for CapturedPane {
    fn endpoint(&self) -> Option<&HerdrEndpointId> {
        Some(&self.endpoint)
    }

    fn read_pane(&self, pane_id: &str) -> HerdrPaneReading {
        if pane_id == self.pane_id {
            self.reading.clone()
        } else {
            HerdrPaneReading::Unreadable("capture is for another pane".into())
        }
    }
}

pub(super) async fn reconnect_source(
    pool: &PgPool,
    observed: &HostedObservation,
    verified: Option<VerifiedTarget<'_>>,
) -> RestartAttachment {
    // The current boot caller only restores the existing source. The new path needs a target
    // handed over by the caller that coordinates input and resume presentation.
    if let Some(target) = verified {
        return RestartAttachment::Verified(
            verify_owner(target.shared, pool, target.transition, observed).await,
        );
    }
    let HostedRecord::Known(record) = &observed.record else {
        return RestartAttachment::Legacy(HerdrSourceAttach::Refused(HostReconcile::Unresolved(
            "no hosted execution".into(),
        )));
    };
    let Ok(channel) = record.owner.channel_id.parse::<u64>() else {
        return RestartAttachment::Legacy(HerdrSourceAttach::Refused(HostReconcile::Unresolved(
            "channel id".into(),
        )));
    };
    let attached = match SocketHerdrReader::of(record) {
        Some(reader) => attach_restarted_herdr_source(pool, &record.owner, channel, &reader).await,
        None => attach_restarted_herdr_source(pool, &record.owner, channel, &NoHerdrEndpoint).await,
    };
    tracing::info!(channel, ?attached, "herdr reconnect after restart");
    RestartAttachment::Legacy(attached)
}

struct Verification {
    live: fn(&HostReconcile) -> bool,
    stale: fn(&HostedExecution, &SpawnNonceMarker) -> bool,
    ended: fn(&HerdrPaneView, &HostedExecution, &PaneReading) -> bool,
    #[cfg(test)]
    omit: Option<&'static str>,
}

impl Default for Verification {
    fn default() -> Self {
        Self {
            live: HostReconcile::admits_reconnect,
            stale: stale_nonce,
            ended: HerdrPaneView::restart_ended,
            #[cfg(test)]
            omit: None,
        }
    }
}

async fn verify_with(
    shared: &SharedData,
    pool: &PgPool,
    transition: &SessionTransitionLease<'_>,
    observed: &HostedObservation,
    checks: Verification,
) -> VerifiedReattach {
    verify_selected(shared, pool, transition, observed, checks, None).await
}

async fn verify_selected(
    shared: &SharedData,
    pool: &PgPool,
    transition: &SessionTransitionLease<'_>,
    observed: &HostedObservation,
    checks: Verification,
    named: Option<ServerWitness>,
) -> VerifiedReattach {
    use VerifiedReattach::{Ended, Held};
    let HostedRecord::Known(record) = &observed.record else {
        return Held;
    };
    let Ok((provider, channel)) = root_replaced::local(shared, record) else {
        return Held;
    };
    if record.state != HostedState::Bound
        || shared.provider != provider
        || !transition.matches(shared, channel)
    {
        return Held;
    }
    let owner = &record.owner;
    let epoch = clear_epoch::snapshot(&owner.provider, &owner.discord_token_hash, channel.get());
    let (Some(reader), Some(location)) = (SocketHerdrReader::of(record), &record.location) else {
        return Held;
    };
    let stored = record.clone();
    let ended = checks.ended;
    let Ok(Some((witness, reading, exited))) =
        tokio::task::spawn_blocking(move || reader.observe(&stored, ended)).await
    else {
        return Held;
    };
    let captured = CapturedPane {
        endpoint: HerdrEndpointId::of(location),
        pane_id: location.pane_id.clone(),
        reading: reading.clone(),
    };
    let verdict = reconcile_hosted(&HostedLookup::Found(observed.clone()), &captured);
    let named_live = named.as_ref() == Some(&witness)
        && matches!(&reading, HerdrPaneReading::Present(evidence) if evidence.provider_process.is_some());
    if !(exited || named_live || (checks.live)(&verdict)) || (named.is_some() && !named_live) {
        return Held;
    }

    // Reading and scanning a cold rollout can take time. Prepare before the final DB and
    // pane observations, and leave the global watcher registry available to other channels.
    let source = (!exited)
        .then(|| nonce_baseline(channel.get(), &owner.logical_key, &record.execution_nonce))
        .flatten();
    let prepared = source.as_ref().and_then(|source| {
        let live = with_tmux_source_authority(
            &owner.logical_key,
            runtime_binding_for_tmux_session_under_source_authority,
        );
        prepare_restarted_binding(owner, source, live)
    });

    let current = root_replaced::current(pool, observed).await.is_ok();
    #[cfg(test)]
    let current = current || checks.omit == Some("row");
    if !current {
        return Held;
    }
    let Some(reader) = SocketHerdrReader::of(record) else {
        return Held;
    };
    let stored = record.clone();
    let launch = herdr_endpoints().launch_host();
    let unchanged = tokio::task::spawn_blocking(move || {
        if named_live
            && !launch
                .and_then(|host| host.named_panes(&stored).ok().flatten())
                .is_some_and(|(current, panes)| {
                    current == witness && panes.contains(&captured.pane_id)
                })
        {
            return false;
        }
        reader.observe(&stored, ended) == Some((witness, reading, exited))
    })
    .await;
    let unchanged = matches!(unchanged, Ok(true));
    #[cfg(test)]
    let unchanged = unchanged || checks.omit == Some("last-pane");
    if !unchanged {
        return Held;
    }

    let permit = epoch.begin_effect();
    #[cfg(test)]
    let permit = match checks.omit {
        Some("epoch") => {
            clear_epoch::snapshot(&owner.provider, &owner.discord_token_hash, channel.get())
                .begin_effect()
        }
        _ => permit,
    };
    let Some(permit) = permit else {
        return Held;
    };
    let result = with_tmux_source_authority(&owner.logical_key, |authority| {
        let nonce = observe_spawn_nonce_marker(&owner.logical_key);
        if (checks.stale)(record, &nonce) {
            return Held;
        }
        let host = read_host_kind_marker(&owner.logical_key);
        if exited {
            return match host {
                HostKindMarker::Known(HostKind::Herdr) | HostKindMarker::Absent => Ended,
                _ => Held,
            };
        }
        if host != HostKindMarker::Known(HostKind::Herdr)
            || !matches!(nonce, SpawnNonceMarker::Known(_))
            || nonce_baseline(channel.get(), &owner.logical_key, &record.execution_nonce) != source
        {
            return Held;
        }

        match (source, prepared) {
            (Some(source), Some(prepared)) => {
                let published = shared.tmux_watchers.publish_restored_owner(
                    &owner.logical_key,
                    channel,
                    || {
                        publish_restarted_binding(
                            authority,
                            owner,
                            channel.get(),
                            &source,
                            prepared,
                        )
                        .is_some_and(Persisted::published)
                    },
                );
                if !published {
                    return Held;
                }
                VerifiedReattach::Live
            }
            (None, _)
                if owner.provider == "claude"
                    && awaits_own_clear(
                        channel.get(),
                        &owner.logical_key,
                        &record.execution_nonce,
                    ) =>
            {
                if !shared
                    .tmux_watchers
                    .publish_restored_owner(&owner.logical_key, channel, || true)
                {
                    return Held;
                }
                VerifiedReattach::AwaitingClear
            }
            _ => Held,
        }
    });
    if matches!(
        result,
        VerifiedReattach::Live | VerifiedReattach::AwaitingClear
    ) {
        restore_execution_fence(record);
        admit_herdr_execution(&owner.logical_key, &record.execution_nonce);
    }
    drop(permit);
    result
}

fn stale_nonce(record: &HostedExecution, nonce: &SpawnNonceMarker) -> bool {
    match nonce {
        SpawnNonceMarker::Known(current) => current != &record.execution_nonce,
        SpawnNonceMarker::Unreadable => true,
        SpawnNonceMarker::Absent => false,
    }
}

#[cfg(test)]
#[path = "herdr_reattach_tests.rs"]
pub(in crate::services::discord) mod tests;
