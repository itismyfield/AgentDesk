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

fn model_picker_save_result_response(
    result: std::io::Result<String>,
) -> serenity::CreateInteractionResponse {
    match result {
        Ok(notice) => build_model_picker_saved_response(notice),
        Err(error) => serenity::CreateInteractionResponse::Message(
            serenity::CreateInteractionResponseMessage::new()
                .content(format!("모델 설정을 저장하지 못했습니다: {error}"))
                .ephemeral(true),
        ),
    }
}

async fn persist_model_picker_notice(
    shared: &std::sync::Arc<super::SharedData>,
    token: &str,
    channel: serenity::ChannelId,
    provider: &crate::services::provider::ProviderKind,
    next: Option<Option<String>>,
) -> std::io::Result<String> {
    shared.check_settings_owner(provider, channel).await?;
    let changed = match &next {
        Some(model)
            if super::commands::would_channel_model_override_change(
                shared,
                channel,
                model.as_deref(),
            ) =>
        {
            super::commands::update_channel_model_override(
                shared,
                token,
                channel,
                provider,
                model.clone(),
            )
            .await?
        }
        Some(_) => false,
        None => false,
    };
    let saved = next.as_ref().and_then(|model| model.as_deref());
    let key = channel.get().to_string();
    let relaunch = shared
        .settings
        .read()
        .await
        .channel_relaunch_pending
        .contains(&key);
    Ok(if changed && relaunch {
        model_picker_relaunch_notice(saved)
    } else if changed {
        model_picker_submit_notice(
            saved,
            shared.overrides.session_reset_pending.contains(&channel),
        )
    } else {
        model_picker_no_change_notice()
    })
}

fn model_picker_submit_notice(saved_model: Option<&str>, reset_pending: bool) -> String {
    let session_note = if reset_pending {
        "다음 사용자 턴 시작 전 세션 리셋이 필요합니다(맥락 소실). 리셋 후 새 세션에 반영됩니다."
    } else {
        "기존 세션은 리셋하지 않습니다. 저장한 기본값은 새 세션에 사용됩니다."
    };
    match saved_model {
        Some(model) => format!("모델 설정을 `{model}`로 저장했습니다. {session_note}"),
        None => format!("모델 설정을 기본값으로 저장했습니다. {session_note}"),
    }
}

fn model_picker_relaunch_notice(saved_model: Option<&str>) -> String {
    let saved = saved_model.map_or("기본값".to_string(), |model| format!("`{model}`"));
    format!(
        "모델 설정을 {saved}(으)로 저장했습니다. 다음 새 턴 시작 때 같은 세션으로 TUI를 재기동해 적용합니다(맥락 유지, 수 초). 진행 중인 턴에 합류하는 메시지는 기존 모델로 처리됩니다."
    )
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
                        .content("Model override is only supported for Claude, Codex, Gemini, OpenCode, and Qwen channels.")
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
        .overrides
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
        data.shared
            .overrides
            .model_picker_pending
            .remove(&message_id);
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

    let provider = data
        .shared
        .settings_provider(target_channel_id, &data.provider)
        .await;

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
                    &provider,
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

            if let Some(mut state) = data
                .shared
                .overrides
                .model_picker_pending
                .get_mut(&message_id)
            {
                state.pending_model = pending_model;
                state.updated_at = Instant::now();
            }
            "선택을 임시 저장했습니다. `저장`을 눌러 적용하세요."
        }
        super::commands::ModelPickerAction::Submit | super::commands::ModelPickerAction::Reset => {
            let next_override = if action == super::commands::ModelPickerAction::Reset {
                Some(None)
            } else {
                let pending = data
                    .shared
                    .overrides
                    .model_picker_pending
                    .get(&message_id)
                    .and_then(|state| state.pending_model.clone());
                super::commands::model_picker_pending_to_override(pending.as_deref())
            };
            let result = persist_model_picker_notice(
                &data.shared,
                &data.token,
                target_channel_id,
                &provider,
                next_override,
            )
            .await;
            if result.is_ok() {
                super::commands::clear_model_picker_pending(&data.shared, message_id);
            }
            component
                .create_response(ctx, model_picker_save_result_response(result))
                .await?;
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
        .overrides
        .model_picker_pending
        .get(&message_id)
        .and_then(|state| state.pending_model.clone());

    let snapshot = super::commands::effective_model_snapshot(&data.shared, target_channel_id).await;
    let working_dir = super::commands::current_working_dir(&data.shared, target_channel_id).await;
    let embed = super::commands::build_model_picker_embed_from_snapshot(
        &snapshot,
        &provider,
        pending_model.as_deref(),
        Some(notice),
        working_dir.as_deref(),
    );
    let components = super::commands::build_model_picker_components_from_snapshot(
        &snapshot,
        target_channel_id,
        &provider,
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
mod truthful_tests {
    use super::*;

    #[test]
    fn truthful_reset_notice_explicitly_warns_about_context_loss() {
        let notice = model_picker_submit_notice(Some("model-a"), true);
        assert!(notice.contains("맥락 소실"), "{notice}");
        assert!(!notice.contains("세션을 유지"));
        assert!(model_picker_submit_notice(None, false).contains("새 세션"));
    }
    #[test]
    fn truthful_failed_save_response_is_failure_and_keeps_session_state() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CONFIG",
            &root.path().join("config/agentdesk.yaml"),
        );
        std::fs::create_dir_all(root.path().join("config/bot_settings.json")).unwrap();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = serenity::ChannelId::new(64869);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(persist_model_picker_notice(
            &shared,
            "test-token",
            channel,
            &crate::services::provider::ProviderKind::Codex,
            Some(Some("model-a".into())),
        ));
        assert!(result.is_err());
        let response = serde_json::to_value(model_picker_save_result_response(result)).unwrap();
        assert_eq!(response["type"], 4);
        let content = response["data"]["content"].as_str().unwrap();
        assert!(content.contains("저장하지 못했습니다"), "{response}");
        assert!(!content.contains("저장했습니다"), "{response}");
        assert!(!shared.overrides.model_overrides.contains_key(&channel));
        assert!(!shared.overrides.session_reset_pending.contains(&channel));
    }
}
