mod crud;
pub(crate) mod discord_delivery;
mod outbox;
pub(crate) mod thread_reuse;

pub(crate) use crate::services::dispatches::outbox_route::parse_channel_id;

// ── Re-exports: CRUD routes ──────────────────────────────────
pub use crud::{
    UpdateDispatchBody, create_dispatch, get_dispatch, get_dispatch_delivery_events,
    get_dispatch_delivery_reconcile_stats, list_dispatches, update_dispatch,
};

// ── Re-exports: Outbox ───────────────────────────────────────
pub(crate) use outbox::dispatch_outbox_loop;
pub use outbox::resolve_channel_alias_pub;
pub(crate) use outbox::use_counter_model_channel;

// ── Re-exports: Thread reuse route handlers ──────────────────
// The Postgres/Discord-API thread-map helpers were relocated to
// `crate::services::dispatches::discord_delivery` (#3037); only the axum route
// handlers remain in the route layer.
pub use thread_reuse::{get_card_thread, get_pending_dispatch_for_thread, link_dispatch_thread};
