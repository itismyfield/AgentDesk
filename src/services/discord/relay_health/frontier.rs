use std::sync::{Arc, atomic::Ordering};

use poise::serenity_prelude::ChannelId;

use super::super::{SharedData, TmuxRelayCoord};

#[derive(Default)]
pub(in crate::services::discord) struct FrontierResetState {
    incarnation: u64,
    active_mutations: usize,
}

pub(in crate::services::discord) struct RelayFrontierMutationGuard {
    coord: Arc<TmuxRelayCoord>,
}

impl Drop for RelayFrontierMutationGuard {
    fn drop(&mut self) {
        let mut state = self
            .coord
            .reset_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.active_mutations -= 1;
    }
}

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

    pub(in crate::services::discord) fn acquire_relay_frontier_mutation(
        &self,
        channel_id: ChannelId,
        token: RelayFrontierToken,
    ) -> Option<RelayFrontierMutationGuard> {
        let coord = self.tmux_relay_coord(channel_id);
        let mut state = coord
            .reset_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.incarnation != token.reset_incarnation
            || coord.confirmed_end_offset.load(Ordering::Acquire) != token.committed_offset
        {
            return None;
        }
        state.active_mutations += 1;
        drop(state);
        Some(RelayFrontierMutationGuard { coord })
    }
}

impl TmuxRelayCoord {
    pub(in crate::services::discord) fn frontier_token(&self) -> RelayFrontierToken {
        let reset_incarnation = self
            .reset_state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .incarnation;
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
        let mut state = self
            .reset_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.active_mutations != 0 {
            return false;
        }
        let reset = self
            .confirmed_end_offset
            .compare_exchange(
                expected_offset,
                new_offset,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        if reset {
            state.incarnation = state.incarnation.wrapping_add(1);
        }
        reset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn reset_yields_to_pending_mutation_on_same_thread_without_deadlock() {
        let coord = Arc::new(TmuxRelayCoord::new(ChannelId::new(4_182)));
        coord.confirmed_end_offset.store(100, Ordering::Release);
        let mut state = coord
            .reset_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.active_mutations += 1;
        drop(state);
        let guard = RelayFrontierMutationGuard {
            coord: Arc::clone(&coord),
        };

        tokio::time::timeout(std::time::Duration::from_millis(50), async {
            assert!(!coord.reset_confirmed_frontier(100, 40));
        })
        .await
        .expect("reset must not block the executor needed to release the mutation");
        assert_eq!(coord.confirmed_end_offset.load(Ordering::Acquire), 100);
        drop(guard);
        tokio::task::yield_now().await;
        assert!(coord.reset_confirmed_frontier(100, 40));
    }

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
