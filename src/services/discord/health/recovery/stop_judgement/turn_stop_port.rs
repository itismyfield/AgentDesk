//! The registry's side of the KE turn-stop port: each call runs the registry's own stop,
//! lookup or host guard unchanged.

use std::sync::Arc;

use futures::future::BoxFuture;
use poise::serenity_prelude::ChannelId;

use super::ProviderChannelStop;
use crate::domain::tmux_cleanup::TmuxCleanupPolicy;
use crate::services::discord::health::{self, HealthRegistry};
use crate::services::discord::{SharedData, host_teardown_gate};
use crate::services::engine_runtime::health_port::{
    PendingQueueSnapshot, RuntimeTurnStopResult, TurnStopJudgement, TurnStopPort,
};
use crate::services::provider::ProviderKind;
use crate::services::session_host::ChannelTeardown;

impl TurnStopJudgement for ProviderChannelStop {
    fn host_refused(&self) -> bool {
        ProviderChannelStop::host_refused(self)
    }

    fn unobserved_reason(&self) -> Option<&'static str> {
        ProviderChannelStop::unobserved_reason(self)
    }

    fn session(&self) -> Option<String> {
        ProviderChannelStop::session(self)
    }
}

impl TurnStopPort<SharedData> for HealthRegistry {
    type Judged = ProviderChannelStop;

    fn judge_provider_channel_stop<'a>(
        &'a self,
        provider_name: &'a str,
        channel: ChannelId,
    ) -> BoxFuture<'a, ProviderChannelStop> {
        Box::pin(health::judge_provider_channel_stop(
            self,
            provider_name,
            channel,
        ))
    }

    fn stop_judged_provider_channel<'a>(
        &'a self,
        judged: ProviderChannelStop,
        reason: &'a str,
        cleanup_policy: TmuxCleanupPolicy,
    ) -> BoxFuture<'a, Option<RuntimeTurnStopResult>> {
        Box::pin(health::stop_judged_provider_channel(
            judged,
            reason,
            cleanup_policy,
        ))
    }

    fn stop_channel_runtime<'a>(
        &'a self,
        shared: &'a Arc<SharedData>,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
        reason: &'a str,
        cleanup_policy: TmuxCleanupPolicy,
        approved: Option<Option<&'a str>>,
    ) -> BoxFuture<'a, RuntimeTurnStopResult> {
        let stop = health::stop_channel_runtime;
        Box::pin(stop(
            shared,
            provider,
            channel_id,
            reason,
            cleanup_policy,
            approved,
        ))
    }

    fn stop_runtime_turn_preserving_watcher<'a>(
        &'a self,
        provider_name: Option<&'a str>,
        channel_id: Option<u64>,
        tmux_name: Option<&'a str>,
        stop_source: &'static str,
    ) -> BoxFuture<'a, &'static str> {
        let stop = health::stop_runtime_turn_preserving_watcher;
        let stop = stop(
            Some(self),
            provider_name,
            channel_id,
            tmux_name,
            stop_source,
        );
        Box::pin(async move { stop.await.cleanup_path })
    }

    fn resolve_tmux_session_for_cancel<'a>(
        &'a self,
        provider_name: &'a str,
        channel_id: ChannelId,
    ) -> BoxFuture<'a, Option<String>> {
        let resolve = health::resolve_tmux_session_for_cancel;
        Box::pin(resolve(self, provider_name, channel_id))
    }

    fn snapshot_pending_queue_state<'a>(
        &'a self,
        provider_name: &'a str,
        channel_id: ChannelId,
    ) -> BoxFuture<'a, Option<PendingQueueSnapshot>> {
        let snapshot = health::snapshot_pending_queue_state;
        Box::pin(snapshot(self, provider_name, channel_id))
    }

    fn guard_tmux_name<'a>(
        &'a self,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
    ) -> BoxFuture<'a, Result<(Option<String>, bool), String>> {
        Box::pin(host_teardown_gate::guard_tmux_name(
            self, provider, channel_id,
        ))
    }

    fn runtime_teardown<'a>(
        &'a self,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
        tmux_name: &'a str,
        explicit_key: Option<&'a str>,
        caller: &'a str,
    ) -> BoxFuture<'a, (ChannelTeardown, Option<Arc<SharedData>>)> {
        let gate = host_teardown_gate::runtime_teardown;
        Box::pin(gate(
            self,
            provider,
            channel_id,
            tmux_name,
            explicit_key,
            caller,
        ))
    }

    fn nameless_runtime_teardown<'a>(
        &'a self,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
        caller: &'a str,
    ) -> BoxFuture<'a, Option<Arc<SharedData>>> {
        let gate = host_teardown_gate::nameless_runtime_teardown;
        Box::pin(gate(self, provider, channel_id, caller))
    }
}
