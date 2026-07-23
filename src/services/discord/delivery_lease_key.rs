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
        Self::new_for_site_with_fallback_offset(
            channel_id,
            generation,
            user_msg_id,
            turn_started_at,
            turn_start_offset,
            None,
            site,
        )
    }

    /// Build a lease key while allowing the canonical relay-range start to stand
    /// in for a missing persisted turn boundary. The fallback is authoritative
    /// only when `started_at` is also absent; partially identified legacy callers
    /// keep the historical all-degenerate collapse so their actor backstop keys do
    /// not silently change. Watcher and sink owners pass the same `[start, end)`
    /// delivery range start, so an inflight-less TUI-direct turn still converges.
    #[track_caller]
    pub(in crate::services::discord) fn new_for_site_with_fallback_offset(
        channel_id: ChannelId,
        generation: u64,
        user_msg_id: u64,
        turn_started_at: Option<&str>,
        turn_start_offset: Option<u64>,
        fallback_turn_start_offset: Option<u64>,
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
            if started_at.is_none()
                && turn_start_offset.is_none()
                && let Some(start_offset) = fallback_turn_start_offset
            {
                return Self {
                    channel_id,
                    generation,
                    user_msg_id,
                    turn_started_at: None,
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
            return Self {
                channel_id,
                generation,
                user_msg_id,
                turn_started_at: None,
                turn_start_offset: None,
            };
        }

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

    pub(in crate::services::discord) fn is_degenerate_legacy(&self) -> bool {
        self.user_msg_id == 0 && self.turn_started_at.is_none() && self.turn_start_offset.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fallback_key(channel: ChannelId, offset: u64, site: &'static str) -> DeliveryLeaseKey {
        DeliveryLeaseKey::new_for_site_with_fallback_offset(
            channel,
            33,
            0,
            None,
            None,
            Some(offset),
            site,
        )
    }

    #[test]
    fn id_zero_without_inflight_uses_observed_range_start_4277() {
        let channel = ChannelId::new(4_277);
        let first = fallback_key(channel, 2_484_989, "watcher");
        let same = fallback_key(channel, 2_484_989, "idle_tail");
        let next = fallback_key(channel, 2_486_000, "watcher");

        assert!(!first.is_degenerate_legacy());
        assert_eq!(
            first, same,
            "both relay owners must contend on one turn key"
        );
        assert_ne!(first, next, "a later TUI-direct turn needs a distinct key");
    }

    #[test]
    fn watcher_and_idle_tail_cannot_acquire_same_fallback_turn_concurrently_4277() {
        let channel = ChannelId::new(4_278);
        let cell = super::super::DeliveryLeaseCell::new(channel);
        let watcher_key = fallback_key(channel, 900, "watcher");
        let idle_key = fallback_key(channel, 900, "idle_tail");

        assert!(cell.try_acquire(
            watcher_key,
            super::super::LeaseHolder::Watcher { instance_id: 1 },
            900,
            1_200,
            super::super::lease_now_ms().saturating_add(1_000),
        ));
        assert!(
            !cell.try_acquire(
                idle_key,
                super::super::LeaseHolder::Sink,
                900,
                1_200,
                super::super::lease_now_ms().saturating_add(1_000),
            ),
            "the second relay owner must lose the one-turn delivery lease"
        );
    }
}
