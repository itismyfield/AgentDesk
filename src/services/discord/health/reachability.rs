//! Relay reachability observation (#5071 T4-B2c). Everything except
//! [`composite`] is pure observation (no relay delivery/recovery/health effect).
//!
//! * [`composite`] — sole authority, over polarity only:
//!   `worst(ReachabilityVerdict, ExternalRelayVerdict)` via `RelayVerdictSource`.
//! * [`verdict`] — `ReachabilityVerdict` type set; no composition, no threshold.
//! * [`discovery`] — row-independent transcript resolution, fail-closed to
//!   `Unknown{TranscriptUnresolved}`.
//! * [`tail`] — bounded incremental reader (1 MiB/tick cap, file-identity
//!   revalidation).
//! * [`obligation`] — canonical `(generation, start, end, identity, reason)`
//!   framing, byte-equivalent with `relay_watchdog.py` against the golden
//!   corpus in `tests/fixtures/relay_obligation/`.
//! * [`divergence`] — row-coordinate ↔ resolved-coordinate identity comparison;
//!   the only module that sees the inflight row's path (I14, comparison only).
//!
//! I14: nothing here may reach the inflight row — enforced by
//! `scripts/check_reachability_row_independence.py` (lint, not a type proof).
//! I15: no destructive action (cancel/kill/force-clean) — convention only,
//! not a sealed capability.

pub(in crate::services::discord) mod composite;
pub(in crate::services::discord) mod discovery;
pub(in crate::services::discord) mod divergence;
pub(in crate::services::discord) mod external_verdict;
pub(in crate::services::discord) mod ledger;
pub(in crate::services::discord) mod ledger_ttl;
pub(in crate::services::discord) mod obligation;
pub(in crate::services::discord) mod observation;
pub(in crate::services::discord) mod tail;
pub(in crate::services::discord) mod verdict;
