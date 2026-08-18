use super::{ChannelId, DeliveryLeaseKey};

// ===========================================================================
// #3041 §2-§3 — Delivery-lease `DeliveryLeaseCell` state machine.
//
// As of P1-1 the WATCHER terminal-delivery path wires this LIVE: the watcher
// acquires the cell before sending, heartbeat-renews it during the send, and
// commits+advances+releases INLINE. The `relay_slot` field above is LEFT
// UNTOUCHED for now (its guard migration is a later step). The SINK/BRIDGE
// committers (P1-2) and the 3-way ACK reconciliation (P1-3) are not wired yet,
// so the actor `CommitDelivery`/`ReleaseDelivery` messages and some helpers
// remain dormant — those still carry targeted `#[allow(dead_code)]` attributes
// tagged with this issue/phase, to be wired/removed by the follow-up phases.
//
// Design (faithful to #3041 §2-§3):
//   lease = (delivery_lease_key, byte_range [start,end))
//           → a "one-time terminal-delivery right".
//   The lease key is deliberately separate from the finalizer's `TurnKey`: the
//   finalizer keeps its id-0 channel-collapse semantics, while delivery leasing
//   needs id-0 turns disambiguated by their inflight start identity.
//   State machine:
//     Unleased --(CAS acquire)--> Leased{holder, deadline, range}
//               --(commit)-------> Committed{Delivered|NotDelivered|Unknown}
//               --(release)------> Unleased
//     deadline reclaim: Leased --(deadline elapsed)--> Unleased
// ===========================================================================

/// Who currently holds (or is attempting to hold) the delivery lease.
///
/// #3041 P1-0: dormant, wired in P1-1.. — the holder is matched on
/// compare-and-release so an actor can only release a lease it actually owns.
#[allow(dead_code)] // #3041 P1-0: dormant, wired in P1-1..
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::services::discord) enum LeaseHolder {
    /// A tmux watcher instance. `instance_id` distinguishes an outgoing
    /// watcher from its successor across a reattach so a stale watcher cannot
    /// release the live watcher's lease.
    Watcher { instance_id: u64 },
    /// The standby / output sink relay.
    Sink,
    /// The bridge (turn-bridge handoff path).
    Bridge,
}

/// The three-way commit outcome (#3041 §3). `Unknown` is the safety value for
/// any ambiguous terminal (drop / panic / partial write) and MUST NOT advance
/// the confirmed-delivery offset — only `Delivered` does.
///
/// #3041 P1-0: dormant, wired in P1-1...
#[allow(dead_code)] // #3041 P1-0: dormant, wired in P1-1..
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::services::discord) enum LeaseOutcome {
    /// Terminal output was confirmed delivered to Discord; the offset may
    /// advance to `end`.
    Delivered,
    /// Delivery was intentionally suppressed / not performed; offset unchanged.
    NotDelivered,
    /// Ambiguous (drop / panic / partial). Offset MUST NOT advance.
    Unknown,
}

/// The lease state machine value, owned behind the cell's mutex. The `AtomicU8`
/// tag below is the single-winner CAS gate for acquire; this payload is only
/// ever mutated by that winner (or by a deadline reclaim), and every mutation
/// flips the tag AND writes the payload under the SAME mutex, so the tag and
/// payload are always observed coherently (#3041 codex). `read()` also takes
/// the mutex — there is no lock-free read fast path.
///
/// #3041 P1-0: dormant, wired in P1-1...
#[allow(dead_code)] // #3041 P1-0: dormant, wired in P1-1..
#[derive(Clone, Debug)]
enum LeaseState {
    /// No holder; the lease is available to acquire.
    Unleased,
    /// Held by `holder` for delivery identity `key` until `deadline` (monotonic ms
    /// since process start); covers the half-open byte range `[start, end)`.
    /// The lease key is the FULL `(DeliveryLeaseKey, [start,end))` identity
    /// (#3041 §2): `commit`/`release` verify it so a stale commit or release
    /// from an OLDER turn (or the same turn with a different range) cannot act
    /// on a reacquired NEWER lease. `reclaim_if_expired` is intentionally
    /// deadline-only (identity-agnostic) — it force-returns an expired lease
    /// regardless of holder/key so a dead holder cannot strand the cell.
    Leased {
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
        deadline_ms: u64,
        start: u64,
        end: u64,
    },
    /// Committed with a three-way outcome; carries the same `(holder, key,
    /// range)` identity forward so a stale release is rejected. Awaits a
    /// `release` to return to `Unleased`.
    Committed {
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
        start: u64,
        end: u64,
        outcome: LeaseOutcome,
    },
}

/// #3041 P1-1: process-monotonic millisecond clock for delivery-lease
/// deadlines. The acquire deadline and the reconciler's `reclaim_if_expired`
/// MUST read the SAME clock; a wall clock would jump on NTP steps and could
/// reclaim a live holder or strand a dead one. Anchored to a process-start
/// `Instant` so it is purely monotonic (never goes backwards). NOTE: this is a
/// real wall-monotonic clock, not the Tokio test clock; gated-clock tests drive
/// `reclaim_if_expired` with explicit `now_ms` arguments rather than this fn.
pub(in crate::services::discord) fn lease_now_ms() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// Internal CAS gate tag for the [`DeliveryLeaseCell`]. The CAS that flips
/// `UNLEASED → LEASED` is the single-winner acquire primitive — exactly one
/// acquirer wins; concurrent losers serialize on the payload mutex and observe
/// a non-`UNLEASED` tag under the lock. The tag is taken/flipped under the
/// payload mutex (never on its own); it is NOT a lock-free read fast path —
/// `read()` always takes the mutex (#3041 R1 coherence fix).
const TAG_UNLEASED: u8 = 0;
const TAG_LEASED: u8 = 1;
const TAG_COMMITTED: u8 = 2;

/// One-time terminal-delivery right for a single `(channel, turn, byte_range)`
/// (#3041 §2-§3). DORMANT in P1-0 — added alongside, NOT replacing,
/// `TmuxRelayCoord::relay_slot`. The `state_tag` is the single-winner CAS
/// acquire primitive; the `payload` mutex carries the rich lease state (holder
/// / deadline / range / outcome). The tag flip, payload write, and `read()` all
/// happen under the one mutex, so they are always mutually coherent.
///
/// #3041 P1-0: dormant, wired in P1-1...
#[allow(dead_code)] // #3041 P1-0: dormant, wired in P1-1..
pub(in crate::services::discord) struct DeliveryLeaseCell {
    /// The channel this lease coordinates. Part of the lease identity.
    channel_id: ChannelId,
    /// Internal CAS gate tag (`TAG_*`). The acquire CAS on this word is the
    /// single-winner gate; it is flipped under the payload mutex, NOT lock-free
    /// for readers — `read()` takes the mutex.
    state_tag: std::sync::atomic::AtomicU8,
    /// Rich lease payload. Mutated by the CAS winner or a deadline reclaim, and
    /// read by `read()` — all under this one mutex (the coherence invariant).
    payload: std::sync::Mutex<LeaseState>,
}

/// A point-in-time snapshot of a [`DeliveryLeaseCell`], returned by `read()`
/// (which materializes it under the payload mutex).
///
/// #3041 P1-0: dormant, wired in P1-1...
#[allow(dead_code)] // #3041 P1-0: dormant, wired in P1-1..
#[derive(Clone, Debug)]
pub(in crate::services::discord) enum LeaseSnapshot {
    Unleased,
    Leased {
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
        deadline_ms: u64,
        start: u64,
        end: u64,
    },
    Committed {
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
        start: u64,
        end: u64,
        outcome: LeaseOutcome,
    },
}

/// What a [`LeaseSnapshot`] holds for ONE expected [`DeliveryLeaseKey`], as
/// returned by [`LeaseSnapshot::identity_matched`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct IdentityMatchedLease {
    /// Exclusive end of the `[start, end)` delivery range the matched lease
    /// covers.
    pub(in crate::services::discord) end: u64,
    /// `Some(deadline_ms)` while the lease is still `Leased`: the holder-liveness
    /// deadline, on the [`lease_now_ms`] clock, that `reclaim_if_expired` will
    /// return the cell to `Unleased` at. `None` once `Committed` — that state
    /// carries no deadline field and is never deadline-reclaimed, so a caller
    /// asking "is a holder still live?" must read `None` as "no live holder",
    /// not as "expired".
    pub(in crate::services::discord) deadline_ms: Option<u64>,
}

impl LeaseSnapshot {
    /// The range and (uncommitted) deadline this snapshot holds FOR
    /// `expected_key`, or `None` when the cell is `Unleased` or holds some other
    /// turn's key.
    ///
    /// Key equality is the whole relevance test, and it is only as precise as
    /// [`DeliveryLeaseKey`] itself: an id-0 turn that reached
    /// `is_degenerate_legacy` collapses to `(channel, generation, 0)`, so two
    /// such turns on one channel in one process generation compare EQUAL here.
    /// A caller that refuses on a match therefore refuses slightly more often
    /// than the turn identity alone would justify; one that acts on a match acts
    /// on a range that may belong to a sibling id-0 turn.
    pub(in crate::services::discord) fn identity_matched(
        &self,
        expected_key: &DeliveryLeaseKey,
    ) -> Option<IdentityMatchedLease> {
        match self {
            Self::Leased {
                key,
                deadline_ms,
                end,
                ..
            } if key == expected_key => Some(IdentityMatchedLease {
                end: *end,
                deadline_ms: Some(*deadline_ms),
            }),
            Self::Committed { key, end, .. } if key == expected_key => Some(IdentityMatchedLease {
                end: *end,
                deadline_ms: None,
            }),
            Self::Unleased | Self::Leased { .. } | Self::Committed { .. } => None,
        }
    }
}

/// #3041 P1-1/P1-2: delivery-lease acquire deadline shared by BOTH the watcher
/// and the bridge terminal-delivery paths. The deadline is a HOLDER-LIVENESS
/// signal, NOT a hard cap on delivery duration — while a send future is in
/// flight the holder keeps the lease alive with a background HEARTBEAT that
/// `renew()`s the deadline every [`DELIVERY_LEASE_HEARTBEAT_MS`]. Because a LIVE
/// holder always re-extends within one interval, a long multi-chunk send (which
/// can exceed any FIXED deadline) is NEVER reclaimed mid-flight; a genuinely
/// DEAD holder stops renewing, so the lease expires and a replacement reclaims
/// it within ~one deadline. Picked as 3× the heartbeat (15s = 3 × 5s): one tick
/// can be skipped entirely and the lease still survives to the next, while
/// dead-holder recovery is ~15s. P1-2 reuses this so the WATCHER and the BRIDGE
/// share one deadline against the one per-channel cell — whoever holds it blocks
/// the other's acquire (cross-actor duplicate prevention).
pub(in crate::services::discord) const DELIVERY_LEASE_DEADLINE_MS: u64 = 15_000;

/// #3041 P1-1/P1-2: how often an in-flight holder renews its delivery lease.
/// Must be strictly less than (and a small fraction of)
/// [`DELIVERY_LEASE_DEADLINE_MS`] so a live holder always re-extends before
/// expiry even if one tick is delayed (the deadline is 3× this).
pub(in crate::services::discord) const DELIVERY_LEASE_HEARTBEAT_MS: u64 = 5_000;

/// #3041 P1-1 (§3, codex R2 Issue-1) / P1-2: RAII handle for the in-flight
/// delivery-lease heartbeat task, shared by the watcher and the bridge. The
/// holder spawns the heartbeat right after a successful `try_acquire` and
/// `stop()`s it BEFORE the inline commit (and the `Drop` impl aborts it on any
/// early return / panic), so the renew loop can NEVER outlive the send and race
/// the commit. While the holder task lives the heartbeat keeps the lease alive
/// (`renew`); if the holder TASK dies the spawned heartbeat is dropped/aborted
/// with it → the lease stops being renewed → it expires → a replacement reclaims
/// it. A heartbeat tick can only ever `renew` THIS holder's OWN still-`Leased`
/// lease (matched on holder+key), so a last tick that races `stop()`+commit
/// merely extends our own deadline, which the immediately-following commit then
/// flips to `Committed` — harmless.
pub(in crate::services::discord) struct DeliveryLeaseHeartbeat {
    handle: tokio::task::JoinHandle<()>,
}

impl DeliveryLeaseHeartbeat {
    /// Spawn a background task that renews `(holder, key)`'s lease on `cell`
    /// every [`DELIVERY_LEASE_HEARTBEAT_MS`], each time pushing the deadline to
    /// `lease_now_ms() + DELIVERY_LEASE_DEADLINE_MS`. The first tick fires AFTER
    /// one interval (the acquire already set a fresh deadline). The loop exits on
    /// its own as soon as a `renew` returns false (the lease is no longer ours —
    /// committed, released, or reclaimed), so it self-terminates even before an
    /// explicit `stop()`.
    pub(in crate::services::discord) fn spawn(
        cell: std::sync::Arc<DeliveryLeaseCell>,
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
    ) -> Self {
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(
                DELIVERY_LEASE_HEARTBEAT_MS,
            ));
            // Skip the immediate tick `interval` emits at t=0; the acquire just
            // set a fresh deadline, so the first renew is one interval later.
            interval.tick().await;
            loop {
                interval.tick().await;
                let renewed = cell.renew(
                    holder,
                    key.clone(),
                    lease_now_ms().saturating_add(DELIVERY_LEASE_DEADLINE_MS),
                );
                if !renewed {
                    // Lease is no longer ours (committed/released/reclaimed):
                    // nothing left to keep alive.
                    break;
                }
            }
        });
        Self { handle }
    }

    /// Stop the heartbeat. Idempotent. Called BEFORE the inline commit so the
    /// renew loop is guaranteed not to race the commit.
    pub(in crate::services::discord) fn stop(self) {
        self.handle.abort();
    }
}

impl Drop for DeliveryLeaseHeartbeat {
    fn drop(&mut self) {
        // Safety net: if the send path returns early / panics before an explicit
        // `stop()`, aborting on drop guarantees the heartbeat cannot outlive the
        // owning holder frame.
        self.handle.abort();
    }
}

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
    ///
    /// NOTE the snapshot is stale the instant the mutex is dropped on return. A
    /// caller that must ACT on what it read — rather than merely report it —
    /// has a read/act window a concurrent `try_acquire` fits inside, and must
    /// use [`DeliveryLeaseCell::with_state_locked`] instead.
    pub(in crate::services::discord) fn read(&self) -> LeaseSnapshot {
        self.with_state_locked(|snapshot| snapshot)
    }

    /// #5071 relay-tail S4 r2 (P1-1): run `act` with the payload mutex HELD,
    /// handing it the same snapshot [`DeliveryLeaseCell::read`] would return.
    ///
    /// This exists because `read()` alone cannot fence anything. `read()` takes
    /// the mutex, materializes a snapshot, and DROPS the mutex on return, so a
    /// caller that decides "no live holder → safe to destroy" and then performs
    /// the destruction has a window between the two in which a `try_acquire`
    /// can win. Every mutation of this cell runs under this one mutex, so a
    /// decision taken and acted on inside `act` is atomic with respect to
    /// acquire / commit / release / renew / reclaim on this cell.
    ///
    /// # Contract for `act`
    ///
    /// * It MUST NOT call back into ANY method of the SAME cell: `std::sync::
    ///   Mutex` is not reentrant, so that self-deadlocks.
    /// * It is called while a lock is held, so it should stay short and must not
    ///   block on I/O or on a lock that some other thread could hold while
    ///   waiting on this cell. The one production caller (the
    ///   `TerminalDeliveryFence` conjunct in `tmux_watcher_registry`) runs only
    ///   in-memory registry map mutations; the lock-order enumeration for that
    ///   nesting lives on that fence's doc comment.
    /// * A panic inside `act` poisons the mutex. That is survivable here: every
    ///   lock site in this file recovers with `PoisonError::into_inner`, which
    ///   is sound because a panicking `act` cannot have mutated the payload (it
    ///   is handed a snapshot by value, not the guard).
    pub(in crate::services::discord) fn with_state_locked<T>(
        &self,
        act: impl FnOnce(LeaseSnapshot) -> T,
    ) -> T {
        let guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snapshot = match &*guard {
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
        };
        let acted = act(snapshot);
        drop(guard);
        acted
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
