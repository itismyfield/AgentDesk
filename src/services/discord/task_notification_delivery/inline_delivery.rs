//! Old POST attempts retain their original bytes and only reconcile; they never re-send.
use super::*;

pub(super) fn needs_reconciliation(
    event: &TaskCardEvent,
    clients: &CardDeliveryClients,
    claimed: &ClaimedCard,
) -> bool {
    if event.payload.is_completion_card() {
        return false;
    }
    let desired = clients.for_event(event);
    desired.is_none_or(|bot| bot.key != claimed.bot_key)
        || !super::super::session_notice::is_inline(&claimed.rendered_content)
}

pub(super) async fn reconcile_legacy<T: TaskCardTransport>(
    pool: Option<&PgPool>,
    clients: &CardDeliveryClients,
    transport: &T,
    claimed: &ClaimedCard,
) -> Result<CardEnsureOutcome, CardEnsureError> {
    let Some(bot) = clients.by_key(&claimed.bot_key) else {
        let error = "legacy task POST bot unavailable; preserved without fallback";
        store::mark_post_failure(pool, claimed, error)
            .await
            .map_err(CardEnsureError::Store)?;
        return Err(CardEnsureError::Transient(error.to_string()));
    };
    let attempt = store::begin_card_post(pool, claimed)
        .await
        .map_err(CardEnsureError::Store)?;
    let hash = content_hash(&claimed.rendered_content);
    match transport
        .reconcile_card_post(
            bot,
            claimed.scope.channel_id,
            &claimed.discord_nonce,
            &hash,
            attempt.started_at,
        )
        .await
    {
        CardPostReconcile::Found(message_id) => {
            store::mark_posted(pool, claimed, message_id, &claimed.rendered_content, &hash)
                .await
                .map_err(CardEnsureError::Store)?;
            Ok(CardEnsureOutcome {
                message_id,
                bot_key: claimed.bot_key.clone(),
                disposition: CardDisposition::Existing,
            })
        }
        CardPostReconcile::Ambiguous(error)
        | CardPostReconcile::Transient(error)
        | CardPostReconcile::Permanent(error) => {
            store::mark_card_post_ambiguous(pool, claimed, &error)
                .await
                .map_err(CardEnsureError::Store)?;
            Err(CardEnsureError::Ambiguous(format!(
                "legacy task POST requires its original receipt: {error}"
            )))
        }
    }
}
pub(super) async fn deliver_claim<T: TaskCardTransport>(
    pool: Option<&PgPool>,
    clients: &CardDeliveryClients,
    transport: &T,
    event: &TaskCardEvent,
    _intent: EnsureIntent,
    claimed: ClaimedCard,
    metadata: Option<&super::super::completion_footer_metadata::CompletionFooterMetadata>,
) -> Result<CardEnsureOutcome, CardEnsureError> {
    if matches!(claimed.action, store::ClaimAction::Post)
        && inline_delivery::needs_reconciliation(event, clients, &claimed)
    {
        return inline_delivery::reconcile_legacy(pool, clients, transport, &claimed).await;
    }
    let Some(bot) = clients.by_key(&claimed.bot_key) else {
        let error = format!("the card's pinned bot {} is unavailable", claimed.bot_key);
        match claimed.action {
            store::ClaimAction::Post => store::mark_post_failure(pool, &claimed, &error).await,
            store::ClaimAction::Edit { message_id } => {
                store::mark_edit_failure(pool, &claimed, message_id, &error).await
            }
        }
        .map_err(CardEnsureError::Store)?;
        return Err(CardEnsureError::Permanent(error));
    };
    let content = match &claimed.action {
        store::ClaimAction::Post if !claimed.rendered_content.is_empty() => {
            claimed.rendered_content.clone()
        }
        _ => metadata.map_or_else(
            || event.payload.render(claimed.update_count),
            |metadata| {
                event
                    .payload
                    .render_with_completion_metadata(claimed.update_count, metadata)
            },
        ),
    };
    let hash = content_hash(&content);

    match claimed.action {
        store::ClaimAction::Post => {
            let message_id =
                deliver_card_post_claim(pool, transport, bot, &claimed, &content, &hash).await?;
            Ok(CardEnsureOutcome {
                message_id,
                bot_key: claimed.bot_key,
                disposition: if claimed.revision > 1 && !claimed.new_terminal_completion {
                    CardDisposition::Replaced
                } else {
                    CardDisposition::Created
                },
            })
        }
        store::ClaimAction::Edit { message_id } => {
            match transport
                .edit_card(bot, event.scope.channel_id, message_id, &content)
                .await
            {
                Ok(()) => {
                    store::mark_edited(pool, &claimed, message_id, &content, &hash)
                        .await
                        .map_err(CardEnsureError::Store)?;
                    Ok(CardEnsureOutcome {
                        message_id,
                        bot_key: claimed.bot_key,
                        disposition: CardDisposition::Edited,
                    })
                }
                Err(TaskCardTransportError::ConfirmedMissing(_error)) => {
                    let replacement =
                        store::prepare_replacement(pool, &claimed, message_id, &content, &hash)
                            .await
                            .map_err(CardEnsureError::Store)?;
                    let replacement_id = deliver_card_post_claim(
                        pool,
                        transport,
                        bot,
                        &replacement,
                        &content,
                        &hash,
                    )
                    .await?;
                    Ok(CardEnsureOutcome {
                        message_id: replacement_id,
                        bot_key: replacement.bot_key,
                        disposition: CardDisposition::Replaced,
                    })
                }
                Err(error) => {
                    store::mark_edit_failure(pool, &claimed, message_id, &error.to_string())
                        .await
                        .map_err(CardEnsureError::Store)?;
                    Err(map_transport_error(error))
                }
            }
        }
    }
}
