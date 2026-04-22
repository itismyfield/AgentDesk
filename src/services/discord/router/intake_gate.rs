use super::super::*;

pub(in crate::services::discord) fn should_process_turn_message(
    kind: serenity::model::channel::MessageType,
) -> bool {
    matches!(
        kind,
        serenity::model::channel::MessageType::Regular
            | serenity::model::channel::MessageType::InlineReply
    )
}

pub(super) fn content_has_explicit_user_mention(content: &str, user_id: serenity::UserId) -> bool {
    let raw_id = user_id.get();
    content.contains(&format!("<@{raw_id}>")) || content.contains(&format!("<@!{raw_id}>"))
}

pub(super) fn should_skip_for_missing_required_mention(
    settings: &DiscordBotSettings,
    effective_channel_id: serenity::ChannelId,
    is_dm: bool,
    content: &str,
    bot_user_id: serenity::UserId,
) -> bool {
    !is_dm
        && settings
            .require_mention_channel_ids
            .contains(&effective_channel_id.get())
        && !content_has_explicit_user_mention(content, bot_user_id)
}

fn should_skip_human_slash_message(
    content: &str,
    known_slash_commands: Option<&std::collections::HashSet<String>>,
) -> bool {
    if !content.starts_with('/') {
        return false;
    }

    let command_name = content[1..].split_whitespace().next().unwrap_or("");
    if command_name.is_empty() {
        return false;
    }

    known_slash_commands.is_some_and(|set| set.contains(command_name))
}

fn build_soft_intervention(
    author_id: serenity::UserId,
    message_id: serenity::MessageId,
    text: &str,
    reply_context: Option<String>,
    has_reply_boundary: bool,
    merge_consecutive: bool,
) -> Intervention {
    Intervention {
        author_id,
        message_id,
        source_message_ids: vec![message_id],
        text: text.to_string(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context,
        has_reply_boundary,
        merge_consecutive,
    }
}

async fn enqueue_soft_intervention(
    data: &Data,
    channel_id: serenity::ChannelId,
    author_id: serenity::UserId,
    message_id: serenity::MessageId,
    text: &str,
    reply_context: Option<String>,
    has_reply_boundary: bool,
    merge_consecutive: bool,
) -> bool {
    mailbox_enqueue_intervention(
        &data.shared,
        &data.provider,
        channel_id,
        build_soft_intervention(
            author_id,
            message_id,
            text,
            reply_context,
            has_reply_boundary,
            merge_consecutive,
        ),
    )
    .await
}

#[cfg(test)]
pub(super) async fn enqueue_soft_intervention_for_test(
    shared: &std::sync::Arc<SharedData>,
    channel_id: serenity::ChannelId,
    author_id: serenity::UserId,
    message_id: serenity::MessageId,
    text: &str,
) -> bool {
    mailbox_enqueue_intervention(
        shared,
        &ProviderKind::Codex,
        channel_id,
        build_soft_intervention(author_id, message_id, text, None, false, false),
    )
    .await
}

fn should_merge_consecutive_messages(text: &str, is_allowed_bot: bool) -> bool {
    !is_allowed_bot
        && !text.starts_with('!')
        && !text.starts_with('/')
        && !text.starts_with("DISPATCH:")
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
        format!("[Reply to {}'s message (no text content)]", ref_author)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RemovedControlReaction {
    CancelQueuedTurn,
    StopActiveTurn,
}

pub(super) fn classify_removed_control_reaction(
    emoji: &serenity::ReactionType,
) -> Option<RemovedControlReaction> {
    match emoji {
        serenity::ReactionType::Unicode(value) if value == "📬" => {
            Some(RemovedControlReaction::CancelQueuedTurn)
        }
        serenity::ReactionType::Unicode(value) if value == "⏳" => {
            Some(RemovedControlReaction::StopActiveTurn)
        }
        _ => None,
    }
}

async fn send_reaction_control_reply(
    ctx: &serenity::Context,
    shared: &std::sync::Arc<SharedData>,
    channel_id: serenity::ChannelId,
    message_id: serenity::MessageId,
    content: &str,
) {
    rate_limit_wait(shared, channel_id).await;
    let _ = channel_id
        .send_message(
            &ctx.http,
            serenity::builder::CreateMessage::new()
                .reference_message((channel_id, message_id))
                .content(content),
        )
        .await;
}

async fn handle_reaction_remove(
    ctx: &serenity::Context,
    removed_reaction: &serenity::Reaction,
    data: &Data,
) -> Result<(), Error> {
    let Some(action) = classify_removed_control_reaction(&removed_reaction.emoji) else {
        return Ok(());
    };
    let Some(user_id) = removed_reaction.user_id else {
        return Ok(());
    };
    if user_id == ctx.cache.current_user().id {
        return Ok(());
    }

    // Ignore reactions removed by ANY bot — only human users should be able
    // to cancel turns via reaction removal. Bots (announce/notify) remove
    // reactions during dispatch status sync, which races with active turns
    // in the same thread (#670).
    // If user is not in cache, fetch from API before deciding.
    let cache_result = ctx.cache.user(user_id).map(|u| u.bot);
    let is_bot = match cache_result {
        Some(bot) => bot,
        None => {
            // Cache miss — fetch from Discord API to determine bot status
            match ctx.http.get_user(user_id).await {
                Ok(user) => user.bot,
                Err(_) => true, // API error — safe to treat as bot (ignore)
            }
        }
    };
    if is_bot {
        return Ok(());
    }

    let channel_id = removed_reaction.channel_id;
    let settings_snapshot = { data.shared.settings.read().await.clone() };
    if validate_live_channel_routing_with_dm_hint(
        ctx,
        &data.provider,
        &settings_snapshot,
        channel_id,
        Some(removed_reaction.guild_id.is_none()),
    )
    .await
    .is_err()
    {
        return Ok(());
    }

    // Reaction-removal controls must never imprint owner state.
    // Only already-authorized users may trigger queue cancel / turn stop.
    if !super::super::discord_io::user_is_authorized(&settings_snapshot, user_id.get()) {
        return Ok(());
    }

    match action {
        RemovedControlReaction::CancelQueuedTurn => {
            let removed = mailbox_cancel_soft_intervention(
                &data.shared,
                &data.provider,
                channel_id,
                removed_reaction.message_id,
            )
            .await;
            if removed.is_some() {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 📭 QUEUE-CANCEL: removed queued message {} in channel {} via reaction removal",
                    removed_reaction.message_id,
                    channel_id
                );
                send_reaction_control_reply(
                    ctx,
                    &data.shared,
                    channel_id,
                    removed_reaction.message_id,
                    "📭 Queued turn cancelled.",
                )
                .await;
            }
        }
        RemovedControlReaction::StopActiveTurn => {
            // #441: flows through cancel_text_stop_token_mailbox (mailbox_cancel_active_turn)
            // → cancel_active_token → token.cancelled triggers turn_bridge loop exit
            // → mailbox_finish_turn canonical cleanup
            let active_message_id = mailbox_snapshot(&data.shared, channel_id)
                .await
                .active_user_message_id
                .or_else(|| {
                    super::super::inflight::load_inflight_state(&data.provider, channel_id.get())
                        .map(|state| serenity::MessageId::new(state.user_msg_id))
                });
            if active_message_id != Some(removed_reaction.message_id) {
                return Ok(());
            }

            let stop_lookup =
                super::message_handler::cancel_text_stop_token_mailbox(&data.shared, channel_id)
                    .await;
            match stop_lookup {
                super::message_handler::TextStopLookup::Stop(token) => {
                    super::super::turn_bridge::cancel_active_token(
                        &token,
                        super::super::turn_bridge::TmuxCleanupPolicy::PreserveSession,
                        "reaction remove ⏳",
                    );
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 🛑 TURN-STOP: cancelled active turn for message {} in channel {} via reaction removal",
                        removed_reaction.message_id,
                        channel_id
                    );
                    super::super::commands::notify_turn_stop(
                        &ctx.http,
                        &data.shared,
                        &data.provider,
                        channel_id,
                        "reaction remove ⏳",
                    )
                    .await;
                    send_reaction_control_reply(
                        ctx,
                        &data.shared,
                        channel_id,
                        removed_reaction.message_id,
                        "Turn cancelled.",
                    )
                    .await;
                }
                super::message_handler::TextStopLookup::AlreadyStopping => {
                    send_reaction_control_reply(
                        ctx,
                        &data.shared,
                        channel_id,
                        removed_reaction.message_id,
                        "Already stopping...",
                    )
                    .await;
                }
                super::message_handler::TextStopLookup::NoActiveTurn => {}
            }
        }
    }

    Ok(())
}

pub(super) fn is_model_picker_component_custom_id(
    custom_id: &str,
    fallback_channel_id: serenity::ChannelId,
) -> bool {
    super::super::commands::parse_model_picker_custom_id(custom_id, fallback_channel_id).is_some()
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
                if is_model_picker_component_custom_id(
                    &component.data.custom_id,
                    component.channel_id,
                ) {
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
                            "  [{ts}] ⏭ COMPONENT-GUARD: skipping model picker in channel {} for provider {}",
                            component.channel_id,
                            data.provider.as_str()
                        );
                        return Ok(());
                    }
                    return handle_model_picker_interaction(ctx, component, data).await;
                }
            }
        }
        serenity::FullEvent::ReactionRemove { removed_reaction } => {
            handle_reaction_remove(ctx, removed_reaction, data).await?;
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
                // Only runs every ~50 messages to amortize cost.
                {
                    static CLEANUP_COUNTER: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let count = CLEANUP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if count % 50 == 0 {
                        data.shared.intake_dedup.retain(|k, v| {
                            if k.starts_with("mid:") {
                                now.duration_since(v.0) < MSG_DEDUP_TTL
                            } else {
                                true // non-mid entries are cleaned by their own path
                            }
                        });
                    }
                }

                // Check if this arrival is from a thread context
                let is_thread_context = resolve_thread_parent(&ctx.http, new_message.channel_id)
                    .await
                    .is_some();

                let is_dup = match data.shared.intake_dedup.entry(key.clone()) {
                    dashmap::mapref::entry::Entry::Occupied(mut e) => {
                        let (ts, was_thread) = *e.get();
                        if now.duration_since(ts) >= MSG_DEDUP_TTL {
                            // Entry expired — treat as new
                            e.insert((now, is_thread_context));
                            false
                        } else if is_thread_context && !was_thread {
                            // Thread event for a message previously seen via parent —
                            // allow thread through and mark as thread-processed.
                            e.insert((now, true));
                            false
                        } else {
                            true // genuine duplicate (same context or already thread-processed)
                        }
                    }
                    dashmap::mapref::entry::Entry::Vacant(e) => {
                        e.insert((now, is_thread_context));
                        false
                    }
                };
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

            // Ignore bot messages, unless the bot is in the allowed_bot_ids list.
            // Some utility bot deliveries are identified by explicit author ID even
            // when Discord does not mark the sender as `bot`, so a second text-level
            // gate runs later once we have the full message content.
            if new_message.author.bot {
                let allowed = {
                    let settings = data.shared.settings.read().await;
                    settings
                        .allowed_bot_ids
                        .contains(&new_message.author.id.get())
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
                let bot_id = ctx.cache.current_user().id;
                let mentions_other_humans = new_message
                    .mentions
                    .iter()
                    .any(|u| u.id != bot_id && !u.bot);
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
            if validate_live_channel_routing_with_dm_hint(
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
            if should_skip_for_missing_required_mention(
                &settings_snapshot,
                effective_channel_id,
                is_dm,
                &new_message.content,
                ctx.cache.current_user().id,
            ) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏭ MENTION-GUARD: skipping message {} in channel {} (effective {}) because bot mention is required",
                    new_message.id,
                    channel_id,
                    effective_channel_id,
                );
                return Ok(());
            }
            if !is_dm {
                match resolve_runtime_channel_binding_status(&ctx.http, effective_channel_id).await
                {
                    RuntimeChannelBindingStatus::Owned => {}
                    RuntimeChannelBindingStatus::Unowned => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ⏭ BINDING-GUARD: skipping message {} in unbound channel {} (effective {})",
                            new_message.id,
                            channel_id,
                            effective_channel_id
                        );
                        return Ok(());
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

            let text = new_message.content.trim();
            let announce_bot_id = super::super::resolve_announce_bot_user_id(&data.shared).await;

            let is_allowed_bot_sender = settings_snapshot.allowed_bot_ids.contains(&user_id.get())
                || announce_bot_id.is_some_and(|id| id == user_id.get());
            if is_allowed_bot_sender
                && !super::super::is_allowed_turn_sender(
                    &settings_snapshot.allowed_bot_ids,
                    announce_bot_id,
                    user_id.get(),
                    new_message.author.bot,
                    text,
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

            // #189: Generic DM reply tracking — consume pending entry if present.
            // Keep this after auth so unauthorized DM senders cannot inject
            // answers into pending workflows.
            // Consumed DM answers must stop here; falling through into normal
            // message handling produces a bogus "No active session" error in DMs.
            if !text.is_empty() {
                if try_handle_pending_dm_reply(
                    data.shared.sqlite.as_ref(),
                    data.shared.pg_pool.as_ref(),
                    new_message,
                )
                .await
                {
                    return Ok(());
                }
            }

            // Handle file attachments — download regardless of session state
            if !new_message.attachments.is_empty() {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ◀ [{user_name}] Upload: {} file(s)",
                    new_message.attachments.len()
                );
                // Ensure session exists before handling uploads
                auto_restore_session_with_dm_hint(&data.shared, channel_id, ctx, Some(is_dm)).await;
                super::message_handler::handle_file_upload(ctx, new_message, &data.shared).await?;
            }

            if text.is_empty() {
                return Ok(());
            }

            // ── Text commands (!start, !meeting, !stop, !clear) ──
            // Strip leading bot mention to get the actual command text
            let cmd_text = {
                let re = regex::Regex::new(r"^<@!?\d+>\s*").unwrap();
                re.replace(text, "").to_string()
            };
            if cmd_text.starts_with('!') {
                let handled = super::message_handler::handle_text_command(
                    ctx,
                    new_message,
                    &data,
                    channel_id,
                    &cmd_text,
                )
                .await?;
                if handled {
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
                        bootstrap_thread_session(&data.shared, channel_id, &path, ctx).await;
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
                data.shared.intake_dedup.retain(|k, v| {
                    if k.starts_with("mid:") {
                        true // preserved; cleaned by universal dedup cleanup
                    } else {
                        now.duration_since(v.0) < INTAKE_DEDUP_TTL
                    }
                });

                // Atomic check+insert via entry() — holds shard lock so two
                // simultaneous arrivals cannot both see a miss.
                let is_duplicate = match data.shared.intake_dedup.entry(dedup_key.clone()) {
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
            let merge_consecutive = should_merge_consecutive_messages(text, is_allowed_bot);

            // ── Dispatch-thread guard ─────────────────────────────────
            // When a dispatch thread is active for this channel, bot messages
            // to the parent channel are queued so they don't start a parallel
            // turn (the thread's cancel_token is keyed by thread_id, leaving
            // the parent channel "unlocked").
            if is_allowed_bot {
                if let Some(thread_id_ref) = data.shared.dispatch_thread_parents.get(&channel_id) {
                    let thread_id = *thread_id_ref.value();
                    // Thread still has an active turn?
                    let thread_active = mailbox_has_active_turn(&data.shared, thread_id).await;
                    if thread_active {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 🔀 THREAD-GUARD: bot message to parent {} queued (dispatch thread {} active)",
                            channel_id,
                            thread_id
                        );
                        let _ = enqueue_soft_intervention(
                            data,
                            channel_id,
                            user_id,
                            new_message.id,
                            text,
                            None,
                            false,
                            false,
                        )
                        .await;
                        add_reaction(ctx, channel_id, new_message.id, '📬').await;
                        data.shared
                            .last_message_ids
                            .insert(channel_id, new_message.id.get());
                        return Ok(());
                    } else {
                        // Thread turn finished — clean up stale mapping
                        data.shared.dispatch_thread_parents.remove(&channel_id);
                    }
                }
            }

            // ── Dispatch collision guard ────────────────────────────────
            // When a DISPATCH: message arrives on a channel that already has
            // an active turn (inflight), queue it as an intervention instead
            // of starting a parallel turn that would stomp the current
            // placeholder.
            if text.starts_with("DISPATCH:") {
                if mailbox_has_active_turn(&data.shared, channel_id).await {
                    let _ = enqueue_soft_intervention(
                        data,
                        channel_id,
                        user_id,
                        new_message.id,
                        text,
                        None,
                        false,
                        false,
                    )
                    .await;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 📬 DISPATCH-GUARD: queued dispatch message in channel {} (active turn in progress)",
                        channel_id
                    );
                    add_reaction(ctx, channel_id, new_message.id, '📬').await;
                    data.shared
                        .last_message_ids
                        .insert(channel_id, new_message.id.get());
                    return Ok(());
                }
                // No active turn — fall through to normal processing below
            }

            // Queue messages while AI is in progress (executed as next turn after current finishes)
            if mailbox_has_active_turn(&data.shared, channel_id).await {
                let inserted = enqueue_soft_intervention(
                    data,
                    channel_id,
                    user_id,
                    new_message.id,
                    text,
                    reply_context.clone(),
                    has_reply_boundary,
                    merge_consecutive,
                )
                .await;
                let is_shutting_down = data
                    .shared
                    .shutting_down
                    .load(std::sync::atomic::Ordering::Relaxed);

                if !inserted {
                    rate_limit_wait(&data.shared, channel_id).await;
                    let _ = channel_id
                        .say(&ctx.http, "↪ 같은 메시지가 방금 이미 큐잉되어서 무시했어.")
                        .await;
                    return Ok(());
                }

                // React with 📬 to indicate message is queued
                add_reaction(ctx, channel_id, new_message.id, '📬').await;

                // Checkpoint: message successfully queued
                data.shared
                    .last_message_ids
                    .insert(channel_id, new_message.id.get());
                if is_shutting_down {
                    let ids: std::collections::HashMap<u64, u64> = data
                        .shared
                        .last_message_ids
                        .iter()
                        .map(|entry| (entry.key().get(), *entry.value()))
                        .collect();
                    runtime_store::save_all_last_message_ids(data.provider.as_str(), &ids);
                }
                return Ok(());
            }

            // Reconcile gate (#122): until startup recovery is complete, queue messages.
            if !data
                .shared
                .reconcile_done
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                let _ = enqueue_soft_intervention(
                    data,
                    channel_id,
                    user_id,
                    new_message.id,
                    text,
                    reply_context.clone(),
                    has_reply_boundary,
                    merge_consecutive,
                )
                .await;
                // Checkpoint: track last processed message
                data.shared
                    .last_message_ids
                    .insert(channel_id, new_message.id.get());
                formatting::add_reaction_raw(&ctx.http, channel_id, new_message.id, '🔄').await;
                return Ok(());
            }

            // Drain mode: when restart is pending, queue new messages instead of
            // starting new turns. This ensures only existing turns drain to completion.
            if data
                .shared
                .restart_pending
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                let is_shutting_down = data
                    .shared
                    .shutting_down
                    .load(std::sync::atomic::Ordering::Relaxed);

                let _ = enqueue_soft_intervention(
                    data,
                    channel_id,
                    user_id,
                    new_message.id,
                    text,
                    reply_context.clone(),
                    has_reply_boundary,
                    merge_consecutive,
                )
                .await;

                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏸ DRAIN: queued message from [{user_name}] in channel {} (restart pending)",
                    channel_id
                );

                // React with 📬 to indicate message is queued
                add_reaction(ctx, channel_id, new_message.id, '📬').await;

                // Checkpoint: message successfully queued in drain mode
                data.shared
                    .last_message_ids
                    .insert(channel_id, new_message.id.get());

                if is_shutting_down {
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
                    Some(
                        enqueue_soft_intervention(
                            data,
                            channel_id,
                            user_id,
                            new_message.id,
                            text,
                            reply_context.clone(),
                            has_reply_boundary,
                            merge_consecutive,
                        )
                        .await,
                    )
                }
            };
            if let Some(inserted) = queued_behind_idle_backlog {
                let ts = chrono::Local::now().format("%H:%M:%S");
                if inserted {
                    tracing::info!(
                        "  [{ts}] 📬 IDLE-QUEUE: queued message from [{user_name}] in channel {} behind pending backlog",
                        channel_id
                    );
                    add_reaction(ctx, channel_id, new_message.id, '📬').await;
                    data.shared
                        .last_message_ids
                        .insert(channel_id, new_message.id.get());
                } else {
                    tracing::info!(
                        "  [{ts}] ↪ IDLE-QUEUE: duplicate message from [{user_name}] already pending in channel {}",
                        channel_id
                    );
                }
                super::super::kickoff_idle_queues(ctx, &data.shared, &data.token, &data.provider)
                    .await;
                return Ok(());
            }

            // Meeting command from text (e.g. announce bot sending "/meeting start ...")
            if text.starts_with("/meeting ") {
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
                    return Ok(());
                }
            }

            // Shell command shortcut
            if text.starts_with('!') {
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
            data.shared
                .last_message_ids
                .insert(channel_id, new_message.id.get());

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

            super::message_handler::handle_text_message(
                ctx,
                channel_id,
                new_message.id,
                user_id,
                user_name,
                text,
                &data.shared,
                &data.token,
                false,
                false,
                false,
                merge_consecutive,
                reply_context,
                has_reply_boundary,
                Some(is_dm),
                turn_kind,
            )
            .await?;
        }
        _ => {}
    }
    Ok(())
}

use super::super::model_picker_interaction::handle_model_picker_interaction;
