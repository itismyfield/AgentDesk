//! The recap card's suggested-reply button. An unprotected channel enqueues and then drops the
//! card as in main; a channel the input fence admits consumes the card only once enqueued.

use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use super::compact::{consume_recap_card, reserve_recap_card};
use super::{
    Data, Error, RecapClearTarget, admit_recap_effect, authorize_component,
    lookup_current_recap_target, parse_message_id, prompt_sent_ephemeral, send_ephemeral,
};
use crate::services::discord::idle_recap::{
    IDLE_RECAP_SUGGEST_BUTTON_PREFIX, clear_recap_pointer, delete_previous_card,
};
use crate::services::discord::input_runtime::fence;
use crate::services::provider::ProviderKind;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum RecapSuggestOutcome {
    /// The input fence refused the click before the enqueue.
    Refused(String),
    /// Another recap action holds the card, or it is no longer the current one.
    Stale,
    NotEnqueued,
    /// The legacy queue took the reply; `consumed` when the card's pointer went with it.
    Enqueued {
        consumed: bool,
    },
}

pub(super) async fn handle_idle_recap_suggest_interaction(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    if !authorize_component(ctx, component, data).await {
        return Ok(());
    }

    let Some(message_id) =
        parse_message_id(&component.data.custom_id, IDLE_RECAP_SUGGEST_BUTTON_PREFIX)
    else {
        let _ = component
            .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
            .await;
        return Ok(());
    };
    let Some(suggested_reply) =
        crate::services::discord::idle_recap::suggested_reply_from_recap_content(
            &component.message.content,
        )
    else {
        send_ephemeral(ctx, component, "추천 답변을 찾을 수 없습니다.").await;
        return Ok(());
    };
    let prompt_text = suggested_reply.clone();
    let Some(pool) = data.shared.pg_pool.as_ref().cloned() else {
        send_ephemeral(ctx, component, "추천 답변 전송 실패: DB 연결 없음.").await;
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
            "추천 답변 대상이 더 이상 유효하지 않습니다.",
        )
        .await;
        return Ok(());
    };

    let session_key = target.session_key.clone();
    let card = (target, message_id, component.channel_id.get());
    let enqueue = || {
        crate::services::discord::enqueue_internal_followup(
            &data.shared,
            &data.provider,
            component.channel_id,
            serenity::MessageId::new(message_id),
            suggested_reply,
            "idle recap suggested reply",
        )
    };
    let consumed = match run_recap_suggest(&data.provider, &pool, card, enqueue).await {
        RecapSuggestOutcome::Refused(reason) => {
            tracing::warn!(message_id, %reason, "idle_recap suggest: input fence refused");
            let refusal =
                "추천 답변 전송 거절: 이 채널의 입력이 전환 중이라 지금은 보낼 수 없습니다.";
            send_ephemeral(ctx, component, refusal).await;
            return Ok(());
        }
        RecapSuggestOutcome::Stale => {
            let stale = "추천 답변 대상이 더 이상 유효하지 않거나 이미 처리 중입니다.";
            send_ephemeral(ctx, component, stale).await;
            return Ok(());
        }
        RecapSuggestOutcome::NotEnqueued => {
            send_ephemeral(ctx, component, "추천 답변을 큐에 넣지 못했습니다.").await;
            return Ok(());
        }
        RecapSuggestOutcome::Enqueued { consumed } => consumed,
    };

    send_ephemeral(ctx, component, &prompt_sent_ephemeral(&prompt_text)).await;
    if !consumed {
        let _ = clear_recap_pointer(&pool, &session_key, message_id).await;
    }
    delete_previous_card(&ctx.http, component.channel_id.get(), message_id).await;
    Ok(())
}

/// Admits the click first. An unprotected channel only enqueues; an admitted one reserves the
/// card, enqueues once under its permit and consumes the exact pointer once the queue took it.
pub(super) async fn run_recap_suggest<Enqueue, EnqueueFut>(
    provider: &ProviderKind,
    pool: &PgPool,
    (target, message_id, channel_id): (RecapClearTarget, u64, u64),
    enqueue: Enqueue,
) -> RecapSuggestOutcome
where
    Enqueue: FnOnce() -> EnqueueFut,
    EnqueueFut: std::future::Future<Output = bool>,
{
    let permit = match admit_recap_effect(provider, channel_id) {
        Ok(permit) => permit,
        Err(reason) => return RecapSuggestOutcome::Refused(reason),
    };
    let Some(permit) = permit else {
        return match enqueue().await {
            true => RecapSuggestOutcome::Enqueued { consumed: false },
            false => RecapSuggestOutcome::NotEnqueued,
        };
    };

    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(error) => {
            tracing::warn!(%error, message_id, "idle_recap suggest: reservation unavailable");
            return RecapSuggestOutcome::NotEnqueued;
        }
    };
    match reserve_recap_card(&mut tx, &target, message_id, channel_id).await {
        Ok(true) => {}
        Ok(false) => return RecapSuggestOutcome::Stale,
        Err(error) => {
            tracing::warn!(%error, message_id, "idle_recap suggest: reservation failed");
            return RecapSuggestOutcome::NotEnqueued;
        }
    }
    if !fence::effect::scope(Some(permit), enqueue()).await {
        return RecapSuggestOutcome::NotEnqueued;
    }
    // The queue owns the reply now; a failed pointer write falls back to main's message-id clear.
    let consumed = consume_recap_card(&mut tx, &target, message_id, channel_id).await;
    let committed = async {
        consumed?;
        tx.commit().await
    }
    .await;
    if let Err(error) = &committed {
        tracing::warn!(%error, message_id, "idle_recap suggest: enqueued, pointer write failed");
    }
    RecapSuggestOutcome::Enqueued {
        consumed: committed.is_ok(),
    }
}
