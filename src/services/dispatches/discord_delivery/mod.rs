pub(crate) use crate::server::dto::dispatches::{
    DispatchMessagePostError, DispatchMessagePostErrorKind, DispatchMessagePostOutcome,
    DispatchNotifyDeliveryResult, ReviewFollowupKind,
};

mod guard;
mod orchestration;
mod transport;

pub(crate) use guard::send_dispatch_with_delivery_guard;
pub(crate) use orchestration::{
    HttpDispatchTransport, persist_dispatch_message_target_and_add_pending_reaction_with_pg,
    resolve_dispatch_delivery_channel_on_conn, send_dispatch_to_discord,
    send_dispatch_to_discord_with_pg, send_dispatch_to_discord_with_pg_result,
    send_dispatch_to_discord_with_transport, send_review_result_to_primary,
    send_review_result_to_primary_with_transport, sync_dispatch_status_reaction,
    sync_dispatch_status_reaction_with_pg,
};
pub(crate) use transport::{
    DispatchTransport, archive_duplicate_slot_threads, discord_api_base_url, discord_api_url,
    edit_raw_message_once, is_discord_length_error, maybe_add_owner_to_dispatch_thread,
    post_dispatch_message_to_channel_with_delivery, post_raw_message_once,
    reset_stale_slot_thread_if_needed,
};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) use transport::{
    add_thread_member_to_dispatch_thread, post_dispatch_message_to_channel,
};
