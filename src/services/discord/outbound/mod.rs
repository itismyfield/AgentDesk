//! Discord outbound delivery domain (#1006).
//!
//! This directory module hosts two parallel surfaces during the #1006
//! migration:
//!
//! - [`legacy`] — the existing length-safe idempotent delivery API (types
//!   prefixed with `Discord*`, `OutboundDeduper`, `deliver_outbound`, etc.).
//!   Every public symbol is re-exported here so existing callers continue to
//!   compile against `crate::services::discord::outbound::*` unchanged.
//! - New domain types in [`message`], [`policy`], [`result`], plus the pure
//!   policy planner in [`decision`]. These are the cleaner v3 shapes that
//!   future slices (1.1 = service impl, 1.2 = outbox migration) will rewire
//!   callers onto. They live alongside the legacy types until the migration
//!   completes and are reachable through their submodule paths only — they
//!   intentionally do NOT shadow the legacy `Discord*` re-exports here.
//!
//! Slice 1.0 is a domain API only: the new types and pure policy planner
//! compile and have unit-test coverage, but no production callsite references
//! them yet. The deliver implementation that consumes these types and the
//! outbox migration are deferred to slice 1.1 / 1.2.

pub(crate) mod decision;
mod legacy;
pub(crate) mod message;
pub(crate) mod policy;
pub(crate) mod result;

// ── Legacy re-exports (preserve every existing public symbol) ──────────────
//
// Some of the legacy constants and enum variants are only consumed by the
// legacy module's own tests today; they are re-exported here regardless so
// the public surface of `discord::outbound::*` stays identical to the
// pre-split file.
#[allow(unused_imports)]
pub(crate) use legacy::{
    DISCORD_HARD_LIMIT_CHARS, DISCORD_SAFE_LIMIT_CHARS, DeliveryResult, DiscordOutboundClient,
    DiscordOutboundMessage, DiscordOutboundPolicy, FallbackKind, FileFallback, HttpOutboundClient,
    OutboundDeduper, SkipReason, SplitStrategy, ThreadFallback, deliver_outbound,
    outbound_fingerprint,
};
