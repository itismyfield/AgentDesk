//! The registry's side of the KE health ports and its binding as the application's registry:
//! each call runs the health function KE called before, with the same arguments.

use std::sync::Arc;

use futures::future::BoxFuture;
use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use super::{HeadlessAgentTurnReservation, HealthRegistry};
use crate::services::discord::SharedData;
use crate::services::discord::admin_host_guard::ManagedResetVerdict;
use crate::services::discord::relay_recovery::{self, AxisBSite};
use crate::services::discord::zombie_foreground_release;
use crate::services::discord::{host_defer_gate, host_teardown_gate};
use crate::services::engine_runtime::health_port::{
    AppHealth, AppHealthBinding, FinishCancelledMailboxResult, HeadlessDmTurnStart,
    HeadlessTurnPort, HeadlessTurnReservationView, HeadlessTurnStart, QueueCancelPort,
    ResumeRuntimePort, RoutineProcessPort, SlotResetPort, StaleSweepPort,
};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{
    ChannelMailboxHandle, HeadlessTurnStartError, HeadlessTurnStartOutcome,
};

impl QueueCancelPort for HealthRegistry {
    fn schedule_pending_queue_drain_after_cancel<'a>(
        &'a self,
        provider_name: &'a str,
        channel_id: ChannelId,
        reason: &'static str,
    ) -> BoxFuture<'a, Option<usize>> {
        Box::pin(async move {
            let outcome = super::schedule_pending_queue_drain_after_cancel(
                self,
                provider_name,
                channel_id,
                reason,
            )
            .await;
            outcome.queue_depth_after
        })
    }

    fn finish_cancelled_provider_channel_mailbox<'a>(
        registry: Option<&'a Self>,
        provider_name: Option<&'a str>,
        channel_id: Option<u64>,
        stop_source: &'static str,
        expected_actor: Option<&'a ChannelMailboxHandle>,
    ) -> BoxFuture<'a, FinishCancelledMailboxResult> {
        Box::pin(super::finish_cancelled_provider_channel_mailbox(
            registry,
            provider_name,
            channel_id,
            stop_source,
            expected_actor,
        ))
    }

    fn channel_token_hash<'a>(
        &'a self,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let shared = self
                .shared_for_provider_on_channel(provider, channel_id)
                .await?;
            Some(shared.token_hash.clone())
        })
    }

    fn run_herdr_cancel<'a>(
        &'a self,
        provider: &'a ProviderKind,
        channel: u64,
    ) -> BoxFuture<'a, Option<serde_json::Value>> {
        Box::pin(zombie_foreground_release::run_herdr_cancel(
            self, provider, channel,
        ))
    }
}

impl StaleSweepPort<AxisBSite> for HealthRegistry {
    fn automatic_stale_sweep_warrants<'a>(
        &'a self,
        session_key: &'a str,
        provider_name: &'a str,
        site: AxisBSite,
    ) -> BoxFuture<'a, bool> {
        Box::pin(relay_recovery::automatic_stale_sweep_warrants(
            Some(self),
            session_key,
            provider_name,
            site,
        ))
    }
}

impl HeadlessTurnReservationView for HeadlessAgentTurnReservation {
    fn turn_id(&self) -> &str {
        HeadlessAgentTurnReservation::turn_id(self)
    }
}

impl HeadlessTurnPort for HealthRegistry {
    type Reservation = HeadlessAgentTurnReservation;

    fn reserve_headless_agent_turn(&self, channel_id: ChannelId) -> HeadlessAgentTurnReservation {
        super::reserve_headless_agent_turn(channel_id)
    }

    fn start_reserved_headless_agent_turn_with_owner_channel<'a>(
        &'a self,
        start: HeadlessTurnStart,
        reservation: HeadlessAgentTurnReservation,
    ) -> BoxFuture<'a, Result<HeadlessTurnStartOutcome, HeadlessTurnStartError>> {
        let HeadlessTurnStart {
            owner_channel_id,
            turn_channel_id,
            owner_provider,
            prompt,
            source,
            metadata,
            channel_name_hint,
            tmux_session_label,
        } = start;
        Box::pin(
            super::start_reserved_headless_agent_turn_with_owner_channel(
                self,
                owner_channel_id,
                turn_channel_id,
                owner_provider,
                prompt,
                source,
                metadata,
                channel_name_hint,
                tmux_session_label,
                reservation,
            ),
        )
    }

    fn reserve_headless_agent_turn_in_dm<'a>(
        &'a self,
        owner_channel_id: ChannelId,
        dm_user_id: u64,
        owner_provider: &'a ProviderKind,
    ) -> BoxFuture<'a, Result<(ChannelId, HeadlessAgentTurnReservation), HeadlessTurnStartError>>
    {
        Box::pin(super::reserve_headless_agent_turn_in_dm(
            self,
            owner_channel_id,
            dm_user_id,
            owner_provider,
        ))
    }

    fn start_reserved_headless_agent_turn_in_dm<'a>(
        &'a self,
        start: HeadlessDmTurnStart,
        reservation: HeadlessAgentTurnReservation,
    ) -> BoxFuture<'a, Result<HeadlessTurnStartOutcome, HeadlessTurnStartError>> {
        let HeadlessDmTurnStart {
            owner_channel_id,
            dm_channel_id,
            dm_user_id,
            owner_provider,
            prompt,
            source,
            metadata,
            tmux_session_label,
        } = start;
        Box::pin(super::start_reserved_headless_agent_turn_in_dm(
            self,
            owner_channel_id,
            dm_channel_id,
            dm_user_id,
            owner_provider,
            prompt,
            source,
            metadata,
            tmux_session_label,
            reservation,
        ))
    }
}

impl AppHealthBinding for AppHealth {
    type Registry = HealthRegistry;
}

impl RoutineProcessPort for HealthRegistry {
    fn cancel_routine_process_turn<'a>(
        &'a self,
        provider: &'a ProviderKind,
        channel: ChannelId,
        turn_id: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, bool> {
        Box::pin(super::cancel_routine_process_turn(
            self, provider, channel, turn_id, reason,
        ))
    }
}

impl ResumeRuntimePort for HealthRegistry {
    type Runtime = SharedData;

    fn resume_runtime_for_channel<'a>(
        &'a self,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
    ) -> BoxFuture<'a, Option<Arc<SharedData>>> {
        Box::pin(super::resume_runtime_for_channel(
            self, provider, channel_id,
        ))
    }

    fn channel_has_active_turn<'a>(
        &'a self,
        provider_name: &'a str,
        channel_id: ChannelId,
    ) -> BoxFuture<'a, bool> {
        Box::pin(super::channel_has_active_turn(
            self,
            provider_name,
            channel_id,
        ))
    }

    fn retain_resume_runtime_owner_before_teardown(
        shared: &SharedData,
        channel_id: ChannelId,
        tmux_session_name: &str,
    ) -> bool {
        super::retain_resume_runtime_owner_before_teardown(shared, channel_id, tmux_session_name)
    }

    fn clear_resume_runtime_owner_after_death(shared: &SharedData, tmux_session_name: &str) {
        super::clear_resume_runtime_owner_after_death(shared, tmux_session_name)
    }

    fn rebind_channel_provider_session<'a>(
        shared: &'a SharedData,
        provider: &'a ProviderKind,
        channel_id: ChannelId,
        cwd: &'a str,
        session_id: &'a str,
        tmux_session_name: &'a str,
        retain_runtime_owner: bool,
    ) -> BoxFuture<'a, ()> {
        Box::pin(super::rebind_channel_provider_session(
            shared,
            provider,
            channel_id,
            cwd,
            session_id,
            tmux_session_name,
            retain_runtime_owner,
        ))
    }

    fn resume_host_refusal<'a>(
        pool: &'a PgPool,
        provider: Option<&'a ProviderKind>,
        channel_id: Option<ChannelId>,
        session_key: &'a str,
        tmux_name: &'a str,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(host_defer_gate::resume_host_refusal(
            pool,
            provider,
            channel_id,
            session_key,
            tmux_name,
        ))
    }
}

impl SlotResetPort for HealthRegistry {
    type Verdict = ManagedResetVerdict;

    fn verdict_refusal(verdict: &ManagedResetVerdict) -> Option<&str> {
        verdict.refusal()
    }

    fn row_host_refusal<'a>(
        pool: &'a PgPool,
        stored_provider: Option<&'a str>,
        channel_id: Option<&'a str>,
        session_key: &'a str,
        tmux_name: &'a str,
        caller: &'a str,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(host_teardown_gate::row_host_refusal(
            pool,
            stored_provider,
            channel_id,
            session_key,
            tmux_name,
            caller,
        ))
    }
}
