//! The restarted turn's own progress card after a silent end: deleted only while it is still this
//! bot's bare spinner and nothing on the channel points at it. The clear never waits on it.

use super::*;

/// Deletes the cleared snapshot's own card off the clear's path; every unclear read leaves it.
pub(super) fn spawn_cleanup(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
) {
    let card = inflight::opt_message_id(row.current_msg_id);
    let Some(card) = card.filter(|card| card.get() != row.user_msg_id) else {
        return;
    };
    let (shared, provider, row) = (shared.clone(), provider.clone(), row.clone());
    super::super::task_supervisor::spawn_observed("herdr_row_end_card", async move {
        let outcome = cleanup(&shared, &provider, &row, card).await;
        tracing::info!(
            channel_id = row.channel_id,
            message_id = card.get(),
            outcome,
            "herdr row end left or removed the restarted turn's card"
        );
    });
}

async fn cleanup(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
    card: MessageId,
) -> &'static str {
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return "http_unavailable";
    };
    let channel = ChannelId::new(row.channel_id);
    let message = match channel.message(&http, card).await {
        Ok(message) => message,
        Err(error) if not_found(&error) => return "already_gone",
        Err(_) => return "unread",
    };
    let Ok(bot) = http.get_current_user().await else {
        return "bot_unread";
    };
    if message.author.id != bot.id {
        return "not_this_bot";
    }
    let bare = message.attachments.is_empty() && message.embeds.is_empty();
    if !bare
        || !crate::services::discord::placeholder_sweeper::is_message_still_placeholder(
            &message.content,
        )
    {
        return "not_a_bare_spinner";
    }
    if referenced(provider, row.channel_id, card) {
        return "referenced";
    }
    let unbind = crate::services::discord::busy_followup_retry_store::unbind_notice_if_current;
    unbind(provider, row.channel_id, row.user_msg_id, card.get());
    // A successor may have taken the card while it was read.
    if referenced(provider, row.channel_id, card) {
        return "referenced";
    }
    match channel.delete_message(&http, card).await {
        Ok(()) => "deleted",
        Err(error) if not_found(&error) => "already_gone",
        Err(_) => "delete_failed",
    }
}

/// The channel's fresh row, a successor's, names the card, or does not read.
fn referenced(provider: &ProviderKind, channel: u64, card: MessageId) -> bool {
    match inflight::load_inflight_state_read_only_result(provider, channel) {
        Ok(row) => row.is_some_and(|row| row.current_msg_id == card.get()),
        Err(_) => true,
    }
}

fn not_found(error: &serenity::Error) -> bool {
    matches!(error, serenity::Error::Http(http) if http.status_code().map(|s| s.as_u16()) == Some(404))
}
