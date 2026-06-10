//! Extracted from `services::discord::health` (#3038 Phase A) — verbatim
//! move; behavior unchanged. Headless agent-turn reserve/start API
//! (reservation channel/turn-id invariants) and the direct-meeting starter.

use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::ChannelId;

use super::HealthRegistry;
use super::runtime_resolve::{resolve_direct_meeting_runtime, resolve_direct_meeting_shared};
use crate::services::discord::SharedData;
use crate::services::discord::{meeting, router};
use crate::services::provider::ProviderKind;

pub async fn start_headless_agent_turn(
    registry: &HealthRegistry,
    channel_id: ChannelId,
    owner_provider: ProviderKind,
    prompt: String,
    source: Option<String>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
) -> Result<router::HeadlessTurnStartOutcome, router::HeadlessTurnStartError> {
    let reservation = reserve_headless_agent_turn(channel_id);
    start_reserved_headless_agent_turn(
        registry,
        channel_id,
        owner_provider,
        prompt,
        source,
        metadata,
        channel_name_hint,
        reservation,
    )
    .await
}

#[derive(Debug, Clone)]
pub struct HeadlessAgentTurnReservation {
    channel_id: ChannelId,
    turn_id: String,
    inner: router::HeadlessTurnReservation,
}

impl HeadlessAgentTurnReservation {
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }
}

pub fn reserve_headless_agent_turn(channel_id: ChannelId) -> HeadlessAgentTurnReservation {
    let inner = router::reserve_headless_turn();
    HeadlessAgentTurnReservation {
        channel_id,
        turn_id: inner.turn_id(channel_id),
        inner,
    }
}

pub async fn start_reserved_headless_agent_turn(
    registry: &HealthRegistry,
    channel_id: ChannelId,
    owner_provider: ProviderKind,
    prompt: String,
    source: Option<String>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
    reservation: HeadlessAgentTurnReservation,
) -> Result<router::HeadlessTurnStartOutcome, router::HeadlessTurnStartError> {
    if reservation.channel_id != channel_id {
        return Err(router::HeadlessTurnStartError::Internal(format!(
            "headless turn reservation channel mismatch: reserved {} but starting {}",
            reservation.channel_id.get(),
            channel_id.get()
        )));
    }

    let expected_turn_id = reservation.turn_id.clone();
    let shared = resolve_direct_meeting_shared(registry, channel_id, &owner_provider)
        .await
        .map_err(router::HeadlessTurnStartError::Internal)?;

    start_reserved_headless_agent_turn_with_shared(
        shared,
        channel_id,
        owner_provider,
        prompt,
        source,
        metadata,
        channel_name_hint,
        None,
        reservation,
        expected_turn_id,
    )
    .await
}

pub async fn start_headless_agent_turn_in_dm(
    registry: &HealthRegistry,
    owner_channel_id: ChannelId,
    dm_user_id: u64,
    owner_provider: ProviderKind,
    prompt: String,
    source: Option<String>,
    metadata: Option<serde_json::Value>,
) -> Result<router::HeadlessTurnStartOutcome, router::HeadlessTurnStartError> {
    let (_, shared) = resolve_direct_meeting_runtime(registry, owner_channel_id, &owner_provider)
        .await
        .map_err(router::HeadlessTurnStartError::Internal)?;
    let ctx = shared.cached_serenity_ctx.get().cloned().ok_or_else(|| {
        router::HeadlessTurnStartError::Internal(format!(
            "provider runtime is not ready for channel {}",
            owner_channel_id.get()
        ))
    })?;
    let dm_channel = serenity::UserId::new(dm_user_id)
        .create_dm_channel(&ctx.http)
        .await
        .map_err(|error| {
            router::HeadlessTurnStartError::Internal(format!(
                "DM channel creation failed for user {dm_user_id}: {error}"
            ))
        })?;
    let dm_channel_id = dm_channel.id;
    let reservation = reserve_headless_agent_turn(dm_channel_id);
    let expected_turn_id = reservation.turn_id.clone();
    let channel_name_hint = Some(format!("dm-{dm_user_id}"));

    start_reserved_headless_agent_turn_with_shared(
        shared,
        dm_channel_id,
        owner_provider,
        prompt,
        source,
        metadata,
        channel_name_hint,
        Some(true),
        reservation,
        expected_turn_id,
    )
    .await
}

pub async fn reserve_headless_agent_turn_in_dm(
    registry: &HealthRegistry,
    owner_channel_id: ChannelId,
    dm_user_id: u64,
    owner_provider: &ProviderKind,
) -> Result<(ChannelId, HeadlessAgentTurnReservation), router::HeadlessTurnStartError> {
    let (_, shared) = resolve_direct_meeting_runtime(registry, owner_channel_id, owner_provider)
        .await
        .map_err(router::HeadlessTurnStartError::Internal)?;
    let ctx = shared.cached_serenity_ctx.get().cloned().ok_or_else(|| {
        router::HeadlessTurnStartError::Internal(format!(
            "provider runtime is not ready for channel {}",
            owner_channel_id.get()
        ))
    })?;
    let dm_channel = serenity::UserId::new(dm_user_id)
        .create_dm_channel(&ctx.http)
        .await
        .map_err(|error| {
            router::HeadlessTurnStartError::Internal(format!(
                "DM channel creation failed for user {dm_user_id}: {error}"
            ))
        })?;
    let dm_channel_id = dm_channel.id;
    Ok((dm_channel_id, reserve_headless_agent_turn(dm_channel_id)))
}

pub async fn start_reserved_headless_agent_turn_in_dm(
    registry: &HealthRegistry,
    owner_channel_id: ChannelId,
    dm_channel_id: ChannelId,
    dm_user_id: u64,
    owner_provider: ProviderKind,
    prompt: String,
    source: Option<String>,
    metadata: Option<serde_json::Value>,
    reservation: HeadlessAgentTurnReservation,
) -> Result<router::HeadlessTurnStartOutcome, router::HeadlessTurnStartError> {
    if reservation.channel_id != dm_channel_id {
        return Err(router::HeadlessTurnStartError::Internal(format!(
            "headless turn reservation channel mismatch: reserved {} but starting {}",
            reservation.channel_id.get(),
            dm_channel_id.get()
        )));
    }

    let (_, shared) = resolve_direct_meeting_runtime(registry, owner_channel_id, &owner_provider)
        .await
        .map_err(router::HeadlessTurnStartError::Internal)?;
    let expected_turn_id = reservation.turn_id.clone();
    let channel_name_hint = Some(format!("dm-{dm_user_id}"));

    start_reserved_headless_agent_turn_with_shared(
        shared,
        dm_channel_id,
        owner_provider,
        prompt,
        source,
        metadata,
        channel_name_hint,
        Some(true),
        reservation,
        expected_turn_id,
    )
    .await
}

async fn start_reserved_headless_agent_turn_with_shared(
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    _owner_provider: ProviderKind,
    prompt: String,
    source: Option<String>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
    is_dm_hint: Option<bool>,
    reservation: HeadlessAgentTurnReservation,
    expected_turn_id: String,
) -> Result<router::HeadlessTurnStartOutcome, router::HeadlessTurnStartError> {
    if reservation.channel_id != channel_id {
        return Err(router::HeadlessTurnStartError::Internal(format!(
            "headless turn reservation channel mismatch: reserved {} but starting {}",
            reservation.channel_id.get(),
            channel_id.get()
        )));
    }

    let ctx = shared.cached_serenity_ctx.get().cloned().ok_or_else(|| {
        router::HeadlessTurnStartError::Internal(format!(
            "provider runtime is not ready for channel {}",
            channel_id.get()
        ))
    })?;
    let token = shared
        .cached_bot_token
        .get()
        .cloned()
        .or_else(|| crate::services::discord::resolve_discord_token_by_hash(&shared.token_hash))
        .ok_or_else(|| {
            router::HeadlessTurnStartError::Internal(format!(
                "provider token unavailable for channel {}",
                channel_id.get()
            ))
        })?;

    let outcome = router::start_reserved_headless_turn(
        &ctx,
        channel_id,
        &prompt,
        source.as_deref().unwrap_or("system"),
        &shared,
        &token,
        source.as_deref(),
        metadata,
        channel_name_hint,
        is_dm_hint,
        reservation.inner,
    )
    .await?;

    if outcome.turn_id != expected_turn_id {
        return Err(router::HeadlessTurnStartError::Internal(format!(
            "reserved headless turn id mismatch: expected {} but started {}",
            expected_turn_id, outcome.turn_id
        )));
    }

    Ok(outcome)
}

pub async fn start_direct_meeting(
    registry: &HealthRegistry,
    channel_id: ChannelId,
    owner_provider: ProviderKind,
    primary_provider: ProviderKind,
    reviewer_provider: ProviderKind,
    agenda: String,
    fixed_participants: Vec<String>,
) -> Result<(), String> {
    let (http, shared) =
        resolve_direct_meeting_runtime(registry, channel_id, &owner_provider).await?;

    meeting::spawn_direct_start(
        http,
        channel_id,
        agenda,
        primary_provider,
        reviewer_provider,
        fixed_participants,
        shared,
    )
    .await
}
