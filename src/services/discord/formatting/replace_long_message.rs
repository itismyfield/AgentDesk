use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use super::*;
use crate::services::discord::SharedData;

/// Replace an existing Discord message with the first chunk, then send the remaining chunks.
pub(in crate::services::discord) async fn replace_long_message_raw(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: MessageId,
    text: &str,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    replace_long_message_outcome_to_result(
        // #3805 P1: this wrapper discards the last-chunk anchor (footer re-anchor
        // is watcher-only); pass a throwaway sink.
        replace_long_message_raw_with_outcome(
            http, channel_id, message_id, text, shared, &mut None,
        )
        .await?,
    )
}

pub(in crate::services::discord) fn replace_long_message_outcome_to_result(
    outcome: ReplaceLongMessageOutcome,
) -> Result<(), Error> {
    match outcome {
        ReplaceLongMessageOutcome::EditedOriginal => Ok(()),
        ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { .. } => Ok(()),
        ReplaceLongMessageOutcome::PartialContinuationFailure { error, .. } => Err(error.into()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum DeferredReplaceLongMessageOutcome {
    Edited(ReplaceLongMessageOutcome),
    EditFailed { edit_error: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum ReplaceLongMessageOutcome {
    EditedOriginal,
    SentFallbackAfterEditFailure {
        edit_error: String,
        /// First fallback fresh-send message; recovery records it after a stale-anchor edit miss.
        replacement_anchor: Option<MessageId>,
    },
    PartialContinuationFailure {
        sent_chunks: usize,
        total_chunks: usize,
        failed_chunk_index: usize,
        sent_continuation_message_ids: Vec<u64>,
        cleanup_errors: Vec<String>,
        error: String,
    },
}

/// #3805 P1: the LAST continuation chunk produced by a fully-successful
/// multi-chunk `replace_long_message_raw_with_outcome` (`EditedOriginal` where
/// the body split into 2+ chunks). Carries BOTH the tail chunk's message id
/// (the highest snowflake — #3717 latest-wins) AND its exact text so a caller
/// that appends a completion footer can re-anchor onto the tail chunk instead of
/// stranding the footer on the edited chunk 0. The text MUST be the tail chunk's
/// OWN text: the footer edit rewrites the target message with `strip(text) +
/// footer`, so registering the full body would clobber the tail chunk with the
/// entire answer. Reported via a `&mut Option` out-param (the enum stays a unit
/// variant — ~20 `matches!` commit/delivered sites depend on that) and left
/// `None` for single-chunk answers, where chunk 0 already IS the tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct ReplaceLastChunkAnchor {
    pub(in crate::services::discord) msg_id: u64,
    pub(in crate::services::discord) text: String,
}

/// #3805 P1: pick the completion-footer terminal target (message id + text) for
/// the watcher's in-place edit arm (`replace_long_message_raw_with_outcome`
/// `EditedOriginal`). When the answer split into multiple chunks the tail
/// continuation is the durable anchor (highest snowflake — #3717 latest-wins),
/// and its edit text MUST be the tail chunk's OWN text: the completion edit
/// rewrites the target with `strip(text) + footer`, so passing the full body
/// would overwrite the tail chunk with the entire answer (§4 regression). For a
/// single-chunk answer there is no continuation anchor, so the edited chunk-0 id
/// plus the full relay text are the target (identical there — no regression).
pub(in crate::services::discord) fn watcher_completion_footer_anchor<'a>(
    last_chunk_anchor: Option<&'a ReplaceLastChunkAnchor>,
    edited_chunk0_msg_id: MessageId,
    full_relay_text: &'a str,
) -> (MessageId, &'a str) {
    match last_chunk_anchor {
        Some(anchor) => (MessageId::new(anchor.msg_id), anchor.text.as_str()),
        None => (edited_chunk0_msg_id, full_relay_text),
    }
}

/// Replace an existing Discord message and report whether the original
/// placeholder was actually edited. If the edit fails but the fallback send
/// succeeds, wrapper callers treat delivery as committed, while callers that
/// own placeholder lifecycle can still use this outcomeful variant to delete
/// or terminal-edit the stale original.
pub(in crate::services::discord) async fn replace_long_message_raw_with_outcome(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: MessageId,
    text: &str,
    shared: &Arc<SharedData>,
    last_chunk_anchor: &mut Option<ReplaceLastChunkAnchor>,
) -> Result<ReplaceLongMessageOutcome, Error> {
    match replace_long_message_raw_deferred(
        http,
        channel_id,
        message_id,
        text,
        shared,
        last_chunk_anchor,
    )
    .await?
    {
        DeferredReplaceLongMessageOutcome::Edited(outcome) => Ok(outcome),
        DeferredReplaceLongMessageOutcome::EditFailed { edit_error } => {
            let replacement_message_ids =
                send_long_message_raw_with_rollback(http, channel_id, message_id, text, shared)
                    .await?;
            Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                edit_error,
                replacement_anchor: replacement_message_ids.first().copied(),
            })
        }
    }
}

pub(in crate::services::discord) async fn replace_long_message_raw_deferred(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: MessageId,
    text: &str,
    shared: &Arc<SharedData>,
    // #3805 P1: on the fully-successful multi-chunk edit path this is set to the
    // tail continuation chunk (id + text) so a footer-appending caller can
    // re-anchor onto it; left untouched (caller-initialised `None`) on every
    // other path (single-chunk, edit-failure fallback, partial failure).
    last_chunk_anchor: &mut Option<ReplaceLastChunkAnchor>,
) -> Result<DeferredReplaceLongMessageOutcome, Error> {
    let payload_byte_len = text.len();
    let chunks = split_message(text);
    let total = chunks.len();
    let Some(first_chunk) = chunks.first() else {
        tracing::debug!(
            target: "discord::chunker",
            path = "replace_long_message_raw",
            channel_id = channel_id.get(),
            payload_byte_len,
            total_chunks = 0usize,
            "discord replace: no chunks"
        );
        return Ok(DeferredReplaceLongMessageOutcome::Edited(
            ReplaceLongMessageOutcome::EditedOriginal,
        ));
    };
    let rollback_key = replace_continuation_rollback_key(channel_id, message_id);
    match claim_replace_continuation_rollback(&rollback_key) {
        ReplaceContinuationRollbackClaim::None => {}
        ReplaceContinuationRollbackClaim::InProgress(pending_ids) => {
            return Ok(DeferredReplaceLongMessageOutcome::Edited(
                ReplaceLongMessageOutcome::PartialContinuationFailure {
                    sent_chunks: 0,
                    total_chunks: total,
                    failed_chunk_index: 0,
                    sent_continuation_message_ids: pending_ids,
                    cleanup_errors: Vec::new(),
                    error: watcher_send_failure_message(
                        crate::services::discord::replace_outcome_policy::WatcherSendFailureClass::RollbackIncomplete,
                        "previous continuation cleanup in progress",
                    ),
                },
            ));
        }
        ReplaceContinuationRollbackClaim::Owner(pending_ids) => {
            let cleanup =
                cleanup_replace_continuations_after_failure(http, channel_id, &pending_ids, shared)
                    .await;
            if cleanup.failed_message_ids.is_empty() {
                if let Err(error) = clear_replace_continuation_rollback(&rollback_key) {
                    unclaim_replace_continuation_rollback(&rollback_key);
                    let mut cleanup_errors = cleanup.errors;
                    cleanup_errors.push(error.clone());
                    return Ok(DeferredReplaceLongMessageOutcome::Edited(
                        ReplaceLongMessageOutcome::PartialContinuationFailure {
                            sent_chunks: 0,
                            total_chunks: total,
                            failed_chunk_index: 0,
                            sent_continuation_message_ids: pending_ids,
                            cleanup_errors,
                            error: watcher_send_failure_message(
                                crate::services::discord::replace_outcome_policy::WatcherSendFailureClass::Transient,
                                format!(
                                    "previous continuation rollback state was not cleared: {error}"
                                ),
                            ),
                        },
                    ));
                }
            } else {
                let mut cleanup_errors = cleanup.errors;
                if let Err(error) = record_replace_continuation_rollback(
                    &rollback_key,
                    cleanup.failed_message_ids.clone(),
                ) {
                    record_replace_continuation_rollback_memory_only(
                        &rollback_key,
                        cleanup.failed_message_ids.clone(),
                    );
                    cleanup_errors.push(error.clone());
                }
                return Ok(DeferredReplaceLongMessageOutcome::Edited(
                ReplaceLongMessageOutcome::PartialContinuationFailure {
                    sent_chunks: 0,
                    total_chunks: total,
                    failed_chunk_index: 0,
                    sent_continuation_message_ids: pending_ids,
                    cleanup_errors,
                    error: watcher_send_failure_message(
                        crate::services::discord::replace_outcome_policy::WatcherSendFailureClass::RollbackIncomplete,
                        "previous continuation cleanup incomplete",
                    ),
                }));
            }
        }
    }

    // #3082 part B (codex P1-1): the edit/replace path is ALSO a multi-chunk
    // send (chunk 0 edited, continuations sent). Hold the same per-channel
    // answer-flush barrier across the whole edit+continuation send so a
    // queued-turn "📬" notice POST cannot interleave between this answer's
    // chunks. Acquired BEFORE the first edit await and held (RAII) across every
    // continuation send and every cleanup/error return below — the guard clears
    // the gate on every exit path (Ok, early `return`, `?`, panic-unwind). The
    // fallback `send_long_message_raw_with_rollback` acquires its own guard, so
    // we intentionally do NOT also hold one there (no double-count needed).
    let _answer_flush_guard =
        (total > 1).then(|| shared.answer_flush_barrier.begin_flush(channel_id));

    tracing::debug!(
        target: "discord::chunker",
        path = "replace_long_message_raw",
        channel_id = channel_id.get(),
        message_id = message_id.get(),
        payload_byte_len,
        chunk_index = 0usize,
        byte_len = first_chunk.len(),
        total_chunks = total,
        is_last_chunk = total == 1,
        "discord edit first chunk"
    );
    rate_limit_wait(shared, channel_id).await;
    let edit_result = crate::services::discord::http::edit_channel_message(
        http,
        channel_id,
        message_id,
        first_chunk,
    )
    .await;

    if let Err(e) = edit_result {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ⚠ replace_long_message_raw edit failed for channel {} msg {}: {e}",
            channel_id.get(),
            message_id.get()
        );
        tracing::warn!(
            target: "discord::chunker",
            path = "replace_long_message_raw",
            channel_id = channel_id.get(),
            message_id = message_id.get(),
            payload_byte_len,
            chunk_index = 0usize,
            total_chunks = total,
            outcome = "edit_failed_falling_back_to_send",
            error = %e,
            "discord first-chunk edit failed; deferring fallback-send authority to caller"
        );
        return Ok(DeferredReplaceLongMessageOutcome::EditFailed {
            edit_error: e.to_string(),
        });
    }

    // #3082 P1-2 residual: the FIRST edited chunk also delivers answer payload
    // while the multi-chunk barrier guard is held. Mirror the continuation loop
    // (and the two send loops) by bumping the answer-flush barrier's inactivity
    // window here too, so a queued-card waiter's inactivity grace cannot
    // spuriously expire between this first edit and the first continuation send.
    // Only on the multi-chunk path (guard active) — single-chunk edits hold no
    // guard and have no continuation to race against.
    if total > 1 {
        shared.answer_flush_barrier.note_progress(channel_id);
    }

    if total == 1 {
        tracing::debug!(
            target: "discord::chunker",
            path = "replace_long_message_raw",
            channel_id = channel_id.get(),
            message_id = message_id.get(),
            payload_byte_len,
            chunk_index = 0usize,
            total_chunks = total,
            last_chunk = true,
            outcome = "ok",
            "discord edit single-chunk ok"
        );
    }

    let mut sent_continuation_message_ids = Vec::new();
    for (offset, chunk) in chunks.iter().skip(1).enumerate() {
        let i = offset + 1;
        let is_last = i + 1 == total;
        tracing::debug!(
            target: "discord::chunker",
            path = "replace_long_message_raw",
            channel_id = channel_id.get(),
            chunk_index = i,
            byte_len = chunk.len(),
            total_chunks = total,
            is_last_chunk = is_last,
            "discord send continuation chunk"
        );
        rate_limit_wait(shared, channel_id).await;
        match crate::services::discord::http::send_channel_message(http, channel_id, chunk).await {
            Ok(message) => {
                // #3082 P1-2: this chunk landed — reset the answer-flush
                // barrier's inactivity window so a long edit/replace answer
                // that keeps making progress never trips the queued-card wait.
                shared.answer_flush_barrier.note_progress(channel_id);
                sent_continuation_message_ids.push(message.id.get());
                if let Err(error) = record_replace_continuation_rollback(
                    &rollback_key,
                    sent_continuation_message_ids.clone(),
                ) {
                    tracing::warn!(
                        target: "discord::chunker",
                        path = "replace_long_message_raw",
                        channel_id = channel_id.get(),
                        chunk_index = i,
                        total_chunks = total,
                        error = %error,
                        "discord replace continuation sent but rollback state was not durable; deleting sent continuations before retry"
                    );
                    let cleanup_errors = cleanup_replace_continuations_after_failure(
                        http,
                        channel_id,
                        &sent_continuation_message_ids,
                        shared,
                    )
                    .await;
                    let mut errors = cleanup_errors.errors;
                    errors.push(error.clone());
                    if cleanup_errors.failed_message_ids.is_empty() {
                        if let Err(clear_error) = clear_replace_continuation_rollback(&rollback_key)
                        {
                            errors.push(clear_error);
                        }
                    } else if let Err(record_error) = record_replace_continuation_rollback(
                        &rollback_key,
                        cleanup_errors.failed_message_ids.clone(),
                    ) {
                        record_replace_continuation_rollback_memory_only(
                            &rollback_key,
                            cleanup_errors.failed_message_ids.clone(),
                        );
                        errors.push(record_error);
                    }
                    let class = if cleanup_errors.failed_message_ids.is_empty() {
                        crate::services::discord::replace_outcome_policy::WatcherSendFailureClass::Transient
                    } else {
                        crate::services::discord::replace_outcome_policy::WatcherSendFailureClass::RollbackIncomplete
                    };
                    return Ok(DeferredReplaceLongMessageOutcome::Edited(
                        ReplaceLongMessageOutcome::PartialContinuationFailure {
                            sent_chunks: i + 1,
                            total_chunks: total,
                            failed_chunk_index: i,
                            sent_continuation_message_ids: sent_continuation_message_ids.clone(),
                            cleanup_errors: errors,
                            error: watcher_send_failure_message(class, error),
                        },
                    ));
                }
                if is_last {
                    tracing::debug!(
                        target: "discord::chunker",
                        path = "replace_long_message_raw",
                        channel_id = channel_id.get(),
                        chunk_index = i,
                        total_chunks = total,
                        last_chunk = true,
                        outcome = "ok",
                        "discord replace last chunk ok"
                    );
                }
            }
            Err(err) => {
                let failure_class =
                    crate::services::discord::replace_outcome_policy::classify_watcher_send_failure(
                        &err,
                    );
                let error = err.to_string();
                tracing::warn!(
                    target: "discord::chunker",
                    path = "replace_long_message_raw",
                    channel_id = channel_id.get(),
                    chunk_index = i,
                    total_chunks = total,
                    last_chunk = is_last,
                    outcome = "err",
                    error = %error,
                    "discord replace continuation failed; deleting sent continuations before retry"
                );
                let cleanup_errors = cleanup_replace_continuations_after_failure(
                    http,
                    channel_id,
                    &sent_continuation_message_ids,
                    shared,
                )
                .await;
                if cleanup_errors.failed_message_ids.is_empty() {
                    if let Err(error) = clear_replace_continuation_rollback(&rollback_key) {
                        unclaim_replace_continuation_rollback(&rollback_key);
                        let mut errors = cleanup_errors.errors;
                        errors.push(error.clone());
                        return Ok(DeferredReplaceLongMessageOutcome::Edited(
                            ReplaceLongMessageOutcome::PartialContinuationFailure {
                                sent_chunks: i,
                                total_chunks: total,
                                failed_chunk_index: i,
                                sent_continuation_message_ids: sent_continuation_message_ids
                                    .clone(),
                                cleanup_errors: errors,
                                error: watcher_send_failure_message(failure_class, error),
                            },
                        ));
                    }
                } else {
                    let mut errors = cleanup_errors.errors;
                    if let Err(record_error) = record_replace_continuation_rollback(
                        &rollback_key,
                        cleanup_errors.failed_message_ids.clone(),
                    ) {
                        record_replace_continuation_rollback_memory_only(
                            &rollback_key,
                            cleanup_errors.failed_message_ids.clone(),
                        );
                        errors.push(record_error);
                    }
                    return Ok(DeferredReplaceLongMessageOutcome::Edited(
                ReplaceLongMessageOutcome::PartialContinuationFailure {
                        sent_chunks: i,
                        total_chunks: total,
                        failed_chunk_index: i,
                        sent_continuation_message_ids: sent_continuation_message_ids.clone(),
                        cleanup_errors: errors,
                        error: watcher_send_failure_message(
                            crate::services::discord::replace_outcome_policy::WatcherSendFailureClass::RollbackIncomplete,
                            error,
                        ),
                    }));
                }
                return Ok(DeferredReplaceLongMessageOutcome::Edited(
                    ReplaceLongMessageOutcome::PartialContinuationFailure {
                        sent_chunks: i,
                        total_chunks: total,
                        failed_chunk_index: i,
                        sent_continuation_message_ids: sent_continuation_message_ids.clone(),
                        cleanup_errors: cleanup_errors.errors,
                        error: watcher_send_failure_message(failure_class, error),
                    },
                ));
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    if !sent_continuation_message_ids.is_empty()
        && let Err(error) = clear_replace_continuation_rollback(&rollback_key)
    {
        clear_replace_continuation_rollback_memory_only(&rollback_key);
        tracing::warn!(
            target: "discord::chunker",
            path = "replace_long_message_raw",
            channel_id = channel_id.get(),
            message_id = message_id.get(),
            error = %error,
            "discord replace delivered all chunks but rollback state cleanup failed"
        );
    }
    // #3805 P1: fully-successful edit+continuations. When continuations were
    // sent, hand back the TAIL chunk (id + its own text) so the watcher footer
    // can re-anchor onto it (highest snowflake, #3717 latest-wins). Empty
    // continuations ⇒ single-chunk answer ⇒ leave `None` (chunk 0 is the tail).
    *last_chunk_anchor =
        sent_continuation_message_ids
            .last()
            .copied()
            .map(|msg_id| ReplaceLastChunkAnchor {
                msg_id,
                text: chunks.last().cloned().unwrap_or_default(),
            });
    Ok(DeferredReplaceLongMessageOutcome::Edited(
        ReplaceLongMessageOutcome::EditedOriginal,
    ))
}

#[derive(Debug, Default)]
pub(in crate::services::discord) struct ContinuationCleanupResult {
    pub(in crate::services::discord) failed_message_ids: Vec<u64>,
    pub(in crate::services::discord) errors: Vec<String>,
}

pub(in crate::services::discord) async fn cleanup_replace_continuations_after_failure(
    http: &serenity::Http,
    channel_id: ChannelId,
    sent_continuation_message_ids: &[u64],
    shared: &Arc<SharedData>,
) -> ContinuationCleanupResult {
    let mut result = ContinuationCleanupResult::default();
    for message_id in sent_continuation_message_ids.iter().rev().copied() {
        rate_limit_wait(shared, channel_id).await;
        if let Err(error) =
            delete_rollback_channel_message(http, channel_id, MessageId::new(message_id)).await
        {
            let detail = error.to_string();
            match classify_delete_error(&detail) {
                PlaceholderCleanupOutcome::AlreadyGone | PlaceholderCleanupOutcome::Succeeded => {
                    tracing::debug!(
                        target: "discord::chunker",
                        channel_id = channel_id.get(),
                        message_id,
                        detail = %detail,
                        "continuation cleanup delete is already committed"
                    );
                }
                PlaceholderCleanupOutcome::Failed { .. } => {
                    result.failed_message_ids.push(message_id);
                    result.errors.push(format!("{}: {}", message_id, detail));
                }
            }
        }
    }
    result
}
