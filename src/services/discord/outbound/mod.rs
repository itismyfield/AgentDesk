//! Discord outbound delivery domain (#1006).
//!
//! This directory module hosts two parallel surfaces during the #1006
//! migration:
//!
//! - [`legacy`] — the existing length-safe idempotent delivery API (types
//!   prefixed with `Discord*`, `OutboundDeduper`, `deliver_outbound`, etc.).
//!   Every public symbol is re-exported here so existing callers continue to
//!   compile against `crate::services::discord::outbound::*` unchanged.
//! - New domain types in [`message`], [`policy`], [`result`], the pure policy
//!   planner in [`decision`], and the v3 delivery implementation in
//!   [`delivery`]. They live alongside the legacy types until the migration
//!   completes and are reachable through their submodule paths only — they
//!   intentionally do NOT shadow the legacy `Discord*` re-exports here.
//!
//! The default re-exports remain the legacy compatibility surface while
//! production callsites move to `outbound::delivery::deliver_outbound` with
//! v3 envelopes one producer at a time.

use std::sync::OnceLock;

pub(crate) mod decision;
pub(crate) mod delivery;
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
    OutboundDedupClaim, OutboundDedupReservation, OutboundDedupWait, OutboundDeduper, SkipReason,
    SplitStrategy, ThreadFallback, deliver_outbound, outbound_fingerprint,
};

/// Process-wide in-memory outbound deduper shared by every Discord producer.
///
/// This is the last guard around actual Discord sends. Durable SQL outbox
/// uniqueness still belongs to the `message_outbox` enqueue/claim pipeline;
/// this helper only suppresses duplicate sends once a producer has built an
/// outbound delivery key in-process.
pub(crate) fn shared_outbound_deduper() -> &'static OutboundDeduper {
    static DEDUPER: OnceLock<OutboundDeduper> = OnceLock::new();
    DEDUPER.get_or_init(OutboundDeduper::new)
}
