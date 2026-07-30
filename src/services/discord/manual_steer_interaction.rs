//! Manual queued-message steering controls.
//!
//! The single queue card always names the oldest queued intervention. A click
//! can claim only that exact head while a foreground mailbox turn remains live.

use std::sync::Arc;

use poise::serenity_prelude as serenity;

use super::{Data, Error, SharedData, check_auth};
use crate::services::discord::queue_dispatch::{
    mailbox_abandon_unclaimed_dispatch_after_success, mailbox_claim_manual_steer,
    mailbox_restore_manual_steer_claim,
};
use crate::services::provider::ProviderKind;

const MANUAL_STEER_CUSTOM_ID_PREFIX: &str = "manual-steer:v1:";
const QUEUED_CARD_LABEL: &str = "📬 대기 중 — 지금 주입할 수 있습니다.";

pub(super) fn manual_steer_custom_id(message_id: serenity::MessageId) -> String {
    format!("{MANUAL_STEER_CUSTOM_ID_PREFIX}{}", message_id.get())
}

fn parse_manual_steer_custom_id(custom_id: &str) -> Option<serenity::MessageId> {
    custom_id
        .strip_prefix(MANUAL_STEER_CUSTOM_ID_PREFIX)?
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .map(serenity::MessageId::new)
}

pub(super) fn is_manual_steer_custom_id(custom_id: &str) -> bool {
    parse_manual_steer_custom_id(custom_id).is_some()
}

fn queued_card_components(message_id: serenity::MessageId) -> Vec<serenity::CreateActionRow> {
    vec![serenity::CreateActionRow::Buttons(vec![
        serenity::CreateButton::new(manual_steer_custom_id(message_id))
            .label("지금 주입")
            .style(serenity::ButtonStyle::Primary),
    ])]
}

fn render_head_card(intervention: &crate::services::turn_orchestrator::Intervention) -> String {
    let mut body =
        crate::services::discord::formatting::build_monitor_handoff_placeholder_with_context(
            crate::services::discord::formatting::MonitorHandoffStatus::Queued,
            crate::services::discord::formatting::MonitorHandoffReason::Queued,
            chrono::Utc::now().timestamp(),
            None,
            None,
            Some(QUEUED_CARD_LABEL),
            None,
            Some(&intervention.text),
            None,
        );
    body.push_str("\n\n버튼은 가장 오래 대기한 메시지를 주입합니다.");
    body
}

async fn current_queue_head(
    shared: &SharedData,
    channel_id: serenity::ChannelId,
) -> Option<crate::services::turn_orchestrator::Intervention> {
    crate::services::discord::mailbox_snapshot(shared, channel_id)
        .await
        .intervention_queue
        .into_iter()
        .find(|item| item.mode == crate::services::turn_orchestrator::InterventionMode::Soft)
}

async fn render_current_head_card(
    ctx: &serenity::Context,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    placeholder_message_id: serenity::MessageId,
) -> Result<bool, serenity::Error> {
    let Some(head) = current_queue_head(shared, channel_id).await else {
        return Ok(false);
    };
    let content = render_head_card(&head);
    crate::services::discord::http::edit_channel_message_with_components(
        &ctx.http,
        channel_id,
        placeholder_message_id,
        &content,
        queued_card_components(head.message_id),
    )
    .await?;
    let key = crate::services::discord::placeholder_controller::PlaceholderKey {
        provider: provider.clone(),
        channel_id,
        message_id: placeholder_message_id,
    };
    shared
        .ui
        .placeholder_controller
        .invalidate_render_cache(&key)
        .await;
    let persist_lock = shared.queued_placeholders_persist_lock(channel_id);
    let _persist_guard = persist_lock.lock().await;
    let stale_owners: Vec<_> = shared
        .queued
        .queued_placeholders
        .iter()
        .filter_map(|entry| {
            ((*entry.key()).0 == channel_id && *entry.value() == placeholder_message_id)
                .then_some((*entry.key()).1)
        })
        .collect();
    for owner in stale_owners {
        shared.remove_queued_placeholder_locked(channel_id, owner);
    }
    shared.insert_queued_placeholder_locked(channel_id, head.message_id, placeholder_message_id);
    Ok(true)
}

pub(super) async fn attach_manual_steer_button(
    ctx: &serenity::Context,
    data: &Data,
    channel_id: serenity::ChannelId,
    placeholder_message_id: serenity::MessageId,
) -> bool {
    match render_current_head_card(
        ctx,
        &data.shared,
        &data.provider,
        channel_id,
        placeholder_message_id,
    )
    .await
    {
        Ok(rendered) => rendered,
        Err(error) => {
            tracing::warn!(
                channel_id = channel_id.get(),
                placeholder_message_id = placeholder_message_id.get(),
                error = %error,
                "manual steer button render failed; retaining queue card"
            );
            true
        }
    }
}

async fn update_deferred_component(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    content: &str,
    components: Vec<serenity::CreateActionRow>,
) {
    if let Err(error) = component
        .edit_response(
            ctx,
            serenity::EditInteractionResponse::new()
                .content(content)
                .components(components),
        )
        .await
    {
        tracing::warn!(
            channel_id = component.channel_id.get(),
            message_id = component.message.id.get(),
            error = %error,
            "manual steer failed to edit deferred component response"
        );
    }
}

async fn render_current_head_response(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
    prefix: &str,
) {
    let Some(head) = current_queue_head(&data.shared, component.channel_id).await else {
        update_deferred_component(ctx, component, prefix, Vec::new()).await;
        return;
    };
    let content = format!("{prefix}\n\n{}", render_head_card(&head));
    update_deferred_component(
        ctx,
        component,
        &content,
        queued_card_components(head.message_id),
    )
    .await;
    let key = crate::services::discord::placeholder_controller::PlaceholderKey {
        provider: data.provider.clone(),
        channel_id: component.channel_id,
        message_id: component.message.id,
    };
    data.shared
        .ui
        .placeholder_controller
        .invalidate_render_cache(&key)
        .await;
}

async fn reject_unauthorized_component(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
) {
    if let Err(error) = component
        .create_response(
            ctx,
            serenity::CreateInteractionResponse::Message(
                serenity::CreateInteractionResponseMessage::new()
                    .content("Not authorized for this bot.")
                    .ephemeral(true),
            ),
        )
        .await
    {
        tracing::warn!(error = %error, "manual steer authorization response failed");
    }
}

fn steering_prompt(intervention: &crate::services::turn_orchestrator::Intervention) -> String {
    let mut prompt = format!(
        "[Manual Discord steering request from user {}]\n{}",
        intervention.author_id.get(),
        intervention.text
    );
    if let Some(reply_context) = intervention.reply_context.as_deref()
        && !reply_context.trim().is_empty()
    {
        prompt.push_str("\n\n[Reply context]\n");
        prompt.push_str(reply_context);
    }
    prompt
}

async fn native_tui_steering_context(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> Option<(
    String,
    crate::services::provider_hosting::ProviderSessionSelection,
)> {
    if !provider.uses_managed_tmux_backend() {
        return None;
    }
    let channel_name = {
        let core = shared.core.lock().await;
        let session = core.sessions.get(&channel_id)?;
        if session.remote_profile_name.is_some() {
            return None;
        }
        session.channel_name.clone()?
    };
    let tmux_session_name = provider.build_tmux_session_name(&channel_name);
    if !crate::services::tmux_diagnostics::tmux_session_has_live_pane(&tmux_session_name) {
        return None;
    }
    let selection =
        crate::services::provider_hosting::resolve_provider_session_selection_with_channel(
            provider,
            crate::services::claude::is_tmux_available(),
            Some(channel_id.get()),
        );
    (crate::services::tui_steering::route_input_by_session_driver(&selection)
        == crate::services::tui_steering::SteeringRoute::NativeTui)
        .then_some((tmux_session_name, selection))
}

async fn inject_claimed_intervention(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    intervention: &crate::services::turn_orchestrator::Intervention,
) -> crate::services::tui_steering::SteeringOutcome {
    let Some((tmux_session_name, selection)) =
        native_tui_steering_context(shared, provider, channel_id).await
    else {
        return crate::services::tui_steering::SteeringOutcome::NotDelivered(
            "native TUI session unavailable".to_string(),
        );
    };
    let provider = provider.clone();
    let prompt = steering_prompt(intervention);
    tokio::task::spawn_blocking(move || {
        crate::services::tui_steering::inject_with_bounded_retry(
            &provider,
            &selection,
            &tmux_session_name,
            &prompt,
        )
    })
    .await
    .unwrap_or_else(|error| {
        crate::services::tui_steering::SteeringOutcome::NotDelivered(error.to_string())
    })
}

async fn register_manual_steer_turn(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    intervention: &crate::services::turn_orchestrator::Intervention,
) {
    shared
        .mailbox(channel_id)
        .restore_active_turn(
            Arc::new(crate::services::provider::CancelToken::new()),
            intervention.author_id,
            intervention.message_id,
        )
        .await;
}

pub(super) async fn handle_manual_steer_interaction(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let Some(message_id) = parse_manual_steer_custom_id(&component.data.custom_id) else {
        return Ok(());
    };
    if !check_auth(
        component.user.id,
        &component.user.name,
        &data.shared,
        &data.token,
    )
    .await
    {
        reject_unauthorized_component(ctx, component).await;
        return Ok(());
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
        if let Err(error) = component
            .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
            .await
        {
            tracing::warn!(error = %error, "manual steer channel rejection acknowledgement failed");
            return Ok(());
        }
        render_current_head_response(
            ctx,
            component,
            data,
            "이 버튼은 현재 봇의 채널에 속하지 않습니다.",
        )
        .await;
        return Ok(());
    }
    if let Err(error) = component
        .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
        .await
    {
        tracing::warn!(
            channel_id = component.channel_id.get(),
            message_id = component.message.id.get(),
            error = %error,
            "manual steer acknowledgement failed before queue claim"
        );
        return Ok(());
    }

    let claim = mailbox_claim_manual_steer(
        &data.shared,
        &data.provider,
        component.channel_id,
        message_id,
    )
    .await;
    let Some((intervention, dispatch_lease)) = claim.into_claim() else {
        render_current_head_response(
            ctx,
            component,
            data,
            "대상이 바뀌었거나 이미 처리됨 — 현재 대기 항목으로 갱신했습니다.",
        )
        .await;
        return Ok(());
    };
    let outcome = inject_claimed_intervention(
        &data.shared,
        &data.provider,
        component.channel_id,
        &intervention,
    )
    .await;
    match outcome {
        crate::services::tui_steering::SteeringOutcome::Injected => {
            mailbox_abandon_unclaimed_dispatch_after_success(
                &data.shared,
                &data.provider,
                component.channel_id,
                intervention.message_id,
                dispatch_lease,
            )
            .await;
            register_manual_steer_turn(&data.shared, component.channel_id, &intervention).await;
            data.shared
                .remove_queued_placeholder(component.channel_id, intervention.message_id)
                .await;
            render_current_head_response(ctx, component, data, "즉시 주입됨").await;
        }
        crate::services::tui_steering::SteeringOutcome::NotDelivered(_) => {
            let restored = mailbox_restore_manual_steer_claim(
                &data.shared,
                &data.provider,
                component.channel_id,
                intervention,
                dispatch_lease,
            )
            .await
            .enqueued;
            let notice = if restored {
                "주입 전 실패를 확인해 큐 맨 앞에 복원했습니다."
            } else {
                "주입 전 실패했지만 큐 복원을 확인하지 못했습니다."
            };
            render_current_head_response(ctx, component, data, notice).await;
        }
        crate::services::tui_steering::SteeringOutcome::PossiblyDelivered(_) => {
            mailbox_abandon_unclaimed_dispatch_after_success(
                &data.shared,
                &data.provider,
                component.channel_id,
                intervention.message_id,
                dispatch_lease,
            )
            .await;
            register_manual_steer_turn(&data.shared, component.channel_id, &intervention).await;
            data.shared
                .remove_queued_placeholder(component.channel_id, intervention.message_id)
                .await;
            render_current_head_response(
                ctx,
                component,
                data,
                "주입됐을 수 있어 중복을 막기 위해 큐로 복원하지 않았습니다.",
            )
            .await;
        }
        crate::services::tui_steering::SteeringOutcome::Unsafe(_)
        | crate::services::tui_steering::SteeringOutcome::ExistingMailbox => {
            let restored = mailbox_restore_manual_steer_claim(
                &data.shared,
                &data.provider,
                component.channel_id,
                intervention,
                dispatch_lease,
            )
            .await
            .enqueued;
            render_current_head_response(
                ctx,
                component,
                data,
                if restored {
                    "주입하지 않아 큐를 복원했습니다."
                } else {
                    "주입하지 않았고 큐 복원을 확인하지 못했습니다."
                },
            )
            .await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intervention(message_id: u64) -> crate::services::turn_orchestrator::Intervention {
        crate::services::turn_orchestrator::Intervention {
            author_id: serenity::UserId::new(7),
            author_is_bot: false,
            message_id: serenity::MessageId::new(message_id),
            queued_generation: 1,
            source_message_ids: vec![serenity::MessageId::new(message_id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: "manual steer".to_string(),
            mode: crate::services::turn_orchestrator::InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    #[tokio::test]
    async fn manual_steer_turn_registration_starts_mailbox_turn() {
        let shared = super::super::make_shared_data_for_tests();
        let channel_id = serenity::ChannelId::new(4_754_004);
        let intervention = intervention(4_754_005);

        register_manual_steer_turn(&shared, channel_id, &intervention).await;

        let snapshot = shared.mailbox(channel_id).snapshot().await;
        assert!(snapshot.cancel_token.is_some());
        assert_eq!(
            snapshot.active_user_message_id,
            Some(intervention.message_id)
        );
        assert_eq!(snapshot.active_request_owner, Some(intervention.author_id));
    }
}
