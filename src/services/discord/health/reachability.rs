//! Relay reachability observation (#5071 T4-B2c = 4987 S1 observation task).
//!
//! Production now resolves and tails each registered watcher transcript and
//! records canonical obligations plus its resume cursor in the durable ledger.
//! `health/snapshot.rs` additionally reads [`divergence`] for a descriptive
//! log record. Everything except [`composite`] is observation: no value in the
//! other modules changes relay delivery, recovery, or health on its own.
//!
//! What lands here is the vocabulary plus the file-facing primitives every
//! later slice reads through:
//!
//! * [`composite`] — T4-B6's judgment layer: the Tier A producer, 4987
//!   §4.3-1's `worst(ReachabilityVerdict, ExternalRelayVerdict)`, and the
//!   `RelayVerdictSource` switch that decides whether the product may change
//!   the reported health polarity. It is the only module here with any
//!   authority, and that authority is over polarity alone.
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
//! * [`divergence`] — the 4987 §-1.5 row-coordinate ↔ resolved-coordinate
//!   file-identity comparison (#5071 T4-B4). The ONE place in this tree that
//!   sees the in-flight row's path, as a comparison operand only (I14);
//!   descriptive outcomes, no verdict.
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

pub(in crate::services::discord) mod composite;
pub(in crate::services::discord) mod discovery;
pub(in crate::services::discord) mod divergence;
pub(in crate::services::discord) mod external_verdict;
pub(in crate::services::discord) mod ledger;
pub(in crate::services::discord) mod obligation;
pub(in crate::services::discord) mod observation;
pub(in crate::services::discord) mod tail;
pub(in crate::services::discord) mod verdict;
