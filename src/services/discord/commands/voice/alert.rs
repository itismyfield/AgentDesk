//! Deduplicated delivery of voice auto-join alerts.

use poise::serenity_prelude::ChannelId;

/// Track which (voice_channel, kind) auto-join notifications were already sent
/// so a single process lifetime emits each at most once.
fn voice_notify_dedup() -> &'static dashmap::DashSet<(u64, &'static str)> {
    static DEDUP: std::sync::OnceLock<dashmap::DashSet<(u64, &'static str)>> =
        std::sync::OnceLock::new();
    DEDUP.get_or_init(dashmap::DashSet::new)
}

pub(super) fn voice_notify_should_send(channel_id: ChannelId, kind: &'static str) -> bool {
    voice_notify_dedup().insert((channel_id.get(), kind))
}

pub(in crate::services::discord) async fn notify_voice_alert(
    channel_id: ChannelId,
    content: String,
    kind: &'static str,
) {
    if !voice_notify_should_send(channel_id, kind) {
        return;
    }
    let Some(token) = crate::credential::read_bot_token(
        super::super::super::bot_role::UtilityBotRole::Notify.alias(),
    ) else {
        tracing::warn!(
            channel_id = channel_id.get(),
            kind,
            "voice auto-join alert suppressed: notify bot token not configured"
        );
        return;
    };
    let client = reqwest::Client::new();
    let base = crate::services::dispatches::discord_delivery::discord_api_base_url();
    let target = channel_id.get().to_string();
    if let Err(error) = crate::services::dispatches::discord_delivery::post_raw_message_once(
        &client, &token, &base, &target, &content,
    )
    .await
    {
        tracing::warn!(
            channel_id = channel_id.get(),
            kind,
            error = %error,
            "voice auto-join alert delivery failed via notify bot"
        );
    }
}
