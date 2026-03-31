use std::time::{Duration, Instant};

use poise::serenity_prelude as serenity;

use crate::services::discord::model_catalog::is_default_picker_value;

use super::{Data, Error, check_auth};

pub(super) fn build_model_picker_close_response() -> serenity::CreateInteractionResponse {
    serenity::CreateInteractionResponse::Acknowledge
}

async fn close_model_picker_interaction(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
) -> Result<(), Error> {
    component
        .create_response(ctx, build_model_picker_close_response())
        .await?;
    let _ = component.message.delete(&ctx.http).await;

    Ok(())
}

pub(super) async fn handle_model_picker_interaction(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let ts = chrono::Local::now().format("%H:%M:%S");
    let display_channel_id = component.channel_id;
    let user_id = component.user.id;
    let user_name = &component.user.name;
    println!(
        "  [{ts}] ◀ [{}] model picker {}",
        user_name, display_channel_id
    );

    if !check_auth(user_id, user_name, &data.shared, &data.token).await {
        component
            .create_response(
                ctx,
                serenity::CreateInteractionResponse::Message(
                    serenity::CreateInteractionResponseMessage::new()
                        .content("Not authorized for this bot.")
                        .ephemeral(true),
                ),
            )
            .await?;
        return Ok(());
    }

    if !super::commands::provider_supports_model_override(&data.provider) {
        component
            .create_response(
                ctx,
                serenity::CreateInteractionResponse::Message(
                    serenity::CreateInteractionResponseMessage::new()
                        .content(
                            "Model override is only supported for Claude, Codex, and Gemini channels.",
                        )
                        .ephemeral(true),
                ),
            )
            .await?;
        return Ok(());
    }

    let Some((action, target_channel_id)) = super::commands::parse_model_picker_custom_id(
        &component.data.custom_id,
        display_channel_id,
    ) else {
        component
            .create_response(
                ctx,
                serenity::CreateInteractionResponse::Message(
                    serenity::CreateInteractionResponseMessage::new()
                        .content("Unsupported model picker interaction.")
                        .ephemeral(true),
                ),
            )
            .await?;
        return Ok(());
    };

    let message_id = component.message.id;
    let Some(state_snapshot) = data
        .shared
        .model_picker_pending
        .get(&message_id)
        .map(|entry| entry.clone())
    else {
        component
            .create_response(
                ctx,
                serenity::CreateInteractionResponse::Message(
                    serenity::CreateInteractionResponseMessage::new()
                        .content("This model picker has expired. Run `/model` again.")
                        .ephemeral(true),
                ),
            )
            .await?;
        return Ok(());
    };

    if Instant::now().duration_since(state_snapshot.updated_at) > Duration::from_secs(30 * 60) {
        data.shared.model_picker_pending.remove(&message_id);
        component
            .create_response(
                ctx,
                serenity::CreateInteractionResponse::Message(
                    serenity::CreateInteractionResponseMessage::new()
                        .content("This model picker has expired. Run `/model` again.")
                        .ephemeral(true),
                ),
            )
            .await?;
        return Ok(());
    }

    if state_snapshot.owner_user_id != user_id {
        component
            .create_response(
                ctx,
                serenity::CreateInteractionResponse::Message(
                    serenity::CreateInteractionResponseMessage::new()
                        .content("Only the original requester can use this model picker.")
                        .ephemeral(true),
                ),
            )
            .await?;
        return Ok(());
    }

    if state_snapshot.target_channel_id != target_channel_id {
        component
            .create_response(
                ctx,
                serenity::CreateInteractionResponse::Message(
                    serenity::CreateInteractionResponseMessage::new()
                        .content("This picker no longer matches the target channel.")
                        .ephemeral(true),
                ),
            )
            .await?;
        return Ok(());
    }

    let notice = match action {
        super::commands::ModelPickerAction::Select => {
            let selected = match &component.data.kind {
                serenity::ComponentInteractionDataKind::StringSelect { values } => {
                    values.first().cloned()
                }
                _ => None,
            };

            let Some(selected) = selected else {
                component
                    .create_response(
                        ctx,
                        serenity::CreateInteractionResponse::Message(
                            serenity::CreateInteractionResponseMessage::new()
                                .content("Unsupported model picker interaction.")
                                .ephemeral(true),
                        ),
                    )
                    .await?;
                return Ok(());
            };

            let pending_model = if is_default_picker_value(&selected) {
                Some(selected)
            } else {
                let validated =
                    match super::commands::validate_model_input(&data.provider, &selected) {
                        Ok(model) => model,
                        Err(message) => {
                            component
                                .create_response(
                                    ctx,
                                    serenity::CreateInteractionResponse::Message(
                                        serenity::CreateInteractionResponseMessage::new()
                                            .content(message)
                                            .ephemeral(true),
                                    ),
                                )
                                .await?;
                            return Ok(());
                        }
                    };
                Some(validated)
            };

            if let Some(mut state) = data.shared.model_picker_pending.get_mut(&message_id) {
                state.pending_model = pending_model;
                state.updated_at = Instant::now();
            }
            "선택을 임시 저장했습니다. `저장`을 눌러 적용하세요."
        }
        super::commands::ModelPickerAction::Submit => {
            let pending_model = data
                .shared
                .model_picker_pending
                .get(&message_id)
                .and_then(|state| state.pending_model.clone());

            super::commands::clear_model_picker_pending(&data.shared, message_id);
            close_model_picker_interaction(ctx, component).await?;

            match super::commands::model_picker_pending_to_override(pending_model.as_deref()) {
                Some(Some(model)) => {
                    super::commands::update_channel_model_override(
                        &data.shared,
                        &data.token,
                        target_channel_id,
                        &data.provider,
                        Some(model),
                    )
                    .await;
                }
                Some(None) => {
                    super::commands::update_channel_model_override(
                        &data.shared,
                        &data.token,
                        target_channel_id,
                        &data.provider,
                        None,
                    )
                    .await;
                }
                None => {}
            }

            return Ok(());
        }
        super::commands::ModelPickerAction::Reset => {
            super::commands::clear_model_picker_pending(&data.shared, message_id);
            close_model_picker_interaction(ctx, component).await?;
            super::commands::update_channel_model_override(
                &data.shared,
                &data.token,
                target_channel_id,
                &data.provider,
                None,
            )
            .await;
            return Ok(());
        }
        super::commands::ModelPickerAction::Cancel => {
            super::commands::clear_model_picker_pending(&data.shared, message_id);
            close_model_picker_interaction(ctx, component).await?;
            return Ok(());
        }
    };

    // Only Select reaches here — update the picker embed in-place.
    let pending_model = data
        .shared
        .model_picker_pending
        .get(&message_id)
        .and_then(|state| state.pending_model.clone());

    let snapshot = super::commands::effective_model_snapshot(&data.shared, target_channel_id).await;
    let embed = super::commands::build_model_picker_embed_from_snapshot(
        &snapshot,
        &data.provider,
        pending_model.as_deref(),
        Some(notice),
    );
    let components = super::commands::build_model_picker_components_from_snapshot(
        &snapshot,
        target_channel_id,
        &data.provider,
        pending_model.as_deref(),
    );
    component
        .create_response(
            ctx,
            serenity::CreateInteractionResponse::UpdateMessage(
                serenity::CreateInteractionResponseMessage::new()
                    .embed(embed)
                    .components(components),
            ),
        )
        .await?;
    Ok(())
}
