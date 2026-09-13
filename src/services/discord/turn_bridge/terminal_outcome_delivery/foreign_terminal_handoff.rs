//! Preserve a detached episode without ever editing its successor's anchor.

use super::*;
use crate::services::{discord::outbound, message_outbox as outbox};

pub(super) struct Handoff<'a> {
    pub shared: &'a SharedData,
    pub gateway: &'a dyn TurnGateway,
    pub provider: &'a ProviderKind,
    pub local: &'a InflightTurnState,
    pub admitted: Option<&'a crate::services::discord::inflight::CodexRange>,
    pub content: &'a str,
    pub response_sent_offset: usize,
    pub channel_id: ChannelId,
    pub old_anchor: MessageId,
    pub can_chain_locally: bool,
}

pub(super) enum Outcome {
    Deferred { outbox_id: i64 },
    Published,
    Unresolved { error: String },
}

pub(super) async fn preserve_or_publish(ctx: Handoff<'_>) -> Outcome {
    // Text offsets select already-frozen response prefixes only. They are never
    // converted to JSONL ranges or recorded as exact-source delivery receipts.
    let delivery_response =
        terminal_delivery_response_after_offset(ctx.content, ctx.response_sent_offset, None);
    let content = delivery_response.as_str();
    if content.trim().is_empty() {
        return Outcome::Unresolved { error: "foreign anchor has no independently publishable terminal body; source recovery remains unresolved".into() };
    }
    let identity = serde_json::json!({
        "provider": ctx.provider.as_str(), "owner_channel_id": ctx.local.channel_id,
        "delivery_channel_id": ctx.channel_id.get(), "user_msg_id": ctx.local.user_msg_id,
        "tmux_session_name": ctx.local.tmux_session_name, "turn_nonce": ctx.local.turn_nonce,
        "started_at": ctx.local.started_at, "turn_start_offset": ctx.local.turn_start_offset,
        "last_offset": ctx.local.last_offset, "output_path": ctx.local.output_path, "session_id": ctx.local.session_id,
        "born_generation": ctx.local.born_generation, "source": ctx.admitted.map(|range| &range.source),
        "response_sent_offset": ctx.response_sent_offset,
        "body_digest": outbound::outbound_fingerprint(&[content]),
    }).to_string();
    let dedupe = format!(
        "rowless-terminal:{}",
        outbound::outbound_fingerprint(&[&identity])
    );
    let target = format!("channel:{}", ctx.channel_id.get());
    let bot = super::super::headless_delivery::headless_delivery_bot_alias(
        ctx.local.delivery_bot.as_deref(),
        ctx.provider,
    );
    let outbox_error = if let Some(pool) = ctx.shared.pg_pool.as_ref() {
        // The cancelled provider has finished producing this body; cancellation
        // must not discard an already generated, otherwise ownerless answer.
        // No session marker is written and no foreign message ID enters the row.
        match outbox::enqueue_outbox_pg_returning_outcome_with_exact_dedupe_and_cancel(
            pool,
            outbox::OutboxMessage {
                target: &target,
                content,
                bot,
                source: "headless_turn",
                reason_code: Some(&identity),
                session_key: None,
            },
            &dedupe,
            None,
        )
        .await
        {
            Ok(outbox::OutboxEnqueueOutcome::Enqueued { id }) => {
                return Outcome::Deferred { outbox_id: id };
            }
            Ok(outbox::OutboxEnqueueOutcome::Cancelled) => "unexpected outbox cancellation".into(),
            Ok(outbox::OutboxEnqueueOutcome::NoRow) => {
                "outbox did not retain a delivery obligation".to_string()
            }
            Err(error) => error.to_string(),
        }
    } else {
        "PostgreSQL outbox unavailable".into()
    };
    if !ctx.can_chain_locally {
        return Outcome::Unresolved {
            error: format!("{outbox_error}; no live Discord gateway for fallback POST"),
        };
    }
    // Existing rollback transport deletes only IDs returned by this POST
    // attempt. old_anchor is diagnostic context, never an edit/delete target.
    match ctx
        .gateway
        .send_long_message_with_rollback(ctx.channel_id, ctx.old_anchor, content)
        .await
    {
        Ok(ids)
            if !ids.is_empty()
                && ids.iter().all(|id| {
                    !super::super::headless_delivery::is_synthetic_headless_message_id(*id)
                }) =>
        {
            Outcome::Published
        }
        Ok(_) => Outcome::Unresolved {
            error: format!("{outbox_error}; fallback POST returned no real Discord receipt"),
        },
        Err(error) => Outcome::Unresolved {
            error: format!("outbox: {outbox_error}; fallback POST: {error}"),
        },
    }
}
