//! The reachability ledger's time-to-live — #5942.
//!
//! A ledger is durable so the "should have been delivered" term can outlive the
//! process (4987 §2.2). Nothing made it outlive its PRODUCER, though: a routine
//! thread's tmux session runs for a few minutes and exits, and the ledger it
//! leaves behind has no transcript left to resolve, so `super::composite`
//! answered `unknown{transcript_unresolved}` for it on every later tick and the
//! node's health was pinned non-GREEN by a file nobody could ever retire.
//!
//! This module holds the mechanism that ends that, and only the mechanism: the
//! TTL, when a ledger was last committed, and whether the six conditions that
//! must ALL hold before one may be expired do. The VERDICT stays in
//! `super::composite` — this module produces an age, never a
//! `ReachabilityVerdict`, so the single place that answers "what is this
//! channel" is still one file.
//!
//! # Still non-destructive (4987 §7.1 / I15)
//!
//! Nothing here removes a ledger. Expiry is recomputed from scratch on every
//! tick, so a producer that comes back un-expires its channel with no state to
//! undo — and deleting the file would not even have changed the polarity, since
//! an absent ledger classifies as `unknown{never_observed}`, which is equally
//! not health.

use std::path::Path;

use super::super::session_enrichment::ExecutorWitness;
use super::composite::{ReachabilityInputs, TranscriptLiveness, age_secs};
use super::ledger::ReachabilityLedger;
use super::observation::REACHABILITY_OBSERVATION_INTERVAL_SECS;

/// How many consecutive observation ticks a ledger may miss before it is
/// treated as having outlived its producer (#5942).
///
/// This is the quantity the TTL is actually ABOUT. The observation task
/// (`runtime_bootstrap::spawns::run_bot_spawn_reachability_observation`) walks
/// `tmux_watchers` once per [`REACHABILITY_OBSERVATION_INTERVAL_SECS`] and
/// republishes the whole ledger, so "has a producer been here lately" is
/// counted in ticks and nothing else.
///
/// Twenty is the BOTTOM of the admissible range, not a midpoint, and the two
/// assertions in `the_ledger_ttl_holds_against_the_delivery_bound_and_the_producer_period`
/// are what make the range a range: at today's 30 s cadence any tick count in
/// `[20, 59]` satisfies both. The bottom is chosen because the cost this issue
/// is about is paid ONCE PER TTL PER DAY — a routine stops stamping its ledger
/// when its tmux session exits, and the channel stays non-GREEN for exactly the
/// TTL afterwards. Measured on the reporting node (2026-09-16): three routine
/// channels last stamped 06:32:08, 06:33:08 and 07:01:41, so 600 s leaves
/// residual degraded windows of roughly 06:32–06:43 and 07:01–07:12, about 22
/// minutes a day against the 1440 they occupy now. Every tick added to this
/// constant adds 30 s to each of those windows, and buys only distance from a
/// lower bound the assertion already holds.
pub(in crate::services::discord) const LEDGER_OBSERVATION_TTL_TICKS: u64 = 20;

/// How long a ledger may go without an observation commit before it is treated
/// as having outlived its producer (#5942).
///
/// Derived from the observation cadence, which is what actually stamps the
/// file, and deliberately NOT aliased to a delivery bound. r1 defined it as
/// equal to the obligation fail bound, which coupled two numbers that move for
/// unrelated reasons and turned the lower-bound assertion into a tautology that
/// could never fail. They are equal today (600 s), and that equality is now
/// something a test CHECKS rather than something a definition guarantees.
pub(in crate::services::discord) const LEDGER_OBSERVATION_TTL_SECS: u64 =
    LEDGER_OBSERVATION_TTL_TICKS * REACHABILITY_OBSERVATION_INTERVAL_SECS;

/// The shortest interval at which a scheduled producer re-stamps a ledger it
/// owns, and therefore the upper bound on [`LEDGER_OBSERVATION_TTL_SECS`].
///
/// 30 minutes: the densest routine cron in service at the time of writing
/// (`0,30 12-20 * * *`) fires twice an hour, spawns a tmux session for minutes,
/// and leaves the ledger behind.
///
/// A RUNTIME fact mirrored into a constant, and nothing can make it
/// self-checking — routines are created through `/api/routines`, so a `*/10`
/// routine added tomorrow makes this stale with no test to notice. What bounds
/// the damage is the failure MODE: such a channel simply never expires, which
/// is today's behaviour, so the regression is to the status quo rather than to
/// something new. That belongs in #5942's risks, not in a gate.
pub(in crate::services::discord) const SHORTEST_PERIODIC_PRODUCER_PERIOD_SECS: u64 = 1_800;

/// The published `reason` for an expired entry (#5942).
///
/// Deliberately NOT a `ReachabilityUnknownReason`: expiry is a different answer
/// from "unknown", and giving it an `Unknown` reason would have made it inherit
/// `Unknown`'s polarity through every reader that switches on the variant.
pub(in crate::services::discord) const EXPIRED_REASON: &str = "ledger_unobserved_past_ttl";

/// When the ledger file was last committed (#5942).
///
/// The ledger's own mtime is the clock rather than a new field inside it,
/// because every writer in `super::ledger` publishes the WHOLE record through
/// `runtime_store::atomic_write`'s rename — so the file's modification time
/// already *is* "when the last observation tick committed", with no schema
/// version to bump and no migration for the ledgers already on disk. That
/// matters here specifically: the ledgers this TTL exists to retire were
/// written before it, and a new in-file field would have read as absent on
/// every one of them.
///
/// A clock that cannot be read returns `None`, which never expires anything.
pub(in crate::services::discord) fn ledger_committed_at_epoch_ms(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let since_epoch = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    u64::try_from(since_epoch.as_millis()).ok()
}

/// How long this ledger has been abandoned, or `None` if it has not been
/// (#5942).
///
/// Six conditions, all required, and every one of them is an over-expiry guard
/// rather than a detection rule — the detection is the TTL alone, and each
/// conjunct names a situation where a stale ledger means something OTHER than
/// "its producer is gone" and must therefore keep its vote:
///
/// 1. the execution owner is positively witnessed ABSENT. `Unwitnessed` — a
///    spent probe budget, a probe that did not return — is not a finding, and
///    reading it as one would expire live channels on a busy poll;
/// 2. the ledger owes nothing. This is the guard that keeps the timer from
///    eating a real loss: bytes outstanding when a producer died are a delivery
///    failure, and a delivery failure does not stop being one because time
///    passed;
/// 3. no placeholder is outstanding in Discord for this channel;
/// 4. no UNPAIRED active token is reconfirmed — `rowless_active_turn` is
///    `unpaired_active_token::reconfirm`, i.e. a mailbox turn with NO in-flight
///    row, not any active turn. r2 justified the narrowness by claiming a
///    row-backed turn implies a live tmux producer that conjunct (1) refuses.
///    **That claim was false and r3 withdraws it**: the in-flight row is a
///    durable JSON file under the runtime root, so a row outlives the session
///    that wrote it exactly the way this ledger does, and dead-tmux-with-row is
///    a reachable state. What actually covers that state is conjunct (2) — a
///    row that still owes bytes is an outstanding obligation on this very
///    ledger, and a row that owes nothing has no reader left but the producer
///    that is gone. The narrowness is a judgment about where the loss is
///    recorded, not a deduction from (1);
/// 5. the transcript is not positively alive. This is the conjunct that makes
///    the gate's POSITION defensible (r3 P2-3): the gate runs before the
///    transcript ladder, so without it an expiry could preempt a verdict the
///    ladder would have reached, and `Reachable` is reachable from exactly one
///    input — 4987 §-1.4's positive `alive` evidence. Refusing to expire over
///    that evidence is therefore not a heuristic; it is the whole set of
///    preemptable GREEN answers. `Resolved { alive: false }` still expires, and
///    correctly: the ladder's answer there is
///    `unknown{incarnation_not_alive_witnessed}`, equally non-GREEN and equally
///    permanent, which is the #5942 symptom rather than a verdict worth saving;
/// 6. the ledger's commit time is known AND older than
///    [`LEDGER_OBSERVATION_TTL_SECS`]. An undated ledger is not an old one, so
///    an unreadable clock expires nothing.
///
/// Returns the age rather than a bool so the verdict can publish it: an
/// operator reading `expired` needs to know whether it has been an hour or a
/// month, and recomputing it at the surface would be a second clock read.
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
    if inputs.placeholder_present || inputs.rowless_active_turn {
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
