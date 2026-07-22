use super::*;
use crate::services::discord::adk_session;

pub(super) fn resolve_channel_id(
    adk_session_name: Option<&str>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
) -> Option<u64> {
    adk_session_name
        .and_then(adk_session::parse_thread_channel_id_from_name)
        .or_else(|| {
            shared
                .dispatch
                .thread_parents
                .contains_key(&channel_id)
                .then_some(channel_id.get())
        })
}
