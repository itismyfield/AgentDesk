//! Manual queued-message steering controls.
//!
//! A queue card can expose one authenticated action that atomically claims its
//! current queued head, injects it into a live native TUI, and restores the
//! exact entry at the front if injection does not succeed.

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

pub(super) async fn attach_manual_steer_button(
    ctx: &serenity::Context,
    channel_id: serenity::ChannelId,
    placeholder_message_id: serenity::MessageId,
    queued_message_id: serenity::MessageId,
) -> bool {
    match crate::services::discord::http::edit_channel_message_with_components(
        &ctx.http,
        channel_id,
        placeholder_message_id,
        QUEUED_CARD_LABEL,
        queued_card_components(queued_message_id),
    )
    .await
    {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(
                channel_id = channel_id.get(),
                placeholder_message_id = placeholder_message_id.get(),
                queued_message_id = queued_message_id.get(),
                error = %error,
                "manual steer button render failed"
            );
            false
        }
    }
}

async fn update_component_message(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    content: &str,
) {
    let _ = component
        .create_response(
            ctx,
            serenity::CreateInteractionResponse::UpdateMessage(
                serenity::CreateInteractionResponseMessage::new()
                    .content(content)
                    .components(Vec::new()),
            ),
        )
        .await;
}

async fn reject_unauthorized_component(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
) {
    let _ = component
        .create_response(
            ctx,
            serenity::CreateInteractionResponse::Message(
                serenity::CreateInteractionResponseMessage::new()
                    .content("Not authorized for this bot.")
                    .ephemeral(true),
            ),
        )
        .await;
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
        return crate::services::tui_steering::SteeringOutcome::Unsafe(
            "native TUI session unavailable",
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
        crate::services::tui_steering::SteeringOutcome::Failed(error.to_string())
    })
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
        update_component_message(
            ctx,
            component,
            "이 버튼은 현재 봇의 채널에 속하지 않습니다.",
        )
        .await;
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
        update_component_message(
            ctx,
            component,
            "이미 처리됨 — 이 큐 항목은 더 이상 대기 중이 아닙니다.",
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
    if matches!(
        outcome,
        crate::services::tui_steering::SteeringOutcome::Injected
    ) {
        mailbox_abandon_unclaimed_dispatch_after_success(
            &data.shared,
            &data.provider,
            component.channel_id,
            intervention.message_id,
            dispatch_lease,
        )
        .await;
        update_component_message(ctx, component, "즉시 주입됨").await;
        return Ok(());
    }

    let restored = mailbox_restore_manual_steer_claim(
        &data.shared,
        &data.provider,
        component.channel_id,
        intervention,
        dispatch_lease,
    )
    .await
    .enqueued;
    if restored {
        update_component_message(
            ctx,
            component,
            "주입에 실패하여 메시지를 큐의 맨 앞에 복원했습니다.",
        )
        .await;
    } else {
        tracing::error!(
            channel_id = component.channel_id.get(),
            message_id = message_id.get(),
            outcome = ?outcome,
            "manual steer injection failed and queue rollback did not complete"
        );
        update_component_message(
            ctx,
            component,
            "주입 실패 — 안전 복원을 확인하지 못했습니다. 잠시 후 다시 시도해 주세요.",
        )
        .await;
    }
    Ok(())
}
