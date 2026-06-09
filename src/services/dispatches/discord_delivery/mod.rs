mod guard;
mod orchestration;
mod thread_reuse;
mod transport;

pub(crate) use guard::send_dispatch_with_delivery_guard;
pub(crate) use orchestration::{
    HttpDispatchTransport, persist_dispatch_message_target_and_add_pending_reaction_with_pg,
    send_dispatch_to_discord_with_pg_result, send_review_result_to_primary_with_transport,
    sync_dispatch_status_reaction_with_pg,
};
pub(crate) use thread_reuse::{
    get_mapped_thread_for_channel_pg, get_thread_for_channel_pg,
    set_thread_for_channel_map_only_pg, set_thread_for_channel_pg,
    validate_channel_thread_maps_on_startup_with_backends,
};
pub(crate) use transport::{
    DispatchMessagePostError, DispatchMessagePostErrorKind, DispatchMessagePostOutcome,
    DispatchNotifyDeliveryResult, DispatchTransport, ReviewFollowupKind,
    archive_duplicate_slot_threads, discord_api_base_url, discord_api_url, edit_raw_message_once,
    is_discord_length_error, maybe_add_owner_to_dispatch_thread,
    post_dispatch_message_to_channel_with_delivery, post_raw_message_once,
    reset_stale_slot_thread_if_needed,
};
