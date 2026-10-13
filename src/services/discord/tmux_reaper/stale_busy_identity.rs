use crate::services::provider::ProviderKind;
use poise::serenity_prelude as serenity;

/// Finalize only if the mailbox and recorded runtime identities still match the absent probe.
pub(super) fn stale_busy_turn_is_still_reapable(
    observed_user_msg_id: serenity::MessageId,
    current_user_msg_id: Option<serenity::MessageId>,
    has_session: bool,
    observed_tmux_session_name: &str,
    current_tmux_session_name: Option<&str>,
) -> bool {
    !has_session
        && current_user_msg_id == Some(observed_user_msg_id)
        && current_tmux_session_name == Some(observed_tmux_session_name)
}

/// Use the recorded runtime name so a Fresh turn cannot be confused with its primary session.
pub(super) fn recorded_inflight_tmux_session_name(
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> Option<String> {
    crate::services::discord::inflight::load_inflight_state(provider, channel_id.get())
        .and_then(|state| state.tmux_session_name)
}
