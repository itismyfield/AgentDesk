use super::{
    ChannelId, DeliveryLeaseCell, DeliveryLeaseKey, LeaseHolder, LeaseOutcome, LeaseSnapshot,
    LeaseState, TAG_COMMITTED, TAG_LEASED, TAG_UNLEASED,
};

#[allow(dead_code)] // #3041 P1-0: dormant, wired in P1-1..
impl DeliveryLeaseCell {
    /// Construct a fresh `Unleased` cell for `channel_id`. The lease key and
    /// byte range are supplied per-acquire, not at
    /// construction, so one cell serves the channel across sequential turns.
    pub(in crate::services::discord) fn new(channel_id: ChannelId) -> Self {
        Self {
            channel_id,
            state_tag: std::sync::atomic::AtomicU8::new(TAG_UNLEASED),
            payload: std::sync::Mutex::new(LeaseState::Unleased),
        }
    }

    /// The channel this lease coordinates.
    pub(in crate::services::discord) fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// Read the current lease state. Always materialized UNDER the payload
    /// mutex so the snapshot can never disagree with a concurrently-acquiring
    /// writer (#3041 codex): because `try_acquire`/`commit`/`release`/`reclaim`
    /// flip `state_tag` AND write `payload` while holding the SAME mutex, any
    /// observer that takes the lock sees a tag/payload pair that are mutually
    /// coherent. `state_tag` remains the single-winner CAS gate for acquire; it
    /// is NOT used as a lock-free read fast-path here because that reintroduced
    /// the publish/observe window the codex review flagged.
    pub(in crate::services::discord) fn read(&self) -> LeaseSnapshot {
        let guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*guard {
            LeaseState::Unleased => LeaseSnapshot::Unleased,
            LeaseState::Leased {
                holder,
                key,
                deadline_ms,
                start,
                end,
            } => LeaseSnapshot::Leased {
                holder: *holder,
                key: key.clone(),
                deadline_ms: *deadline_ms,
                start: *start,
                end: *end,
            },
            LeaseState::Committed {
                holder,
                key,
                start,
                end,
                outcome,
            } => LeaseSnapshot::Committed {
                holder: *holder,
                key: key.clone(),
                start: *start,
                end: *end,
                outcome: *outcome,
            },
        }
    }

    /// CAS-acquire the lease for the full `(delivery_lease_key, [start,end))`
    /// identity (#3041 §2) on behalf of `holder` until `deadline_ms`. Records
    /// `key` so a later `commit`/`release` carrying a STALE older lease key is
    /// rejected (the §2 hazard: a reclaim+reacquire reuses the same holder kind,
    /// so holder alone is insufficient).
    ///
    /// Ordering invariant (codex coherence fix): the tag CAS and the payload
    /// write happen UNDER the SAME mutex, and `read()` also locks, so a tag and
    /// its payload are never observed out of step. The CAS keeps single-winner
    /// semantics — exactly one acquirer flips `UNLEASED → LEASED`; every
    /// concurrent loser (already holding the lock by then) sees a non-`UNLEASED`
    /// tag under the lock and returns `false` without mutating the payload.
    pub(in crate::services::discord) fn try_acquire(
        &self,
        key: DeliveryLeaseKey,
        holder: LeaseHolder,
        start: u64,
        end: u64,
        deadline_ms: u64,
    ) -> bool {
        use std::sync::atomic::Ordering;
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Single-winner gate, taken while holding the payload lock so the tag
        // flip and the payload write publish together. Concurrent acquirers
        // serialize on the mutex; whoever runs second sees a non-`UNLEASED` tag.
        if self
            .state_tag
            .compare_exchange(
                TAG_UNLEASED,
                TAG_LEASED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        *guard = LeaseState::Leased {
            holder,
            key,
            deadline_ms,
            start,
            end,
        };
        true
    }

    /// Commit the lease three-way (#3041 §3). Verifies the FULL `(holder, key,
    /// [start,end))` identity against the currently-`Leased` lease (#3041 §2):
    /// any mismatch — wrong holder, a STALE older lease key, or a different range
    /// — or a non-`Leased` state is a no-op that returns `false`. This closes
    /// the §2 hazard where a stale commit from an older turn could act on a
    /// reacquired same-channel/same-holder-kind lease. On success the tag
    /// advances `LEASED → COMMITTED` (under the lock) and the outcome is
    /// recorded. `Unknown` records but the caller MUST NOT advance the offset.
    pub(in crate::services::discord) fn commit(
        &self,
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
        start: u64,
        end: u64,
        outcome: LeaseOutcome,
    ) -> bool {
        use std::sync::atomic::Ordering;
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*guard {
            LeaseState::Leased {
                holder: cur_holder,
                key: cur_key,
                start: cur_start,
                end: cur_end,
                ..
            } if *cur_holder == holder
                && cur_key == &key
                && *cur_start == start
                && *cur_end == end =>
            {
                *guard = LeaseState::Committed {
                    holder,
                    key,
                    start,
                    end,
                    outcome,
                };
                self.state_tag.store(TAG_COMMITTED, Ordering::Release);
                true
            }
            // Identity mismatch (holder / stale turn / range) or not Leased.
            _ => false,
        }
    }

    /// Compare-and-release: return the cell to `Unleased` ONLY if the FULL
    /// `(holder, key, [start,end))` identity matches the recorded lease (#3041
    /// §2-§3) — symmetric with `commit`. Verifying the key AND the byte range
    /// (not just the holder) is what closes the §2 hazard: a stale release from
    /// an OLDER turn — or from the SAME turn but an OLDER byte range after a
    /// reclaim+reacquire re-leased a different range (e.g. a continuation chunk)
    /// — is a no-op returning `false`, so it can never release the live newer
    /// lease. A release is valid from either `Leased` (abandoned without commit)
    /// or `Committed` (the normal post-commit release).
    pub(in crate::services::discord) fn release(
        &self,
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
        start: u64,
        end: u64,
    ) -> bool {
        use std::sync::atomic::Ordering;
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let matches = match &*guard {
            LeaseState::Leased {
                holder: cur,
                key: cur_key,
                start: cur_start,
                end: cur_end,
                ..
            }
            | LeaseState::Committed {
                holder: cur,
                key: cur_key,
                start: cur_start,
                end: cur_end,
                ..
            } => *cur == holder && cur_key == &key && *cur_start == start && *cur_end == end,
            LeaseState::Unleased => false,
        };
        if !matches {
            return false;
        }
        *guard = LeaseState::Unleased;
        self.state_tag.store(TAG_UNLEASED, Ordering::Release);
        true
    }

    /// #3041 P1-1 (§3, codex R2 Issue-1): HEARTBEAT renew. While the holder's
    /// terminal send future is in flight, the holder periodically calls this to
    /// extend the lease deadline so the (deliberately SHORT) deadline is a
    /// HOLDER-LIVENESS signal, not a hard cap on delivery duration. If the cell
    /// is `Leased` by EXACTLY `(holder, key)` (matched on holder + delivery lease
    /// key), its `deadline_ms` is overwritten with `new_deadline_ms`
    /// and `true` is returned. ANY other state — a different holder, a stale
    /// older key, a `Committed`/`Unleased` cell, or a cell already reclaimed and
    /// reacquired by someone else — is a no-op returning `false`. The range is
    /// intentionally NOT matched: a renew only ever needs to prove "this exact
    /// holder for this exact lease key is still alive", and the live holder's range is
    /// fixed for the lifetime of the lease anyway.
    ///
    /// Race-safety (why renew can never extend SOMEONE ELSE's lease): the match
    /// requires the recorded `holder` AND `key` to equal the caller's, both
    /// taken UNDER the same payload mutex as every other mutation. If the cell
    /// was reclaimed (→ `Unleased`) and reacquired by a replacement, the holder
    /// or key will differ and the renew no-ops. A late heartbeat tick that
    /// fires after the holder already committed sees `Committed` (not `Leased`)
    /// and no-ops. The ONLY successful renew extends the caller's OWN live lease.
    pub(in crate::services::discord) fn renew(
        &self,
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
        new_deadline_ms: u64,
    ) -> bool {
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let LeaseState::Leased {
            holder: cur_holder,
            key: cur_key,
            deadline_ms,
            ..
        } = &mut *guard
        {
            if *cur_holder == holder && cur_key == &key {
                *deadline_ms = new_deadline_ms;
                return true;
            }
        }
        false
    }

    /// Deadline reclaim: if the lease is `Leased` and `now_ms >= deadline_ms`,
    /// force it back to `Unleased` regardless of holder (the holder is presumed
    /// dead/stuck). Returns `true` if a reclaim occurred. A `Committed` lease is
    /// never reclaimed by deadline — it awaits an explicit holder `release`.
    pub(in crate::services::discord) fn reclaim_if_expired(&self, now_ms: u64) -> bool {
        use std::sync::atomic::Ordering;
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let LeaseState::Leased { deadline_ms, .. } = &*guard {
            if now_ms >= *deadline_ms {
                *guard = LeaseState::Unleased;
                self.state_tag.store(TAG_UNLEASED, Ordering::Release);
                return true;
            }
        }
        false
    }
}
