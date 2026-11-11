//! Outbound delivery value contracts and transport port, free of Serenity types.
//! Dedup reservation, nonce use and Serenity id conversion stay in the Discord adapter.

pub(crate) mod ids;
pub(crate) mod intent;
pub(crate) mod key;
pub(crate) mod policy;
pub(crate) mod port;
pub(crate) mod result;

pub(crate) use ids::{OutboundChannelId, OutboundMessageId};
pub(crate) use intent::OutboundIntent;
pub(crate) use key::{OutboundOperationKey, OutboundTargetKey};
pub(crate) use policy::DiscordOutboundPolicy;
pub(crate) use port::{DiscordOutboundClient, OutboundDelivery};
pub(crate) use result::{DeliveryResult, first_raw_message_id};
