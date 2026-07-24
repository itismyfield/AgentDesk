use super::*;

/// Inline footer appended to a summary when a long message is delivered as a
/// `.txt` attachment via `/api/discord/send`.
const ATTACHMENT_FOOTER_PREFIX: &str = "📎 전문은 첨부 파일 참고";

/// Send a message using poise Context, splitting if necessary
pub(in crate::services::discord) async fn send_long_message_ctx(
    ctx: Context<'_>,
    text: &str,
) -> Result<(), Error> {
    if char_count(text) <= DISCORD_MSG_LIMIT {
        tracing::debug!(
            target: "discord::chunker",
            path = "send_long_message_ctx",
            chunk_index = 0usize,
            byte_len = text.len(),
            total_chunks = 1usize,
            "discord send single"
        );
        ctx.say(text).await?;
        return Ok(());
    }

    let chunks = split_message(text);
    let total = chunks.len();
    for (i, chunk) in chunks.iter().enumerate() {
        tracing::debug!(
            target: "discord::chunker",
            path = "send_long_message_ctx",
            chunk_index = i,
            byte_len = chunk.len(),
            total_chunks = total,
            "discord send chunk"
        );
        if i == 0 {
            ctx.say(chunk).await?;
        } else {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            ctx.channel_id().say(ctx.serenity_context(), chunk).await?;
        }
    }

    Ok(())
}

pub(in crate::services::discord) fn long_message_reply_builders(
    text: &str,
) -> Vec<poise::CreateReply> {
    if char_count(text) <= DISCORD_MSG_LIMIT {
        return vec![poise::CreateReply::default().content(text.to_string())];
    }

    split_message(text)
        .into_iter()
        .map(|chunk| poise::CreateReply::default().content(chunk))
        .collect()
}

/// Send a long command response through poise's reply abstraction.
///
/// In slash-command contexts, poise maps the first call to an interaction
/// response and later calls to interaction followups. That avoids direct
/// channel sends from slash command handlers while preserving chunking.
pub(in crate::services::discord) async fn send_long_message_reply_ctx(
    ctx: Context<'_>,
    text: &str,
) -> Result<(), Error> {
    let replies = long_message_reply_builders(text);
    let total = replies.len();

    for (i, reply) in replies.into_iter().enumerate() {
        let byte_len = reply.content.as_ref().map_or(0, String::len);
        tracing::debug!(
            target: "discord::chunker",
            path = "send_long_message_reply_ctx",
            chunk_index = i,
            byte_len,
            total_chunks = total,
            "discord command reply chunk"
        );
        if i > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }
        ctx.send(reply).await?;
    }

    Ok(())
}

/// Send a long message using raw HTTP, splitting if necessary
pub(in crate::services::discord) async fn send_long_message_raw(
    http: &serenity::Http,
    channel_id: ChannelId,
    text: &str,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    send_long_message_raw_with_reference(http, channel_id, text, shared, None).await
}

/// Send a long message using raw HTTP, replying to `reference` for the first
/// Discord message when available.
pub(in crate::services::discord) async fn send_long_message_raw_with_reference(
    http: &serenity::Http,
    channel_id: ChannelId,
    text: &str,
    shared: &Arc<SharedData>,
    reference: Option<(ChannelId, MessageId)>,
) -> Result<(), Error> {
    send_long_message_raw_with_reference_returning_message_ids(
        http, channel_id, text, shared, reference,
    )
    .await
    .map(|_| ())
}

/// Send a long message using raw HTTP and return every created Discord message id.
pub(in crate::services::discord) async fn send_long_message_raw_with_reference_returning_message_ids(
    http: &serenity::Http,
    channel_id: ChannelId,
    text: &str,
    shared: &Arc<SharedData>,
    reference: Option<(ChannelId, MessageId)>,
) -> Result<Vec<MessageId>, Error> {
    let payload_byte_len = text.len();
    if char_count(text) <= DISCORD_MSG_LIMIT {
        tracing::debug!(
            target: "discord::chunker",
            path = "send_long_message_raw",
            channel_id = channel_id.get(),
            payload_byte_len,
            chunk_index = 0usize,
            byte_len = payload_byte_len,
            total_chunks = 1usize,
            "discord send single"
        );
        rate_limit_wait(shared, channel_id).await;
        match send_channel_message_with_optional_reference(http, channel_id, text, reference).await
        {
            Ok(message) => {
                tracing::debug!(
                    target: "discord::chunker",
                    path = "send_long_message_raw",
                    channel_id = channel_id.get(),
                    payload_byte_len,
                    last_chunk = true,
                    outcome = "ok",
                    "discord send single done"
                );
                return Ok(vec![message.id]);
            }
            Err(err) => {
                tracing::warn!(
                    target: "discord::chunker",
                    path = "send_long_message_raw",
                    channel_id = channel_id.get(),
                    payload_byte_len,
                    last_chunk = true,
                    outcome = "err",
                    error = %err,
                    "discord send single failed (issue #1043)"
                );
                return Err(err.into());
            }
        }
    }

    let chunks = split_message(text);
    let total = chunks.len();
    // #3082 part B: hold the per-channel answer-flush barrier for the whole
    // multi-chunk send so a queued-turn notice POST cannot interleave between
    // chunks. The guard clears the gate on every exit path (Ok, `?`, panic).
    let _answer_flush_guard =
        (total > 1).then(|| shared.answer_flush_barrier.begin_flush(channel_id));
    tracing::debug!(
        target: "discord::chunker",
        path = "send_long_message_raw",
        channel_id = channel_id.get(),
        payload_byte_len,
        total_chunks = total,
        "discord send begin"
    );
    let mut sent_message_ids = Vec::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let is_last = i + 1 == total;
        tracing::debug!(
            target: "discord::chunker",
            path = "send_long_message_raw",
            channel_id = channel_id.get(),
            chunk_index = i,
            byte_len = chunk.len(),
            total_chunks = total,
            is_last_chunk = is_last,
            "discord send chunk"
        );
        rate_limit_wait(shared, channel_id).await;
        let chunk_reference = if i == 0 { reference.clone() } else { None };
        let send_result =
            send_channel_message_with_optional_reference(http, channel_id, chunk, chunk_reference)
                .await;
        match send_result {
            Ok(message) => {
                // #3082 P1-2: chunk landed — keep the answer-flush barrier's
                // inactivity window fresh so a long answer never trips the
                // queued-card wait while it is still making progress.
                shared.answer_flush_barrier.note_progress(channel_id);
                shared
                    .tmux_relay_coord(channel_id)
                    .note_relay_progress_heartbeat(chrono::Utc::now().timestamp_millis());
                sent_message_ids.push(message.id);
                if is_last {
                    tracing::debug!(
                        target: "discord::chunker",
                        path = "send_long_message_raw",
                        channel_id = channel_id.get(),
                        chunk_index = i,
                        total_chunks = total,
                        last_chunk = true,
                        outcome = "ok",
                        "discord send last chunk ok"
                    );
                }
            }
            Err(err) => {
                tracing::warn!(
                    target: "discord::chunker",
                    path = "send_long_message_raw",
                    channel_id = channel_id.get(),
                    chunk_index = i,
                    total_chunks = total,
                    last_chunk = is_last,
                    outcome = "err",
                    error = %err,
                    "discord send chunk failed (issue #1043 — tail may be missing)"
                );
                return Err(err.into());
            }
        }
    }

    Ok(sent_message_ids)
}

pub(super) async fn send_channel_message_with_optional_reference(
    http: &serenity::Http,
    channel_id: ChannelId,
    content: &str,
    reference: Option<(ChannelId, MessageId)>,
) -> serenity::Result<serenity::Message> {
    let Some((reference_channel_id, reference_message_id)) = reference else {
        return super::super::http::send_channel_message(http, channel_id, content).await;
    };
    match super::super::http::send_channel_message_with_reference(
        http,
        channel_id,
        content,
        reference_channel_id,
        reference_message_id,
    )
    .await
    {
        Ok(message) => Ok(message),
        Err(error) => {
            tracing::warn!(
                channel_id = channel_id.get(),
                reference_channel_id = reference_channel_id.get(),
                reference_message_id = reference_message_id.get(),
                error = %error,
                "discord referenced send failed; falling back to plain message"
            );
            super::super::http::send_channel_message(http, channel_id, content).await
        }
    }
}

/// Split a message into chunks that fit within Discord's 2000 char limit.
/// Handles code block boundaries correctly. Used by stream/slash-command/recovery
/// paths where overflow is delivered as additional inline messages. The manual
/// `/api/discord/send` route uses the shared outbound API length policy instead.
///
/// Emits structured `tracing::debug!` logs at `target: "discord::chunker"` for
/// every chunk produced (chunk_index, byte_len, boundary_kind, in_code_block).
/// This is the tracepoint referenced by issue #1043 to diagnose any case where
/// the tail of a long agent message fails to reach Discord (e.g. option block
/// A/B/C sections disappearing near the 2000-char boundary).
pub(in crate::services::discord) fn split_message(text: &str) -> Vec<String> {
    let total_bytes = text.len();
    let mut chunks = Vec::new();
    let mut remaining = text;
    let mut remaining_chars = char_count(text);
    let mut in_code_block = false;
    let mut code_block_lang = String::new();

    while !remaining.is_empty() {
        // Reserve space for code block tags we may need to add
        let tag_overhead = if in_code_block {
            // closing ``` + opening ```lang\n
            3 + 3 + char_count(&code_block_lang) + 1
        } else {
            0
        };
        let effective_limit = DISCORD_MSG_LIMIT
            .saturating_sub(tag_overhead)
            .saturating_sub(10);

        if remaining_chars <= effective_limit {
            let mut chunk = String::new();
            if in_code_block {
                chunk.push_str("```");
                chunk.push_str(&code_block_lang);
                chunk.push('\n');
            }
            chunk.push_str(remaining);
            let byte_len = chunk.len();
            let was_in_code_block = in_code_block;
            chunks.push(chunk);
            tracing::debug!(
                target: "discord::chunker",
                chunk_index = chunks.len() - 1,
                byte_len,
                boundary_kind = "final",
                in_code_block = was_in_code_block,
                total_bytes,
                "split_message emit"
            );
            break;
        }

        // Find a safe split point.
        //
        // Issue #1043 root cause #1: when the input begins with a leading `\n`
        // and the next ~2000 chars contain no other newline, `rfind('\n')`
        // returns `Some(0)`. That made `raw_chunk` empty, the chunker emitted a
        // zero-byte chunk, and Discord's REST API rejected the send with HTTP
        // 400 ("Cannot send an empty message"). The error short-circuited
        // `send_long_message_raw` / `replace_long_message_raw`, so every later
        // chunk — including the trailing A/B/C option block users were
        // reporting missing — never reached the channel.
        //
        // Fix: if a newline split would yield a zero-byte `raw_chunk`, fall
        // back to a hard split at `safe_end` (or skip the orphan newline when
        // `safe_end` is also 0 due to a multi-byte char on the boundary).
        let safe_end = byte_index_at_char_limit(remaining, effective_limit);
        let (mut split_at, mut boundary_kind) =
            super::super::semantic_boundaries::message_split_boundary(
                remaining,
                safe_end,
                in_code_block,
            );
        if split_at == 0 {
            if safe_end > 0 {
                split_at = safe_end;
                boundary_kind = "hard_after_leading_newline";
            } else {
                // safe_end is also 0 (e.g. multi-byte char straddling a
                // 0-char effective_limit). Skip one character to guarantee
                // forward progress and never emit an empty chunk.
                let step = remaining
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| i)
                    .unwrap_or(remaining.len());
                let skipped_chars = char_count(&remaining[..step]);
                tracing::debug!(
                    target: "discord::chunker",
                    step,
                    total_bytes,
                    "split_message advance over zero-width boundary"
                );
                remaining = &remaining[step..];
                remaining_chars = remaining_chars.saturating_sub(skipped_chars);
                continue;
            }
        }

        let (raw_chunk, rest) = remaining.split_at(split_at);
        let raw_chunk_chars = char_count(raw_chunk);
        let stripped_boundary_chars = usize::from(rest.starts_with('\n'));

        let mut chunk = String::new();
        if in_code_block {
            chunk.push_str("```");
            chunk.push_str(&code_block_lang);
            chunk.push('\n');
        }
        chunk.push_str(raw_chunk);

        // Track code blocks across chunk boundaries
        for line in raw_chunk.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") {
                if in_code_block {
                    in_code_block = false;
                    code_block_lang.clear();
                } else {
                    in_code_block = true;
                    code_block_lang = trimmed.strip_prefix("```").unwrap_or("").to_string();
                }
            }
        }

        // Close unclosed code block at end of chunk
        if in_code_block {
            chunk.push_str("\n```");
        }

        let byte_len = chunk.len();
        let fence_was_open_at_emit = in_code_block;
        // Defensive: never emit an empty chunk to the Discord send path.
        // (split_at == 0 is handled above; this guard catches any future
        // rewrite that could regress.)
        if chunk.is_empty() {
            tracing::warn!(
                target: "discord::chunker",
                boundary_kind,
                total_bytes,
                "split_message would have emitted an empty chunk; skipping (issue #1043 guard)"
            );
            remaining_chars = remaining_chars
                .saturating_sub(raw_chunk_chars)
                .saturating_sub(stripped_boundary_chars);
            remaining = rest.strip_prefix('\n').unwrap_or(rest);
            continue;
        }
        chunks.push(chunk);
        tracing::debug!(
            target: "discord::chunker",
            chunk_index = chunks.len() - 1,
            byte_len,
            boundary_kind,
            in_code_block = fence_was_open_at_emit,
            total_bytes,
            "split_message emit"
        );
        remaining_chars = remaining_chars
            .saturating_sub(raw_chunk_chars)
            .saturating_sub(stripped_boundary_chars);
        remaining = rest.strip_prefix('\n').unwrap_or(rest);
    }

    tracing::debug!(
        target: "discord::chunker",
        total_chunks = chunks.len(),
        total_bytes,
        "split_message done"
    );

    chunks
}

/// Build an `(inline_message, attachment)` pair for content that exceeds
/// `DISCORD_MSG_LIMIT`. The attachment carries the full unmodified `text` as a
/// `.txt` file. The inline message uses `summary` when provided (so the sender
/// controls what humans see); otherwise it falls back to a short generic notice
/// pointing at the attachment instead of dumping a blind byte-prefix of `text`.
pub(in crate::services::discord) fn build_long_message_attachment(
    text: &str,
    summary: Option<&str>,
) -> (String, CreateAttachment) {
    let filename = format!(
        "response-{}.txt",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );
    let attachment = CreateAttachment::bytes(text.as_bytes().to_vec(), filename);
    let inline = build_attachment_inline(text, summary);
    (inline, attachment)
}

fn build_attachment_inline(text: &str, summary: Option<&str>) -> String {
    let footer = format!("\n\n{ATTACHMENT_FOOTER_PREFIX} ({} bytes)", text.len());
    let trimmed_summary = summary.and_then(|s| {
        let t = s.trim();
        (!t.is_empty()).then_some(t)
    });

    if let Some(summary) = trimmed_summary {
        if char_count(summary) + char_count(&footer) <= DISCORD_MSG_LIMIT {
            return format!("{summary}{footer}");
        }
    }

    format!(
        "📎 내용이 길어 전문을 파일로 첨부했습니다. ({} bytes)",
        text.len()
    )
}
