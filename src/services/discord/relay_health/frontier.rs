use std::sync::atomic::Ordering;

use poise::serenity_prelude::ChannelId;

use super::super::{SharedData, TmuxRelayCoord};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::services::discord) struct RelayFrontierToken {
    pub(in crate::services::discord) reset_incarnation: u64,
    pub(in crate::services::discord) committed_offset: u64,
}

impl SharedData {
    pub(in crate::services::discord) fn relay_frontier_token(
        &self,
        channel_id: ChannelId,
    ) -> RelayFrontierToken {
        self.tmux_relay_coord(channel_id).frontier_token()
    }

    pub(in crate::services::discord) fn relay_frontier_token_is_current(
        &self,
        channel_id: ChannelId,
        token: RelayFrontierToken,
    ) -> bool {
        self.relay_frontier_token(channel_id) == token
    }
}

impl TmuxRelayCoord {
    pub(in crate::services::discord) fn frontier_token(&self) -> RelayFrontierToken {
        let reset_incarnation = *self
            .reset_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        RelayFrontierToken {
            reset_incarnation,
            committed_offset: self.confirmed_end_offset.load(Ordering::Acquire),
        }
    }

    pub(in crate::services::discord) fn reset_confirmed_frontier(
        &self,
        expected_offset: u64,
        new_offset: u64,
    ) -> bool {
        debug_assert!(new_offset < expected_offset);
        let mut reset_incarnation = self
            .reset_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self
            .confirmed_end_offset
            .compare_exchange(
                expected_offset,
                new_offset,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        *reset_incarnation = reset_incarnation.wrapping_add(1);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_frontier_publishes_a_new_incarnation_token() {
        let coord = TmuxRelayCoord::new(ChannelId::new(4_181));
        coord.confirmed_end_offset.store(100, Ordering::Release);
        let high = coord.frontier_token();
        assert!(coord.reset_confirmed_frontier(100, 40));
        let low = coord.frontier_token();

        assert_eq!(high.committed_offset, 100);
        assert_eq!(low.committed_offset, 40);
        assert!(low.reset_incarnation > high.reset_incarnation);
        assert_ne!(high, low, "a reset must invalidate stale redrive tokens");
    }
}
