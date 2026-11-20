use sqlx::PgPool;

use super::herdr_reader::SocketHerdrReader;
use super::host_reconcile::{
    HerdrEndpointId, HerdrExecutionReader, HerdrPaneReading, HostReconcile, NoHerdrEndpoint,
    reconcile_hosted,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLookup, HostedObservation, HostedRecord, HostedState,
};
use crate::services::discord::SharedData;
use crate::services::discord::herdr_terminate::root_replaced;
use crate::services::discord::session_transition::SessionTransitionLease;
use crate::services::discord::tui_prompt_relay::herdr_source::{
    HerdrSourceAttach, attach_restarted_herdr_source, nonce_baseline, publish_restarted_binding,
};
use crate::services::herdr_launch::clear_epoch;
use crate::services::session_host::{HostKind, restore_execution_fence};
use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};
use crate::services::tmux_common::with_tmux_source_authority;
use crate::services::tui_prompt_dedupe::binding_context::{
    SpawnNonceMarker, observe_spawn_nonce_marker,
};
use crate::services::tui_prompt_dedupe::{Persisted, admit_herdr_execution};

pub(super) struct VerifiedTarget<'a> {
    pub(super) shared: &'a SharedData,
    pub(super) transition: &'a SessionTransitionLease<'a>,
}

pub(super) enum RestartAttachment {
    Legacy(HerdrSourceAttach),
    Verified(VerifiedReattach),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VerifiedReattach {
    Live,
    Ended,
    Held,
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
            verify_with(
                target.shared,
                pool,
                target.transition,
                observed,
                HostReconcile::admits_reconnect,
                stale_nonce,
            )
            .await,
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

async fn verify_with(
    shared: &SharedData,
    pool: &PgPool,
    transition: &SessionTransitionLease<'_>,
    observed: &HostedObservation,
    is_live: fn(&HostReconcile) -> bool,
    is_stale: fn(&HostedExecution, &SpawnNonceMarker) -> bool,
) -> VerifiedReattach {
    use VerifiedReattach::{Ended, Held, Live};

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
    let Ok((reading, ended)) = tokio::task::spawn_blocking(move || reader.observe(&stored)).await
    else {
        return Held;
    };
    let captured = CapturedPane {
        endpoint: HerdrEndpointId::of(location),
        pane_id: location.pane_id.clone(),
        reading,
    };
    let verdict = reconcile_hosted(&HostedLookup::Found(observed.clone()), &captured);
    let live = is_live(&verdict);
    if (!live && !ended) || root_replaced::current(pool, observed).await.is_err() {
        return Held;
    }

    let Some(effect) = epoch.begin_effect() else {
        return Held;
    };
    let result = with_tmux_source_authority(&owner.logical_key, |authority| {
        let nonce = observe_spawn_nonce_marker(&owner.logical_key);
        if is_stale(record, &nonce) {
            return Held;
        }
        let host = read_host_kind_marker(&owner.logical_key);
        if ended {
            return match host {
                HostKindMarker::Known(HostKind::Herdr) | HostKindMarker::Absent => Ended,
                _ => Held,
            };
        }
        if host != HostKindMarker::Known(HostKind::Herdr)
            || !matches!(nonce, SpawnNonceMarker::Known(_))
        {
            return Held;
        }
        let Some(source) =
            nonce_baseline(channel.get(), &owner.logical_key, &record.execution_nonce)
        else {
            return Held;
        };
        let published =
            shared
                .tmux_watchers
                .publish_restored_owner(&owner.logical_key, channel, || {
                    publish_restarted_binding(authority, owner, channel.get(), &source)
                        .is_some_and(Persisted::published)
                });
        if !published {
            return Held;
        }
        Live
    });
    if result == Live {
        restore_execution_fence(record);
        admit_herdr_execution(&owner.logical_key, &record.execution_nonce);
    }
    drop(effect);
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
mod tests;
