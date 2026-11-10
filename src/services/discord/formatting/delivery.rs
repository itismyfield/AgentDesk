use super::*;

/// Inline footer appended to a summary when a long message is delivered as a
/// `.txt` attachment via `/api/discord/send`.
const ATTACHMENT_FOOTER_PREFIX: &str = "📎 전문은 첨부 파일 참고";

/// Send a message using poise Context, splitting if necessary
pub(in crate::services::discord) async fn send_long_message_ctx(
    ctx: Context<'_>,
    text: &str,
) -> Result<(), Error> {
    if !needs_multiple_messages(text) {
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
            super::super::http::dispatch_channel_message(
                &ctx.serenity_context().http,
                ctx.channel_id(),
                serenity::CreateMessage::new().content(chunk),
            )
            .await?;
        }
    }

    Ok(())
}

pub(in crate::services::discord) fn long_message_reply_builders(
    text: &str,
) -> Vec<(usize, poise::CreateReply)> {
    if !needs_multiple_messages(text) {
        return vec![(text.len(), poise::CreateReply::default().content(text))];
    }

    split_message(text)
        .into_iter()
        .map(|chunk| (chunk.len(), poise::CreateReply::default().content(chunk)))
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

    for (i, (byte_len, reply)) in replies.into_iter().enumerate() {
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
    if !needs_multiple_messages(text) {
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

/// Fresh single-message sink path with an unreconstructed Discord receipt.
pub(in crate::services::discord) async fn send_single_message_returning_receipt(
    http: &serenity::Http,
    channel_id: ChannelId,
    text: &str,
    shared: &Arc<SharedData>,
    reference: Option<(ChannelId, MessageId)>,
) -> Result<super::super::outbound::DiscordTransportReceipt, Error> {
    rate_limit_wait(shared, channel_id).await;
    let message =
        send_channel_message_with_optional_reference(http, channel_id, text, reference).await?;
    Ok(super::super::outbound::DiscordTransportReceipt::from_message(channel_id, &message))
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

pub(in crate::services::discord) use crate::domain::message_split::{
    needs_multiple_messages, split_message,
};

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
    let has_operational_alert_origin =
        crate::services::discord::dispatch_policy::strip_operational_alert_origin(text).1;
    let trimmed_summary = summary.and_then(|s| {
        let t = s.trim();
        (!t.is_empty()).then_some(t)
    });

    // Re-apply provenance before checking the limit. The hidden marker uses one
    // Unicode scalar per bit: `[origin=operational_alert]` is 26 UTF-8 bytes,
    // so the current marker contributes 208 characters. A summary that fits
    // without the marker must therefore be allowed to fall back to the short
    // generic notice instead of producing an over-limit inline message.
    let with_provenance = |inline: String| {
        if has_operational_alert_origin {
            crate::services::discord::dispatch_policy::prepend_operational_alert_origin(&inline)
        } else {
            inline
        }
    };

    if let Some(summary) = trimmed_summary {
        let candidate = with_provenance(format!("{summary}{footer}"));
        // Keep the caller-provided summary only when the fully composed inline
        // candidate fits the shared Discord message-unit limit.
        if !needs_multiple_messages(&candidate) {
            return candidate;
        }
    }

    let fallback = with_provenance(format!(
        "📎 내용이 길어 전문을 파일로 첨부했습니다. ({} bytes)",
        text.len()
    ));
    // The fixed fallback is currently well below the remaining budget after
    // the 208-character marker. If either wording or marker grows past the
    // Discord limit, this attachment path has no second inline fallback and
    // the send will surface Discord's length rejection; keep this assertion so
    // that such a change is caught in debug/test builds rather than hidden.
    debug_assert!(!needs_multiple_messages(&fallback));
    fallback
}

#[cfg(test)]
mod attachment_delivery_tests {
    use super::build_long_message_attachment;
    use crate::services::discord::DISCORD_MSG_LIMIT;
    use crate::services::discord::dispatch_policy::{
        has_non_turn_provenance, is_allowed_turn_sender, prepend_operational_alert_origin,
    };

    const ANNOUNCE_ID: u64 = 1_001;

    #[test]
    fn oversize_operational_alert_attachment_inline_remains_non_turn() {
        let operational_alert = prepend_operational_alert_origin(&format!(
            "DISPATCH:operational alert\n{}",
            "x".repeat(DISCORD_MSG_LIMIT)
        ));
        assert!(operational_alert.chars().count() > DISCORD_MSG_LIMIT);

        let (inline, _attachment) = build_long_message_attachment(&operational_alert, None);

        assert!(has_non_turn_provenance(&inline));
        assert!(!is_allowed_turn_sender(
            &[ANNOUNCE_ID],
            Some(ANNOUNCE_ID),
            ANNOUNCE_ID,
            true,
            &inline,
        ));
        assert!(inline.chars().count() <= DISCORD_MSG_LIMIT);
    }
}

#[cfg(test)]
mod discord_unit_tests {

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn context_followup_chunks_p2_allowed_mentions_preserve_users() {
        use crate::services::discord::admin_host_guard::tests::Recorder;
        use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
        use crate::services::discord::{Data, make_shared_data_for_tests_with_storage};
        use poise::serenity_prelude as serenity;
        use std::sync::Arc;

        let _root = crate::config::TestRuntimeRootGuard::new();
        let api = Recorder::start().await;
        let (_, gateway, gateway_server) =
            discord_mock::start(discord_mock::DiscordMockState::new()).await;
        let serenity_ctx = discord_mock::serenity_context_over(api.http.clone(), gateway).await;
        let (manager, _) =
            serenity::gateway::ShardManager::new(serenity::gateway::ShardManagerOptions {
                data: serenity_ctx.data.clone(),
                event_handlers: vec![],
                raw_event_handlers: vec![],
                framework: Arc::new(std::sync::OnceLock::new()),
                shard_index: 0,
                shard_init: 0,
                shard_total: 1,
                ws_url: Arc::new(tokio::sync::Mutex::new("ws://127.0.0.1:1".into())),
                cache: serenity_ctx.cache.clone(),
                http: api.http.clone(),
                intents: serenity::GatewayIntents::empty(),
                presence: None,
            });
        let data = Data {
            shared: make_shared_data_for_tests_with_storage(None),
            token: "test-token".into(),
            provider: crate::services::provider::ProviderKind::Claude,
        };
        let options = poise::FrameworkOptions::<Data, super::Error>::default();
        let command = poise::Command::<Data, super::Error>::default();
        let command_tree = [&command];
        let message = discord_mock::user_message(741_900, "test command");
        let invocation_data: tokio::sync::Mutex<Box<dyn std::any::Any + Send + Sync>> =
            tokio::sync::Mutex::new(Box::new(()));
        let ctx = poise::Context::Prefix(poise::PrefixContext {
            msg: &message,
            content_start: 0,
            invoked_command_name: "test",
            args: "",
            framework: poise::FrameworkContext {
                serenity_context: &serenity_ctx,
                options: &options,
                user_data: &data,
                shard_manager: &manager,
            },
            command_tree: &command_tree,
            invocation_data: &invocation_data,
            trigger: poise::MessageDispatchTrigger::MessageCreate,
            __non_exhaustive: (),
        });
        let content = "<@1234> @everyone @here <@&5678> 한글\n".repeat(100);
        let chunks = super::split_message(&content);
        assert!(chunks.len() > 1, "fixture must reach channel followups");

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            super::send_long_message_ctx(ctx, &content),
        )
        .await;
        gateway_server.abort();
        result
            .expect("command chunks finish")
            .expect("command chunks delivered");

        let calls = api.take();
        assert_eq!(
            calls.len(),
            chunks.len(),
            "every chunk reaches HTTP: {calls:?}"
        );
        let prefix = format!(
            "POST /api/v10/channels/{}/messages ",
            message.channel_id.get()
        );
        for (call, chunk) in calls.iter().zip(chunks) {
            let body: serde_json::Value =
                serde_json::from_str(call.strip_prefix(&prefix).expect("actual channel POST"))
                    .unwrap();
            assert_eq!(body["content"], chunk);
            assert!(body["content"].as_str().unwrap().contains("<@1234>"));
            assert_eq!(
                body["allowed_mentions"]["parse"],
                serde_json::json!(["users"])
            );
        }
    }
    use super::{discord_message_units, needs_multiple_messages, split_message};
    use crate::services::discord::DISCORD_MSG_LIMIT;

    #[test]
    fn utf16_fit_predicate_pins_units_with_ascii_control_5178() {
        let korean_under_limit = "한".repeat(900);
        assert_eq!(
            discord_message_units(&korean_under_limit),
            900,
            "Korean under-limit fixture unit count"
        );
        assert!(
            korean_under_limit.len() > DISCORD_MSG_LIMIT,
            "fixture must exceed the limit in UTF-8 bytes"
        );
        assert!(
            !needs_multiple_messages(&korean_under_limit),
            "900 UTF-16 units must remain a single message"
        );

        // ASCII has equal UTF-8 byte, Unicode scalar, and UTF-16 unit counts, so
        // these 2000/2001 controls pin only the strict boundary. The Korean and
        // supplementary witnesses distinguish byte- and scalar-count regressions.
        let ascii_at_limit = "x".repeat(DISCORD_MSG_LIMIT);
        let ascii_over_limit = format!("{ascii_at_limit}x");
        assert!(
            !needs_multiple_messages(&ascii_at_limit),
            "ASCII control at 2000 units must fit"
        );
        assert!(
            needs_multiple_messages(&ascii_over_limit),
            "ASCII control at 2001 units must route multi"
        );

        let supplementary_at_limit = "📦".repeat(DISCORD_MSG_LIMIT / 2);
        assert_eq!(
            discord_message_units(&supplementary_at_limit),
            DISCORD_MSG_LIMIT,
            "supplementary fixture must be exactly 2000 UTF-16 units"
        );
        assert!(
            !needs_multiple_messages(&supplementary_at_limit),
            "exactly 2000 UTF-16 units must fit"
        );
        let supplementary_over_limit = format!("{supplementary_at_limit}x");
        assert_eq!(
            discord_message_units(&supplementary_over_limit),
            DISCORD_MSG_LIMIT + 1,
            "supplementary fixture must be exactly 2001 UTF-16 units"
        );
        assert!(
            needs_multiple_messages(&supplementary_over_limit),
            "2001 UTF-16 units must route multi"
        );
    }

    #[test]
    fn utf16_fit_predicate_and_chunker_agree_on_supplementary_overflow_5177() {
        let body = format!("{}{}", "한".repeat(1_965), "📦".repeat(20));
        assert_eq!(
            discord_message_units(&body),
            2_005,
            "mixed over-limit fixture unit count"
        );
        assert!(
            needs_multiple_messages(&body),
            "2005 units must route multi"
        );

        let chunks = split_message(&body);
        assert_eq!(chunks.len(), 2, "2005 units must produce two chunks");
        assert_eq!(chunks.concat(), body, "chunking must preserve every scalar");
        assert!(
            chunks
                .iter()
                .all(|chunk| discord_message_units(chunk) <= DISCORD_MSG_LIMIT)
        );
    }

    #[test]
    fn long_supplementary_fence_info_does_not_discard_body_5177() {
        let body = "TAIL".repeat(300);
        let message = format!("```{}\n{}\n```", "😀".repeat(995), body);
        assert_eq!(discord_message_units(&message), 3_198, "fixture unit count");

        let chunks = split_message(&message);

        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.matches("TAIL").count())
                .sum::<usize>(),
            300,
            "every fenced body token must survive chunking"
        );
        assert!(
            chunks.len() > 1,
            "fixture must exercise continuation chunks"
        );
        assert!(
            chunks
                .iter()
                .all(|chunk| discord_message_units(chunk) <= DISCORD_MSG_LIMIT),
            "every continuation must fit the shared unit limit"
        );
    }
}
