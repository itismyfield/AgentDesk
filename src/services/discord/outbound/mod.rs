//! Discord outbound delivery domain (#1006).
//!
//! This directory module hosts the v3 outbound envelope, policy, planner,
//! delivery implementation, result type, and shared transport/dedup
//! primitives.

use std::sync::OnceLock;

pub(crate) mod confirmation;
pub(crate) mod decision;
pub(crate) mod delivery;
pub(in crate::services::discord) mod delivery_frontier_probe;
pub(in crate::services::discord) mod delivery_record; // #3089 B0
pub(crate) mod manual_delivery;
pub(crate) mod message;
pub(crate) mod policy;
pub(in crate::services::discord) mod reaction_control;
pub(crate) mod result;
pub(crate) mod send_api;
pub(crate) mod send_gate;
pub(crate) mod send_target;
pub(crate) mod send_to_agent;
pub(in crate::services::discord) mod serenity_reference;
pub(crate) mod source_registry;
mod transport;
pub(in crate::services::discord) mod turn_output_controller; // #3089 A1

pub(crate) use decision::{
    DISCORD_MESSAGE_HARD_LIMIT_CHARS as DISCORD_HARD_LIMIT_CHARS,
    DISCORD_MESSAGE_SAFE_CHARS as DISCORD_SAFE_LIMIT_CHARS,
};
pub(crate) use message::DiscordOutboundMessage;
pub(crate) use policy::DiscordOutboundPolicy;
pub(crate) use result::DeliveryResult;
pub(crate) use transport::{
    DiscordOutboundClient, HttpOutboundClient, OutboundDedupClaim, OutboundDedupReservation,
    OutboundDedupWait, OutboundDeduper, outbound_fingerprint,
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

pub(crate) fn delivery_record_rollout_health_json() -> serde_json::Value {
    delivery_record::delivery_record_rollout_health_json()
}
