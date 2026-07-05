use super::*;

pub(in crate::services::discord) async fn send_long_message_raw_with_rollback(
    http: &serenity::Http,
    channel_id: ChannelId,
    rollback_anchor_msg_id: MessageId,
    text: &str,
    shared: &Arc<SharedData>,
) -> Result<Vec<MessageId>, Error> {
    send_long_message_raw_with_reference_rollback(
        http,
        channel_id,
        rollback_anchor_msg_id,
        text,
        shared,
        None,
    )
    .await
}

pub(in crate::services::discord) async fn send_long_message_raw_with_reference_rollback(
    http: &serenity::Http,
    channel_id: ChannelId,
    rollback_anchor_msg_id: MessageId,
    text: &str,
    shared: &Arc<SharedData>,
    reference: Option<(ChannelId, MessageId)>,
) -> Result<Vec<MessageId>, Error> {
    let payload_byte_len = text.len();
    let chunks = split_message(text);
    let total = chunks.len();
    let rollback_key = replace_continuation_rollback_key(channel_id, rollback_anchor_msg_id);

    match claim_replace_continuation_rollback(rollback_key) {
        ReplaceContinuationRollbackClaim::None => {}
        ReplaceContinuationRollbackClaim::InProgress(pending_ids) => {
            return Err(format!(
                "previous chunk cleanup in progress for anchor {} in channel {}: {:?}",
                rollback_anchor_msg_id.get(),
                channel_id.get(),
                pending_ids
            )
            .into());
        }
        ReplaceContinuationRollbackClaim::Owner(pending_ids) => {
            let cleanup =
                cleanup_replace_continuations_after_failure(http, channel_id, &pending_ids, shared)
                    .await;
            if cleanup.failed_message_ids.is_empty() {
                if let Err(error) = clear_replace_continuation_rollback(rollback_key) {
                    unclaim_replace_continuation_rollback(rollback_key);
                    return Err(format!(
                        "previous chunk rollback state was not cleared for anchor {} in channel {}: {error}",
                        rollback_anchor_msg_id.get(),
                        channel_id.get()
                    )
                    .into());
                }
            } else {
                if let Err(error) = record_replace_continuation_rollback(
                    rollback_key,
                    cleanup.failed_message_ids.clone(),
                ) {
                    record_replace_continuation_rollback_memory_only(
                        rollback_key,
                        cleanup.failed_message_ids,
                    );
                    return Err(format!(
                        "previous chunk cleanup incomplete and rollback state was not durable for anchor {} in channel {}: {error}",
                        rollback_anchor_msg_id.get(),
                        channel_id.get()
                    )
                    .into());
                }
                return Err(format!(
                    "previous chunk cleanup incomplete for anchor {} in channel {}: {:?}",
                    rollback_anchor_msg_id.get(),
                    channel_id.get(),
                    cleanup.errors
                )
                .into());
            }
        }
    }

    if total == 0 {
        return Ok(Vec::new());
    }

    // #3082 part B: same answer-flush barrier as `send_long_message_raw` so a
    // queued-turn notice POST cannot interleave between this turn's final-answer
    // chunks. RAII guard clears the gate on every exit path.
    let _answer_flush_guard =
        (total > 1).then(|| shared.answer_flush_barrier.begin_flush(channel_id));

    tracing::debug!(
        target: "discord::chunker",
        path = "send_long_message_raw_with_rollback",
        channel_id = channel_id.get(),
        anchor_message_id = rollback_anchor_msg_id.get(),
        payload_byte_len,
        total_chunks = total,
        "discord rollback send begin"
    );

    let mut sent_message_ids = Vec::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let is_last = i + 1 == total;
        tracing::debug!(
            target: "discord::chunker",
            path = "send_long_message_raw_with_rollback",
            channel_id = channel_id.get(),
            anchor_message_id = rollback_anchor_msg_id.get(),
            chunk_index = i,
            byte_len = chunk.len(),
            total_chunks = total,
            is_last_chunk = is_last,
            "discord rollback send chunk"
        );
        rate_limit_wait(shared, channel_id).await;
        let chunk_reference = if i == 0 { reference.clone() } else { None };
        match send_rollback_channel_message(http, channel_id, chunk, chunk_reference).await {
            Ok(message_id) => {
                // #3082 P1-2: chunk landed — refresh the answer-flush barrier's
                // inactivity window so a long rollback-tracked answer never
                // trips the queued-card wait while still progressing.
                shared.answer_flush_barrier.note_progress(channel_id);
                sent_message_ids.push(message_id.get());
                if let Err(error) =
                    record_replace_continuation_rollback(rollback_key, sent_message_ids.clone())
                {
                    let cleanup = cleanup_replace_continuations_after_failure(
                        http,
                        channel_id,
                        &sent_message_ids,
                        shared,
                    )
                    .await;
                    let mut errors = cleanup.errors;
                    errors.push(error.clone());
                    if cleanup.failed_message_ids.is_empty() {
                        if let Err(clear_error) = clear_replace_continuation_rollback(rollback_key)
                        {
                            errors.push(clear_error);
                        }
                    } else if let Err(record_error) = record_replace_continuation_rollback(
                        rollback_key,
                        cleanup.failed_message_ids.clone(),
                    ) {
                        record_replace_continuation_rollback_memory_only(
                            rollback_key,
                            cleanup.failed_message_ids,
                        );
                        errors.push(record_error);
                    }
                    return Err(format!(
                        "sent chunk but rollback state was not durable for anchor {} in channel {}: {}",
                        rollback_anchor_msg_id.get(),
                        channel_id.get(),
                        errors.join("; ")
                    )
                    .into());
                }
            }
            Err(err) => {
                let error = err.to_string();
                tracing::warn!(
                    target: "discord::chunker",
                    path = "send_long_message_raw_with_rollback",
                    channel_id = channel_id.get(),
                    anchor_message_id = rollback_anchor_msg_id.get(),
                    chunk_index = i,
                    total_chunks = total,
                    last_chunk = is_last,
                    outcome = "err",
                    error = %error,
                    "discord rollback send failed; deleting sent chunks before retry"
                );
                let cleanup = cleanup_replace_continuations_after_failure(
                    http,
                    channel_id,
                    &sent_message_ids,
                    shared,
                )
                .await;
                if cleanup.failed_message_ids.is_empty() {
                    if let Err(clear_error) = clear_replace_continuation_rollback(rollback_key) {
                        unclaim_replace_continuation_rollback(rollback_key);
                        return Err(format!(
                            "send chunk {i}/{total} failed for anchor {} in channel {}, and rollback state was not cleared: {error}; {clear_error}",
                            rollback_anchor_msg_id.get(),
                            channel_id.get()
                        )
                        .into());
                    }
                } else if let Err(record_error) = record_replace_continuation_rollback(
                    rollback_key,
                    cleanup.failed_message_ids.clone(),
                ) {
                    record_replace_continuation_rollback_memory_only(
                        rollback_key,
                        cleanup.failed_message_ids,
                    );
                    return Err(format!(
                        "send chunk {i}/{total} failed for anchor {} in channel {}, cleanup incomplete and rollback state was not durable: {error}; {record_error}",
                        rollback_anchor_msg_id.get(),
                        channel_id.get()
                    )
                    .into());
                }
                return Err(format!(
                    "send chunk {i}/{total} failed for anchor {} in channel {}; sent chunks cleaned before retry: {error}",
                    rollback_anchor_msg_id.get(),
                    channel_id.get()
                )
                .into());
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    if let Err(error) = clear_replace_continuation_rollback(rollback_key) {
        return Err(format!(
            "delivered chunks for anchor {} in channel {}, but rollback state was not cleared: {error}",
            rollback_anchor_msg_id.get(),
            channel_id.get()
        )
        .into());
    }

    Ok(sent_message_ids.into_iter().map(MessageId::new).collect())
}

async fn send_rollback_channel_message(
    http: &serenity::Http,
    channel_id: ChannelId,
    content: &str,
    reference: Option<(ChannelId, MessageId)>,
) -> Result<MessageId, Error> {
    #[cfg(test)]
    if let Some(result) = super::rollback_transport_test_hook::send(channel_id, content, reference)
    {
        return result;
    }

    match reference {
        Some((reference_channel_id, reference_message_id)) => {
            send_channel_message_with_optional_reference(
                http,
                channel_id,
                content,
                Some((reference_channel_id, reference_message_id)),
            )
            .await
            .map(|message| message.id)
            .map_err(Into::into)
        }
        None => super::super::http::send_channel_message(http, channel_id, content)
            .await
            .map(|message| message.id)
            .map_err(Into::into),
    }
}

pub(super) async fn delete_rollback_channel_message(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Result<(), Error> {
    #[cfg(test)]
    if let Some(result) = super::rollback_transport_test_hook::delete(channel_id, message_id) {
        return result;
    }

    super::super::http::delete_channel_message(http, channel_id, message_id)
        .await
        .map_err(Into::into)
}
