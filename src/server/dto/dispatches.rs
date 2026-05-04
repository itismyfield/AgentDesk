//! Dispatch-route DTOs.
//!
//! #1693 introduced this module when splitting
//! `src/server/routes/dispatches/discord_delivery.rs` into thin handlers +
//! orchestration + repo + DTOs. The actual type definitions still live in
//! `crate::services::dispatches::discord_delivery` (where they shape the
//! return values of the orchestration layer); this module re-exports them
//! so route-layer callers can `use crate::server::dto::dispatches::...`
//! instead of reaching across into services.
//!
//! When new request/response shapes are added for dispatch routes, prefer
//! defining them here directly to keep the route surface declarative.

pub(crate) use crate::services::dispatches::discord_delivery::{
    DispatchMessagePostError, DispatchMessagePostErrorKind, DispatchMessagePostOutcome,
    DispatchNotifyDeliveryResult, DispatchTransport, ReviewFollowupKind,
};

// CRUD body re-exports (current canonical home: routes::dispatches::crud).
pub use crate::server::routes::dispatches::UpdateDispatchBody;

// #1694: Followup configuration DTO that the outbox followup orchestration
// uses to thread Discord API base URL + bot tokens through. Lives here so
// the route + test layers can construct it without depending on the
// route module internals.
#[derive(Clone, Debug)]
pub(crate) struct DispatchFollowupConfig {
    pub discord_api_base: String,
    pub notify_bot_token: Option<String>,
    pub announce_bot_token: Option<String>,
}

impl DispatchFollowupConfig {
    pub(crate) fn from_runtime() -> Self {
        Self {
            discord_api_base: crate::services::dispatches::discord_delivery::discord_api_base_url(),
            notify_bot_token: crate::credential::read_bot_token("notify"),
            announce_bot_token: crate::credential::read_bot_token("announce"),
        }
    }
}
