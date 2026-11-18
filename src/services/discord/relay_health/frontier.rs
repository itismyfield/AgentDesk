use super::super::{SharedData, TmuxRelayCoord};
#[cfg(all(test, unix))]
pub(crate) use crate::services::engine_runtime::relay_state::FrontierPublication;
pub(in crate::services::discord) use crate::services::engine_runtime::relay_state::RelayFrontierToken;
use poise::serenity_prelude::ChannelId;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::Ordering;
#[cfg(all(test, unix))]
thread_local! {
    static BEFORE_GENERATION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
fn before_generation() {
    #[cfg(all(test, unix))]
    BEFORE_GENERATION.with_borrow_mut(|hook| {
        if let Some(hook) = hook.take() {
            hook();
        }
    });
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
        state.release_mutation();
    }
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
        if !state.admit_mutation(token, &coord.confirmed_end_offset) {
            return None;
        }
        drop(state);
        Some(RelayFrontierMutationGuard { coord })
    }

    /// Admission for a delivery whose authority is the source **incarnation**,
    /// not a whole frontier snapshot.
    ///
    /// #4911 R10: this guard is NOT mutual exclusion — it is an admission
    /// counter whose only effect is to keep [`TmuxRelayCoord::reset_confirmed_frontier_with_generation`]
    /// from rewinding the frontier while a delivery is in flight. Admitting on
    /// the full [`RelayFrontierToken`] therefore gates on `committed_offset`,
    /// which a HARMLESS concurrent advance changes; that turned valid deliveries
    /// into spurious `LandedStale`. Reset incarnation is the only field that
    /// actually says "these bytes are still from the source I captured".
    pub(in crate::services::discord) fn acquire_relay_frontier_mutation_for_incarnation(
        &self,
        channel_id: ChannelId,
        reset_incarnation: u64,
    ) -> Option<RelayFrontierMutationGuard> {
        let coord = self.tmux_relay_coord(channel_id);
        let mut state = coord
            .reset_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !state.admit_incarnation(reset_incarnation) {
            return None;
        }
        drop(state);
        Some(RelayFrontierMutationGuard { coord })
    }
}

impl TmuxRelayCoord {
    pub(in crate::services::discord) fn advance_untyped_frontier(
        &self,
        end: u64,
        generation: Option<i64>,
    ) -> u64 {
        let mut state = self
            .reset_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.advance_untyped_frontier(
            end,
            generation,
            &self.confirmed_end_offset,
            &self.confirmed_end_generation_mtime_ns,
            before_generation,
        )
    }
    pub(in crate::services::discord) fn frontier_token(&self) -> RelayFrontierToken {
        let mut state = self
            .reset_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.frontier_token(
            &self.confirmed_end_offset,
            &self.confirmed_end_generation_mtime_ns,
        )
    }
    #[cfg(test)]
    pub(in crate::services::discord) fn reset_confirmed_frontier(
        &self,
        expected: u64,
        new: u64,
    ) -> bool {
        self.reset_confirmed_frontier_with_generation(expected, new, None)
    }
    pub(in crate::services::discord) fn reset_confirmed_frontier_with_generation(
        &self,
        expected: u64,
        new: u64,
        generation: Option<i64>,
    ) -> bool {
        debug_assert!(new < expected);
        let mut state = self
            .reset_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.reset_confirmed_frontier_with_generation(
            expected,
            new,
            generation,
            &self.confirmed_end_offset,
            &self.confirmed_end_generation_mtime_ns,
            before_generation,
        )
    }
    #[cfg(all(test, unix))]
    pub(crate) fn publish_confirmed_for_tests(
        &self,
        end: u64,
        generation: i64,
        reset: u64,
        seal: Option<&crate::services::cluster::stream_relay::source_read_proof::PrefixSeal>,
    ) -> FrontierPublication {
        let certificate = seal.and_then(|seal| {
            crate::services::cluster::stream_relay::source_read_proof::SourceCertificate::confirmed(
                seal, end, generation, reset,
            )
        });
        let mut state = self
            .reset_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.publish_confirmed_for_tests(
            end,
            generation,
            certificate,
            &self.confirmed_end_offset,
            &self.confirmed_end_generation_mtime_ns,
        )
    }
    #[cfg(all(test, unix))]
    pub(crate) fn frontier_tuple_for_tests(&self) -> (u64, i64, u64, bool) {
        let state = self
            .reset_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.frontier_tuple_for_tests(
            &self.confirmed_end_offset,
            &self.confirmed_end_generation_mtime_ns,
        )
    }
    #[cfg(all(test, unix))]
    pub(crate) fn certificate_prefix_is_current_for_tests(&self) -> bool {
        let certificate = self
            .reset_state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .certificate_for_tests();
        certificate.is_some_and(|certificate| certificate.prefix_is_current())
    }
    #[cfg(all(test, unix))]
    pub(crate) fn before_generation_for_tests(&self, hook: impl FnOnce() + 'static) {
        BEFORE_GENERATION.with_borrow_mut(|slot| *slot = Some(Box::new(hook)));
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn reset_yields_to_pending_mutation_on_same_thread_without_deadlock() {
        let coord = Arc::new(TmuxRelayCoord::new(ChannelId::new(4_182)));
        coord.confirmed_end_offset.store(100, Ordering::Release);
        {
            let mut state = coord
                .reset_state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            assert!(state.admit_incarnation(0));
        }
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
