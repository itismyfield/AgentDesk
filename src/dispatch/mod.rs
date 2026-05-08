mod dispatch_cancel;
mod dispatch_channel;
mod dispatch_context;
mod dispatch_create;
mod dispatch_query;
mod dispatch_status;
mod dispatch_summary;
#[cfg(test)]
pub(crate) mod test_support;
mod types;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) use dispatch_cancel::{
    cancel_active_dispatches_for_card_on_conn, cancel_dispatch_and_reset_auto_queue_on_conn,
};
pub use dispatch_cancel::{
    cancel_dispatch_and_reset_auto_queue_on_pg, cancel_dispatch_and_reset_auto_queue_on_pg_tx,
};
pub(crate) use dispatch_channel::dispatch_destination_provider_override;
#[allow(unused_imports)]
pub use dispatch_channel::{
    drain_unified_thread_kill_signals, extract_thread_channel_id, is_unified_thread_channel_active,
    is_unified_thread_channel_name_active,
};
#[allow(unused_imports)]
pub(crate) use dispatch_context::{
    DispatchSessionStrategy, REVIEW_QUALITY_CHECKLIST, REVIEW_QUALITY_SCOPE_REMINDER,
    REVIEW_VERDICT_IMPROVE_GUIDANCE, commit_belongs_to_card_issue, commit_belongs_to_card_issue_pg,
    dispatch_session_strategy_from_context, dispatch_type_force_new_session_default,
    dispatch_type_requires_fresh_worktree, dispatch_type_session_strategy_default,
    dispatch_type_uses_thread_routing, ensure_card_worktree, inject_review_dispatch_identifiers,
    resolve_card_worktree,
};
#[allow(unused_imports)]
pub(crate) use dispatch_create::{
    apply_dispatch_attached_intents_on_pg_tx, dispatch_required_capabilities_from_routing,
};
#[allow(unused_imports)]
pub use dispatch_create::{
    create_dispatch, create_dispatch_core, create_dispatch_core_with_id,
    create_dispatch_core_with_id_and_options, create_dispatch_core_with_options,
    create_dispatch_pg_only, create_dispatch_with_options, create_dispatch_with_options_pg_only,
};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) use dispatch_create::{
    create_dispatch_record_sqlite_test, create_dispatch_record_with_id_sqlite_test,
};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[allow(unused_imports)]
pub use dispatch_query::query_dispatch_row;
pub(crate) use dispatch_query::query_dispatch_row_pg;
#[allow(unused_imports)]
pub(crate) use dispatch_status::set_dispatch_status_without_queue_sync_with_backends;
#[allow(unused_imports)]
pub use dispatch_status::{
    complete_dispatch, finalize_dispatch, finalize_dispatch_with_backends,
    load_dispatch_row_pg_first, load_dispatch_row_with_backends, mark_dispatch_completed_pg_first,
    set_dispatch_status_pg_first, set_dispatch_status_with_backends,
};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[allow(unused_imports)]
pub(crate) use dispatch_status::{
    ensure_dispatch_notify_outbox_on_conn, ensure_dispatch_status_reaction_outbox_on_conn,
    record_dispatch_status_event_on_conn, set_dispatch_status_on_conn,
    set_dispatch_status_without_queue_sync_on_conn,
};
pub(crate) use dispatch_summary::{summarize_dispatch_from_text, summarize_dispatch_result};
pub use types::DispatchCreateOptions;
