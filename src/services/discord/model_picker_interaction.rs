use std::time::{Duration, Instant};

use poise::serenity_prelude as serenity;

use crate::services::discord::model_catalog::is_default_picker_value;

use super::{Data, Error, check_auth};

pub(super) fn build_model_picker_close_response() -> serenity::CreateInteractionResponse {
    serenity::CreateInteractionResponse::Acknowledge
}

pub(super) fn build_model_picker_saved_response(
    content: impl Into<String>,
) -> serenity::CreateInteractionResponse {
    serenity::CreateInteractionResponse::UpdateMessage(
        serenity::CreateInteractionResponseMessage::new()
            .content(content)
            .embeds(Vec::new())
            .components(Vec::new()),
    )
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

async fn save_model_picker_interaction(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    content: impl Into<String>,
) -> Result<(), Error> {
    component
        .create_response(ctx, build_model_picker_saved_response(content))
        .await?;
    Ok(())
}

fn model_picker_submit_notice(
    saved_model: Option<&str>,
    provider: &crate::services::provider::ProviderKind,
) -> String {
    use crate::services::provider::ProviderKind;
    let session_note = match provider {
        ProviderKind::Gemini => "새 세션 + 새 모델로 적용됩니다.",
        _ => "현재 세션을 유지한 채 다음 turn부터 새 모델로 적용됩니다.",
    };
    match saved_model {
        Some(model) => format!("모델 설정을 `{model}`로 저장했습니다. {session_note}"),
        None => format!("모델 설정을 기본값으로 저장했습니다. {session_note}"),
    }
}

fn model_picker_no_change_notice() -> String {
    "변경 사항이 없습니다. 현재 모델 설정을 유지합니다.".to_string()
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
    tracing::info!(
        "  [{ts}] ◀ [{}] model picker {}",
        user_name,
        display_channel_id
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
                        .content("Model override is only supported for Claude, Codex, Gemini, and Qwen channels.")
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
                let working_dir =
                    super::commands::current_working_dir(&data.shared, target_channel_id).await;
                let validated = match super::commands::validate_model_input(
                    &data.provider,
                    &selected,
                    working_dir.as_deref(),
                ) {
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

            let next_override =
                super::commands::model_picker_pending_to_override(pending_model.as_deref());
            let changed = super::commands::would_channel_model_override_change(
                &data.shared,
                target_channel_id,
                next_override.as_ref().and_then(|model| model.as_deref()),
            );
            let notice = match next_override.as_ref() {
                Some(Some(model)) => {
                    if changed {
                        model_picker_submit_notice(Some(model), &data.provider)
                    } else {
                        model_picker_no_change_notice()
                    }
                }
                Some(None) => {
                    if changed {
                        model_picker_submit_notice(None, &data.provider)
                    } else {
                        model_picker_no_change_notice()
                    }
                }
                None => model_picker_no_change_notice(),
            };

            super::commands::clear_model_picker_pending(&data.shared, message_id);
            save_model_picker_interaction(ctx, component, notice).await?;
            if changed {
                if let Some(override_to_persist) = next_override {
                    super::commands::update_channel_model_override(
                        &data.shared,
                        &data.token,
                        target_channel_id,
                        &data.provider,
                        override_to_persist,
                    )
                    .await;
                }
            }
            return Ok(());
        }
        super::commands::ModelPickerAction::Reset => {
            let changed = super::commands::would_channel_model_override_change(
                &data.shared,
                target_channel_id,
                None,
            );
            super::commands::clear_model_picker_pending(&data.shared, message_id);
            let notice = if changed {
                model_picker_submit_notice(None, &data.provider)
            } else {
                model_picker_no_change_notice()
            };
            save_model_picker_interaction(ctx, component, notice).await?;
            if changed {
                super::commands::update_channel_model_override(
                    &data.shared,
                    &data.token,
                    target_channel_id,
                    &data.provider,
                    None,
                )
                .await;
            }
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
    let working_dir = super::commands::current_working_dir(&data.shared, target_channel_id).await;
    let embed = super::commands::build_model_picker_embed_from_snapshot(
        &snapshot,
        &data.provider,
        pending_model.as_deref(),
        Some(notice),
        working_dir.as_deref(),
    );
    let components = super::commands::build_model_picker_components_from_snapshot(
        &snapshot,
        target_channel_id,
        &data.provider,
        pending_model.as_deref(),
        working_dir.as_deref(),
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

#[cfg(test)]
mod tests {
    use super::{
        build_model_picker_saved_response, model_picker_no_change_notice,
        model_picker_submit_notice,
    };

    #[test]
    fn model_picker_submit_notice_non_gemini_keeps_session() {
        use crate::services::provider::ProviderKind;
        for provider in [
            ProviderKind::Claude,
            ProviderKind::Codex,
            ProviderKind::Qwen,
        ] {
            let notice = model_picker_submit_notice(Some("gpt-5.4-mini"), &provider);
            assert!(
                notice.contains("`gpt-5.4-mini`"),
                "{provider:?}: model name missing"
            );
            assert!(
                notice.contains("현재 세션을 유지"),
                "{provider:?}: should say session is kept"
            );
            assert!(
                !notice.contains("새 세션"),
                "{provider:?}: must not claim new session"
            );
        }
    }

    #[test]
    fn model_picker_submit_notice_gemini_starts_new_session() {
        use crate::services::provider::ProviderKind;
        let notice = model_picker_submit_notice(Some("gemini-2.5-pro"), &ProviderKind::Gemini);
        assert!(notice.contains("`gemini-2.5-pro`"));
        assert!(notice.contains("새 세션"));
    }

    #[test]
    fn model_picker_submit_notice_default_reset_is_provider_aware() {
        use crate::services::provider::ProviderKind;
        let notice_claude = model_picker_submit_notice(None, &ProviderKind::Claude);
        assert!(notice_claude.contains("현재 세션을 유지"));
        let notice_gemini = model_picker_submit_notice(None, &ProviderKind::Gemini);
        assert!(notice_gemini.contains("새 세션"));
    }

    #[test]
    fn model_picker_no_change_notice_does_not_claim_new_session() {
        let notice = model_picker_no_change_notice();
        assert!(notice.contains("변경 사항이 없습니다"));
        assert!(!notice.contains("새 세션"));
    }

    #[test]
    fn model_picker_saved_response_clears_components_and_embeds() {
        let response = build_model_picker_saved_response("저장 완료");
        let debug = format!("{response:?}");
        assert!(debug.contains("UpdateMessage"));
        assert!(debug.contains("components: Some([])"));
        assert!(debug.contains("embeds: Some([])"));
    }
}
