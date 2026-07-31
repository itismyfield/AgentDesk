use anyhow::{Result, anyhow};
use poise::serenity_prelude::{ChannelId, MessageId};

use crate::db::scheduled_messages as db;
use crate::db::scheduled_messages::ClaimedFire;
use crate::services::discord::health::{
    HeadlessAgentTurnReservation, reserve_headless_agent_turn,
    reserve_headless_agent_turn_with_user_msg_id,
};

pub(super) fn reserve_scheduled_agent_turn(
    channel_id: ChannelId,
    reservation_user_msg_id: Option<i64>,
) -> HeadlessAgentTurnReservation {
    match reservation_user_msg_id {
        Some(user_msg_id) => reserve_headless_agent_turn_with_user_msg_id(
            channel_id,
            MessageId::new(user_msg_id as u64),
        ),
        None => reserve_headless_agent_turn(channel_id),
    }
}

pub(super) async fn persist_scheduled_reservation(
    pool: &sqlx::PgPool,
    fire: &ClaimedFire,
    reservation: &HeadlessAgentTurnReservation,
) -> Result<()> {
    if fire.reservation_user_msg_id.is_some()
        || db::record_delivery_reservation_user_msg_id_pg(
            pool,
            &fire.message.id,
            &fire.delivery_id,
            &fire.claim_token,
            reservation.user_msg_id().get() as i64,
        )
        .await
        .map_err(|error| anyhow!("record scheduled message reservation identity: {error}"))?
    {
        return Ok(());
    }
    Err(anyhow!(
        "scheduled message claim was lost before reservation identity could be recorded"
    ))
}
