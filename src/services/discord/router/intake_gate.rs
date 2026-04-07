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
                        println!(
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
                    println!(
                        "  [{ts}] ⏭ MSG-DEDUP: skipping duplicate message {} in channel {}",
                        new_message.id, new_message.channel_id
                    );
                    return Ok(());
                }
            }

            if !should_process_turn_message(new_message.kind) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] ⏭ MSG-KIND: skipping {:?} message {} in channel {}",
                    new_message.kind, new_message.id, new_message.channel_id
                );
                return Ok(());
            }

            // Ignore bot messages, unless the bot is in the allowed_bot_ids list
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
            let (channel_name, _) = resolve_channel_category(ctx, channel_id).await;
            // For threads, inherit role binding from the parent channel
            let (effective_channel_id, effective_channel_name) =
                if let Some((parent_id, parent_name)) =
                    resolve_thread_parent(&ctx.http, channel_id).await
                {
                    (parent_id, parent_name.or_else(|| channel_name.clone()))
                } else {
                    (channel_id, channel_name.clone())
                };
            let settings_snapshot = { data.shared.settings.read().await.clone() };
            if validate_bot_channel_routing(
                &settings_snapshot,
                &data.provider,
                effective_channel_id,
                effective_channel_name.as_deref(),
                is_dm,
            )
            .is_err()
            {
                return Ok(());
            }

            // #189: Generic DM reply tracking — consume pending entry if present.
            // The message always falls through to normal handling so the agent
            // can respond contextually in the DM conversation.
            let text = new_message.content.trim();
            if !text.is_empty() {
                if let Some(ref db) = data.shared.db {
                    try_handle_pending_dm_reply(db, new_message).await;
                }
            }

            // Auth check (allowed bots bypass auth)
            let is_allowed_bot = new_message.author.bot && {
                let settings = data.shared.settings.read().await;
                settings.allowed_bot_ids.contains(&user_id.get())
            };
            if !is_allowed_bot && !check_auth(user_id, user_name, &data.shared, &data.token).await {
                return Ok(());
            }

            // Handle file attachments — download regardless of session state
            if !new_message.attachments.is_empty() {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] ◀ [{user_name}] Upload: {} file(s)",
                    new_message.attachments.len()
                );
                // Ensure session exists before handling uploads
                auto_restore_session(&data.shared, channel_id, ctx).await;
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
            auto_restore_session(&data.shared, channel_id, ctx).await;
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
            if new_message.author.bot {
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
                    println!(
                        "  [{ts}] ⏭ DEDUP: skipping duplicate intake in channel {} (key={})",
                        channel_id, dedup_key
                    );
                    return Ok(());
                }
            }

            // ── Dispatch-thread guard ─────────────────────────────────
            // When a dispatch thread is active for this channel, bot messages
            // to the parent channel are queued so they don't start a parallel
            // turn (the thread's cancel_token is keyed by thread_id, leaving
            // the parent channel "unlocked").
            if new_message.author.bot {
                if let Some(thread_id) = data.shared.dispatch_thread_parents.get(&channel_id) {
                    // Thread still has an active turn?
                    let thread_active = {
                        let d = data.shared.core.lock().await;
                        d.cancel_tokens.contains_key(thread_id.value())
                    };
                    if thread_active {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!(
                            "  [{ts}] 🔀 THREAD-GUARD: bot message to parent {} queued (dispatch thread {} active)",
                            channel_id, *thread_id
                        );
                        let mut d = data.shared.core.lock().await;
                        let queue = d.intervention_queue.entry(channel_id).or_default();
                        enqueue_intervention(
                            queue,
                            Intervention {
                                author_id: user_id,
                                message_id: new_message.id,
                                text: text.to_string(),
                                mode: InterventionMode::Soft,
                                created_at: Instant::now(),
                            },
                        );
                        if let Some(q) = d.intervention_queue.get(&channel_id) {
                            save_channel_queue(&data.provider, channel_id, q);
                        }
                        drop(d);
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
                let mut d = data.shared.core.lock().await;
                if d.cancel_tokens.contains_key(&channel_id) {
                    let inserted = {
                        let queue = d.intervention_queue.entry(channel_id).or_default();
                        enqueue_intervention(
                            queue,
                            Intervention {
                                author_id: user_id,
                                message_id: new_message.id,
                                text: text.to_string(),
                                mode: InterventionMode::Soft,
                                created_at: Instant::now(),
                            },
                        )
                    };
                    if inserted {
                        if let Some(q) = d.intervention_queue.get(&channel_id) {
                            save_channel_queue(&data.provider, channel_id, q);
                        }
                    }
                    drop(d);

                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] 📬 DISPATCH-GUARD: queued dispatch message in channel {} (active turn in progress)",
                        channel_id
                    );
                    add_reaction(ctx, channel_id, new_message.id, '📬').await;
                    data.shared
                        .last_message_ids
                        .insert(channel_id, new_message.id.get());
                    return Ok(());
                }
                drop(d);
                // No active turn — fall through to normal processing below
            }

            // Queue messages while AI is in progress (executed as next turn after current finishes)
            {
                let mut d = data.shared.core.lock().await;
                if d.cancel_tokens.contains_key(&channel_id) {
                    let inserted = {
                        let queue = d.intervention_queue.entry(channel_id).or_default();
                        enqueue_intervention(
                            queue,
                            Intervention {
                                author_id: user_id,
                                message_id: new_message.id,
                                text: text.to_string(),
                                mode: InterventionMode::Soft,
                                created_at: Instant::now(),
                            },
                        )
                    };

                    // Write-through: persist this channel's queue to disk immediately
                    // so it survives SIGKILL, OOM kill, or crash.
                    if inserted {
                        if let Some(q) = d.intervention_queue.get(&channel_id) {
                            save_channel_queue(&data.provider, channel_id, q);
                        }
                    }

                    let is_shutting_down = data
                        .shared
                        .shutting_down
                        .load(std::sync::atomic::Ordering::Relaxed);

                    drop(d);

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
            }

            // Reconcile gate (#122): until startup recovery is complete, queue messages.
            if !data
                .shared
                .reconcile_done
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                let mut d = data.shared.core.lock().await;
                let queue = d.intervention_queue.entry(channel_id).or_default();
                enqueue_intervention(
                    queue,
                    Intervention {
                        author_id: user_id,
                        message_id: new_message.id,
                        text: text.to_string(),
                        mode: InterventionMode::Soft,
                        created_at: Instant::now(),
                    },
                );
                // Write-through: persist queue to disk (matches drain-mode contract)
                if let Some(q) = d.intervention_queue.get(&channel_id) {
                    save_channel_queue(&data.provider, channel_id, q);
                }
                drop(d);
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

                let mut d = data.shared.core.lock().await;
                let queue = d.intervention_queue.entry(channel_id).or_default();
                enqueue_intervention(
                    queue,
                    Intervention {
                        author_id: user_id,
                        message_id: new_message.id,
                        text: text.to_string(),
                        mode: InterventionMode::Soft,
                        created_at: Instant::now(),
                    },
                );

                // Write-through: persist this channel's queue to disk immediately
                if let Some(q) = d.intervention_queue.get(&channel_id) {
                    save_channel_queue(&data.provider, channel_id, q);
                }
                drop(d);

                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
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
                let mut d = data.shared.core.lock().await;
                if d.cancel_tokens.contains_key(&channel_id)
                    || !channel_has_pending_soft_queue(&mut d.intervention_queue, channel_id)
                {
                    None
                } else {
                    let inserted = {
                        let queue = d.intervention_queue.entry(channel_id).or_default();
                        enqueue_intervention(
                            queue,
                            Intervention {
                                author_id: user_id,
                                message_id: new_message.id,
                                text: text.to_string(),
                                mode: InterventionMode::Soft,
                                created_at: Instant::now(),
                            },
                        )
                    };
                    if inserted {
                        if let Some(q) = d.intervention_queue.get(&channel_id) {
                            save_channel_queue(&data.provider, channel_id, q);
                        }
                    }
                    Some(inserted)
                }
            };
            if let Some(inserted) = queued_behind_idle_backlog {
                let ts = chrono::Local::now().format("%H:%M:%S");
                if inserted {
                    println!(
                        "  [{ts}] 📬 IDLE-QUEUE: queued message from [{user_name}] in channel {} behind pending backlog",
                        channel_id
                    );
                    add_reaction(ctx, channel_id, new_message.id, '📬').await;
                    data.shared
                        .last_message_ids
                        .insert(channel_id, new_message.id.get());
                } else {
                    println!(
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
                println!("  [{ts}] ◀ [{user_name}] Meeting cmd: {text}");
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
                println!("  [{ts}] ◀ [{user_name}] Shell: {preview}");
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
            println!("  [{ts}] ◀ [{user_name}] {preview}");

            // Extract reply context if user replied to another message
            let reply_context = if let Some(ref_msg) = new_message.referenced_message.as_ref() {
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

                // Fetch preceding messages for Q&A context (best-effort)
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
                    // preceding comes newest-first; reverse for chronological order
                    let mut msgs: Vec<_> = preceding
                        .iter()
                        .filter(|m| !m.content.trim().is_empty())
                        .collect();
                    msgs.reverse();
                    // Keep last 2 Q&A-style messages (budget: ~1000 chars total)
                    let mut budget: usize = 1000;
                    for m in msgs
                        .iter()
                        .rev()
                        .take(4)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                    {
                        let entry =
                            format!("{}: {}", m.author.name, truncate_str(m.content.trim(), 300));
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
            } else {
                None
            };

            // Checkpoint: message about to be processed as a turn
            data.shared
                .last_message_ids
                .insert(channel_id, new_message.id.get());

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
                reply_context,
            )
            .await?;
        }
        _ => {}
    }
    Ok(())
}

use super::super::model_picker_interaction::handle_model_picker_interaction;
