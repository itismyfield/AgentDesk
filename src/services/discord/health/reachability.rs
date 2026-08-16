//! Relay reachability observation library (#5071 T4-B1 = 4987 S1, first half).
//!
//! # This module OBSERVES and does not judge (#5071 T4-B2 = 4987 S1, second half)
//!
//! T4-B1 landed this tree wired to nothing. T4-B2 gave it exactly one caller —
//! [`observation::run_observation_loop`], an independent 30 s task — so S1
//! observation is now live. What is emphatically NOT live is judgment: no
//! health field, snapshot, API response or recovery input reads anything
//! produced here, no verdict is composed into final health (that is T4-B6,
//! behind `G-T4`), and no value in this tree authorizes a destructive action or
//! a redelivery. The task's entire output is a per-channel sidecar ledger and a
//! log line.
//!
//! The tree:
//!
//! * [`verdict`] — the `ReachabilityVerdict` type set (4987 §-1.3b, §4.1) and
//!   its polarity, with no composition rule and no threshold.
//! * [`discovery`] — the row-independent transcript resolution ladder of
//!   4987 §-1.3, fail-closed to `Unknown{TranscriptUnresolved}`.
//! * [`tail`] — the bounded incremental byte reader, with the 1 MiB/tick cap
//!   and file-identity revalidation.
//! * [`obligation`] — the canonical `(generation, start, end, identity,
//!   reason)` framing of 4987 §-1.5, whose Python twin is
//!   `relay_watchdog.py::canonical_obligation_records` and whose equivalence is
//!   gated byte for byte against `tests/fixtures/relay_obligation/`.
//! * [`ledger`] — the durable obligation sidecar with 4987 I13's typed
//!   extinction. `ReceiptCovered` has no producer until T4-B3.
//! * [`observation`] — the tick. Records; concludes only what 4987 §-1.4 lets
//!   it conclude; withholds, by named reason, the rest.
//!
//! # Thresholds are NOT here
//!
//! `Degraded` and `Unreachable` need `warn_bound`/`fail_bound`, and 4987 §10
//! lists hardcoding a threshold at S1 as NO-GO: the bounds are the OUTPUT of
//! the 30-day observation §3.4 requires, not an input to it. No number in this
//! tree is a decision threshold.
//!
//! # Row independence (4987 §-1.5 I14)
//!
//! Nothing in this tree may reach the inflight row. That rule is enforced by
//! `scripts/check_reachability_row_independence.py`, wired into
//! `scripts/ci-script-checks.sh`, and it is a **source lint, not a type
//! proof**: `InflightTurnState` is `pub(in crate::services::discord)`, so the
//! compiler would happily accept an import here. 4987 §-1.5 states that
//! downgrade explicitly and this comment repeats it so a reader of the code
//! never infers a guarantee the lint does not give.
//!
//! # Non-destructive (4987 §7.1 / I15)
//!
//! No value in this tree authorizes cancelling a turn, killing a tmux session
//! or a process, removing a registry entry, or force-cleaning a mailbox or
//! inflight row. I15 is a **convention** in this series — 4987 §-1.5 records
//! the decision not to build the private-constructor refactor — so the typed
//! surface here is `authorizes_destructive_action()` returning false on every
//! variant, and a source lint. It is not a sealed capability.

// T4-B1's blanket `#![allow(dead_code)]` is gone: the observation task is a
// real consumer, so the tree no longer needs one. What remains unconsumed is
// narrow and named at its own definition — the verdict shapes whose producers
// are later slices (`Degraded`/`Unreachable` need bounds T4-B6 calibrates,
// `TransportUnknown` needs the evidence sources of T4-B3/B5) and the
// `BindingComparison` datum T4-B4 reads. Each carries its own item-level allow
// with the slice that removes it, so "unused" stays a statement about a
// specific pending consumer rather than a blanket over the whole tree.

pub(in crate::services::discord) mod discovery;
pub(in crate::services::discord) mod ledger;
pub(in crate::services::discord) mod obligation;
pub(in crate::services::discord) mod observation;
pub(in crate::services::discord) mod tail;
pub(in crate::services::discord) mod verdict;
