//! A click on a Decision button: acknowledged at once, then the clicker, the post and the handle
//! are checked and the choice is stored before anything is delivered.

use std::sync::Arc;

use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use super::{card, lifecycle};
use crate::db::decisions::selection::{self, Chosen, Click, Selection};
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::{Data, Error};
use crate::services::manual_steer::action_handle::ActionHandle;

/// Handles the interaction when its custom-id is a Decision handle; `false` leaves it to others.
pub(crate) async fn component(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<bool, Error> {
    if !ActionHandle::is_decision_custom_id(&component.data.custom_id) {
        return Ok(false);
    }
    let defer = serenity::CreateInteractionResponseMessage::new().ephemeral(true);
    let ack = serenity::CreateInteractionResponse::Defer(defer);
    component.create_response(ctx, ack).await?;
    let reply = click(ctx, component, data).await;
    let answer = serenity::EditInteractionResponse::new().content(reply);
    component.edit_response(ctx, answer).await?;
    Ok(true)
}

/// Stores the owner's click as the request's choice and says what the click did. The stored
/// choice is the only thing that starts a delivery; a repeated or late click starts none.
pub(super) async fn click(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> String {
    let Some(pool) = data.shared.pg_pool.as_ref() else {
        return "선택을 저장할 수 없습니다. 잠시 뒤 다시 눌러 주세요.".into();
    };
    if !crate::services::discord::check_owner(component.user.id, &data.shared).await {
        return "운영자만 선택할 수 있습니다.".into();
    }
    let click = Click {
        handle: &component.data.custom_id,
        thread_id: component.channel_id.get(),
        message_id: component.message.id.get(),
        bot_user_id: ctx.cache.current_user().id.get(),
        actor: component.user.id.get(),
    };
    match selection::choose_once(pool, &click).await {
        Ok(Some(Chosen::First(selection))) => {
            let reply = format!("선택 접수: **{}**", selection.label);
            let registry = data.shared.health_registry();
            tokio::spawn(accepted(
                ctx.http.clone(),
                pool.clone(),
                registry,
                selection,
            ));
            reply
        }
        Ok(Some(Chosen::Already { label, same: true })) => format!("이미 선택됨: **{label}**"),
        Ok(Some(Chosen::Already { label, same: false })) => {
            format!("이미 **{label}**(으)로 선택되어 바꿀 수 없습니다.")
        }
        Ok(Some(Chosen::NotOpen)) => "이 결정은 지금 선택을 받지 않습니다.".into(),
        Ok(None) => "등록된 결정 글의 버튼이 아닙니다.".into(),
        Err(error) => {
            tracing::warn!(%error, "decision choice not stored");
            "선택을 저장하지 못했습니다. 잠시 뒤 다시 눌러 주세요.".into()
        }
    }
}

/// After the choice is stored: the card closes its buttons and the selection is delivered once.
async fn accepted(
    http: Arc<serenity::Http>,
    pool: PgPool,
    registry: Option<Arc<HealthRegistry>>,
    selection: Selection,
) {
    let (post, status) = (
        (selection.request_id, selection.thread_id),
        card::selecting(&selection.label),
    );
    if let Err(error) = lifecycle::show(&pool, &http, post, &status, false).await {
        tracing::warn!(%error, "decision card still shows open buttons");
    }
    // Without a registry here the selection pass starts the stored selection.
    if let Some(registry) = registry {
        lifecycle::deliver(&pool, &registry, &selection).await;
        lifecycle::converge(&pool, &registry).await;
    }
}
