//! The turn-stop port: what a KE turn stop asks of the provider runtimes the Discord health
//! registry holds, and the plain results those runtimes hand back.

use std::sync::Arc;

use futures::future::BoxFuture;
use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use crate::db::dispatched_sessions::hosted_execution::HostedLookup;
use crate::domain::tmux_cleanup::TmuxCleanupPolicy;
use crate::services::provider::ProviderKind;
use crate::services::session_host::ChannelTeardown;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeTurnStopResult {
    pub lifecycle_path: &'static str,
    pub had_active_turn: bool,
    pub queue_depth: usize,
    pub inflight: InflightDisposition,
    pub termination_recorded: bool,
    /// #5176 — whether this stop actually took the mailbox foreground anchor.
    /// `true` also covers "the mailbox was already free when we checked": the
    /// contract this field reports is *ownership released*, and the caller only
    /// needs to know whether the channel is still locked.
    pub mailbox_foreground_free: bool,
}

impl RuntimeTurnStopResult {
    pub(crate) fn native_preserved(
        path: &'static str,
        queue_depth: usize,
        mailbox_foreground_free: bool,
    ) -> Self {
        Self {
            lifecycle_path: path,
            had_active_turn: true,
            queue_depth,
            inflight: InflightDisposition::PreservedByNativeStop,
            termination_recorded: false,
            mailbox_foreground_free,
        }
    }

    /// The host guard kept the judged turn: nothing was cancelled, finished or cleared.
    pub(crate) fn preserved_by_host_guard(queue_depth: usize) -> Self {
        Self {
            lifecycle_path: "host-guard-preserved",
            had_active_turn: true,
            queue_depth,
            inflight: InflightDisposition::PreservedByHostGuard,
            termination_recorded: false,
            mailbox_foreground_free: false,
        }
    }

    /// The judged turn ended and another took the channel: the stop left the successor alone.
    pub(crate) fn token_superseded(queue_depth: usize, termination_recorded: bool) -> Self {
        Self {
            lifecycle_path: TOKEN_SUPERSEDED_PATH,
            inflight: InflightDisposition::NotNeeded,
            termination_recorded,
            ..Self::preserved_by_host_guard(queue_depth)
        }
    }

    /// The finish went unobserved: the turn, its row and session are kept as a refused host's.
    pub(crate) fn finish_unobserved(queue_depth: usize) -> Self {
        Self {
            lifecycle_path: FINISH_UNOBSERVED_PATH,
            inflight: InflightDisposition::PreservedByHostGuard,
            ..Self::preserved_by_host_guard(queue_depth)
        }
    }
}

/// A stop whose judged turn was replaced before its finish; the channel stays locked.
pub const TOKEN_SUPERSEDED_PATH: &str = "token-superseded";
/// A stop whose finish the channel's actor did not answer; nothing was cleared.
pub const FINISH_UNOBSERVED_PATH: &str = "finish-unobserved";

/// What a runtime stop did with the channel's persistent inflight row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InflightDisposition {
    Cleared,
    NotNeeded,
    /// The stop was not a legacy one: the row stays for the turn's owner, and no caller clears it.
    PreservedByHostGuard,
    /// A native stop has not published cancellation; its reader and row remain owned.
    PreservedByNativeStop,
}

impl InflightDisposition {
    pub(crate) fn cleared_if(cleared: bool) -> Self {
        if cleared {
            Self::Cleared
        } else {
            Self::NotNeeded
        }
    }
}

/// A pending-queue snapshot read only: its memory depth, disk presence and queued message ids.
pub(crate) trait PendingQueueView {
    fn queue_depth(&self) -> usize;
    fn disk_present(&self) -> Option<bool>;
    fn message_ids(&self) -> &[u64];
}

/// A provider channel's turn stop, judged before its first write; nothing written.
pub(crate) trait TurnStopJudgement {
    /// The channel's active turn runs on a host that is not a confirmed legacy tmux, or its
    /// turn could not be read.
    fn host_refused(&self) -> bool;
    fn unobserved_reason(&self) -> Option<&'static str>;
    /// The session the verdict judged.
    fn session(&self) -> Option<String>;
}

/// The runtime reads, host guards and stops a KE turn stop runs against the registry of
/// provider runtimes. `S` is the runtime a guard admits; the registry owns its type.
pub(crate) trait TurnStopPort<S>: Send + Sync {
    type Judged: TurnStopJudgement + Send;
    type Snapshot: PendingQueueView + Send;

    fn judge_provider_channel_stop<'a>(
        &'a self,
        provider_name: &'a str,
        channel: ChannelId,
    ) -> BoxFuture<'a, Self::Judged>;

    /// Stops the judged turn on the runtime the verdict resolved; `None` with no runtime.
    fn stop_judged_provider_channel<'a>(
        &'a self,
        judged: Self::Judged,
        reason: &'a str,
        cleanup_policy: TmuxCleanupPolicy,
    ) -> BoxFuture<'a, Option<RuntimeTurnStopResult>>;

    /// A turn stop on `shared`'s channel run on the session a force-kill verdict approved.
    fn stop_channel_runtime<'a>(
        &'a self,
        shared: &'a Arc<S>,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
        reason: &'a str,
        cleanup_policy: TmuxCleanupPolicy,
        approved: Option<Option<&'a str>>,
    ) -> BoxFuture<'a, RuntimeTurnStopResult>;

    /// The cleanup path of a preserve stop of the runtime turn found by these keys.
    fn stop_runtime_turn_preserving_watcher<'a>(
        &'a self,
        provider_name: Option<&'a str>,
        channel_id: Option<u64>,
        tmux_name: Option<&'a str>,
        stop_source: &'static str,
    ) -> BoxFuture<'a, &'static str>;

    fn resolve_tmux_session_for_cancel<'a>(
        &'a self,
        provider_name: &'a str,
        channel_id: ChannelId,
    ) -> BoxFuture<'a, Option<String>>;

    fn snapshot_pending_queue_state<'a>(
        &'a self,
        provider_name: &'a str,
        channel_id: ChannelId,
    ) -> BoxFuture<'a, Option<Self::Snapshot>>;

    fn guard_tmux_name<'a>(
        &'a self,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
    ) -> BoxFuture<'a, Result<(Option<String>, bool), String>>;

    fn runtime_teardown<'a>(
        &'a self,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
        tmux_name: &'a str,
        explicit_key: Option<&'a str>,
        caller: &'a str,
    ) -> BoxFuture<'a, (ChannelTeardown, Option<Arc<S>>)>;

    fn nameless_runtime_teardown<'a>(
        &'a self,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
        caller: &'a str,
    ) -> BoxFuture<'a, Option<Arc<S>>>;

    /// Whether everything `channel_id`'s runtime would stop is the session a verdict approved.
    fn runtime_target_holds<'a>(
        shared: &'a S,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
        approved: Option<&'a str>,
    ) -> BoxFuture<'a, bool>;

    /// The host gate for a caller that read `session_key` from its own row, with the row lookup.
    fn row_gate<'a>(
        pool: &'a PgPool,
        stored_provider: Option<&'a str>,
        channel_id: u64,
        session_key: &'a str,
        tmux_name: &'a str,
        caller: &'a str,
    ) -> BoxFuture<'a, (ChannelTeardown, HostedLookup)>;

    /// The inflight finalizer backfill the cancel lookup writes, run only once the guard admits.
    fn backfill_inflight_after_guard(provider: &ProviderKind, channel_id: ChannelId);
}

/// A shared registry answers as the registry it holds.
impl<S, T: TurnStopPort<S>> TurnStopPort<S> for Arc<T> {
    type Judged = <T as TurnStopPort<S>>::Judged;
    type Snapshot = <T as TurnStopPort<S>>::Snapshot;

    fn judge_provider_channel_stop<'a>(
        &'a self,
        provider_name: &'a str,
        channel: ChannelId,
    ) -> BoxFuture<'a, Self::Judged> {
        (**self).judge_provider_channel_stop(provider_name, channel)
    }

    fn stop_judged_provider_channel<'a>(
        &'a self,
        judged: Self::Judged,
        reason: &'a str,
        cleanup_policy: TmuxCleanupPolicy,
    ) -> BoxFuture<'a, Option<RuntimeTurnStopResult>> {
        (**self).stop_judged_provider_channel(judged, reason, cleanup_policy)
    }

    fn stop_channel_runtime<'a>(
        &'a self,
        shared: &'a Arc<S>,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
        reason: &'a str,
        cleanup_policy: TmuxCleanupPolicy,
        approved: Option<Option<&'a str>>,
    ) -> BoxFuture<'a, RuntimeTurnStopResult> {
        let policy = cleanup_policy;
        (**self).stop_channel_runtime(shared, provider, channel_id, reason, policy, approved)
    }

    fn stop_runtime_turn_preserving_watcher<'a>(
        &'a self,
        provider_name: Option<&'a str>,
        channel_id: Option<u64>,
        tmux_name: Option<&'a str>,
        stop_source: &'static str,
    ) -> BoxFuture<'a, &'static str> {
        let (provider, channel, tmux) = (provider_name, channel_id, tmux_name);
        (**self).stop_runtime_turn_preserving_watcher(provider, channel, tmux, stop_source)
    }

    fn resolve_tmux_session_for_cancel<'a>(
        &'a self,
        provider_name: &'a str,
        channel_id: ChannelId,
    ) -> BoxFuture<'a, Option<String>> {
        (**self).resolve_tmux_session_for_cancel(provider_name, channel_id)
    }

    fn snapshot_pending_queue_state<'a>(
        &'a self,
        provider_name: &'a str,
        channel_id: ChannelId,
    ) -> BoxFuture<'a, Option<Self::Snapshot>> {
        (**self).snapshot_pending_queue_state(provider_name, channel_id)
    }

    fn guard_tmux_name<'a>(
        &'a self,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
    ) -> BoxFuture<'a, Result<(Option<String>, bool), String>> {
        (**self).guard_tmux_name(provider, channel_id)
    }

    fn runtime_teardown<'a>(
        &'a self,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
        tmux_name: &'a str,
        explicit_key: Option<&'a str>,
        caller: &'a str,
    ) -> BoxFuture<'a, (ChannelTeardown, Option<Arc<S>>)> {
        (**self).runtime_teardown(provider, channel_id, tmux_name, explicit_key, caller)
    }

    fn nameless_runtime_teardown<'a>(
        &'a self,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
        caller: &'a str,
    ) -> BoxFuture<'a, Option<Arc<S>>> {
        (**self).nameless_runtime_teardown(provider, channel_id, caller)
    }

    fn runtime_target_holds<'a>(
        shared: &'a S,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
        approved: Option<&'a str>,
    ) -> BoxFuture<'a, bool> {
        T::runtime_target_holds(shared, provider, channel_id, approved)
    }

    fn row_gate<'a>(
        pool: &'a PgPool,
        stored_provider: Option<&'a str>,
        channel_id: u64,
        session_key: &'a str,
        tmux_name: &'a str,
        caller: &'a str,
    ) -> BoxFuture<'a, (ChannelTeardown, HostedLookup)> {
        T::row_gate(
            pool,
            stored_provider,
            channel_id,
            session_key,
            tmux_name,
            caller,
        )
    }

    fn backfill_inflight_after_guard(provider: &ProviderKind, channel_id: ChannelId) {
        T::backfill_inflight_after_guard(provider, channel_id)
    }
}
