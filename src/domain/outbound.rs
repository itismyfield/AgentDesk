//! Outbound delivery value contracts and transport port, free of Serenity types.
//! Dedup reservation, nonce use and Serenity id conversion stay in the Discord adapter.

pub mod alert_origin;
pub mod ids;
pub mod intent;
pub mod key;
pub mod outbox;
pub mod policy;
pub mod port;
pub mod result;

pub use ids::{OutboundChannelId, OutboundMessageId};
pub use intent::OutboundIntent;
pub use key::{OutboundOperationKey, OutboundTargetKey};
pub use policy::DiscordOutboundPolicy;
pub use port::{DiscordOutboundClient, OutboundDelivery};
pub use result::{DeliveryResult, first_raw_message_id};
