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

// ── Re-exports: Thread reuse ─────────────────────────────────
pub(crate) use thread_reuse::validate_channel_thread_maps_on_startup_with_backends;
pub use thread_reuse::{get_card_thread, get_pending_dispatch_for_thread, link_dispatch_thread};
