use super::*;
use crate::services::discord::model_picker_interaction::handle_model_picker_interaction;

pub(super) fn is_model_picker_component_custom_id(
    custom_id: &str,
    fallback_channel_id: serenity::ChannelId,
) -> bool {
    crate::services::discord::commands::parse_model_picker_custom_id(custom_id, fallback_channel_id)
        .is_some()
}

pub(super) async fn handle_model_picker_component_if_applicable(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<bool, Error> {
    if !is_model_picker_component_custom_id(&component.data.custom_id, component.channel_id) {
        return Ok(false);
    }

    let settings_snapshot = { data.shared.settings.read().await.clone() };
    if !crate::services::discord::provider_handles_channel(
        ctx,
        &data.provider,
        &settings_snapshot,
        component.channel_id,
    )
    .await
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ⏭ COMPONENT-GUARD: skipping model picker in channel {} for provider {}",
            component.channel_id,
            data.provider.as_str()
        );
        return Ok(true);
    }

    handle_model_picker_interaction(ctx, component, data).await?;
    Ok(true)
}
