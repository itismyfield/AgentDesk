use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use poise::serenity_prelude::ChannelId;

use super::relay_health::FrontierResetState;
use super::{DeliveryLeaseCell, UNRECORDED_RELAY_OFFSET};

/// Per-channel coordination for watcher-to-Discord relay emission.
///
/// Shared across watcher-handle replacements, this serializes overlapping
/// outgoing/successor relay emission and exposes the confirmed-output watermark.
/// Scope: intra-process only; restart-persistent dedupe remains in
/// `InflightTurnState::last_watcher_relayed_offset`.
pub(in crate::services::discord) struct TmuxRelayCoord {
    /// Non-zero while some watcher instance is actively emitting a relay for
    /// this channel. Holds the `data_start_offset` of the in-progress emission.
    /// Acquired via `compare_exchange(0, offset)` — only one watcher can
    /// hold the slot, so concurrent attempts from outgoing+incoming watchers
    /// serialize rather than double-fire.
    pub(in crate::services::discord) relay_slot: Arc<AtomicU64>,
    /// End offset (exclusive) of the last relay this process has confirmed
    /// delivery for. `UNRECORDED_RELAY_OFFSET` means no frontier writer has
    /// published yet; 0 is an intentionally published zero frontier.
    ///
    /// This atomic stays private to this module so every reader must choose
    /// publication-aware (`confirmed_end_publication`) or compatibility-zero
    /// (`confirmed_end_or_zero`) semantics. The sentinel can never escape into
    /// offset arithmetic or ordering comparisons.
    confirmed_end_offset: Arc<AtomicU64>,
    pub(in crate::services::discord) reset_state: std::sync::Mutex<FrontierResetState>,
    /// Wall-clock timestamp (ms since epoch) of the most recent confirmed
    /// relay. 0 = no confirmed relay observed yet. Read by the
    /// `watcher-state` observability endpoint (#964). Monotonic is NOT
    /// required — this is a telemetry field only.
    pub(in crate::services::discord) last_relay_ts_ms: Arc<AtomicI64>,
    /// Number of watcher reattach/reconnect spawns observed for this channel
    /// in the current dcserver process. Exposed through watcher-state (#964).
    pub(in crate::services::discord) reconnect_count: Arc<AtomicU64>,
    /// `.generation` marker file mtime (nanos since epoch) snapshotted the
    /// last time `confirmed_end_offset` was advanced. 0 = never observed.
    pub(in crate::services::discord) confirmed_end_generation_mtime_ns: Arc<AtomicI64>,
    pub(in crate::services::discord) delivery_lease: Arc<DeliveryLeaseCell>,
}

impl TmuxRelayCoord {
    pub(in crate::services::discord) fn new(channel_id: ChannelId) -> Self {
        Self {
            relay_slot: Arc::new(AtomicU64::new(0)),
            confirmed_end_offset: Arc::new(AtomicU64::new(UNRECORDED_RELAY_OFFSET)),
            reset_state: std::sync::Mutex::new(FrontierResetState::default()),
            last_relay_ts_ms: Arc::new(AtomicI64::new(0)),
            reconnect_count: Arc::new(AtomicU64::new(0)),
            confirmed_end_generation_mtime_ns: Arc::new(AtomicI64::new(0)),
            delivery_lease: Arc::new(DeliveryLeaseCell::new(channel_id)),
        }
    }

    /// Read the confirmed frontier while preserving whether any writer has
    /// published. `Some(0)` is a real recorded-zero frontier; `None` is fresh.
    pub(in crate::services::discord) fn confirmed_end_publication(&self) -> Option<u64> {
        super::published_relay_offset(self.confirmed_end_offset.load(Ordering::Acquire))
    }

    /// Compatibility read for consumers where a fresh frontier historically
    /// behaved as zero and publication identity is not part of the decision.
    pub(in crate::services::discord) fn confirmed_end_or_zero(&self) -> u64 {
        self.confirmed_end_publication().unwrap_or(0)
    }

    /// Monotonically publish or advance the confirmed frontier.
    pub(in crate::services::discord) fn advance_confirmed_end(
        &self,
        committed_end_offset: u64,
    ) -> bool {
        let mut current = self.confirmed_end_offset.load(Ordering::Acquire);
        let mut advanced = false;
        if current == UNRECORDED_RELAY_OFFSET {
            current = match self.confirmed_end_offset.compare_exchange(
                UNRECORDED_RELAY_OFFSET,
                committed_end_offset,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    advanced = true;
                    committed_end_offset
                }
                Err(observed) => observed,
            };
        }
        while current < committed_end_offset {
            match self.confirmed_end_offset.compare_exchange(
                current,
                committed_end_offset,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    advanced = true;
                    current = committed_end_offset;
                    break;
                }
                Err(observed) => current = observed,
            }
        }
        advanced
    }

    pub(in crate::services::discord) fn reset_confirmed_frontier(
        &self,
        expected_offset: u64,
        new_offset: u64,
    ) -> bool {
        debug_assert_ne!(
            expected_offset, UNRECORDED_RELAY_OFFSET,
            "an unpublished relay frontier must never enter reset arithmetic"
        );
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

    pub(in crate::services::discord) fn note_relay_progress_heartbeat(&self, now_ms: i64) {
        self.last_relay_ts_ms.store(now_ms, Ordering::Release);
    }

    #[cfg(test)]
    pub(in crate::services::discord) fn publish_confirmed_end_for_test(&self, offset: u64) {
        assert_ne!(offset, UNRECORDED_RELAY_OFFSET);
        self.confirmed_end_offset.store(offset, Ordering::Release);
    }
}
