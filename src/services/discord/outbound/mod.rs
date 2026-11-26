//! Discord outbound delivery domain (#1006).
//!
//! This directory module hosts the v3 outbound envelope, policy, planner,
//! delivery implementation, result type, and shared transport/dedup
//! primitives.

use std::sync::OnceLock;
pub(in crate::services::discord) mod body_census;

pub(crate) mod completed_turn_ledger; // #4564
pub(crate) mod confirmation;
pub(crate) mod decision;
pub(crate) mod delivery;
pub(in crate::services::discord) mod delivery_frontier_probe;
pub(in crate::services::discord) mod delivery_record; // #3089 B0
pub(crate) mod image_format;
#[cfg(test)]
mod image_format_tests;
pub(crate) mod manual_delivery;
pub(crate) mod message;
pub(crate) mod notification_policy;
#[cfg(all(test, unix))]
mod notification_policy_tests;
#[cfg(unix)]
pub(in crate::services::discord) mod o_delivery_evidence;
#[cfg(unix)]
mod o_delivery_manifest;
pub(crate) mod o_writer_io;
pub(in crate::services::discord) mod o_writer_legacy;
#[cfg(test)]
mod outbound_delivery_port_tests;
pub(crate) mod policy;
pub(in crate::services::discord) mod reaction_control;
// Read-only projection over `delivery_record`'s evidence, without verdict authority.
// Unix-gated because its reader, `health::reachability::composite`, is unix-only.
pub(in crate::services::discord) mod raw_output;
#[cfg(unix)]
pub(in crate::services::discord) mod receipt_index;
pub(crate) mod result;
pub(crate) mod send_api;
pub(crate) mod send_gate;
pub(crate) mod send_target;
pub(crate) mod send_to_agent;
pub(in crate::services::discord) mod serenity_reference;
pub(crate) use serenity_reference::recovery_notice;
mod transport;
pub(in crate::services::discord) mod turn_output_controller; // #3089 A1

pub(crate) use crate::domain::outbound::port::DiscordOutboundClient;
pub(crate) use crate::domain::outbound::result::{DeliveryResult, DiscordTransportReceipt};
pub(crate) use decision::{
    DISCORD_MESSAGE_HARD_LIMIT_CHARS as DISCORD_HARD_LIMIT_CHARS,
    DISCORD_MESSAGE_SAFE_CHARS as DISCORD_SAFE_LIMIT_CHARS,
};
pub(crate) use message::DiscordOutboundMessage;
pub(crate) use policy::DiscordOutboundPolicy;
pub(crate) use transport::{
    HttpOutboundClient, OutboundDedupClaim, OutboundDedupReservation, OutboundDedupWait,
    OutboundDeduper, outbound_fingerprint, post_serenity_message_with_nonce,
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

/// Installs the ports engine producers use to reach Discord outbound, before any command runs.
pub(crate) fn install_engine_ports() {
    crate::services::dispatches::http_outbound::install(transport::engine_http_deliver);
}

pub(crate) fn delivery_record_rollout_health_json() -> serde_json::Value {
    delivery_record::delivery_record_rollout_health_json()
}
