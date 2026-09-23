//! The reachability ledger's time-to-live — #5942.
//!
//! A ledger outlives its process by design (4987 §2.2), but nothing bounded how
//! long it outlives its PRODUCER — an abandoned ledger pinned a channel
//! non-GREEN forever. This module holds only the mechanism that ends that: the
//! TTL, the last-commit clock, and the conditions that must ALL hold before a
//! ledger may be expired. The verdict stays in `super::composite`; this module
//! produces an age, never a `ReachabilityVerdict`.
//!
//! Still non-destructive (4987 §7.1 / I15): nothing here removes a ledger.
//! Expiry is recomputed from scratch every tick, so a returning producer
//! un-expires its channel with no state to undo.

use std::path::Path;

use super::super::session_enrichment::ExecutorWitness;
use super::composite::{ReachabilityInputs, RowlessTurn, TranscriptLiveness, age_secs};
use super::ledger::ReachabilityLedger;
use super::observation::REACHABILITY_OBSERVATION_INTERVAL_SECS;

/// Consecutive observation ticks a ledger may miss before it is treated as
/// having outlived its producer (#5942). Counted in ticks of
/// [`REACHABILITY_OBSERVATION_INTERVAL_SECS`], not wall time.
///
/// 20 is the bottom of the admissible range, not a midpoint —
/// `the_ledger_ttl_holds_against_the_delivery_bound_and_the_producer_period`
/// requires `[20, 59]` at today's 30s cadence. Raising it only trades distance
/// from that lower bound for longer daily degraded windows.
pub(in crate::services::discord) const LEDGER_OBSERVATION_TTL_TICKS: u64 = 20;

/// How long a ledger may go unobserved before it is treated as having outlived
/// its producer (#5942). Derived from the observation cadence, deliberately
/// NOT aliased to a delivery bound — the two are equal today (600s) only
/// because a test checks it, not because the definition guarantees it.
pub(in crate::services::discord) const LEDGER_OBSERVATION_TTL_SECS: u64 =
    LEDGER_OBSERVATION_TTL_TICKS * REACHABILITY_OBSERVATION_INTERVAL_SECS;

/// Shortest interval at which a scheduled producer re-stamps its ledger, and
/// therefore the upper bound on [`LEDGER_OBSERVATION_TTL_SECS`] (#5942).
///
/// A runtime fact mirrored into a constant with no self-check: a routine
/// added via `/api/routines` faster than this makes it stale silently. The
/// failure mode is fail-safe — such a channel simply never expires — so this
/// is a risk tracked in #5942, not a gate.
pub(in crate::services::discord) const SHORTEST_PERIODIC_PRODUCER_PERIOD_SECS: u64 = 1_800;

/// Published `reason` for an expired entry (#5942). Deliberately NOT a
/// `ReachabilityUnknownReason` — expiry is a different answer from "unknown"
/// and would otherwise inherit `Unknown`'s polarity through every reader that
/// switches on the variant.
pub(in crate::services::discord) const EXPIRED_REASON: &str = "ledger_unobserved_past_ttl";

/// When the ledger file was last committed (#5942). Uses the file's own mtime
/// as the clock — every writer in `super::ledger` publishes the whole record
/// via `runtime_store::atomic_write`'s rename, so mtime already means "last
/// observation tick committed" with no schema field to add or migrate.
/// An unreadable clock returns `None`, which never expires anything.
pub(in crate::services::discord) fn ledger_committed_at_epoch_ms(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let since_epoch = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    u64::try_from(since_epoch.as_millis()).ok()
}

/// How long this ledger has been abandoned, or `None` if it has not been
/// (#5942). Returns an age rather than a bool so a verdict can report whether
/// it has been an hour or a month, without a second clock read.
///
/// All six conditions below are over-expiry guards, not detection rules — the
/// detection is the TTL alone; each keeps a stale ledger from being expired
/// when staleness means something OTHER than "its producer is gone":
///
/// 1. execution owner positively witnessed ABSENT — `Unwitnessed` is not a
///    finding and must not expire a live channel on a busy poll;
/// 2. the ledger owes nothing — bytes outstanding when a producer died are a
///    delivery failure that does not stop being one because time passed;
/// 3. no placeholder outstanding in Discord for this channel;
/// 4. no unpaired active token reconfirmed (a mailbox turn with no in-flight
///    row) — a row-backed turn is covered by (2) instead, since a row can
///    outlive its session exactly like this ledger does;
/// 5. transcript not positively alive — this ordering lets an expiry never
///    preempt a verdict the transcript ladder would have reached;
/// 6. commit time known AND older than [`LEDGER_OBSERVATION_TTL_SECS`] — an
///    undated ledger is not an old one.
pub(in crate::services::discord) fn expired_without_a_producer(
    inputs: &ReachabilityInputs<'_>,
    ledger: &ReachabilityLedger,
) -> Option<u64> {
    if !matches!(inputs.executor, ExecutorWitness::Absent) {
        return None;
    }
    if !ledger.live_obligations().is_empty() {
        return None;
    }
    if inputs.placeholder_present || inputs.rowless_turn != RowlessTurn::None {
        return None;
    }
    if matches!(
        inputs.transcript,
        TranscriptLiveness::Resolved { alive: true, .. }
    ) {
        return None;
    }
    let observed_at_epoch_ms = inputs.ledger_observed_at_epoch_ms?;
    // `age_secs` saturates, so a commit time in the future reads as age 0 and
    // expires nothing — the direction a clock skew must fail in.
    let unobserved_for_secs = age_secs(inputs.now_epoch_ms, observed_at_epoch_ms);
    (unobserved_for_secs > LEDGER_OBSERVATION_TTL_SECS).then_some(unobserved_for_secs)
}
