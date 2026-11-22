//! The registry's side of the KE queue-cancel, stale-sweep and headless-turn ports: each call
//! runs the health function KE called before, with the same arguments.

use futures::future::BoxFuture;
use poise::serenity_prelude::ChannelId;

use super::{HeadlessAgentTurnReservation, HealthRegistry};
use crate::services::discord::relay_recovery::{self, AxisBSite};
use crate::services::discord::zombie_foreground_release;
use crate::services::engine_runtime::health_port::{
    FinishCancelledMailboxResult, HeadlessTurnPort, HeadlessTurnReservationView, HeadlessTurnStart,
    QueueCancelPort, StaleSweepPort,
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
}
