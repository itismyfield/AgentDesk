//! Discord side of a delegation: the accept card with its `[상태]`/`[중단]` controls, and the
//! click handler. Decisions live in `services::agents::delegation`.

use poise::serenity_prelude as serenity;
use serde_json::Value;

use super::health::HealthRegistry;
use super::{Data, Error};
use crate::services::agents::delegation::{
    Actor, Click, Delegation, DelegationControl, click_reply, parse_click,
};

/// Posts the accept card into the requesting channel through the bot that runs that channel,
/// whatever its provider, so the clicks come back to this server's handler.
pub(crate) async fn post_accept_card(
    registry: &HealthRegistry,
    delegation: &Delegation,
    worker_channel: u64,
) -> Result<u64, String> {
    let provider = delegation.requester_provider.as_str();
    let http = super::health::resolve_bot_http(registry, provider)
        .await
        .map_err(|(_, error)| error)?;
    let origin = &delegation.origin;
    let (worker, id) = (&origin.worker_agent, origin.id());
    let content = format!("📨 **위임 접수** · `{worker}` → <#{worker_channel}>\n-# origin `{id}`");
    let buttons = vec![
        serenity::CreateButton::new(Click::Status.custom_id(origin))
            .label("상태")
            .style(serenity::ButtonStyle::Secondary),
        serenity::CreateButton::new(Click::Stop.custom_id(origin))
            .label("중단")
            .style(serenity::ButtonStyle::Danger),
    ];
    super::http::send_channel_message_with_components(
        &http,
        serenity::ChannelId::new(origin.requester_channel),
        &content,
        vec![serenity::CreateActionRow::Buttons(buttons)],
    )
    .await
    .map(|message| message.id.get())
    .map_err(|error| error.to_string())
}

/// Answers a delegation card click privately; any other component goes to the next handler.
pub(super) async fn component(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<bool, Error> {
    let Some((click, origin)) = parse_click(&component.data.custom_id) else {
        return Ok(false);
    };
    let defer = serenity::CreateInteractionResponseMessage::new().ephemeral(true);
    component
        .create_response(ctx, serenity::CreateInteractionResponse::Defer(defer))
        .await?;
    let actor = Actor {
        owner: data.shared.settings.read().await.owner_user_id,
        user: component.user.id.get(),
        channel: component.channel_id.get(),
    };
    let reply = click_reply(&Loopback, click, &origin, &actor).await;
    component
        .edit_response(ctx, serenity::EditInteractionResponse::new().content(reply))
        .await?;
    Ok(true)
}

/// The server's own input-result and turn/stop routes, called over loopback.
struct Loopback;

#[async_trait::async_trait]
impl DelegationControl for Loopback {
    async fn input_result(&self, agent_id: &str, origin_id: &str) -> Result<Value, String> {
        super::internal_api::get_input_result(agent_id, origin_id).await
    }

    async fn stop(&self, agent_id: &str) -> Result<Value, String> {
        super::internal_api::stop_agent_turn(agent_id).await
    }
}
