//! Compatibility facade for Discord dispatch delivery.
//!
//! Delivery orchestration lives in `crate::services::dispatches::discord_delivery`;
//! route-level callers keep this module path through re-exports only.

#[allow(unused_imports)]
pub(crate) use crate::services::dispatches::discord_delivery::{
    DispatchMessagePostError, DispatchMessagePostErrorKind, DispatchMessagePostOutcome,
    DispatchNotifyDeliveryResult, DispatchTransport, HttpDispatchTransport, ReviewFollowupKind,
    discord_api_base_url, discord_api_url, edit_raw_message_once,
    persist_dispatch_message_target_and_add_pending_reaction_with_pg,
    post_dispatch_message_to_channel_with_delivery, post_raw_message_once,
    send_dispatch_to_discord_with_pg_result, send_dispatch_with_delivery_guard,
    send_review_result_to_primary_with_transport, sync_dispatch_status_reaction_with_pg,
};
