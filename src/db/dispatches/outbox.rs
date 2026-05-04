//! Dispatch outbox repository — Postgres queries about per-dispatch outbox
//! state (message target, thread binding, slot index, reaction state, review
//! followup metadata).
//!
//! #1693 introduced this submodule when splitting
//! `src/server/routes/dispatches/discord_delivery.rs` into thin handlers +
//! orchestration + repo + DTOs. The full SQL bodies still live in
//! `super` (`db/dispatches/mod.rs`) to keep the refactor mechanical and
//! preserve git blame; this module re-exports the outbox-shaped surface so
//! callers depend on a stable, narrowly-named API.
//!
//! #1694 will move additional outbox queries here (and add new ones) without
//! requiring further restructuring of the route or service layer. New SQL
//! that lives only behind the dispatch outbox should land here directly.
//!
//! Public surface (re-exported from `super`):
//! - `persist_dispatch_message_target_pg` — record (channel_id, message_id)
//!   on a dispatch's context after Discord delivery.
//! - `persist_dispatch_thread_id_pg` — record the thread the dispatch was
//!   posted to (for reuse + reaction sync).
//! - `load_dispatch_reaction_row_pg` — load (status, context) used by
//!   announce-bot reaction sync.
//! - `persist_dispatch_slot_index_pg` — bind a slot to a dispatch's context.
//! - `load_dispatch_context_pg` — read the raw JSON context blob.
//! - `latest_work_dispatch_thread_pg` — locate the most recent
//!   implementation/rework dispatch's thread for thread reuse decisions.
//! - `load_review_followup_card_pg` — load review-followup card metadata.
//! - `review_followup_already_resolved_pg` — dedup check for review followup.
//! - `latest_completed_review_provider_on_conn` (legacy-sqlite-tests only).

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) use super::latest_completed_review_provider_on_conn;
pub(crate) use super::{
    DispatchReactionRow, ReviewFollowupCard, latest_work_dispatch_thread_pg,
    load_dispatch_context_pg, load_dispatch_reaction_row_pg, load_review_followup_card_pg,
    persist_dispatch_message_target_pg, persist_dispatch_slot_index_pg,
    persist_dispatch_thread_id_pg, review_followup_already_resolved_pg,
};
