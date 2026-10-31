//! What a scoped terminate holds once it retired its execution and settled the captured turn:
//! only [`super::terminate_scoped`] builds one, and the channel stays excluded until it drops.

use std::sync::Arc;

use sqlx::PgPool;

use super::{ChannelId, HostedObservation, HostedRecord, ProviderKind};
use crate::db::dispatched_sessions::hosted_execution::http_force::disconnect_retired_pg;
use crate::services::provider::CancelToken;
use crate::services::session_host::TerminationFence;
use crate::services::turn_orchestrator::ChannelMailboxHandle;

/// The transition, the execution fence, the channel's actor seen under the transition with the
/// captured turn's token, the row as read and the dispatch the run was bound to.
pub(crate) struct RetiredExclusion {
    _transition: tokio::sync::OwnedMutexGuard<()>,
    _fence: Arc<TerminationFence>,
    actor: Option<ChannelMailboxHandle>,
    token: Option<Arc<CancelToken>>,
    row: HostedObservation,
    dispatch: Option<String>,
    provider: ProviderKind,
    channel: ChannelId,
}

pub(super) type Held = (
    tokio::sync::OwnedMutexGuard<()>,
    Arc<TerminationFence>,
    Option<ChannelMailboxHandle>,
    Option<Arc<CancelToken>>,
);

impl RetiredExclusion {
    pub(super) fn new(
        (transition, fence, actor, token): Held,
        row: HostedObservation,
        dispatch: Option<String>,
        provider: ProviderKind,
        channel: ChannelId,
    ) -> Self {
        Self {
            _transition: transition,
            _fence: fence,
            actor,
            token,
            row,
            dispatch,
            provider,
            channel,
        }
    }

    /// The channel's actor, the one queue effects may address; `None` when the channel had none.
    pub(crate) fn actor(&self) -> Option<&ChannelMailboxHandle> {
        self.actor.as_ref()
    }

    /// The settled turn's token, when a turn was running.
    pub(crate) fn settled_turn(&self) -> Option<&Arc<CancelToken>> {
        self.token.as_ref()
    }

    pub(crate) fn dispatch(&self) -> Option<&str> {
        self.dispatch.as_deref()
    }

    /// The row's owner token, which the terminate required to be its runtime's own.
    pub(crate) fn owner_token(&self) -> Option<&str> {
        match &self.row.record {
            HostedRecord::Known(record) => Some(&record.owner.discord_token_hash),
            _ => None,
        }
    }

    pub(crate) fn provider(&self) -> &ProviderKind {
        &self.provider
    }

    pub(crate) fn channel(&self) -> ChannelId {
        self.channel
    }

    pub(crate) fn execution(
        &self,
    ) -> Option<&crate::db::dispatched_sessions::hosted_execution::HostedExecution> {
        match &self.row.record {
            HostedRecord::Known(record) => Some(record),
            _ => None,
        }
    }

    /// Disconnects the session while its row still records this retired execution and no dispatch
    /// but the bound one; `Ok(false)` means the row moved on and nothing was written.
    pub(crate) async fn disconnect_session(&self, pool: &PgPool) -> Result<bool, String> {
        self.disconnect(pool, false).await
    }

    /// [`Self::disconnect_session`] that keeps the provider session id, as a session force-kill does.
    pub(crate) async fn disconnect_session_keeping_provider(
        &self,
        pool: &PgPool,
    ) -> Result<bool, String> {
        self.disconnect(pool, true).await
    }

    async fn disconnect(&self, pool: &PgPool, keep_provider_session: bool) -> Result<bool, String> {
        let HostedRecord::Known(record) = &self.row.record else {
            return Err("retired row has no known record".into());
        };
        let id = self.row.session_id();
        let keep = keep_provider_session;
        disconnect_retired_pg(pool, id, record, self.dispatch(), keep).await
    }
}
