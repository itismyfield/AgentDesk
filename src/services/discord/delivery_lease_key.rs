use poise::serenity_prelude::ChannelId;

use super::{inflight, turn_finalizer};

/// Dedicated identity for the delivery-lease state machine.
///
/// Non-zero Discord user-message ids keep the historical `(channel, generation,
/// user_msg_id)` identity. Synthetic / recovery / TUI-direct turns with
/// `user_msg_id == 0` should carry the turn's persisted `started_at` and
/// `turn_start_offset`; when either disambiguator is absent, the residual legacy
/// class falls back to the pre-E13 degenerate `(channel, generation, 0)` key.
#[allow(dead_code)] // #3041 P1-0: dormant in some lease-owner paths.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(in crate::services::discord) struct DeliveryLeaseKey {
    pub(in crate::services::discord) channel_id: ChannelId,
    pub(in crate::services::discord) generation: u64,
    pub(in crate::services::discord) user_msg_id: u64,
    turn_started_at: Option<String>,
    turn_start_offset: Option<u64>,
}

#[allow(dead_code)] // #3041 P1-0: helper coverage is broader than the live paths.
impl DeliveryLeaseKey {
    #[track_caller]
    pub(in crate::services::discord) fn new(
        channel_id: ChannelId,
        generation: u64,
        user_msg_id: u64,
        turn_started_at: Option<&str>,
        turn_start_offset: Option<u64>,
    ) -> Self {
        Self::new_for_site(
            channel_id,
            generation,
            user_msg_id,
            turn_started_at,
            turn_start_offset,
            "delivery_lease_key",
        )
    }

    #[track_caller]
    pub(in crate::services::discord) fn new_for_site(
        channel_id: ChannelId,
        generation: u64,
        user_msg_id: u64,
        turn_started_at: Option<&str>,
        turn_start_offset: Option<u64>,
        site: &'static str,
    ) -> Self {
        if user_msg_id == 0 {
            let started_at = turn_started_at
                .map(str::trim)
                .filter(|value| !value.is_empty());
            if let (Some(started_at), Some(start_offset)) = (started_at, turn_start_offset) {
                return Self {
                    channel_id,
                    generation,
                    user_msg_id,
                    turn_started_at: Some(started_at.to_string()),
                    turn_start_offset: Some(start_offset),
                };
            }

            let caller = std::panic::Location::caller();
            tracing::warn!(
                channel_id = channel_id.get(),
                generation,
                delivery_lease_site = site,
                caller_file = caller.file(),
                caller_line = caller.line(),
                "delivery lease id-0 turn missing disambiguators; using degenerate legacy key"
            );
            // Residual legacy fallback: all sites derive id-0 disambiguators from
            // the same origin (inflight state / frame fence stamped from it), so a
            // same-turn miss degrades everywhere together and dedup still holds.
            Self {
                channel_id,
                generation,
                user_msg_id,
                turn_started_at: None,
                turn_start_offset: None,
            }
        } else {
            // Preserve the old non-zero TurnKey behavior: the Discord snowflake is
            // already the turn discriminator, so auxiliary timestamps must not
            // participate in equality for non-zero ids.
            Self {
                channel_id,
                generation,
                user_msg_id,
                turn_started_at: None,
                turn_start_offset: None,
            }
        }
    }

    #[track_caller]
    pub(in crate::services::discord) fn from_turn_key(turn: turn_finalizer::TurnKey) -> Self {
        Self::from_turn_key_for_site(turn, "delivery_lease_key.turn")
    }

    #[track_caller]
    pub(in crate::services::discord) fn from_turn_key_for_site(
        turn: turn_finalizer::TurnKey,
        site: &'static str,
    ) -> Self {
        Self::new_for_site(
            turn.channel_id,
            turn.generation,
            turn.user_msg_id,
            None,
            None,
            site,
        )
    }

    #[track_caller]
    pub(in crate::services::discord) fn from_inflight_state(
        channel_id: ChannelId,
        generation: u64,
        state: &inflight::InflightTurnState,
    ) -> Self {
        Self::from_inflight_state_for_site(
            channel_id,
            generation,
            state,
            "delivery_lease_key.inflight",
        )
    }

    #[track_caller]
    pub(in crate::services::discord) fn from_inflight_state_for_site(
        channel_id: ChannelId,
        generation: u64,
        state: &inflight::InflightTurnState,
        site: &'static str,
    ) -> Self {
        Self::new_for_site(
            channel_id,
            generation,
            state.user_msg_id,
            Some(&state.started_at),
            state.turn_start_offset,
            site,
        )
    }

    pub(in crate::services::discord) fn channel_id(&self) -> ChannelId {
        self.channel_id
    }
}
