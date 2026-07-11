use super::*;

fn watcher_send_error(
    class: crate::services::discord::replace_outcome_policy::WatcherSendFailureClass,
    message: impl std::fmt::Display,
) -> Error {
    crate::services::discord::replace_outcome_policy::watcher_send_failure_classified_message(
        class, message,
    )
    .into()
}

fn watcher_rollback_incomplete_error(message: impl std::fmt::Display) -> Error {
    watcher_send_error(
        crate::services::discord::replace_outcome_policy::WatcherSendFailureClass::RollbackIncomplete,
        message,
    )
}

#[derive(Debug, thiserror::Error)]
pub(in crate::services::discord) enum RequiredReferenceRollbackError {
    #[error("required task-card reference is no longer valid: {detail}")]
    UnknownReference { detail: String },
    #[error(transparent)]
    Other(#[from] Error),
}

impl RequiredReferenceRollbackError {
    pub(in crate::services::discord) fn as_error(&self) -> &(dyn std::error::Error + 'static) {
        match self {
            Self::UnknownReference { .. } => self,
            Self::Other(error) => error.as_ref(),
        }
    }
}

fn is_unknown_required_reference(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if matches!(
            error.downcast_ref::<super::super::http::RequiredReferenceSendError>(),
            Some(super::super::http::RequiredReferenceSendError::UnknownReference(_))
        ) {
            return true;
        }
        current = error.source();
    }
    false
}

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

/// Task responses must remain visibly attached to their durable completion
/// card. Unlike the general-purpose optional-reference sender, a rejected
/// reference is a delivery failure and never degrades to an unthreaded POST.
pub(in crate::services::discord) async fn send_long_message_raw_with_required_reference_rollback(
    http: &serenity::Http,
    channel_id: ChannelId,
    rollback_anchor_msg_id: MessageId,
    text: &str,
    shared: &Arc<SharedData>,
    reference: (ChannelId, MessageId),
    response_turn_key: &str,
) -> Result<Vec<MessageId>, RequiredReferenceRollbackError> {
    let result = send_long_message_raw_with_reference_rollback_policy(
        http,
        channel_id,
        rollback_anchor_msg_id,
        text,
        shared,
        Some(reference),
        true,
        Some(response_turn_key),
    )
    .await;
    match result {
        Ok(message_ids) => Ok(message_ids),
        Err(error) if is_unknown_required_reference(error.as_ref()) => {
            Err(RequiredReferenceRollbackError::UnknownReference {
                detail: error.to_string(),
            })
        }
        Err(error) => Err(RequiredReferenceRollbackError::Other(error)),
    }
}

pub(in crate::services::discord) async fn send_long_message_raw_with_reference_rollback(
    http: &serenity::Http,
    channel_id: ChannelId,
    rollback_anchor_msg_id: MessageId,
    text: &str,
    shared: &Arc<SharedData>,
    reference: Option<(ChannelId, MessageId)>,
) -> Result<Vec<MessageId>, Error> {
    send_long_message_raw_with_reference_rollback_policy(
        http,
        channel_id,
        rollback_anchor_msg_id,
        text,
        shared,
        reference,
        false,
        None,
    )
    .await
}

async fn send_long_message_raw_with_reference_rollback_policy(
    http: &serenity::Http,
    channel_id: ChannelId,
    rollback_anchor_msg_id: MessageId,
    text: &str,
    shared: &Arc<SharedData>,
    reference: Option<(ChannelId, MessageId)>,
    require_reference: bool,
    response_turn_key: Option<&str>,
) -> Result<Vec<MessageId>, Error> {
    let payload_byte_len = text.len();
    let chunks = split_message(text);
    let total = chunks.len();
    let rollback_key = response_turn_key.map_or_else(
        || replace_continuation_rollback_key(channel_id, rollback_anchor_msg_id),
        |turn_key| {
            task_response_continuation_rollback_key(channel_id, rollback_anchor_msg_id, turn_key)
        },
    );

    match claim_replace_continuation_rollback(&rollback_key) {
        ReplaceContinuationRollbackClaim::None => {}
        ReplaceContinuationRollbackClaim::InProgress(pending_ids) => {
            return Err(watcher_rollback_incomplete_error(format!(
                "previous chunk cleanup in progress for anchor {} in channel {}: {:?}",
                rollback_anchor_msg_id.get(),
                channel_id.get(),
                pending_ids
            )));
        }
        ReplaceContinuationRollbackClaim::Owner(pending_ids) => {
            let cleanup =
                cleanup_replace_continuations_after_failure(http, channel_id, &pending_ids, shared)
                    .await;
            if cleanup.failed_message_ids.is_empty() {
                if let Err(error) = clear_replace_continuation_rollback(&rollback_key) {
                    unclaim_replace_continuation_rollback(&rollback_key);
                    return Err(watcher_send_error(
                        crate::services::discord::replace_outcome_policy::WatcherSendFailureClass::Transient,
                        format!(
                        "previous chunk rollback state was not cleared for anchor {} in channel {}: {error}",
                        rollback_anchor_msg_id.get(),
                        channel_id.get()
                        ),
                    ));
                }
            } else {
                if let Err(error) = record_replace_continuation_rollback(
                    &rollback_key,
                    cleanup.failed_message_ids.clone(),
                ) {
                    record_replace_continuation_rollback_memory_only(
                        &rollback_key,
                        cleanup.failed_message_ids,
                    );
                    return Err(watcher_rollback_incomplete_error(format!(
                        "previous chunk cleanup incomplete and rollback state was not durable for anchor {} in channel {}: {error}",
                        rollback_anchor_msg_id.get(),
                        channel_id.get()
                    )));
                }
                return Err(watcher_rollback_incomplete_error(format!(
                    "previous chunk cleanup incomplete for anchor {} in channel {}: {:?}",
                    rollback_anchor_msg_id.get(),
                    channel_id.get(),
                    cleanup.errors
                )));
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
        let chunk_nonce = response_turn_key.map(|turn_key| {
            super::super::task_notification_delivery::response_chunk_nonce(turn_key, i)
        });
        match send_rollback_channel_message(
            http,
            channel_id,
            chunk,
            chunk_reference,
            require_reference && i == 0,
            chunk_nonce.as_deref(),
        )
        .await
        {
            Ok(message_id) => {
                // #3082 P1-2: chunk landed — refresh the answer-flush barrier's
                // inactivity window so a long rollback-tracked answer never
                // trips the queued-card wait while still progressing.
                shared.answer_flush_barrier.note_progress(channel_id);
                shared
                    .tmux_relay_coord(channel_id)
                    .note_relay_progress_heartbeat(chrono::Utc::now().timestamp_millis());
                sent_message_ids.push(message_id.get());
                if let Err(error) =
                    record_replace_continuation_rollback(&rollback_key, sent_message_ids.clone())
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
                    let rollback_cleanup_complete = cleanup.failed_message_ids.is_empty();
                    if rollback_cleanup_complete {
                        if let Err(clear_error) = clear_replace_continuation_rollback(&rollback_key)
                        {
                            errors.push(clear_error);
                        }
                    } else if let Err(record_error) = record_replace_continuation_rollback(
                        &rollback_key,
                        cleanup.failed_message_ids.clone(),
                    ) {
                        record_replace_continuation_rollback_memory_only(
                            &rollback_key,
                            cleanup.failed_message_ids,
                        );
                        errors.push(record_error);
                    }
                    let class = if rollback_cleanup_complete {
                        crate::services::discord::replace_outcome_policy::WatcherSendFailureClass::Transient
                    } else {
                        crate::services::discord::replace_outcome_policy::WatcherSendFailureClass::RollbackIncomplete
                    };
                    return Err(watcher_send_error(
                        class,
                        format!(
                            "sent chunk but rollback state was not durable for anchor {} in channel {}: {}",
                            rollback_anchor_msg_id.get(),
                            channel_id.get(),
                            errors.join("; ")
                        ),
                    ));
                }
            }
            Err(err) => {
                if require_reference && i == 0 && is_unknown_required_reference(err.as_ref()) {
                    clear_replace_continuation_rollback(&rollback_key).map_err(|clear_error| {
                        watcher_send_error(
                            crate::services::discord::replace_outcome_policy::WatcherSendFailureClass::Transient,
                            format!(
                                "required reference was rejected and empty rollback state was not cleared: {clear_error}"
                            ),
                        )
                    })?;
                    return Err(err);
                }
                let failure_class =
                    crate::services::discord::replace_outcome_policy::classify_watcher_send_failure(
                        err.as_ref(),
                    );
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
                    if let Err(clear_error) = clear_replace_continuation_rollback(&rollback_key) {
                        unclaim_replace_continuation_rollback(&rollback_key);
                        return Err(watcher_send_error(
                            failure_class,
                            format!(
                                "send chunk {i}/{total} failed for anchor {} in channel {}, and rollback state was not cleared: {error}; {clear_error}",
                                rollback_anchor_msg_id.get(),
                                channel_id.get()
                            ),
                        ));
                    }
                } else if let Err(record_error) = record_replace_continuation_rollback(
                    &rollback_key,
                    cleanup.failed_message_ids.clone(),
                ) {
                    record_replace_continuation_rollback_memory_only(
                        &rollback_key,
                        cleanup.failed_message_ids,
                    );
                    return Err(watcher_rollback_incomplete_error(format!(
                        "send chunk {i}/{total} failed for anchor {} in channel {}, cleanup incomplete and rollback state was not durable: {error}; {record_error}",
                        rollback_anchor_msg_id.get(),
                        channel_id.get()
                    )));
                }
                return Err(watcher_send_error(
                    failure_class,
                    format!(
                        "send chunk {i}/{total} failed for anchor {} in channel {}; sent chunks cleaned before retry: {error}",
                        rollback_anchor_msg_id.get(),
                        channel_id.get()
                    ),
                ));
            }
        }
    }

    if let Err(error) = clear_replace_continuation_rollback(&rollback_key) {
        clear_replace_continuation_rollback_memory_only(&rollback_key);
        tracing::warn!(
            target: "discord::chunker",
            path = "send_long_message_raw_with_rollback",
            channel_id = channel_id.get(),
            anchor_message_id = rollback_anchor_msg_id.get(),
            error = %error,
            "discord rollback send delivered all chunks but rollback state cleanup failed"
        )
    }

    Ok(sent_message_ids.into_iter().map(MessageId::new).collect())
}

async fn send_rollback_channel_message(
    http: &serenity::Http,
    channel_id: ChannelId,
    content: &str,
    reference: Option<(ChannelId, MessageId)>,
    require_reference: bool,
    nonce: Option<&str>,
) -> Result<MessageId, Error> {
    #[cfg(test)]
    if let Some(result) = super::rollback_transport_test_hook::send(
        channel_id,
        content,
        reference,
        nonce,
        nonce.is_some(),
    ) {
        return result;
    }

    match (reference, require_reference, nonce) {
        (Some((reference_channel_id, reference_message_id)), true, Some(nonce)) => {
            super::super::http::send_channel_message_with_required_reference_and_nonce(
                http,
                channel_id,
                content,
                reference_channel_id,
                reference_message_id,
                nonce,
            )
            .await
            .map(|message| message.id)
            .map_err(Into::into)
        }
        (Some((reference_channel_id, reference_message_id)), true, None) => {
            super::super::http::send_channel_message_with_required_reference(
                http,
                channel_id,
                content,
                reference_channel_id,
                reference_message_id,
            )
            .await
            .map(|message| message.id)
            .map_err(Into::into)
        }
        (Some((reference_channel_id, reference_message_id)), false, Some(nonce)) => {
            super::super::http::send_channel_message_with_reference_and_nonce(
                http,
                channel_id,
                content,
                reference_channel_id,
                reference_message_id,
                nonce,
            )
            .await
            .map(|message| message.id)
            .map_err(Into::into)
        }
        (Some((reference_channel_id, reference_message_id)), false, None) => {
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
        (None, _, Some(nonce)) => {
            super::super::http::send_channel_message_with_nonce(http, channel_id, content, nonce)
                .await
                .map(|message| message.id)
                .map_err(Into::into)
        }
        (None, _, None) => super::super::http::send_channel_message(http, channel_id, content)
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
