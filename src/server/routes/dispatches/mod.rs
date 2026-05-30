mod crud;
pub(crate) mod discord_delivery;
mod outbox;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests;
pub(crate) mod thread_reuse;

pub(crate) use crate::services::dispatches::outbox_route::{
    parse_channel_id, resolve_channel_alias,
};

// ── Re-exports: CRUD routes ──────────────────────────────────
pub use crud::{
    UpdateDispatchBody, create_dispatch, get_dispatch, get_dispatch_delivery_events,
    get_dispatch_delivery_reconcile_stats, list_dispatches, update_dispatch,
};

// ── Re-exports: Outbox ───────────────────────────────────────
pub use outbox::resolve_channel_alias_pub;
pub(crate) use outbox::use_counter_model_channel;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) use outbox::{
    OutboxNotifier, process_outbox_batch, process_outbox_batch_with_real_notifier,
};
pub(crate) use outbox::{dispatch_outbox_loop, requeue_dispatch_notify_pg};

// ── Re-exports: Thread reuse ─────────────────────────────────
pub(crate) use thread_reuse::validate_channel_thread_maps_on_startup_with_backends;
pub use thread_reuse::{get_card_thread, get_pending_dispatch_for_thread, link_dispatch_thread};
