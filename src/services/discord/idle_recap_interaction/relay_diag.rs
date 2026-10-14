//! The recap card's relay-diagnosis button: an ephemeral integrity report for the card's session.

use poise::serenity_prelude as serenity;

use super::{
    Data, Error, authorize_component, lookup_current_recap_target, parse_message_id,
    send_ephemeral, truncate_interaction_body,
};
use crate::services::discord::idle_recap::IDLE_RECAP_RELAY_DIAG_BUTTON_PREFIX;
use crate::services::provider::ProviderKind;

pub(super) async fn handle_idle_recap_relay_diag_interaction(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    if !authorize_component(ctx, component, data).await {
        return Ok(());
    }

    let Some(message_id) = parse_message_id(
        &component.data.custom_id,
        IDLE_RECAP_RELAY_DIAG_BUTTON_PREFIX,
    ) else {
        let _ = component
            .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
            .await;
        return Ok(());
    };
    let Some(pool) = data.shared.pg_pool.as_ref().cloned() else {
        send_ephemeral(ctx, component, "릴레이 진단 실패: DB 연결 없음.").await;
        return Ok(());
    };
    let Some(target) = lookup_current_recap_target(
        &pool,
        message_id,
        component.channel_id.get(),
        data.provider.as_str(),
    )
    .await?
    else {
        send_ephemeral(
            ctx,
            component,
            "릴레이 진단 대상이 더 이상 유효하지 않습니다.",
        )
        .await;
        return Ok(());
    };
    let Some(snapshot) =
        crate::services::discord::idle_recap::load_recap_snapshot(&pool, &target.session_key)
            .await?
    else {
        send_ephemeral(ctx, component, "릴레이 진단 실패: 세션을 찾을 수 없습니다.").await;
        return Ok(());
    };
    let Some(provider) = ProviderKind::from_str(&snapshot.provider) else {
        send_ephemeral(
            ctx,
            component,
            "릴레이 진단 실패: provider를 확인할 수 없습니다.",
        )
        .await;
        return Ok(());
    };
    let probe = crate::services::discord::idle_recap::probe_relay_integrity(
        &snapshot,
        &provider,
        component.channel_id.get(),
        Some(message_id),
    );
    let report = truncate_interaction_body(&probe.diagnostic_report());
    send_ephemeral(ctx, component, &format!("```text\n{report}\n```")).await;
    Ok(())
}
