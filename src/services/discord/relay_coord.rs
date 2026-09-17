use std::sync::Arc;
use std::sync::atomic::Ordering;

use poise::serenity_prelude::ChannelId;

use super::{DeliveryLeaseCell, relay_health};

/// Per-channel coordination for watcher-to-Discord relay emission.
///
/// Shared across watcher-handle replacements, this serializes overlapping
/// outgoing/successor relay emission and exposes the confirmed-output watermark.
/// Scope: intra-process only; restart-persistent dedupe remains in
/// `InflightTurnState::last_watcher_relayed_offset`.
pub(in crate::services) struct TmuxRelayCoord {
    /// Non-zero while some watcher instance is actively emitting a relay for
    /// this channel. Holds the `data_start_offset` of the in-progress emission.
    /// Acquired via `compare_exchange(0, offset)` — only one watcher can
    /// hold the slot, so concurrent attempts from outgoing+incoming watchers
    /// serialize rather than double-fire.
    pub(in crate::services::discord) relay_slot: Arc<std::sync::atomic::AtomicU64>,
    /// Cooperative replacement state; never a receipt or shared-frontier writer.
    #[cfg(unix)]
    pub(in crate::services::discord) cancel_handoffs:
        super::tmux::tmux_watcher::cancel_handoff::Store,
    /// End offset (exclusive) of the last relay this process has confirmed
    /// delivery for; 0 = none yet this process lifetime (#3017).
    ///
    /// The single output-offset authority for relay-dedup: read via
    /// `SharedData::committed_relay_offset`, advanced by
    /// `advance_watcher_confirmed_end`. For inflight-less turns
    /// (wake/idle-background/monitor-auto), secondary relay actors consult
    /// this watermark so an already-committed byte-range relays exactly once
    /// regardless of observer order (E-13). For a normal Discord-origin turn
    /// the watcher remains sole relay owner.
    pub(in crate::services::discord) confirmed_end_offset: Arc<std::sync::atomic::AtomicU64>,
    pub(in crate::services::discord) reset_state:
        std::sync::Mutex<relay_health::FrontierResetState>,
    /// #5943: the read position the live watcher last committed for this
    /// channel — `tmux_watcher::loop_poll_prologue`'s `current_offset`,
    /// republished on every poll through the same write-back that commits it.
    /// 0 = no watcher has committed one in this process lifetime.
    ///
    /// NOT a delivery frontier and never read as one; `confirmed_end_offset`
    /// remains the sole relay-dedup authority. It exists so a redrive can tell
    /// a frontier that names bytes the watcher has ALREADY READ apart from one
    /// that names where the watcher still is — the two are identical at the
    /// byte level (both are just `u64`) and opposite in meaning. Lags the
    /// watcher by at most one poll, and only ever downward, so a redrive
    /// reading it fails OPEN rather than refusing a rewind it should allow.
    pub(in crate::services::discord) watcher_read_offset: Arc<std::sync::atomic::AtomicU64>,
    /// Wall-clock timestamp (ms since epoch) of the most recent confirmed
    /// relay. 0 = no confirmed relay observed yet. Read by the
    /// `watcher-state` observability endpoint (#964). Monotonic is NOT
    /// required — this is a telemetry field only.
    pub(in crate::services::discord) last_relay_ts_ms: Arc<std::sync::atomic::AtomicI64>,
    /// Number of watcher reattach/reconnect spawns observed for this channel
    /// in the current dcserver process. Exposed through watcher-state (#964).
    pub(in crate::services::discord) reconnect_count: Arc<std::sync::atomic::AtomicU64>,
    /// `.generation` marker file mtime (ns since epoch) snapshotted the last
    /// time `confirmed_end_offset` was advanced; 0 = never observed.
    ///
    /// `reset_stale_relay_watermark_if_output_regressed` (#1270) uses this to
    /// tell apart two output-regression scenarios identical at the byte
    /// level: mid-flight rotation (same `.generation` mtime — pin watermark
    /// to current EOF) vs. cancel→respawn (`.generation` deleted and
    /// recreated — new mtime, reset watermark to 0). `.generation` works as
    /// the wrapper-identity signal because it is written once per spawn and
    /// never touched by the live wrapper: its mtime survives jsonl rotation
    /// but flips on a fresh spawn.
    pub(in crate::services::discord) confirmed_end_generation_mtime_ns:
        Arc<std::sync::atomic::AtomicI64>,
    /// The live per-channel delivery lease (#3041 P1-1), added alongside
    /// `relay_slot` (not yet removed — guard migration is a later step). The
    /// watcher acquires this before delivering the terminal response and
    /// commits it after; the commit advances `confirmed_end_offset`
    /// (replacing the watcher's inline advance). Shared via `Arc` across all
    /// watcher instances so a replacement watcher observes a live holder's
    /// lease and skips the duplicate send (§5.2 B2).
    pub(in crate::services::discord) delivery_lease: Arc<DeliveryLeaseCell>,
}

impl TmuxRelayCoord {
    pub(in crate::services::discord) fn new(channel_id: ChannelId) -> Self {
        Self {
            relay_slot: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(unix)]
            cancel_handoffs: Default::default(),
            confirmed_end_offset: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            reset_state: std::sync::Mutex::new(relay_health::FrontierResetState::default()),
            watcher_read_offset: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_relay_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            reconnect_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            confirmed_end_generation_mtime_ns: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            delivery_lease: Arc::new(DeliveryLeaseCell::new(channel_id)),
        }
    }

    pub(in crate::services::discord) fn note_relay_progress_heartbeat(&self, now_ms: i64) {
        self.last_relay_ts_ms.store(now_ms, Ordering::Release);
    }
}

impl super::SharedData {
    /// #5943: record the read position the watcher is committing and hand the
    /// same value straight back, so the published offset cannot drift from the
    /// one the caller actually commits — the store rides ON the write-back
    /// rather than beside it.
    ///
    /// Lives here rather than beside `committed_relay_offset` in the module root
    /// because that root is a registered giant (`scripts/giant_file_registry.toml`,
    /// #4712) whose production line count may not grow.
    pub(in crate::services::discord) fn publish_watcher_read_offset(
        &self,
        channel_id: ChannelId,
        read_offset: u64,
    ) -> u64 {
        self.tmux_relay_coord(channel_id)
            .watcher_read_offset
            .store(read_offset, Ordering::Release);
        read_offset
    }

    /// #5943: the last read position a watcher published for this channel, or 0
    /// when none has. See [`TmuxRelayCoord::watcher_read_offset`] for why a stale
    /// read of this value can only make a caller more permissive.
    pub(in crate::services::discord) fn watcher_read_offset(&self, channel_id: ChannelId) -> u64 {
        self.tmux_relay_coord(channel_id)
            .watcher_read_offset
            .load(Ordering::Acquire)
    }
}
