//! Relay reachability observation library (#5071 T4-B1 = 4987 S1, first half).
//!
//! # This module is INACTIVE
//!
//! Nothing in production constructs, reads, or consumes anything below. There
//! is no tick, no task, no health field, and no recovery input wired to it —
//! T4-B2 adds the observation task, T4-B6 adds the health composition, and each
//! of those is a separate reviewable landing. Consequently this slice changes
//! **zero** production verdicts and must not be counted as "4987 S1 active".
//! The `#![allow(dead_code)]` below is the honest spelling of exactly that: the
//! blanket comes off when B2 lands the first consumer.
//!
//! What lands here is the vocabulary plus the file-facing primitives every
//! later slice reads through:
//!
//! * [`verdict`] — the `ReachabilityVerdict` type set (4987 §-1.3b, §4.1) and
//!   its polarity, with no composition rule and no threshold.
//! * [`discovery`] — the row-independent transcript resolution ladder of
//!   4987 §-1.3, fail-closed to `Unknown{TranscriptUnresolved}`.
//! * [`tail`] — the bounded incremental byte reader the obligation prober will
//!   sit on, with the 1 MiB/tick cap and file-identity revalidation.
//! * [`obligation`] — the canonical `(generation, start, end, identity,
//!   reason)` framing of 4987 §-1.5 (#5071 T4-B2a), whose Python twin is
//!   `relay_watchdog.py::canonical_obligation_records` and whose equivalence
//!   with it is gated byte for byte against the golden corpus in
//!   `tests/fixtures/relay_obligation/`. It frames bytes a caller already
//!   read; it opens no file and reads no clock.
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

// See the module docs: this is an inactive library by design, so every item is
// unreachable from production until T4-B2 wires the observation task. Remove
// this blanket in that slice rather than letting it outlive the wiring.
#![allow(dead_code)]

pub(in crate::services::discord) mod discovery;
pub(in crate::services::discord) mod ledger;
pub(in crate::services::discord) mod obligation;
pub(in crate::services::discord) mod tail;
pub(in crate::services::discord) mod verdict;
