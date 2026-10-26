use super::*;

/// `!clear`, `!restart` and `!stop`, and the answer to a removed `!` name; `false`
/// means the text is not a command and goes on as chat.
pub(in crate::services::discord::router) async fn handle_text_command(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    data: &Data,
    channel_id: serenity::ChannelId,
    text: &str,
) -> Result<bool, Error> {
    super::super::super::commands::handle_text_command(ctx, msg, data, channel_id, text).await
}
