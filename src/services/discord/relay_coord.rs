use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use crate::services::engine_runtime::relay_state::coord::RelayState;

/// Per-channel coordination for watcher-to-Discord relay emission.
///
/// Shared across watcher-handle replacements, this serializes overlapping
/// outgoing/successor relay emission and exposes the confirmed-output watermark.
/// Scope: intra-process only; restart-persistent dedupe remains in
/// `InflightTurnState::last_watcher_relayed_offset`.
pub(in crate::services) struct TmuxRelayCoord {
    relay: RelayState,
    /// Cooperative replacement custody stays with the Discord wrapper.
    #[cfg(unix)]
    pub(in crate::services::discord) cancel_handoffs:
        super::tmux::tmux_watcher::cancel_handoff::Store,
}

impl TmuxRelayCoord {
    pub(in crate::services::discord) fn new(channel_id: ChannelId) -> Self {
        let relay_slot = Arc::new(std::sync::atomic::AtomicU64::new(0));
        #[cfg(unix)]
        let cancel_handoffs = Default::default();
        let relay = RelayState::new(channel_id, relay_slot);
        Self {
            relay,
            #[cfg(unix)]
            cancel_handoffs,
        }
    }
}

impl std::ops::Deref for TmuxRelayCoord {
    type Target = RelayState;

    fn deref(&self) -> &Self::Target {
        &self.relay
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::engine_runtime::relay_state::coord::RelayState;
    use std::sync::atomic::Ordering;

    #[test]
    fn mixed_coord_exposes_the_same_relay_state_and_atomics() {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(819_202);
        let coord = shared.tmux_relay_coord(channel);
        let state: &RelayState = coord.as_ref();
        assert!(std::ptr::eq(state, &coord.relay));
        assert!(Arc::ptr_eq(&state.relay_slot, &coord.relay_slot));
        assert!(Arc::ptr_eq(
            &state.confirmed_end_offset,
            &coord.confirmed_end_offset
        ));
        assert!(std::ptr::eq(&state.reset_state, &coord.reset_state));
        assert!(Arc::ptr_eq(
            &state.last_relay_ts_ms,
            &coord.last_relay_ts_ms
        ));
        assert!(Arc::ptr_eq(&state.reconnect_count, &coord.reconnect_count));
        assert!(Arc::ptr_eq(
            &state.confirmed_end_generation_mtime_ns,
            &coord.confirmed_end_generation_mtime_ns,
        ));
        assert!(Arc::ptr_eq(&state.delivery_lease, &coord.delivery_lease));
        assert_eq!(state.relay_slot.load(Ordering::Acquire), 0);
        assert_eq!(state.confirmed_end_offset.load(Ordering::Acquire), 0);
        assert_eq!(state.last_relay_ts_ms.load(Ordering::Acquire), 0);
        assert_eq!(state.reconnect_count.load(Ordering::Acquire), 0);
        assert_eq!(
            state
                .confirmed_end_generation_mtime_ns
                .load(Ordering::Acquire),
            0
        );
        assert!(matches!(
            state.delivery_lease.read(),
            crate::services::engine_runtime::lease::LeaseSnapshot::Unleased,
        ));
        assert!(
            coord
                .relay_slot
                .compare_exchange(0, 17, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        );
        assert!(
            state
                .relay_slot
                .compare_exchange(0, 18, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        );
        coord.note_relay_progress_heartbeat(123);
        assert_eq!(state.last_relay_ts_ms.load(Ordering::Acquire), 123);
        let again = shared.tmux_relay_coord(channel);
        assert!(Arc::ptr_eq(&coord, &again));
        assert!(std::ptr::eq(&coord.relay, &again.relay));
        assert_eq!(again.relay_slot.load(Ordering::Acquire), 17);
        let other = shared.tmux_relay_coord(ChannelId::new(819_203));
        assert!(!Arc::ptr_eq(&state.delivery_lease, &other.delivery_lease));
        assert_eq!(other.relay_slot.load(Ordering::Acquire), 0);
    }

    #[cfg(unix)]
    #[test]
    fn repeated_lookup_preserves_the_custody_store_and_delivery_lease() {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(819_204);
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        shared.tmux_watchers.insert(
            channel,
            crate::services::discord::TmuxWatcherHandle {
                tmux_session_name: format!("relay-state-{}", uuid::Uuid::new_v4()),
                output_path: String::new(),
                paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                resume_offset: Arc::new(std::sync::Mutex::new(None)),
                cancel: Arc::clone(&cancel),
                pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            },
        );
        let original = shared.tmux_relay_coords.get(&channel).unwrap().clone();
        assert!(original.delivery_lease.watcher_reservation_matches(&cancel));
        let store = original.cancel_handoffs.clone();
        let lease = Arc::clone(&original.delivery_lease);
        let slot = Arc::clone(&original.relay_slot);
        drop(original);
        let successor = shared.tmux_relay_coord(channel);
        assert!(Arc::ptr_eq(&store, &successor.cancel_handoffs));
        assert!(Arc::ptr_eq(&lease, &successor.delivery_lease));
        assert!(Arc::ptr_eq(&slot, &successor.relay_slot));
        let other = shared.tmux_relay_coord(ChannelId::new(819_205));
        assert!(!Arc::ptr_eq(&store, &other.cancel_handoffs));
    }
}
