//! Retire completion metadata after a task card has a durable, known receipt.

use super::*;

pub(super) fn without_metadata(
    content: &str,
    payload: &TaskCardPayload,
    update_count: u64,
) -> Option<String> {
    if !matches!(payload, TaskCardPayload::Task(_)) {
        return None;
    }
    let canonical = payload.render(update_count);
    if content == canonical {
        return None;
    }
    let (body, suffix) = content.rsplit_once("\n\n")?;
    let plain = suffix
        .lines()
        .map(|line| line.strip_prefix("-# "))
        .collect::<Option<Vec<_>>>()?
        .join("\n");
    let separated = format!("\n\u{2063}\n{plain}");
    let metadata = super::super::completion_footer_metadata::completion_footer_metadata_from_block(
        Some(&separated),
    );
    if metadata.subtext_lines().join("\n") != suffix {
        return None;
    }
    // Only canonical body equality proves this separate block was appended metadata.
    // An opaque old task body may own a structured usage/footer with identical spelling.
    (body == canonical.trim_end()).then(|| body.to_string())
}

async fn clean_known_card<T: TaskCardTransport>(
    pool: Option<&PgPool>,
    clients: &CardDeliveryClients,
    transport: &T,
    event: &TaskCardEvent,
    outcome: &CardEnsureOutcome,
) {
    let result = async {
        let Some((claim, content)) = store::claim_retired_metadata(
            pool,
            &event.scope,
            &event.payload,
            &outcome.bot_key,
            outcome.message_id,
        )
        .await?
        else {
            return Ok::<(), String>(());
        };
        let Some(bot) = clients.by_key(&claim.bot_key) else {
            return store::mark_edit_failure(
                pool,
                &claim,
                outcome.message_id,
                "pinned metadata-edit bot unavailable",
            )
            .await;
        };
        match transport
            .edit_card(bot, event.scope.channel_id, outcome.message_id, &content)
            .await
        {
            Ok(()) => {
                store::mark_edited(
                    pool,
                    &claim,
                    outcome.message_id,
                    &content,
                    &content_hash(&content),
                )
                .await
            }
            Err(error) => {
                store::mark_edit_failure(pool, &claim, outcome.message_id, &error.to_string()).await
            }
        }
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(channel_id = event.scope.channel_id, %error, "task-card metadata retirement deferred");
    }
}

pub(super) async fn ensure_card_with_metadata<T: TaskCardTransport>(
    pool: Option<&PgPool>,
    clients: &CardDeliveryClients,
    transport: &T,
    event: &TaskCardEvent,
    intent: EnsureIntent,
    metadata: Option<&super::super::completion_footer_metadata::CompletionFooterMetadata>,
) -> Result<CardEnsureOutcome, CardEnsureError> {
    let outcome =
        super::ensure_card_with_metadata_inner(pool, clients, transport, event, intent, metadata)
            .await?;
    if metadata.is_none() {
        clean_known_card(pool, clients, transport, event, &outcome).await;
    }
    Ok(outcome)
}

pub(in crate::services::discord) async fn replace_confirmed_missing_card<T: TaskCardTransport>(
    pool: Option<&PgPool>,
    clients: &CardDeliveryClients,
    transport: &T,
    event: &TaskCardEvent,
    missing_message_id: u64,
) -> Result<CardEnsureOutcome, CardEnsureError> {
    let restored = store::restore_completion_event(pool, event)
        .await
        .map_err(CardEnsureError::Store)?;
    let event = &restored;
    let outcome = super::replace_confirmed_missing_card_inner(
        pool,
        clients,
        transport,
        event,
        missing_message_id,
    )
    .await?;
    clean_known_card(pool, clients, transport, event, &outcome).await;
    Ok(outcome)
}

#[cfg(test)]
#[path = "metadata_retirement_replay_tests.rs"]
mod tests;
