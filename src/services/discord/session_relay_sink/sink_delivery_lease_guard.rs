use std::sync::Arc;

/// #3151: RAII in-flight sink-delivery marker on the per-channel
/// [`crate::services::discord::DeliveryLeaseCell`], acquired as [`crate::services::discord::LeaseHolder::Sink`] for the SAME
/// `(channel, turn, [start,end))` the watcher's §3.2 reconciliation computes, BEFORE the
/// POST; a [`crate::services::discord::DeliveryLeaseHeartbeat`] renews the deadline so the watcher reads
/// `Leased{Sink, fresh}` and WAITS instead of re-sending (slow-sink dup). RECLAIMABLE: a
/// crashed sink stops renewing → the watcher reclaims within ~one deadline (no black-hole).
/// CLEAR ordering (SUCCESS): advance committed FIRST (`advance_after_confirmed_post`) THEN
/// [`Self::commit`] → watcher reads `committed >= end` → Skip. EVERY exit Drop RELEASES
/// (full-identity → stale no-ops); a never-committed failure leaves `Unleased`, committed
/// NOT advanced → watcher SendFull.
pub(super) struct SinkDeliveryLeaseGuard {
    cell: Arc<crate::services::discord::DeliveryLeaseCell>,
    key: crate::services::discord::DeliveryLeaseKey,
    start: u64,
    end: u64,
    /// The in-flight heartbeat; aborted on Drop (mirrors the watcher's RAII).
    _heartbeat: crate::services::discord::DeliveryLeaseHeartbeat,
}

impl SinkDeliveryLeaseGuard {
    /// Self-heal a dead PRIOR holder, then CAS-acquire as `LeaseHolder::Sink` for
    /// `(turn, [start,end))`. `Some` (spawning the heartbeat) only when the acquire wins;
    /// `None` means another holder owns the range and the caller must return NotDelivered
    /// without reaching transport.
    pub(super) fn acquire(
        cell: &Arc<crate::services::discord::DeliveryLeaseCell>,
        key: crate::services::discord::DeliveryLeaseKey,
        start: u64,
        end: u64,
    ) -> Option<Self> {
        // Mirror the watcher's self-healing acquire: reclaim an expired prior holder.
        cell.reclaim_if_expired(crate::services::discord::lease_now_ms());
        let acquired = cell.try_acquire(
            key.clone(),
            crate::services::discord::LeaseHolder::Sink,
            start,
            end,
            crate::services::discord::lease_now_ms()
                .saturating_add(crate::services::discord::DELIVERY_LEASE_DEADLINE_MS),
        );
        if !acquired {
            return None;
        }
        let heartbeat = crate::services::discord::DeliveryLeaseHeartbeat::spawn(
            cell.clone(),
            crate::services::discord::LeaseHolder::Sink,
            key.clone(),
        );
        Some(Self {
            cell: cell.clone(),
            key,
            start,
            end,
            _heartbeat: heartbeat,
        })
    }

    /// Terminal-decision commit, AFTER the advance was attempted: `outcome` reflects
    /// whether it ACTUALLY happened — `Delivered` only when the offset advanced (so the
    /// watcher reads `committed >= end` → Skip), else `NotDelivered` (offset `< end` →
    /// the watcher re-sends → SendFull, no black-hole). Full-identity compare-and-X →
    /// a stale older-turn clear no-ops. Drop still releases.
    pub(super) fn commit(&self, outcome: crate::services::discord::LeaseOutcome) {
        self.cell.commit(
            crate::services::discord::LeaseHolder::Sink,
            self.key.clone(),
            self.start,
            self.end,
            outcome,
        );
    }
}

impl Drop for SinkDeliveryLeaseGuard {
    fn drop(&mut self) {
        // Release on EVERY exit. `release` is valid from `Leased` (failure) and `Committed`
        // (success) and full-identity-gated, so it clears ONLY our marker — a newer turn
        // that re-leased this cell survives. (`_heartbeat` Drop aborts the renew task.)
        self.cell.release(
            crate::services::discord::LeaseHolder::Sink,
            self.key.clone(),
            self.start,
            self.end,
        );
    }
}
