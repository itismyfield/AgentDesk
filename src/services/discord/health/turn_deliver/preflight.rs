//! Deterministic host refusals precede origin custody and any input effect.

use super::{ProviderKind, SharedData};
use crate::domain::human_input::HumanInputError;
use poise::serenity_prelude::ChannelId;
use std::sync::Arc;

pub(super) async fn host(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<bool, HumanInputError> {
    if !crate::services::discord::owner_takes_start(provider, channel.get()) {
        return Ok(false);
    }
    let key = || {
        crate::services::discord::adk_session::build_adk_session_key(
            shared, channel, provider, None,
        )
    };
    match crate::services::turn_host::intake_refusal_before_turn(
        shared.pg_pool.as_ref(),
        provider,
        channel.get(),
        key,
    )
    .await
    {
        Some(refusal) => Err(HumanInputError::InvalidTarget(refusal.to_string())),
        None => Ok(crate::services::session_host::thread_host::runs_herdr(
            channel.get(),
        )),
    }
}
