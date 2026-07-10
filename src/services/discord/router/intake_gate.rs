use super::super::*;
use super::super::{queue_marker, queue_reactions};
use super::intake_queue_transaction::{
    IntakeQueueAuthorClass, IntakeQueueCommitOptions, IntakeQueueCommitSource,
    IntakeQueuePendingReactionPolicy, SoftInterventionCommitRequest, SoftInterventionSpec,
    commit_soft_intervention_transaction,
};

mod busy_duplicate_notice;
mod component_events;
mod gate;
mod queue_effects;
mod stale_turn;

pub(in crate::services::discord) use gate::should_process_turn_message;
#[cfg(test)]
pub(super) use queue_effects::queue_pending_reaction_for;

use gate::{
    bot_author_allowed_for_live_intake, should_merge_consecutive_messages,
    should_skip_for_missing_required_mention, should_skip_human_slash_message,
    should_skip_self_authored_turn_message, should_start_attachment_only_turn,
    strip_leading_bot_mention,
};
pub(in crate::services::discord::router) use queue_effects::should_schedule_post_enqueue_idle_drain;
use queue_effects::{IntakeGateQueueEffects, render_visible_queued_ack};
use stale_turn::{
    mailbox_has_live_active_turn_or_cleanup_stale_proof, thread_guard_force_clean_stale_thread,
    thread_guard_should_force_clean_stale_thread,
};

async fn record_upload_history(
    shared: &std::sync::Arc<SharedData>,
    channel_id: serenity::ChannelId,
    upload_records: &[String],
) {
    if upload_records.is_empty() {
        return;
    }
    let mut data = shared.core.lock().await;
    if let Some(session) = data.sessions.get_mut(&channel_id) {
        session
            .history
            .extend(upload_records.iter().cloned().map(|content| HistoryItem {
                item_type: HistoryType::User,
                content,
            }));
    }
}

async fn append_pending_uploads(
    shared: &std::sync::Arc<SharedData>,
    channel_id: serenity::ChannelId,
    upload_records: &[String],
) -> bool {
    if upload_records.is_empty() {
        return true;
    }
    let mut data = shared.core.lock().await;
    if let Some(session) = data.sessions.get_mut(&channel_id) {
        session
            .pending_uploads
            .extend(upload_records.iter().cloned());
        true
    } else {
        false
    }
}

fn session_has_usable_path(session: Option<&DiscordSession>) -> bool {
    session
        .and_then(|session| session.current_path.as_deref())
        .is_some_and(|path| !path.trim().is_empty())
}

async fn has_direct_runtime_session(
    data: &Data,
    channel_id: serenity::ChannelId,
    effective_channel_id: serenity::ChannelId,
) -> bool {
    let core = data.shared.core.lock().await;
    session_has_usable_path(core.sessions.get(&channel_id))
        || (effective_channel_id != channel_id
            && session_has_usable_path(core.sessions.get(&effective_channel_id)))
}

async fn can_route_unbound_direct_session(
    data: &Data,
    ctx: &serenity::Context,
    channel_id: serenity::ChannelId,
    effective_channel_id: serenity::ChannelId,
    is_dm: bool,
) -> bool {
    if has_direct_runtime_session(data, channel_id, effective_channel_id).await {
        return true;
    }

    // Use the `_force` variant: standard `auto_restore_session_*` early-returns
    // for unbound channels, but here we have already classified this as the
    // legitimate agentless-direct case and want disk/DB restoration to run so
    // the in-memory session is recreated after a dcserver restart.
    auto_restore_session_force(&data.shared, channel_id, ctx, Some(is_dm)).await;
    if effective_channel_id != channel_id {
        auto_restore_session_force(&data.shared, effective_channel_id, ctx, None).await;
    }

    has_direct_runtime_session(data, channel_id, effective_channel_id).await
}

async fn resolve_voice_transcript_announcement_for_intake(
    pg_pool: Option<&sqlx::PgPool>,
    channel_id: serenity::ChannelId,
    message_id: serenity::MessageId,
    author_id: serenity::UserId,
    announce_bot_id: Option<u64>,
    content: &str,
) -> Option<crate::voice::prompt::VoiceTranscriptAnnouncement> {
    if announce_bot_id != Some(author_id.get()) {
        return None;
    }

    if let Some(pool) = pg_pool {
        match crate::voice::announce_meta::load_voice_announcement_durable(pool, message_id).await {
            Ok(Some(announcement)) => return Some(announcement),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    channel_id = channel_id.get(),
                    message_id = message_id.get(),
                    "voice transcript announcement durable metadata load failed at intake gate"
                );
            }
        }
    } else if let Some(announcement) =
        crate::voice::announce_meta::global_store().peek_clone(message_id)
    {
        return Some(announcement);
    }
    if !crate::voice::prompt::is_readable_voice_transcript_announcement(content) {
        return None;
    }
    let pending_key = crate::voice::prompt::parse_voice_transcript_announcement_ref(content);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if pg_pool.is_none()
            && let Some(announcement) =
                crate::voice::announce_meta::global_store().peek_clone(message_id)
        {
            return Some(announcement);
        }
        if let Some(pool) = pg_pool {
            match crate::voice::announce_meta::load_voice_announcement_durable(pool, message_id)
                .await
            {
                Ok(Some(announcement)) => return Some(announcement),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        channel_id = channel_id.get(),
                        message_id = message_id.get(),
                        "voice transcript announcement durable metadata retry-load failed at intake gate"
                    );
                }
            }
            if let Some(pending_key) = pending_key.as_deref() {
                match crate::voice::announce_meta::bind_pending_voice_announcement_by_key_durable(
                    pool,
                    pending_key,
                    channel_id,
                    message_id,
                )
                .await
                {
                    Ok(Some(announcement)) => return Some(announcement),
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(
                            error = %error,
                            channel_id = channel_id.get(),
                            message_id = message_id.get(),
                            "voice transcript announcement pending metadata bind failed at intake gate"
                        );
                    }
                }
            }
        }

        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

async fn build_reply_context(
    ctx: &serenity::Context,
    channel_id: serenity::ChannelId,
    new_message: &serenity::Message,
) -> Option<String> {
    let ref_msg = new_message.referenced_message.as_ref()?;
    let ref_author = &ref_msg.author.name;
    let ref_content = ref_msg.content.trim();
    let ref_text = if ref_content.is_empty() {
        let attachments = ref_msg
            .attachments
            .iter()
            .map(AttachmentReplyItem::from)
            .collect::<Vec<_>>();
        format_attachment_reply_context(ref_author, ref_msg.id.get(), &attachments)
    } else {
        let truncated = truncate_str(ref_content, 500);
        format!(
            "[Reply context]\nAuthor: {}\nContent: {}",
            ref_author, truncated
        )
    };

    let mut context_parts = Vec::new();
    if let Ok(preceding) = channel_id
        .messages(
            &ctx.http,
            serenity::builder::GetMessages::new()
                .before(ref_msg.id)
                .limit(4),
        )
        .await
    {
        let mut msgs: Vec<_> = preceding
            .iter()
            .filter(|m| !m.content.trim().is_empty())
            .collect();
        msgs.reverse();
        let mut budget: usize = 1000;
        for m in msgs
            .iter()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            let entry = format!("{}: {}", m.author.name, truncate_str(m.content.trim(), 300));
            if entry.len() > budget {
                break;
            }
            budget -= entry.len();
            context_parts.push(entry);
        }
    }

    if context_parts.is_empty() {
        Some(ref_text)
    } else {
        let preceding_ctx = context_parts.join("\n");
        Some(format!(
            "[Reply context — preceding conversation]\n{}\n\n{}",
            preceding_ctx, ref_text
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AttachmentReplyItem {
    filename: String,
    size: u32,
    description: Option<String>,
}

impl From<&serenity::Attachment> for AttachmentReplyItem {
    fn from(attachment: &serenity::Attachment) -> Self {
        Self {
            filename: attachment.filename.clone(),
            size: attachment.size,
            description: attachment.description.clone(),
        }
    }
}

fn format_attachment_reply_context(
    ref_author: &str,
    ref_message_id: u64,
    attachments: &[AttachmentReplyItem],
) -> String {
    if attachments.is_empty() {
        return format!("[Reply to {}'s message (no text content)]", ref_author);
    }

    let mut lines = vec![
        "[Reply context]".to_string(),
        format!("Author: {ref_author}"),
        format!("Canonical Discord message id: {ref_message_id}"),
        "Content: [message has attachments but no text]".to_string(),
        "Attachments:".to_string(),
    ];
    for (index, attachment) in attachments.iter().take(10).enumerate() {
        let description = attachment.description.as_deref().unwrap_or("").trim();
        let mut line = format!(
            "{}. {} ({} bytes)",
            index + 1,
            attachment.filename,
            attachment.size
        );
        if !description.is_empty() {
            line.push_str(&format!(" — {}", truncate_str(description, 160)));
        }
        lines.push(line);
    }
    if attachments.len() > 10 {
        lines.push(format!("... {} more attachment(s)", attachments.len() - 10));
    }
    lines.join("\n")
}

pub(in crate::services::discord) async fn handle_event(
    ctx: &serenity::Context,
    event: &serenity::FullEvent,
    data: &Data,
) -> Result<(), Error> {
    maybe_cleanup_sessions(&data.shared).await;
    match event {
        serenity::FullEvent::InteractionCreate { interaction } => {
            if let Some(component) = interaction.as_message_component() {
                if component_events::handle_model_picker_component_if_applicable(
                    ctx, component, data,
                )
                .await?
                {
                    return Ok(());
                }
                if super::super::idle_recap_interaction::is_idle_recap_custom_id(
                    &component.data.custom_id,
                ) {
                    return super::super::idle_recap_interaction::handle_idle_recap_interaction(
                        ctx, component, data,
                    )
                    .await;
                }
                if super::super::steering::is_steer_cancel_custom_id(&component.data.custom_id) {
                    return super::super::steering::handle_steer_cancel_interaction(
                        ctx, component, data,
                    )
                    .await;
                }
                if super::super::commands::is_node_picker_custom_id(&component.data.custom_id) {
                    let settings_snapshot = { data.shared.settings.read().await.clone() };
                    if !super::super::provider_handles_channel(
                        ctx,
                        &data.provider,
                        &settings_snapshot,
                        component.channel_id,
                    )
                    .await
                    {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ⏭ COMPONENT-GUARD: skipping node picker in channel {} for provider {}",
                            component.channel_id,
                            data.provider.as_str()
                        );
                        return Ok(());
                    }
                    return super::super::commands::handle_node_picker_interaction(
                        ctx, component, data,
                    )
                    .await;
                }
                if super::super::sidecar_interaction::is_sidecar_custom_id(
                    &component.data.custom_id,
                ) {
                    return super::super::sidecar_interaction::handle_sidecar_interaction(
                        ctx, component, data,
                    )
                    .await;
                }
            }
        }
        serenity::FullEvent::Message { new_message } => {
            // ── Universal message-ID dedup ─────────────────────────────
            // Guards against the same Discord message being processed twice,
            // which can happen when thread messages are delivered as both a
            // thread-context event AND a parent-channel event, or during
            // gateway reconnections.
            //
            // Thread-preference: when a duplicate arrives, prefer the thread
            // context over the parent context.  If a parent-channel event
            // was processed first, a subsequent thread event for the same
            // message_id is allowed through (and the parent turn will have
            // already been filtered by should_process_turn_message or the
            // dispatch-thread guard).
            {
                const MSG_DEDUP_TTL: std::time::Duration = std::time::Duration::from_secs(60);
                let now = std::time::Instant::now();
                let key = format!("mid:{}", new_message.id);

                // Lazy cleanup of expired mid:* entries to prevent unbounded growth.
                //
                // #2044 F10: previously this ran every 50 messages
                // (`CLEANUP_COUNTER % 50 == 0`), which meant a quiet
                // instance could hold thousands of expired mid:* entries
                // indefinitely (49 messages had to arrive before any
                // cleanup, regardless of how stale the existing entries
                // were). Switched to a wall-clock interval — at most one
                // sweep per `MSG_DEDUP_CLEANUP_INTERVAL` regardless of
                // message volume, and a sweep is guaranteed within the
                // interval after the next message arrives.
                {
                    const MSG_DEDUP_CLEANUP_INTERVAL: std::time::Duration =
                        std::time::Duration::from_secs(30);
                    static LAST_CLEANUP_NANOS: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    static CLEANUP_EPOCH: std::sync::LazyLock<std::time::Instant> =
                        std::sync::LazyLock::new(std::time::Instant::now);
                    let elapsed_since_epoch_nanos =
                        now.duration_since(*CLEANUP_EPOCH).as_nanos() as u64;
                    let last = LAST_CLEANUP_NANOS.load(std::sync::atomic::Ordering::Relaxed);
                    let should_sweep = last == 0
                        || elapsed_since_epoch_nanos.saturating_sub(last)
                            >= MSG_DEDUP_CLEANUP_INTERVAL.as_nanos() as u64;
                    if should_sweep
                        && LAST_CLEANUP_NANOS
                            .compare_exchange(
                                last,
                                elapsed_since_epoch_nanos,
                                std::sync::atomic::Ordering::Relaxed,
                                std::sync::atomic::Ordering::Relaxed,
                            )
                            .is_ok()
                    {
                        data.shared.dispatch.intake_dedup.retain(|k, v| {
                            if k.starts_with("mid:") {
                                now.duration_since(v.0) < MSG_DEDUP_TTL
                            } else {
                                true // non-mid entries are cleaned by their own path
                            }
                        });
                    }
                }

                // Check if this arrival is from a thread context
                let thread_parent = resolve_thread_parent(&ctx.http, new_message.channel_id).await;
                let is_thread_context = thread_parent.is_some();

                // #2044 F6: when promoting a parent → thread arrival, verify
                // that the first (parent) arrival did NOT already make it
                // into the mailbox. Otherwise the second (thread) arrival
                // would produce a double intake for the same user_msg_id
                // — same response sent twice, dispatch automation steps
                // re-executed, etc. The first arrival can sneak through
                // the parent path because regular text passes
                // `should_process_turn_message` (`Regular|InlineReply`)
                // and the dispatch-thread guard only fires for
                // `is_allowed_bot`. We trust the mailbox as the source
                // of truth.
                let mut thread_promotion_blocked = false;
                let is_dup = match data.shared.dispatch.intake_dedup.entry(key.clone()) {
                    dashmap::mapref::entry::Entry::Occupied(mut e) => {
                        let (ts, was_thread) = *e.get();
                        if now.duration_since(ts) >= MSG_DEDUP_TTL {
                            // Entry expired — treat as new
                            e.insert((now, is_thread_context));
                            false
                        } else if is_thread_context && !was_thread {
                            // Thread event for a message previously seen via parent —
                            // allow thread through ONLY if the parent path
                            // did not already create mailbox state for
                            // this message_id.
                            let parent_channel = thread_parent
                                .as_ref()
                                .map(|(parent_id, _)| *parent_id)
                                .unwrap_or(new_message.channel_id);
                            let snapshot = mailbox_snapshot(&data.shared, parent_channel).await;
                            let already_intake = snapshot.active_user_message_id
                                == Some(new_message.id)
                                || snapshot.intervention_queue.iter().any(|iv| {
                                    iv.message_id == new_message.id
                                        || iv.source_message_ids.contains(&new_message.id)
                                });
                            if already_intake {
                                thread_promotion_blocked = true;
                                // Mark thread-processed so subsequent duplicates are no-ops.
                                e.insert((now, true));
                                true
                            } else {
                                e.insert((now, true));
                                false
                            }
                        } else {
                            true // genuine duplicate (same context or already thread-processed)
                        }
                    }
                    dashmap::mapref::entry::Entry::Vacant(e) => {
                        e.insert((now, is_thread_context));
                        false
                    }
                };
                if thread_promotion_blocked {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ⏭ MSG-DEDUP: blocking thread-promotion of message {} in channel {} (parent path already intook it)",
                        new_message.id,
                        new_message.channel_id
                    );
                    return Ok(());
                }
                if is_dup {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ⏭ MSG-DEDUP: skipping duplicate message {} in channel {}",
                        new_message.id,
                        new_message.channel_id
                    );
                    return Ok(());
                }
            }

            if !should_process_turn_message(new_message.kind) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏭ MSG-KIND: skipping {:?} message {} in channel {}",
                    new_message.kind,
                    new_message.id,
                    new_message.channel_id
                );
                return Ok(());
            }

            let current_bot_id = ctx.cache.current_user().id;
            if should_skip_self_authored_turn_message(new_message.author.id, current_bot_id) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏭ SELF-AUTHORED: skipping message {} in channel {} authored by current bot",
                    new_message.id,
                    new_message.channel_id
                );
                return Ok(());
            }

            let announce_bot_id = super::super::resolve_announce_bot_user_id(&data.shared).await;

            // Ignore bot messages, unless they are allowed bot traffic or the
            // announce bot used by agent handoffs. Some utility bot deliveries
            // are identified by explicit author ID even when Discord does not
            // mark the sender as `bot`, so a second text-level gate runs later
            // once we have the full message content.
            if new_message.author.bot {
                let allowed = {
                    let settings = data.shared.settings.read().await;
                    bot_author_allowed_for_live_intake(
                        &settings.allowed_bot_ids,
                        announce_bot_id,
                        new_message.author.id.get(),
                    )
                };
                if !allowed {
                    return Ok(());
                }
            }

            // Registered slash commands are handled by poise interactions.
            // Unknown `/...` text should fall through to the AI provider.
            if !new_message.author.bot
                && should_skip_human_slash_message(
                    &new_message.content,
                    data.shared.known_slash_commands.get(),
                )
            {
                return Ok(());
            }

            // Ignore messages that mention other (human) users — not directed at
            // this bot.  Bot mentions are excluded because Discord auto-adds the
            // replied-to author to the mentions array for InlineReply messages;
            // filtering on those would silently drop legitimate replies to
            // announce/notify/codex bot messages.
            if !new_message.mentions.is_empty() {
                let mentions_other_humans = new_message
                    .mentions
                    .iter()
                    .any(|u| u.id != current_bot_id && !u.bot);
                if mentions_other_humans {
                    return Ok(());
                }
            }

            let user_id = new_message.author.id;
            let user_name = &new_message.author.name;
            let channel_id = new_message.channel_id;
            let is_dm = new_message.guild_id.is_none();
            let effective_channel_id = resolve_thread_parent(&ctx.http, channel_id)
                .await
                .map(|(parent_id, _)| parent_id)
                .unwrap_or(channel_id);
            let settings_snapshot = { data.shared.settings.read().await.clone() };
            // #2266: resolve the voice-transcript payload ONCE at the
            // intake-gate so queue commits can classify voice messages and
            // keep them as standalone queue entries. The transaction helper
            // strips the accepted-replay payload before enqueue; the eventual
            // dispatch re-resolves and claims the durable row from the readable
            // announcement text. Resolution is non-consuming: local store,
            // durable PG row, then a short pending-key wait for the
            // gateway-before-send-response race. Legacy hidden metadata is
            // deliberately not trusted here; the durable/ref path is the
            // authority for new runtime routing.
            let resolved_voice_announcement = resolve_voice_transcript_announcement_for_intake(
                data.shared.pg_pool.as_ref(),
                channel_id,
                new_message.id,
                user_id,
                announce_bot_id,
                &new_message.content,
            )
            .await;
            let is_voice_transcript_announcement = resolved_voice_announcement.is_some();
            if !is_voice_transcript_announcement
                && validate_live_channel_routing_with_dm_hint(
                    ctx,
                    &data.provider,
                    &settings_snapshot,
                    channel_id,
                    Some(is_dm),
                )
                .await
                .is_err()
            {
                return Ok(());
            }
            if !is_voice_transcript_announcement
                && should_skip_for_missing_required_mention(
                    &settings_snapshot,
                    effective_channel_id,
                    is_dm,
                    &new_message.content,
                    ctx.cache.current_user().id,
                )
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏭ MENTION-GUARD: skipping message {} in channel {} (effective {}) because bot mention is required",
                    new_message.id,
                    channel_id,
                    effective_channel_id,
                );
                return Ok(());
            }
            if !is_voice_transcript_announcement
                && data
                    .shared
                    .voice_barge_in
                    .try_handle_voice_channel_text_reply(
                        &ctx.http,
                        channel_id,
                        &new_message.content,
                    )
                    .await
            {
                return Ok(());
            }
            if !is_dm && !is_voice_transcript_announcement {
                match resolve_runtime_channel_binding_status(&ctx.http, effective_channel_id).await
                {
                    RuntimeChannelBindingStatus::Owned => {}
                    RuntimeChannelBindingStatus::Unowned => {
                        if can_route_unbound_direct_session(
                            data,
                            ctx,
                            channel_id,
                            effective_channel_id,
                            is_dm,
                        )
                        .await
                        {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::info!(
                                "  [{ts}] ↪ BINDING-GUARD: allowing unbound channel {} (effective {}) because a direct session exists",
                                channel_id,
                                effective_channel_id
                            );
                        } else {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::info!(
                                "  [{ts}] ⏭ BINDING-GUARD: skipping message {} in unbound channel {} (effective {})",
                                new_message.id,
                                channel_id,
                                effective_channel_id
                            );
                            return Ok(());
                        }
                    }
                    RuntimeChannelBindingStatus::Unknown => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⏭ BINDING-GUARD: skipping message {} because channel binding lookup failed for {} (effective {})",
                            new_message.id,
                            channel_id,
                            effective_channel_id
                        );
                        return Ok(());
                    }
                }
            }

            let raw_text = new_message.content.trim();
            let (sanitized_text, has_monitor_auto_turn_origin) =
                super::super::strip_monitor_auto_turn_origin(raw_text);
            let text = sanitized_text.trim();

            let is_allowed_bot_sender = bot_author_allowed_for_live_intake(
                &settings_snapshot.allowed_bot_ids,
                announce_bot_id,
                user_id.get(),
            );
            if is_allowed_bot_sender
                && !super::super::is_allowed_turn_sender(
                    &settings_snapshot.allowed_bot_ids,
                    announce_bot_id,
                    user_id.get(),
                    new_message.author.bot,
                    raw_text,
                )
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏭ BOT-INTAKE: skipping non-turn bot message {} in channel {}",
                    new_message.id,
                    channel_id
                );
                return Ok(());
            }

            // Auth check (allowed bots bypass auth)
            let is_allowed_bot = is_allowed_bot_sender;
            if !is_allowed_bot && !check_auth(user_id, user_name, &data.shared, &data.token).await {
                return Ok(());
            }
            if let Some(stale) =
                super::super::stale_dispatch_turn_for_text(data.shared.pg_pool.as_ref(), text).await
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⏭ DISPATCH-GUARD: skipped terminal dispatch message {} in channel {} (dispatch={}, status={})",
                    new_message.id,
                    channel_id,
                    stale.dispatch_id,
                    stale.status
                );
                super::super::advance_last_message_checkpoint(
                    &data.shared,
                    &data.provider,
                    channel_id,
                    new_message.id,
                );
                let emoji = super::super::queue_exit_feedback_emoji(stale.queue_exit_kind);
                queue_marker::note_exit_feedback_added(
                    &data.shared,
                    &ctx.http,
                    channel_id,
                    new_message.id,
                    emoji,
                )
                .await;
                return Ok(());
            }
            // #3148: the idle-recap card clear (and the per-channel
            // turn-generation bump) was RELOCATED from here to
            // `intake_turn::handle_text_message`, immediately AFTER the mailbox
            // claim succeeds (`started == true`), mirroring the TUI path
            // (`tui_prompt_relay` claim → bump → clear). Clearing at intake
            // time — BEFORE the later mailbox claim — was not truly
            // capture-at-claim: a recap POST could recheck-idle while intake had
            // captured old/none but the claim had not happened, persist a fresh
            // card, and the old-id-keyed clear could not remove it (Window 2).
            // Performing the clear after the claim (and after the claim's
            // generation bump) closes that window with the same capture-at-claim
            // semantics the TUI path already has.

            // #189: Generic DM reply tracking — consume pending entry if present.
            // Keep this after auth so unauthorized DM senders cannot inject
            // answers into pending workflows.
            // Consumed DM answers must stop here; falling through into normal
            // message handling produces a bogus "No active session" error in DMs.
            if !text.is_empty() {
                if try_handle_pending_dm_reply(data.shared.pg_pool.as_ref(), new_message).await {
                    return Ok(());
                }
            }

            // Handle file attachments — download regardless of session state.
            // For thread messages, bootstrap the thread session before saving so
            // upload context attaches to the eventual turn instead of being
            // dropped while only the parent session exists.
            let upload_records = if !new_message.attachments.is_empty() {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ◀ [{user_name}] Upload: {} file(s)",
                    new_message.attachments.len()
                );
                auto_restore_session_with_dm_hint(&data.shared, channel_id, ctx, Some(is_dm)).await;
                if effective_channel_id != channel_id {
                    let needs_parent = {
                        let d = data.shared.core.lock().await;
                        !d.sessions.contains_key(&channel_id)
                    };
                    if needs_parent {
                        auto_restore_session(&data.shared, effective_channel_id, ctx).await;
                        let parent_path = {
                            let d = data.shared.core.lock().await;
                            d.sessions
                                .get(&effective_channel_id)
                                .and_then(|s| s.current_path.clone())
                        };
                        if let Some(path) = parent_path {
                            bootstrap_thread_session(
                                &data.shared,
                                channel_id,
                                &path,
                                &ctx.http,
                                Some(&ctx.cache),
                            )
                            .await;
                        }
                    }
                }
                super::message_handler::handle_file_upload(ctx, new_message, &data.shared).await?
            } else {
                Vec::new()
            };
            record_upload_history(&data.shared, channel_id, &upload_records).await;
            let mut upload_records_appended_to_session = false;

            let attachment_only_turn =
                should_start_attachment_only_turn(text, upload_records.len());
            let text = if attachment_only_turn { "" } else { text };
            if text.is_empty() && !attachment_only_turn {
                return Ok(());
            }

            if has_monitor_auto_turn_origin {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏭ MONITOR-AUTO-TURN: dropping bot-authored monitor relay {} in channel {}",
                    new_message.id,
                    channel_id
                );
                return Ok(());
            }

            // ── Text commands (!start, !meeting, !stop, !clear) ──
            // Strip leading bot mention to get the actual command text.
            //
            // #2044 F11: the helper uses a constant regex compiled once via
            // `LazyLock`, avoiding a per-message compile cost in the hot path.
            let cmd_text = strip_leading_bot_mention(text);
            if cmd_text.starts_with('!') {
                // Skill prompts enter central admission with their upload paths
                // still in the submission. Do not inject them into this
                // gateway's local session before owner routing has admitted
                // local execution.
                let mut command_parts = cmd_text.split_whitespace();
                let command = command_parts.next().unwrap_or_default();
                let skill_name = command_parts.next().unwrap_or_default();
                let is_skill_command = matches!(command, "!skill" | "!cc")
                    && !skill_name.is_empty()
                    && !matches!(
                        skill_name,
                        "clear" | "stop" | "pwd" | "health" | "status" | "inflight" | "help"
                    );
                if !is_skill_command {
                    upload_records_appended_to_session =
                        append_pending_uploads(&data.shared, channel_id, &upload_records).await;
                }
                let handled = super::message_handler::handle_text_command(
                    ctx,
                    new_message,
                    &data,
                    channel_id,
                    &cmd_text,
                    &upload_records,
                )
                .await?;
                if handled {
                    if !is_skill_command && !upload_records_appended_to_session {
                        let _ =
                            append_pending_uploads(&data.shared, channel_id, &upload_records).await;
                    }
                    return Ok(());
                }
            }

            // Auto-restore session (for threads, fall back to parent channel's session)
            auto_restore_session_with_dm_hint(
                &data.shared,
                channel_id,
                ctx,
                Some(new_message.guild_id.is_none()),
            )
            .await;
            if effective_channel_id != channel_id {
                // Thread: if no session found for thread, try to bootstrap from parent
                let needs_parent = {
                    let d = data.shared.core.lock().await;
                    !d.sessions.contains_key(&channel_id)
                };
                if needs_parent {
                    auto_restore_session(&data.shared, effective_channel_id, ctx).await;
                    // Clone parent session's path for the thread
                    let parent_path = {
                        let d = data.shared.core.lock().await;
                        d.sessions
                            .get(&effective_channel_id)
                            .and_then(|s| s.current_path.clone())
                    };
                    if let Some(path) = parent_path {
                        bootstrap_thread_session(
                            &data.shared,
                            channel_id,
                            &path,
                            &ctx.http,
                            Some(&ctx.cache),
                        )
                        .await;
                    }
                }
            }

            // ── Intake-level dedup guard ──────────────────────────────────
            // Prevents the same bot dispatch from starting two parallel turns
            // when Discord delivers the message twice in rapid succession.
            if is_allowed_bot {
                let dedup_key =
                    if let Some(dispatch_id) = super::super::adk_session::parse_dispatch_id(text) {
                        // Same dispatch_id = genuine duplicate (Discord retry)
                        format!("dispatch:{}", dispatch_id)
                    } else {
                        // Use Discord message_id as dedup key — each message is unique
                        // This prevents false-positive dedup of different bot messages
                        // with similar text content
                        format!("msg:{}", new_message.id)
                    };

                const INTAKE_DEDUP_TTL: std::time::Duration = std::time::Duration::from_secs(30);
                let now = std::time::Instant::now();

                // Lazy cleanup: remove expired bot-specific entries.
                // Skip mid:* entries — they use a longer TTL and are cleaned
                // separately in the universal dedup section above.
                data.shared.dispatch.intake_dedup.retain(|k, v| {
                    if k.starts_with("mid:") {
                        true // preserved; cleaned by universal dedup cleanup
                    } else {
                        now.duration_since(v.0) < INTAKE_DEDUP_TTL
                    }
                });

                // Atomic check+insert via entry() — holds shard lock so two
                // simultaneous arrivals cannot both see a miss.
                let is_duplicate = match data.shared.dispatch.intake_dedup.entry(dedup_key.clone())
                {
                    dashmap::mapref::entry::Entry::Occupied(e) => {
                        now.duration_since(e.get().0) < INTAKE_DEDUP_TTL
                    }
                    dashmap::mapref::entry::Entry::Vacant(e) => {
                        e.insert((now, false));
                        false
                    }
                };
                if is_duplicate {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ⏭ DEDUP: skipping duplicate intake in channel {} (key={})",
                        channel_id,
                        dedup_key
                    );
                    return Ok(());
                }
            }

            let has_reply_boundary = new_message.message_reference.is_some();
            let reply_context = if has_reply_boundary {
                build_reply_context(ctx, channel_id, &new_message).await
            } else {
                None
            };
            let merge_consecutive = upload_records.is_empty()
                && should_merge_consecutive_messages(text, is_allowed_bot);

            // ── Dispatch-thread guard ─────────────────────────────────
            // When a dispatch thread is active for this channel, bot messages
            // to the parent channel are queued so they don't start a parallel
            // turn (the thread's cancel_token is keyed by thread_id, leaving
            // the parent channel "unlocked").
            if is_allowed_bot {
                // #1446 — copy the mapped thread_id and immediately drop the
                // DashMap ref. `thread_guard_force_clean_stale_thread`
                // re-acquires the same shard lock to call `.remove()`; if the
                // ref were still held we would deadlock on the shard's
                // RwLock. The narrow scope below releases the ref at the `}`.
                let thread_id_opt = {
                    data.shared
                        .dispatch
                        .thread_parents
                        .get(&channel_id)
                        .map(|entry| *entry.value())
                };
                if let Some(thread_id) = thread_id_opt {
                    // Thread still has an active turn?
                    let thread_active = mailbox_has_active_turn(&data.shared, thread_id).await;
                    if thread_active {
                        // #1446 stall-deadlock recovery: a phase-gate dispatch can
                        // terminate without firing its inflight-cleanup hook,
                        // leaving the thread's mailbox + inflight state file
                        // pinned. The THREAD-GUARD then queues every parent-
                        // channel bot message forever because
                        // `mailbox_has_active_turn(thread)` keeps returning
                        // true. We require BOTH a stale `updated_at` AND a
                        // watcher-state desync signal before force-cleaning,
                        // mirroring the stall-watchdog's conjunction so a
                        // quiet-but-live long turn (e.g. mid-Bash) is never
                        // mistaken for a dead dispatch.
                        let stale_inflight = thread_guard_should_force_clean_stale_thread(
                            &data.shared,
                            &data.provider,
                            thread_id,
                            chrono::Utc::now().timestamp(),
                        )
                        .await;
                        if stale_inflight {
                            thread_guard_force_clean_stale_thread(
                                &data.shared,
                                &data.provider,
                                channel_id,
                                thread_id,
                            )
                            .await;
                            // Fall through to normal processing below.
                        } else {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::info!(
                                "  [{ts}] 🔀 THREAD-GUARD: bot message to parent {} queued (dispatch thread {} active)",
                                channel_id,
                                thread_id
                            );
                            let mut queue_effects = IntakeGateQueueEffects { ctx, data };
                            let _commit = commit_soft_intervention_transaction(
                                &mut queue_effects,
                                SoftInterventionCommitRequest {
                                    source: IntakeQueueCommitSource::ThreadGuard,
                                    author_class: IntakeQueueAuthorClass::from_flags(
                                        new_message.author.bot,
                                        is_allowed_bot,
                                    ),
                                    intervention: SoftInterventionSpec {
                                        channel_id,
                                        author_id: user_id,
                                        author_is_bot: new_message.author.bot,
                                        message_id: new_message.id,
                                        text: text.to_string(),
                                        reply_context: None,
                                        has_reply_boundary: false,
                                        merge_consecutive: false,
                                        pending_uploads: upload_records.clone(),
                                        // #2266: thread-guard queue path —
                                        // pass resolved voice metadata only so
                                        // the transaction can detect voice and
                                        // enqueue standalone readable text.
                                        voice_announcement: resolved_voice_announcement.clone(),
                                    },
                                    options: IntakeQueueCommitOptions::default(),
                                },
                            )
                            .await;
                            return Ok(());
                        }
                    } else {
                        // Thread turn finished — clean up stale mapping
                        data.shared.dispatch.thread_parents.remove(&channel_id);
                    }
                }
            }

            // ── Dispatch collision guard ────────────────────────────────
            // When a DISPATCH: message arrives on a channel that already has
            // an active turn (inflight), queue it as an intervention instead
            // of starting a parallel turn that would stomp the current
            // placeholder.
            if text.starts_with("DISPATCH:") {
                if mailbox_has_live_active_turn_or_cleanup_stale_proof(
                    &data.shared,
                    &data.provider,
                    channel_id,
                )
                .await
                {
                    let mut queue_effects = IntakeGateQueueEffects { ctx, data };
                    let commit = commit_soft_intervention_transaction(
                        &mut queue_effects,
                        SoftInterventionCommitRequest {
                            source: IntakeQueueCommitSource::DispatchGuard,
                            author_class: IntakeQueueAuthorClass::from_flags(
                                new_message.author.bot,
                                is_allowed_bot,
                            ),
                            intervention: SoftInterventionSpec {
                                channel_id,
                                author_id: user_id,
                                author_is_bot: new_message.author.bot,
                                message_id: new_message.id,
                                text: text.to_string(),
                                reply_context: None,
                                has_reply_boundary: false,
                                merge_consecutive: false,
                                pending_uploads: upload_records.clone(),
                                // #2266: DISPATCH: collision guard — DISPATCH messages
                                // never carry voice transcripts, so this is always
                                // None. Explicit for clarity / future audits.
                                voice_announcement: None,
                            },
                            options: IntakeQueueCommitOptions::default(),
                        },
                    )
                    .await;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    if commit.accepted() {
                        tracing::info!(
                            "  [{ts}] 📬 DISPATCH-GUARD: queued dispatch message in channel {} (active turn in progress)",
                            channel_id
                        );
                    }
                    return Ok(());
                }
                // No active turn — fall through to normal processing below
            }

            // Queue messages while AI is in progress (executed as next turn after current finishes)
            if mailbox_has_live_active_turn_or_cleanup_stale_proof(
                &data.shared,
                &data.provider,
                channel_id,
            )
            .await
            {
                let is_shutting_down = data
                    .shared
                    .restart
                    .shutting_down
                    .load(std::sync::atomic::Ordering::Relaxed);
                let commit = {
                    let mut queue_effects = IntakeGateQueueEffects { ctx, data };
                    commit_soft_intervention_transaction(
                        &mut queue_effects,
                        SoftInterventionCommitRequest {
                            source: IntakeQueueCommitSource::BusyActiveTurn,
                            author_class: IntakeQueueAuthorClass::from_flags(
                                new_message.author.bot,
                                is_allowed_bot,
                            ),
                            intervention: SoftInterventionSpec {
                                channel_id,
                                author_id: user_id,
                                author_is_bot: new_message.author.bot,
                                message_id: new_message.id,
                                text: text.to_string(),
                                reply_context: reply_context.clone(),
                                has_reply_boundary,
                                merge_consecutive,
                                pending_uploads: upload_records.clone(),
                                // #2266: main busy-active-turn queue path — voice transcripts that
                                // arrive while a previous turn is running flow through here. Embed the
                                // announcement so the queued dispatch reinserts it into the store even
                                // if the >30s in-memory TTL expires first.
                                voice_announcement: resolved_voice_announcement.clone(),
                            },
                            options: IntakeQueueCommitOptions::default(),
                        },
                    )
                    .await
                };
                let enqueued = commit.accepted();

                if !commit.accepted() {
                    if commit.failed() {
                        rate_limit_wait(&data.shared, channel_id).await;
                        let _ = channel_id
                            .say(
                                &ctx.http,
                                "⚠️ 메시지 큐 저장 중 오류가 감지되어 접수 표시를 생략했어.",
                            )
                            .await;
                        return Ok(());
                    }
                    if busy_duplicate_notice::silence_if_already_queued(
                        commit.refusal_reason(),
                        new_message.id,
                        channel_id,
                    ) {
                        return Ok(());
                    }
                    rate_limit_wait(&data.shared, channel_id).await;
                    let _ = channel_id
                        .say(&ctx.http, "↪ 같은 메시지가 방금 이미 큐잉되어서 무시했어.")
                        .await;
                    return Ok(());
                }

                if !is_allowed_bot {
                    render_visible_queued_ack(
                        ctx,
                        data,
                        channel_id,
                        new_message.id,
                        text,
                        commit.merged(),
                    )
                    .await;
                }

                if is_shutting_down && commit.checkpoint_advanced() {
                    let ids: std::collections::HashMap<u64, u64> = data
                        .shared
                        .last_message_ids
                        .iter()
                        .map(|entry| (entry.key().get(), *entry.value()))
                        .collect();
                    runtime_store::save_all_last_message_ids(data.provider.as_str(), &ids);
                }
                let has_blocking = crate::services::discord::mailbox_has_blocking_active_turn(
                    &data.shared,
                    channel_id,
                )
                .await;
                if should_schedule_post_enqueue_idle_drain(enqueued, has_blocking) {
                    crate::services::discord::schedule_deferred_idle_queue_kickoff(
                        data.shared.clone(),
                        data.provider.clone(),
                        channel_id,
                        "busy-active enqueue idle drain",
                    );
                }
                return Ok(());
            }

            // Reconcile gate (#122): until startup recovery is complete, queue messages.
            if !data
                .shared
                .restart
                .reconcile_done
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                let mut queue_effects = IntakeGateQueueEffects { ctx, data };
                let _commit = commit_soft_intervention_transaction(
                    &mut queue_effects,
                    SoftInterventionCommitRequest {
                        source: IntakeQueueCommitSource::ReconcileGate,
                        author_class: IntakeQueueAuthorClass::from_flags(
                            new_message.author.bot,
                            is_allowed_bot,
                        ),
                        intervention: SoftInterventionSpec {
                            channel_id,
                            author_id: user_id,
                            author_is_bot: new_message.author.bot,
                            message_id: new_message.id,
                            text: text.to_string(),
                            reply_context: reply_context.clone(),
                            has_reply_boundary,
                            merge_consecutive,
                            pending_uploads: upload_records.clone(),
                            // #2266: reconcile gate — startup-recovery queue
                            // path. Resolved voice metadata makes this a
                            // standalone queued voice entry; durable claim
                            // remains deferred to dispatch.
                            voice_announcement: resolved_voice_announcement.clone(),
                        },
                        options: IntakeQueueCommitOptions {
                            pending_reaction: IntakeQueuePendingReactionPolicy::Static(
                                queue_reactions::QUEUE_RECONCILE_PENDING_REACTION,
                            ),
                            ..IntakeQueueCommitOptions::default()
                        },
                    },
                )
                .await;
                return Ok(());
            }

            // Drain mode: when restart is pending, queue new messages instead of
            // starting new turns. This ensures only existing turns drain to completion.
            if data
                .shared
                .restart
                .restart_pending
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                let is_shutting_down = data
                    .shared
                    .restart
                    .shutting_down
                    .load(std::sync::atomic::Ordering::Relaxed);

                let mut queue_effects = IntakeGateQueueEffects { ctx, data };
                let commit = commit_soft_intervention_transaction(
                    &mut queue_effects,
                    SoftInterventionCommitRequest {
                        source: IntakeQueueCommitSource::DrainMode,
                        author_class: IntakeQueueAuthorClass::from_flags(
                            new_message.author.bot,
                            is_allowed_bot,
                        ),
                        intervention: SoftInterventionSpec {
                            channel_id,
                            author_id: user_id,
                            author_is_bot: new_message.author.bot,
                            message_id: new_message.id,
                            text: text.to_string(),
                            reply_context: reply_context.clone(),
                            has_reply_boundary,
                            merge_consecutive,
                            pending_uploads: upload_records.clone(),
                            // #2266: drain-mode queue path (restart pending) —
                            // pass resolved voice metadata so the transaction
                            // keeps readable voice text standalone.
                            voice_announcement: resolved_voice_announcement.clone(),
                        },
                        options: IntakeQueueCommitOptions::default(),
                    },
                )
                .await;

                let ts = chrono::Local::now().format("%H:%M:%S");
                if !commit.accepted() {
                    tracing::info!(
                        "  [{ts}] ⏸ DRAIN: message from [{user_name}] in channel {} was not accepted into restart queue",
                        channel_id
                    );
                    return Ok(());
                }
                tracing::info!(
                    "  [{ts}] ⏸ DRAIN: queued message from [{user_name}] in channel {} (restart pending)",
                    channel_id
                );

                if is_shutting_down && commit.checkpoint_advanced() {
                    // Persist checkpoint to disk immediately during shutdown
                    let ids: std::collections::HashMap<u64, u64> = data
                        .shared
                        .last_message_ids
                        .iter()
                        .map(|entry| (entry.key().get(), *entry.value()))
                        .collect();
                    runtime_store::save_all_last_message_ids(data.provider.as_str(), &ids);
                } else {
                    rate_limit_wait(&data.shared, channel_id).await;
                    let _ = channel_id
                        .say(
                            &ctx.http,
                            "⏸ 재시작 대기 중 — 메시지가 큐에 저장되었고, 재시작 후 처리됩니다.",
                        )
                        .await;
                }
                return Ok(());
            }

            // Idle backlog guard: if older queued messages are still pending on an
            // otherwise-idle channel, keep FIFO order by queuing this message behind
            // them and re-triggering idle queue kickoff instead of letting this turn
            // jump ahead.
            let queued_behind_idle_backlog = {
                let has_active_turn = mailbox_has_active_turn(&data.shared, channel_id).await;
                let has_pending_backlog =
                    mailbox_has_pending_soft_queue(&data.shared, &data.provider, channel_id)
                        .await
                        .has_pending;
                if has_active_turn || !has_pending_backlog {
                    None
                } else {
                    let mut queue_effects = IntakeGateQueueEffects { ctx, data };
                    Some(
                        commit_soft_intervention_transaction(
                            &mut queue_effects,
                            SoftInterventionCommitRequest {
                                source: IntakeQueueCommitSource::IdleBacklog,
                                author_class: IntakeQueueAuthorClass::from_flags(
                                    new_message.author.bot,
                                    is_allowed_bot,
                                ),
                                intervention: SoftInterventionSpec {
                                    channel_id,
                                    author_id: user_id,
                                    author_is_bot: new_message.author.bot,
                                    message_id: new_message.id,
                                    text: text.to_string(),
                                    reply_context: reply_context.clone(),
                                    has_reply_boundary,
                                    merge_consecutive,
                                    pending_uploads: upload_records.clone(),
                                    // #2266: queued-behind-idle-backlog path —
                                    // FIFO ordering keeps voice transcripts behind
                                    // pre-existing queue items; the transaction
                                    // keeps voice text standalone and defers the
                                    // durable claim until dispatch.
                                    voice_announcement: resolved_voice_announcement.clone(),
                                },
                                options: IntakeQueueCommitOptions::idle_backlog(),
                            },
                        )
                        .await,
                    )
                }
            };
            if let Some(commit) = queued_behind_idle_backlog {
                let ts = chrono::Local::now().format("%H:%M:%S");
                if commit.accepted() {
                    tracing::info!(
                        "  [{ts}] 📬 IDLE-QUEUE: queued message from [{user_name}] in channel {} behind pending backlog",
                        channel_id
                    );
                } else {
                    tracing::info!(
                        "  [{ts}] ↪ IDLE-QUEUE: message from [{user_name}] was not accepted into channel {} backlog",
                        channel_id
                    );
                }
                return Ok(());
            }

            // Meeting command from text (e.g. announce bot sending "/meeting start ...")
            if text.starts_with("/meeting ") {
                if !upload_records_appended_to_session {
                    upload_records_appended_to_session =
                        append_pending_uploads(&data.shared, channel_id, &upload_records).await;
                }
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ◀ [{user_name}] Meeting cmd: {text}");
                let http = ctx.http.clone();
                if meeting::handle_meeting_command(
                    http,
                    channel_id,
                    text,
                    data.provider.clone(),
                    &data.shared,
                )
                .await?
                {
                    if !upload_records_appended_to_session {
                        let _ =
                            append_pending_uploads(&data.shared, channel_id, &upload_records).await;
                    }
                    return Ok(());
                }
            }

            // Shell command shortcut
            if text.starts_with('!') {
                if !upload_records_appended_to_session {
                    let _ = append_pending_uploads(&data.shared, channel_id, &upload_records).await;
                }
                let ts = chrono::Local::now().format("%H:%M:%S");
                let preview = truncate_str(text, 60);
                tracing::info!("  [{ts}] ◀ [{user_name}] Shell: {preview}");
                super::message_handler::handle_shell_command_raw(
                    ctx,
                    channel_id,
                    text,
                    &data.shared,
                )
                .await?;
                return Ok(());
            }

            // Regular text → Claude AI
            let ts = chrono::Local::now().format("%H:%M:%S");
            let preview = truncate_str(text, 60);
            tracing::info!("  [{ts}] ◀ [{user_name}] {preview}");

            // Checkpoint: message about to be processed as a turn
            // (#2044 F12 — monotonic).
            super::super::advance_last_message_checkpoint(
                &data.shared,
                &data.provider,
                channel_id,
                new_message.id,
            );

            // #796: classify the originating sender so the race handler in
            // `handle_text_message` knows whether it's safe to delete the
            // placeholder when the new turn loses to an in-flight one. Notify-
            // bot deliveries are background-task notifications whose
            // placeholder content is the only visible record of the event;
            // foreground (human) messages keep the legacy delete-on-loss
            // behavior.
            let notify_bot_id = super::super::resolve_notify_bot_user_id(&data.shared).await;
            let turn_kind = super::message_handler::classify_turn_kind_from_author(
                user_id.get(),
                notify_bot_id,
            );

            let deps = super::message_handler::IntakeDeps {
                http: &ctx.http,
                cache: Some(&ctx.cache),
                ctx_for_chained_dispatch: Some(ctx),
                shared: &data.shared,
                token: &data.token,
            };
            let preloaded_uploads = if upload_records_appended_to_session {
                Vec::new()
            } else {
                upload_records.clone()
            };
            let submission = super::IntakeSubmission {
                provider: data.provider.clone(),
                request: super::IntakeRequest {
                    channel_id,
                    user_msg_id: new_message.id,
                    request_owner: user_id,
                    request_owner_name: user_name.to_string(),
                    user_text: text.to_string(),
                    reply_to_user_message: false,
                    defer_watcher_resume: false,
                    wait_for_completion: false,
                    merge_consecutive,
                    reply_context,
                    has_reply_boundary,
                    dm_hint: Some(is_dm),
                    turn_kind,
                },
                origin: super::IntakeOrigin::LiveMessage,
                has_nonportable_uploads: !new_message.attachments.is_empty(),
                preloaded_uploads,
                // #3905: carry the gate's already-authorized, non-consuming
                // voice resolution into direct dispatch.
                voice_announcement: resolved_voice_announcement,
            };
            super::dispatch_text_intake(&deps, submission).await?;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod reply_context_tests {
    use super::gate::{should_start_attachment_only_turn, strip_leading_bot_mention};
    use super::{AttachmentReplyItem, format_attachment_reply_context};

    #[test]
    fn attachment_reply_context_keeps_canonical_message_id_and_all_files() {
        let attachments = (1..=5)
            .map(|index| AttachmentReplyItem {
                filename: format!("photo-{index}.png"),
                size: 1024 * index,
                description: (index == 3).then_some("middle attachment".to_string()),
            })
            .collect::<Vec<_>>();

        let context = format_attachment_reply_context("사용자", 1500, &attachments);

        assert!(context.contains("Canonical Discord message id: 1500"));
        assert!(context.contains("photo-1.png"));
        assert!(context.contains("photo-3.png"));
        assert!(context.contains("middle attachment"));
        assert!(context.contains("photo-5.png"));
    }

    #[test]
    fn attachment_only_empty_check_ignores_leading_bot_mention() {
        assert_eq!(strip_leading_bot_mention("<@123456789>   "), "");
        assert_eq!(strip_leading_bot_mention("<@!123456789> look"), "look");
    }

    #[test]
    fn attachment_only_turn_accepts_any_saved_file_without_prompt() {
        assert!(should_start_attachment_only_turn("", 1));
        assert!(should_start_attachment_only_turn("<@123456789>   ", 1));
        assert!(!should_start_attachment_only_turn("please inspect", 1));
        assert!(!should_start_attachment_only_turn("", 0));
    }
}
