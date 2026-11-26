//! The idle end of a Bound Herdr execution, asked by the idle-kill policy through kill-tmux: it
//! closes nothing while a turn, an inflight row or an active dispatch holds the channel, and keeps
//! the provider session so the next message resumes it.
use std::sync::Arc;

use sqlx::PgPool;

use super::{
    ChannelId, HealthRegistry, NeverStartedTerminate, OperatorTerminate, SharedData,
    TerminationResult, load, terminate_enabled, terminate_scoped,
};
use crate::db::dispatched_sessions::hosted_execution::http_force::{
    active_dispatch_pg, disconnect_retired_pg,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedObservation, HostedRecord, HostedState,
};
use crate::services::provider::ProviderKind;
use crate::services::session_host::{HerdrTarget, TerminationFence};
use crate::services::turn_orchestrator::Expected;

/// An end no operator asked for; either kind refuses any turn holding the channel before a fence.
pub(super) enum AutoEnd<'a> {
    NeverStarted(&'a NeverStartedTerminate),
    Idle,
}

impl AutoEnd<'_> {
    pub(super) fn refuses(&self, record: &HostedExecution) -> Option<&'static str> {
        match self {
            Self::NeverStarted(asked) => {
                (!asked.admits(record)).then_some("execution is not a never-started pending")
            }
            Self::Idle => (record.state != HostedState::Bound).then_some("execution is not bound"),
        }
    }

    /// The idle end re-reads the inflight row under the transition lock, then shuts the fence.
    pub(super) async fn fence(
        &self,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel: ChannelId,
        record: &HostedExecution,
        target: &HerdrTarget,
    ) -> Result<TerminationFence, TerminationResult> {
        use TerminationResult::Refused;
        match self {
            Self::NeverStarted(asked) => asked.fence(shared, channel, record, target).await,
            Self::Idle if inflight_open(provider, channel) => Err(Refused(INFLIGHT_OPEN.into())),
            Self::Idle => target
                .termination_fence()
                .map_err(|error| Refused(format!("{error:?}"))),
        }
    }
}

const INFLIGHT_OPEN: &str = "inflight row open";

/// An unreadable inflight row is an open one.
fn inflight_open(provider: &ProviderKind, channel: ChannelId) -> bool {
    let read = super::super::inflight::load_inflight_state_read_only_result;
    !matches!(read(provider, channel.get()), Ok(None))
}

/// What the idle end did: `closed` when it closed a pane, `disconnected` when the row left `idle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IdleEnd {
    pub(crate) closed: bool,
    pub(crate) disconnected: bool,
}

/// `None` when the row records no Herdr execution. A Retired one only has its row disconnected.
pub(crate) async fn expire_idle_herdr(
    registry: Option<&HealthRegistry>,
    pool: &PgPool,
    session_key: &str,
) -> Option<Result<IdleEnd, TerminationResult>> {
    use TerminationResult::Refused;
    let row = load(pool, session_key).await.ok()?;
    let HostedRecord::Known(record) = row.record.clone() else {
        return None;
    };
    if !terminate_enabled() {
        return Some(Err(TerminationResult::Disabled));
    }
    let channel = record
        .owner
        .channel_id
        .parse::<u64>()
        .ok()
        .filter(|c| *c != 0);
    let (Some(provider), Some(channel)) = (
        ProviderKind::from_str(&record.owner.provider),
        channel.map(ChannelId::new),
    ) else {
        return Some(Err(Refused("unknown provider or channel".into())));
    };
    if active_dispatch_pg(pool, row.session_id()).await != Ok(None) {
        return Some(Err(Refused("active dispatch".into())));
    }
    if inflight_open(&provider, channel) {
        return Some(Err(Refused(INFLIGHT_OPEN.into())));
    }
    if record.state == HostedState::Retired {
        let disconnected = disconnect(pool, &row, &record).await;
        return Some(Ok(IdleEnd {
            closed: false,
            disconnected,
        }));
    }
    let Some(registry) = registry else {
        return Some(Err(Refused("runtime unavailable".into())));
    };
    let shared = match owner_runtime(registry, &provider, channel, &record).await {
        Some(shared) => shared,
        None => return Some(Err(Refused("no runtime owns the channel".into()))),
    };
    let request = OperatorTerminate {
        session_key: session_key.into(),
        execution_nonce: record.execution_nonce.clone(),
    };
    let expected = Expected {
        owner: record.owner.clone(),
        dispatch: None,
        captured_dispatch: false,
    };
    let pool = pool.clone();
    // Detached so a dropped request cannot stop the run between its close and its settlement.
    let run = tokio::spawn(async move {
        let auto = Some(AutoEnd::Idle);
        let scoped = terminate_scoped(request, shared, &pool, Some(expected), false, false, auto);
        let (exclusion, wake) = scoped.await?;
        let disconnected = disconnect(&pool, &row, &record).await;
        drop(exclusion);
        wake.after_release().await;
        Ok::<_, TerminationResult>(IdleEnd {
            closed: true,
            disconnected,
        })
    });
    let cancelled = TerminationResult::Indeterminate("idle end task cancelled".into());
    Some(run.await.unwrap_or(Err(cancelled)))
}

/// The channel's runtime, or, for a channel no runtime serves any more, the runtime of the bot
/// that owns the row; the scoped run still checks the owner.
async fn owner_runtime(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    channel: ChannelId,
    record: &HostedExecution,
) -> Option<Arc<SharedData>> {
    if let Some(shared) = registry
        .shared_for_provider_on_channel(provider, channel)
        .await
    {
        return Some(shared);
    }
    let runtimes = registry.all_shared_for_provider(provider).await;
    let token = &record.owner.discord_token_hash;
    runtimes
        .into_iter()
        .find(|shared| &shared.token_hash == token)
}

/// Leaves `idle` with the provider session kept, so idle-kill stops selecting the row.
async fn disconnect(pool: &PgPool, row: &HostedObservation, record: &HostedExecution) -> bool {
    match disconnect_retired_pg(pool, row.session_id(), record, None, true).await {
        Ok(disconnected) => disconnected,
        Err(error) => {
            tracing::warn!(
                session_id = row.session_id(),
                error,
                "herdr idle end: row kept"
            );
            false
        }
    }
}
