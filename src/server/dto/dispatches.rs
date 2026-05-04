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
