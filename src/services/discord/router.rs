use super::*;
#[cfg(unix)]
use crate::services::tmux_common::tmux_exact_target;

pub(super) fn should_process_turn_message(kind: serenity::model::channel::MessageType) -> bool {
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

fn is_model_picker_component_custom_id(
    custom_id: &str,
    fallback_channel_id: serenity::ChannelId,
) -> bool {
    super::commands::parse_model_picker_custom_id(custom_id, fallback_channel_id).is_some()
}

async fn reset_provider_session_if_pending(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) {
    if shared
        .model_session_reset_pending
        .remove(&channel_id)
        .is_none()
    {
        return;
    }

    let channel_name = {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.session_id = None;
            session.channel_name.clone()
        } else {
            None
        }
    };

    if provider.uses_managed_tmux_backend() {
        if let Some(name) = channel_name.as_deref() {
            crate::services::claude::terminate_local_session(
                &provider.build_tmux_session_name(name),
            );
        }
    }

    if let Some(session_key) = build_adk_session_key(shared, channel_id, provider).await {
        super::adk_session::clear_provider_session_id(&session_key, shared.api_port).await;
    }
}

pub(super) async fn handle_event(
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
                    if !super::provider_handles_channel(
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
                handle_file_upload(ctx, new_message, &data.shared).await?;
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
                let handled =
                    handle_text_command(ctx, new_message, &data, channel_id, &cmd_text).await?;
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
                    if let Some(dispatch_id) = super::adk_session::parse_dispatch_id(text) {
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
                handle_shell_command_raw(ctx, channel_id, text, &data.shared).await?;
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

            handle_text_message(
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

use super::model_picker_interaction::handle_model_picker_interaction;

pub(super) async fn handle_text_message(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    request_owner: UserId,
    request_owner_name: &str,
    user_text: &str,
    shared: &Arc<SharedData>,
    token: &str,
    reply_to_user_message: bool,
    defer_watcher_resume: bool,
    wait_for_completion: bool,
    reply_context: Option<String>,
) -> Result<(), Error> {
    // Get session info, allowed tools, and pending uploads
    let (session_info, provider, allowed_tools, pending_uploads) = {
        let mut data = shared.core.lock().await;
        let info = data.sessions.get(&channel_id).and_then(|session| {
            session.current_path.as_ref().map(|_| {
                (
                    session.session_id.clone(),
                    session.current_path.clone().unwrap_or_default(),
                )
            })
        });
        let uploads = data
            .sessions
            .get_mut(&channel_id)
            .map(|s| {
                s.cleared = false;
                std::mem::take(&mut s.pending_uploads)
            })
            .unwrap_or_default();
        drop(data);
        let settings = shared.settings.read().await;
        (
            info,
            settings.provider.clone(),
            settings.allowed_tools.clone(),
            uploads,
        )
    };

    let (session_id, current_path) = match session_info {
        Some(info) => info,
        None => {
            // Try auto-start from role_map workspace
            let ch_name = {
                let data = shared.core.lock().await;
                data.sessions
                    .get(&channel_id)
                    .and_then(|s| s.channel_name.clone())
            };
            let mut workspace = settings::resolve_workspace(channel_id, ch_name.as_deref());
            // Fallback: if this is a thread, try resolving workspace from parent channel
            if workspace.is_none() {
                if let Some((parent_id, parent_name)) =
                    super::resolve_thread_parent(&ctx.http, channel_id).await
                {
                    // Use parent name from Discord API first, fall back to session map
                    let parent_ch_name = parent_name.or_else(|| {
                        let data = shared.core.try_lock().ok()?;
                        data.sessions
                            .get(&parent_id)
                            .and_then(|s| s.channel_name.clone())
                    });
                    workspace = settings::resolve_workspace(parent_id, parent_ch_name.as_deref());
                    if workspace.is_some() {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!(
                            "  [{ts}] 🧵 Thread auto-start: resolved workspace from parent channel {}",
                            parent_id
                        );
                    }
                }
            }
            if let Some(ws_path) = workspace {
                let ws = std::path::Path::new(&ws_path);
                if ws.is_dir() {
                    let canonical = ws
                        .canonicalize()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| ws_path.clone());
                    // Check worktree: always use worktree for thread sessions,
                    // or when conflict detected with another session on same path.
                    let wt_info = {
                        let is_thread = shared.dispatch_thread_parents.contains_key(&channel_id);
                        let data = shared.core.lock().await;
                        let conflict =
                            detect_worktree_conflict(&data.sessions, &canonical, channel_id);
                        drop(data);
                        let needs_worktree = is_thread || conflict.is_some();
                        if needs_worktree {
                            let reason = if is_thread {
                                "thread session"
                            } else {
                                "conflict"
                            };
                            let ch = ch_name.as_deref().unwrap_or("unknown");
                            match create_git_worktree(&canonical, ch, provider.as_str()) {
                                Ok((wt_path, branch)) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    println!(
                                        "  [{ts}] 🌿 Auto-start worktree ({reason}): {ch} → {}",
                                        wt_path
                                    );
                                    Some(WorktreeInfo {
                                        original_path: canonical.clone(),
                                        worktree_path: wt_path,
                                        branch_name: branch,
                                    })
                                }
                                Err(_) => None,
                            }
                        } else {
                            None
                        }
                    };
                    let eff_path = wt_info
                        .as_ref()
                        .map(|wt| wt.worktree_path.clone())
                        .unwrap_or_else(|| canonical.clone());
                    let (ch_name_resolved, cat_name) =
                        resolve_channel_category(ctx, channel_id).await;
                    // For thread channels, build a stable channel_name from the
                    // parent channel name so resolve_agent_id_from_channel_name
                    // can match it (same logic as bootstrap_thread_session).
                    let ch_name = match super::resolve_thread_parent(&ctx.http, channel_id).await {
                        Some((_parent_id, parent_name)) => {
                            let parent = parent_name.unwrap_or_else(|| format!("{}", _parent_id));
                            Some(super::synthetic_thread_channel_name(&parent, channel_id))
                        }
                        None => ch_name_resolved,
                    };
                    {
                        let mut data = shared.core.lock().await;
                        let session =
                            data.sessions
                                .entry(channel_id)
                                .or_insert_with(|| DiscordSession {
                                    session_id: None,
                                    current_path: None,
                                    history: Vec::new(),
                                    pending_uploads: Vec::new(),
                                    cleared: false,
                                    channel_name: None,
                                    category_name: None,
                                    remote_profile_name: None,
                                    channel_id: Some(channel_id.get()),
                                    last_active: tokio::time::Instant::now(),
                                    worktree: None,

                                    born_generation: super::runtime_store::load_generation(),
                                });
                        session.current_path = Some(eff_path.clone());
                        session.channel_name = ch_name;
                        session.category_name = cat_name;
                        session.channel_id = Some(channel_id.get());
                        session.last_active = tokio::time::Instant::now();
                        session.worktree = wt_info;
                    }
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] ▶ Auto-started session from workspace: {eff_path}");
                    let sid = {
                        let data = shared.core.lock().await;
                        data.sessions
                            .get(&channel_id)
                            .and_then(|s| s.session_id.clone())
                    };
                    (sid, eff_path)
                } else {
                    rate_limit_wait(shared, channel_id).await;
                    let _ = channel_id
                        .say(&ctx.http, "No active session. Use `/start <path>` first.")
                        .await;
                    return Ok(());
                }
            } else {
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id
                    .say(&ctx.http, "No active session. Use `/start <path>` first.")
                    .await;
                return Ok(());
            }
        }
    };

    // Add hourglass reaction to user's message
    add_reaction(ctx, channel_id, user_msg_id, '⏳').await;

    // ── Dispatch thread auto-creation ──────────────────────────────
    // When a dispatch message arrives, create a Discord thread for
    // isolated context.  All subsequent agent output goes to the thread.
    // Skip if already inside a thread (threads cannot nest).
    // Thread reuse: if the card already has an active_thread_id, redirect
    // to the existing thread instead of creating a new one.
    let is_already_thread = super::resolve_thread_parent(&ctx.http, channel_id)
        .await
        .is_some();
    let dispatch_id_for_thread = super::adk_session::parse_dispatch_id(user_text);
    let mut dispatch_type_str: Option<String> = None;
    // #259: Fetch dispatch metadata once before thread creation so we can extract
    // worktree_path for both thread bootstrap and the subsequent session CWD override.
    let dispatch_info_cached = if let Some(ref did) = dispatch_id_for_thread {
        lookup_dispatch_info(shared.api_port, did).await
    } else {
        None
    };
    // #259: Prefer card-bound worktree over parent channel CWD for dispatch sessions.
    // All dispatch types now inject worktree_path into context via resolve_card_worktree().
    let dispatch_worktree_path = dispatch_info_cached
        .as_ref()
        .and_then(|info| {
            info.context
                .as_ref()
                .and_then(|ctx_str| serde_json::from_str::<serde_json::Value>(ctx_str).ok())
                .and_then(|v| v.get("worktree_path")?.as_str().map(String::from))
        })
        .filter(|p| std::path::Path::new(p).exists());
    if let (Some(wt), Some(did)) = (&dispatch_worktree_path, &dispatch_id_for_thread) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!("  [{ts}] 🌿 Dispatch {did}: resolved worktree CWD: {wt}");
    }
    let dispatch_effective_path = dispatch_worktree_path
        .clone()
        .unwrap_or_else(|| current_path.clone());
    let channel_id = if let Some(ref did) = dispatch_id_for_thread {
        // Use cached dispatch metadata for thread reuse and cross-channel role override
        let dispatch_info = &dispatch_info_cached;
        dispatch_type_str = dispatch_info.as_ref().and_then(|i| i.dispatch_type.clone());
        let is_counter_model_dispatch =
            crate::server::routes::dispatches::use_counter_model_channel(
                dispatch_type_str.as_deref(),
            );
        let alt_channel_id = dispatch_info
            .as_ref()
            .and_then(|i| i.discord_channel_alt.as_deref())
            .and_then(|s| s.parse::<u64>().ok())
            .map(ChannelId::new);

        if is_already_thread {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] 🧵 Dispatch {did} arrived in existing thread, skipping thread creation"
            );
            // For review dispatches in reused threads, set role override
            // so this turn uses the counter-model channel's role/model.
            if is_counter_model_dispatch {
                if let Some(alt_ch) = alt_channel_id {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] 🔄 Review dispatch in reused thread: overriding role to alt channel {}",
                        alt_ch
                    );
                    shared.dispatch_role_overrides.insert(channel_id, alt_ch);
                }
            }
            channel_id
        } else {
            // Check if card already has an active thread via internal API
            let existing_thread = dispatch_info
                .as_ref()
                .and_then(|i| i.active_thread_id.clone());
            let reuse_tid = existing_thread.as_ref().and_then(|t| {
                let id = t.parse::<u64>().unwrap_or(0);
                if id != 0 {
                    Some(ChannelId::new(id))
                } else {
                    None
                }
            });

            let reused = if let Some(tid) = reuse_tid {
                if verify_thread_accessible(ctx, tid).await {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] 🧵 Reusing existing thread {} for dispatch {}",
                        tid, did
                    );
                    super::bootstrap_thread_session(shared, tid, &dispatch_effective_path, ctx)
                        .await;
                    shared.dispatch_thread_parents.insert(channel_id, tid);
                    // For review dispatches reusing an implementation thread,
                    // override role/model to use the counter-model channel.
                    if is_counter_model_dispatch {
                        if let Some(alt_ch) = alt_channel_id {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            println!(
                                "  [{ts}] 🔄 Review dispatch reusing thread: overriding role to alt channel {}",
                                alt_ch
                            );
                            shared.dispatch_role_overrides.insert(tid, alt_ch);
                        }
                    }
                    Some(tid)
                } else {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] 🧵 Thread {} is locked/inaccessible, creating new for {}",
                        tid, did
                    );
                    None
                }
            } else {
                None
            };

            if let Some(tid) = reused {
                tid
            } else {
                // No existing usable thread — create new
                let thread_title = user_text
                    .find(" - ")
                    .map(|idx| &user_text[idx + 3..])
                    .unwrap_or("dispatch")
                    .chars()
                    .take(90)
                    .collect::<String>();

                match channel_id
                    .create_thread(
                        &ctx.http,
                        poise::serenity_prelude::builder::CreateThread::new(thread_title)
                            .kind(poise::serenity_prelude::ChannelType::PublicThread)
                            .auto_archive_duration(
                                poise::serenity_prelude::AutoArchiveDuration::OneDay,
                            ),
                    )
                    .await
                {
                    Ok(thread) => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!(
                            "  [{ts}] 🧵 Created dispatch thread {} for dispatch {}",
                            thread.id, did
                        );
                        super::bootstrap_thread_session(
                            shared,
                            thread.id,
                            &dispatch_effective_path,
                            ctx,
                        )
                        .await;
                        shared.dispatch_thread_parents.insert(channel_id, thread.id);
                        link_dispatch_thread(
                            shared.api_port,
                            did,
                            thread.id.get(),
                            channel_id.get(),
                        )
                        .await;
                        thread.id
                    }
                    Err(e) => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        eprintln!("  [{ts}] ⚠ Failed to create dispatch thread: {e}");
                        channel_id // fallback to main channel
                    }
                }
            }
        }
    } else {
        channel_id
    };

    // #259: Override current_path with the pre-computed dispatch worktree path.
    // Also update the in-memory session so the worktree sticks for subsequent turns.
    let current_path = if dispatch_worktree_path.is_some() {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.current_path = Some(dispatch_effective_path.clone());
        }
        dispatch_effective_path.clone()
    } else {
        current_path
    };

    // Send placeholder message
    rate_limit_wait(shared, channel_id).await;
    let placeholder = channel_id
        .send_message(&ctx.http, {
            let builder = CreateMessage::new().content("...");
            if reply_to_user_message && dispatch_id_for_thread.is_none() {
                builder.reference_message((channel_id, user_msg_id))
            } else {
                builder
            }
        })
        .await?;
    let placeholder_msg_id = placeholder.id;

    // Sanitize input
    let sanitized_input = ai_screen::sanitize_user_input(user_text);

    let role_binding = {
        // For cross-channel dispatch reuse (e.g. review in implementation thread),
        // resolve role from the override channel instead of the thread's parent.
        if let Some(override_ch) = shared.dispatch_role_overrides.get(&channel_id) {
            let alt_ch = *override_ch;
            resolve_role_binding(alt_ch, None)
        } else {
            let data = shared.core.lock().await;
            let ch_name = data
                .sessions
                .get(&channel_id)
                .and_then(|s| s.channel_name.as_deref());
            resolve_role_binding(channel_id, ch_name)
        }
    };

    // For cross-channel dispatch reuse, override the provider so the turn
    // executes via the counter-model CLI (e.g. Codex reviews Claude's work).
    let provider = if shared.dispatch_role_overrides.contains_key(&channel_id) {
        role_binding
            .as_ref()
            .and_then(|rb| rb.provider.clone())
            .unwrap_or(provider)
    } else {
        provider
    };

    // Prepend pending file uploads
    let mut context_chunks = Vec::new();
    if !pending_uploads.is_empty() {
        context_chunks.push(pending_uploads.join("\n"));
    }
    // Load SAK early — Claude gets it in the system prompt (for caching),
    // non-Claude providers get it as a first-turn user message (no cache benefit).
    let is_review_lite = matches!(
        dispatch_type_str.as_deref(),
        Some("review") | Some("review-decision")
    );
    let shared_knowledge_content = if !is_review_lite {
        load_shared_knowledge()
    } else {
        None
    };
    // Non-Claude SAK injection is deferred until after session recovery
    // (see below, after fetch_provider_session_id) to avoid duplicate injection
    // when dcserver restarts and restores an existing session from DB.
    if let Some(ref reply_ctx) = reply_context {
        context_chunks.push(reply_ctx.clone());
    }
    // Re-inject compact formatting reminder for interactive follow-up turns.
    // System prompt is only sent at session creation; after context compaction
    // these rules can be lost.
    if session_id.is_some() {
        context_chunks.push(
            "<system-reminder>\n\
             Discord formatting: minimize code blocks, keep messages concise.\n\
             </system-reminder>"
                .to_string(),
        );
    }
    context_chunks.push(sanitized_input);
    let mut context_prompt = context_chunks.join("\n\n");

    // Build disabled tools notice
    let default_tools: std::collections::HashSet<&str> =
        DEFAULT_ALLOWED_TOOLS.iter().copied().collect();
    let allowed_set: std::collections::HashSet<&str> =
        allowed_tools.iter().map(|s| s.as_str()).collect();
    let disabled: Vec<&&str> = default_tools
        .iter()
        .filter(|t| !allowed_set.contains(**t))
        .collect();
    let disabled_notice = if disabled.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = disabled.iter().map(|t| **t).collect();
        format!(
            "\n\nDISABLED TOOLS: The following tools have been disabled by the user: {}.\n\
             You MUST NOT attempt to use these tools. \
             If a user's request requires a disabled tool, do NOT proceed with the task. \
             Instead, clearly inform the user which tool is needed and that it is currently disabled. \
             Suggest they re-enable it with: /allowed +ToolName",
            names.join(", ")
        )
    };

    // Build skills notice for system prompt
    let skills_notice = {
        let skills = shared.skills_cache.read().await;
        if skills.is_empty() {
            String::new()
        } else {
            let list: Vec<String> = skills
                .iter()
                .map(|(name, desc)| format!("  - /{}: {}", name, desc))
                .collect();
            match &provider {
                ProviderKind::Claude => format!(
                    "\n\nAvailable skills (invoke via the Skill tool):\n{}",
                    list.join("\n")
                ),
                ProviderKind::Codex => format!(
                    "\n\nAvailable local Codex skills (use them by name when relevant):\n{}",
                    list.join("\n")
                ),
                ProviderKind::Gemini => format!(
                    "\n\nAvailable local Gemini skills (use them by name when relevant):\n{}",
                    list.join("\n")
                ),
                ProviderKind::Qwen => format!(
                    "\n\nAvailable local Qwen skills (use them by name when relevant):\n{}",
                    list.join("\n")
                ),
                ProviderKind::Unsupported(_) => String::new(),
            }
        }
    };

    // Build Discord context info
    let discord_context = {
        let data = shared.core.lock().await;
        let session = data.sessions.get(&channel_id);
        let ch_name = session.and_then(|s| s.channel_name.as_deref());
        let cat_name = session.and_then(|s| s.category_name.as_deref());
        match ch_name {
            Some(name) => {
                let cat_part = cat_name
                    .map(|c| format!(" (category: {})", c))
                    .unwrap_or_default();
                format!(
                    "Discord context: channel #{} (ID: {}){}, user: {} (ID: {})",
                    name,
                    channel_id.get(),
                    cat_part,
                    request_owner_name,
                    request_owner.get()
                )
            }
            None => format!(
                "Discord context: DM, user: {} (ID: {})",
                request_owner_name,
                request_owner.get()
            ),
        }
    };

    // Derive dispatch prompt profile: review/rework dispatches get a lighter prompt.
    let dispatch_profile = DispatchProfile::from_dispatch_type(
        dispatch_id_for_thread
            .as_ref()
            .and_then(|_| dispatch_type_str.as_deref()),
    );

    // Claude: pass SAK to build_system_prompt for stable cache prefix placement.
    // Non-Claude: SAK was already pushed to context_chunks above.
    let sak_for_system = if matches!(provider, ProviderKind::Claude) {
        shared_knowledge_content.as_deref()
    } else {
        None
    };
    let system_prompt_owned = build_system_prompt(
        &discord_context,
        &current_path,
        channel_id,
        token,
        &disabled_notice,
        &skills_notice,
        role_binding.as_ref(),
        reply_to_user_message,
        dispatch_profile,
        dispatch_type_str.as_deref(),
        sak_for_system,
    );
    if sak_for_system.is_some() {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}] 📦 SAK in system prompt ({} chars) for channel {}",
            sak_for_system.unwrap().len(),
            channel_id.get()
        );
    }

    // Create cancel token — with second check to close the TOCTOU race window.
    // Multiple messages can pass the initial cancel_tokens check (line 169) concurrently
    // because the async gap between check and insert allows interleaving.
    // If another message won the race, queue ourselves and clean up.
    let cancel_token = Arc::new(CancelToken::new());
    {
        let mut data = shared.core.lock().await;
        if data.cancel_tokens.contains_key(&channel_id) {
            // Race lost — another message already started a turn.
            // Queue this message as an intervention instead.
            let queue = data.intervention_queue.entry(channel_id).or_default();
            super::enqueue_intervention(
                queue,
                super::Intervention {
                    author_id: request_owner,
                    message_id: user_msg_id,
                    text: user_text.to_string(),
                    mode: super::InterventionMode::Soft,
                    created_at: std::time::Instant::now(),
                },
            );
            // Write-through: persist this channel's queue to disk
            if let Some(q) = data.intervention_queue.get(&channel_id) {
                super::save_channel_queue(&provider, channel_id, q);
            }
            drop(data);
            // Clean up: remove placeholder and reaction created before this check
            let _ = channel_id
                .delete_message(&ctx.http, placeholder_msg_id)
                .await;
            super::formatting::remove_reaction_raw(&ctx.http, channel_id, user_msg_id, '⏳').await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] 🔀 RACE: message queued (another turn won), channel {}",
                channel_id
            );
            return Ok(());
        }
        shared
            .global_active
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        data.cancel_tokens.insert(channel_id, cancel_token.clone());
        data.active_request_owner.insert(channel_id, request_owner);
    }
    shared
        .turn_start_times
        .insert(channel_id, std::time::Instant::now());

    // Spawn turn watchdog — cancels the turn if it exceeds the deadline.
    // The deadline is stored in cancel_token.watchdog_deadline_ms and can be
    // extended via POST /api/turns/{channel_id}/extend-timeout (up to 3h cap).
    {
        let watchdog_token = cancel_token.clone();
        let watchdog_shared = shared.clone();
        let watchdog_http = ctx.http.clone();
        let timeout = super::turn_watchdog_timeout();

        // Set initial deadline and max ceiling (initial + 3h)
        let now_ms = chrono::Utc::now().timestamp_millis();
        let deadline_ms = now_ms + timeout.as_millis() as i64;
        let max_deadline_ms = now_ms + 3 * 3600 * 1000; // 3 hours absolute cap
        watchdog_token
            .watchdog_deadline_ms
            .store(deadline_ms, std::sync::atomic::Ordering::Relaxed);
        watchdog_token
            .watchdog_max_deadline_ms
            .store(max_deadline_ms, std::sync::atomic::Ordering::Relaxed);

        let watchdog_channel_id_num = channel_id.get();
        let watchdog_provider = provider.clone();
        tokio::spawn(async move {
            const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

            loop {
                tokio::time::sleep(CHECK_INTERVAL).await;

                // Exit early if the turn already completed/cancelled
                if watchdog_token
                    .cancelled
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    super::clear_watchdog_deadline_override(watchdog_channel_id_num);
                    return;
                }

                // Check for API-based deadline extension
                if let Some(new_deadline) =
                    super::take_watchdog_deadline_override(watchdog_channel_id_num)
                {
                    let max_dl = watchdog_token
                        .watchdog_max_deadline_ms
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let clamped = std::cmp::min(new_deadline, max_dl);
                    watchdog_token
                        .watchdog_deadline_ms
                        .store(clamped, std::sync::atomic::Ordering::Relaxed);
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    let remaining_min =
                        (clamped - chrono::Utc::now().timestamp_millis()) / 1000 / 60;
                    println!(
                        "  [{ts}] ⏰ WATCHDOG: deadline extended for channel {} — {remaining_min}m remaining",
                        channel_id
                    );
                }

                // Auto-extend based on inflight updated_at: if inflight was updated recently
                // (within last 5 min), push deadline forward by the default timeout
                {
                    let current_dl = watchdog_token
                        .watchdog_deadline_ms
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let now_ms_check = chrono::Utc::now().timestamp_millis();
                    // Only auto-extend when close to deadline (within 2 minutes)
                    if now_ms_check > current_dl - 120_000 {
                        if let Some(inflight) = super::inflight::load_inflight_state(
                            &watchdog_provider,
                            watchdog_channel_id_num,
                        ) {
                            if let Ok(updated) = chrono::NaiveDateTime::parse_from_str(
                                &inflight.updated_at,
                                "%Y-%m-%d %H:%M:%S",
                            ) {
                                let updated_ms = updated.and_utc().timestamp_millis();
                                let age_ms = now_ms_check - updated_ms;
                                // If inflight was updated within the last 5 minutes, auto-extend
                                if age_ms < 300_000 {
                                    let max_dl = watchdog_token
                                        .watchdog_max_deadline_ms
                                        .load(std::sync::atomic::Ordering::Relaxed);
                                    let new_dl = std::cmp::min(
                                        now_ms_check + timeout.as_millis() as i64,
                                        max_dl,
                                    );
                                    if new_dl > current_dl {
                                        watchdog_token
                                            .watchdog_deadline_ms
                                            .store(new_dl, std::sync::atomic::Ordering::Relaxed);
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        let remaining_min = (new_dl - now_ms_check) / 1000 / 60;
                                        println!(
                                            "  [{ts}] ⏰ WATCHDOG: auto-extended for channel {} (inflight active) — {remaining_min}m remaining",
                                            channel_id
                                        );
                                    }
                                }
                            }
                        }
                    }
                }

                let current_deadline = watchdog_token
                    .watchdog_deadline_ms
                    .load(std::sync::atomic::Ordering::Relaxed);
                let now = chrono::Utc::now().timestamp_millis();

                if now < current_deadline {
                    continue; // Not yet — deadline may have been extended
                }

                // Deadline reached — fire watchdog
                // Verify this watchdog's token is still the CURRENT active token for this channel.
                let is_current_token = {
                    let data = watchdog_shared.core.lock().await;
                    data.cancel_tokens
                        .get(&channel_id)
                        .map_or(false, |current| {
                            std::sync::Arc::ptr_eq(&watchdog_token, current)
                        })
                };
                if is_current_token {
                    let elapsed_mins =
                        (now - (current_deadline - timeout.as_millis() as i64)) / 1000 / 60;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] ⏰ WATCHDOG: turn timeout (~{elapsed_mins}m) for channel {}, cancelling",
                        channel_id
                    );
                    super::turn_bridge::cancel_active_token(
                        &watchdog_token,
                        true,
                        "watchdog timeout",
                    );

                    // Notify Discord
                    let has_queued = {
                        let mut data = watchdog_shared.core.lock().await;
                        data.intervention_queue
                            .get_mut(&channel_id)
                            .map_or(false, |q| super::has_soft_intervention(q))
                    };
                    let msg = if has_queued {
                        format!(
                            "⚠️ 턴이 {elapsed_mins}분 타임아웃으로 자동 중단되었습니다. 대기 중인 메시지로 다음 턴을 시작합니다.",
                        )
                    } else {
                        format!("⚠️ 턴이 {elapsed_mins}분 타임아웃으로 자동 중단되었습니다.",)
                    };
                    let _ = channel_id.say(&watchdog_http, msg).await;
                }
                return; // Watchdog done regardless
            }
        });
    }

    // Resolve remote profile for this channel
    let remote_profile = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.remote_profile_name.as_ref())
            .and_then(|name| {
                let settings = crate::config::Settings::load();
                settings
                    .remote_profiles
                    .iter()
                    .find(|p| p.name == *name)
                    .cloned()
            })
    };

    reset_provider_session_if_pending(shared, channel_id, &provider).await;

    // Resolve channel/tmux session name from current session state
    let (channel_name, tmux_session_name) = {
        let data = shared.core.lock().await;
        let channel_name = data
            .sessions
            .get(&channel_id)
            .and_then(|s| s.channel_name.clone());
        let tmux_session_name = channel_name
            .as_ref()
            .map(|name| provider.build_tmux_session_name(name));
        (channel_name, tmux_session_name)
    };
    let adk_session_key = build_adk_session_key(shared, channel_id, &provider).await;

    // If in-memory session_id is None (e.g. after dcserver restart),
    // try to restore it from the DB's persisted provider session_id.
    let session_id = if session_id.is_none() {
        if let Some(ref key) = adk_session_key {
            let restored =
                super::adk_session::fetch_provider_session_id(key, &provider, shared.api_port)
                    .await;
            if restored.is_some() {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] ↻ Restored provider session_id from DB for {}",
                    key
                );
                // Also update in-memory session so subsequent turns don't re-fetch
                let mut data = shared.core.lock().await;
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    session.session_id = restored.clone();
                }
            }
            restored
        } else {
            None
        }
    } else {
        session_id
    };

    // Non-Claude first turn: inject SAK as user context (no Anthropic cache benefit).
    // Deferred to here (after session recovery from DB) so that a dcserver restart
    // doesn't duplicate SAK into a resumed session.
    if !matches!(provider, ProviderKind::Claude) && session_id.is_none() {
        if let Some(ref knowledge) = shared_knowledge_content {
            context_prompt = format!("{}\n\n{}", knowledge, context_prompt);
        }
    }

    let adk_session_name = channel_name.clone();
    let adk_session_info = derive_adk_session_info(
        Some(user_text),
        channel_name.as_deref(),
        Some(&current_path),
    );
    // #222: DB-based dispatch lookup takes priority over text parsing.
    // In unified threads, user_text may contain a stale DISPATCH: prefix
    // from a previous dispatch in the same thread. DB lookup uses the
    // thread→card→dispatch link which is always current.
    let dispatch_id =
        super::adk_session::lookup_pending_dispatch_for_thread(shared.api_port, channel_id.get())
            .await
            .or_else(|| super::adk_session::parse_dispatch_id(user_text));
    post_adk_session_status(
        adk_session_key.as_deref(),
        adk_session_name.as_deref(),
        Some(provider.as_str()),
        "working",
        &provider,
        Some(&adk_session_info),
        None,
        Some(&current_path),
        dispatch_id.as_deref(),
        shared.api_port,
    )
    .await;

    let (inflight_tmux_name, inflight_output_path, inflight_input_fifo, inflight_offset) = {
        #[cfg(unix)]
        {
            if remote_profile.is_none()
                && provider.uses_managed_tmux_backend()
                && claude::is_tmux_available()
            {
                if let Some(ref tmux_name) = tmux_session_name {
                    let (output_path, input_fifo_path) = tmux_runtime_paths(tmux_name);
                    let session_exists =
                        crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_name);
                    let last_offset = std::fs::metadata(&output_path)
                        .map(|m| m.len())
                        .unwrap_or(0);
                    (
                        Some(tmux_name.clone()),
                        Some(output_path),
                        Some(input_fifo_path),
                        if session_exists { last_offset } else { 0 },
                    )
                } else {
                    (None, None, None, 0)
                }
            } else {
                (None, None, None, 0)
            }
        }
        #[cfg(not(unix))]
        {
            (None, None, None, 0u64)
        }
    };

    let inflight_state = InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        channel_name.clone(),
        request_owner.get(),
        user_msg_id.get(),
        placeholder_msg_id.get(),
        user_text.to_string(),
        session_id.clone(),
        inflight_tmux_name,
        inflight_output_path,
        inflight_input_fifo.clone(),
        inflight_offset,
    );
    if let Err(e) = save_inflight_state(&inflight_state) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!("  [{ts}]   ⚠ inflight state save failed: {e}");
    }

    // Create channel for streaming
    let (tx, rx) = mpsc::channel();
    let (completion_tx, completion_rx) = if wait_for_completion {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    let session_id_clone = session_id.clone();
    let current_path_clone = current_path.clone();
    let cancel_token_clone = cancel_token.clone();

    // Pause tmux watcher if one exists (so it doesn't read our turn's output)
    if let Some(watcher) = shared.tmux_watchers.get(&channel_id) {
        watcher
            .pause_epoch
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        watcher
            .paused
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // Auto-sync worktree before sending message to session
    {
        let script = super::runtime_store::agentdesk_root()
            .unwrap_or_default()
            .join("scripts/worktree-autosync.sh");
        if script.exists() {
            let ws = current_path.clone();
            let ts = chrono::Local::now().format("%H:%M:%S");
            match std::process::Command::new(&script)
                .arg(&ws)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
            {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let msg = stdout.trim();
                    match out.status.code() {
                        Some(0) => println!("  [{ts}] 🔄 worktree-autosync [{ws}]: {msg}"),
                        Some(1) => println!("  [{ts}] ⏭ worktree-autosync [{ws}]: skipped — {msg}"),
                        _ => eprintln!("  [{ts}] ⚠ worktree-autosync [{ws}]: error — {msg}"),
                    }
                }
                Err(e) => eprintln!("  [{ts}] ⚠ worktree-autosync: failed to run — {e}"),
            }
        }
    }

    let model_for_turn =
        super::commands::resolve_model_for_turn(shared, channel_id, &provider).await;

    // Fetch context compact percent from ADK settings
    let ctx_thresholds = super::adk_session::fetch_context_thresholds(shared.api_port).await;
    let compact_percent = ctx_thresholds.compact_pct;
    // Use model-specific context window (reads Codex models cache), falling
    // back to the provider default if the model isn't found.
    let model_context_window = provider.resolve_context_window(model_for_turn.as_deref());

    // Pre-compute provider-specific compact config
    let compact_percent_for_claude = Some(compact_percent);
    let compact_token_limit_for_codex = {
        let cli_config = provider.compact_cli_config(compact_percent, model_context_window);
        cli_config
            .first()
            .map(|(_, v)| v.parse::<u64>().unwrap_or(0))
    };

    // Run the provider in a blocking thread
    let provider_for_blocking = provider.clone();
    tokio::task::spawn_blocking(move || {
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || match &provider_for_blocking {
                    ProviderKind::Claude => claude::execute_command_streaming(
                        &context_prompt,
                        session_id_clone.as_deref(),
                        &current_path_clone,
                        tx.clone(),
                        Some(&system_prompt_owned),
                        Some(&allowed_tools),
                        Some(cancel_token_clone),
                        remote_profile.as_ref(),
                        tmux_session_name.as_deref(),
                        Some(channel_id.get()),
                        Some(provider_for_blocking.clone()),
                        model_for_turn.as_deref(),
                        compact_percent_for_claude,
                    ),
                    ProviderKind::Codex => codex::execute_command_streaming(
                        &context_prompt,
                        session_id_clone.as_deref(),
                        &current_path_clone,
                        tx.clone(),
                        Some(&system_prompt_owned),
                        Some(&allowed_tools),
                        Some(cancel_token_clone),
                        remote_profile.as_ref(),
                        tmux_session_name.as_deref(),
                        Some(channel_id.get()),
                        Some(provider_for_blocking.clone()),
                        model_for_turn.as_deref(),
                        compact_token_limit_for_codex,
                    ),
                    ProviderKind::Gemini => gemini::execute_command_streaming(
                        &context_prompt,
                        session_id_clone.as_deref(),
                        &current_path_clone,
                        tx.clone(),
                        Some(&system_prompt_owned),
                        Some(&allowed_tools),
                        Some(cancel_token_clone),
                        remote_profile.as_ref(),
                        tmux_session_name.as_deref(),
                        Some(channel_id.get()),
                        Some(provider_for_blocking.clone()),
                        model_for_turn.as_deref(),
                        None, // Gemini: compact not supported
                    ),
                    ProviderKind::Qwen => qwen::execute_command_streaming(
                        &context_prompt,
                        session_id_clone.as_deref(),
                        &current_path_clone,
                        tx.clone(),
                        Some(&system_prompt_owned),
                        Some(&allowed_tools),
                        Some(cancel_token_clone),
                        remote_profile.as_ref(),
                        tmux_session_name.as_deref(),
                        Some(channel_id.get()),
                        Some(provider_for_blocking.clone()),
                        model_for_turn.as_deref(),
                        None, // Qwen: compact not supported
                    ),
                    ProviderKind::Unsupported(name) => {
                        let _ = tx.send(StreamMessage::Error {
                            message: format!("Provider '{}' is not installed", name),
                            stdout: String::new(),
                            stderr: String::new(),
                            exit_code: None,
                        });
                        Ok(())
                    }
                },
            ));

        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                eprintln!("  [streaming] Error: {}", e);
                let _ = tx.send(StreamMessage::Error {
                    message: e,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
            }
            Err(panic_info) => {
                let msg = if let Some(s) = panic_info.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = panic_info.downcast_ref::<&str>() {
                    s.to_string()
                } else {
                    "unknown panic".to_string()
                };
                eprintln!("  [streaming] PANIC: {}", msg);
                let _ = tx.send(StreamMessage::Error {
                    message: format!("Internal error (panic): {}", msg),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
            }
        }
    });

    spawn_turn_bridge(
        ctx.http.clone(),
        shared.clone(),
        cancel_token.clone(),
        rx,
        TurnBridgeContext {
            provider,
            channel_id,
            user_msg_id,
            user_text_owned: user_text.to_string(),
            request_owner_name: request_owner_name.to_string(),
            request_owner: Some(request_owner),
            serenity_ctx: Some(ctx.clone()),
            token: Some(token.to_string()),
            role_binding: role_binding.clone(),
            adk_session_key,
            adk_session_name,
            adk_session_info: Some(adk_session_info),
            adk_cwd: Some(current_path.clone()),
            dispatch_id,
            current_msg_id: placeholder_msg_id,
            response_sent_offset: 0,
            full_response: String::new(),
            tmux_last_offset: Some(inflight_offset),
            new_session_id: session_id.clone(),
            defer_watcher_resume,
            completion_tx,
            inflight_state,
        },
    );

    if let Some(rx) = completion_rx {
        rx.await
            .map_err(|_| "queued turn completion wait failed".to_string())?;
    }

    Ok(())
}

/// Handle file uploads from Discord messages
async fn handle_file_upload(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    let channel_id = msg.channel_id;

    // Always use the runtime uploads directory (works without session)
    let Some(save_dir) = channel_upload_dir(channel_id) else {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(&ctx.http, "Cannot resolve upload directory.")
            .await;
        return Ok(());
    };

    if let Err(e) = fs::create_dir_all(&save_dir) {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(
                &ctx.http,
                format!("Failed to prepare upload directory: {}", e),
            )
            .await;
        return Ok(());
    }

    for attachment in &msg.attachments {
        let file_name = &attachment.filename;

        // Download file from Discord CDN
        let buf = match reqwest::get(&attachment.url).await {
            Ok(resp) => match resp.bytes().await {
                Ok(bytes) => bytes,
                Err(e) => {
                    rate_limit_wait(shared, channel_id).await;
                    let _ = channel_id
                        .say(&ctx.http, format!("Download failed: {}", e))
                        .await;
                    continue;
                }
            },
            Err(e) => {
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id
                    .say(&ctx.http, format!("Download failed: {}", e))
                    .await;
                continue;
            }
        };

        // Save to session path (sanitize filename)
        let safe_name = Path::new(file_name)
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("uploaded_file"));
        let ts = chrono::Utc::now().timestamp_millis();
        let stamped_name = format!("{}_{}", ts, safe_name.to_string_lossy());
        let dest = save_dir.join(stamped_name);
        let file_size = buf.len();

        match fs::write(&dest, &buf) {
            Ok(_) => {
                let msg_text = format!("Saved: {}\n({} bytes)", dest.display(), file_size);
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id.say(&ctx.http, &msg_text).await;
            }
            Err(e) => {
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id
                    .say(&ctx.http, format!("Failed to save file: {}", e))
                    .await;
                continue;
            }
        }

        // Record upload in session
        let upload_record = format!(
            "[File uploaded] {} → {} ({} bytes)",
            file_name,
            dest.display(),
            file_size
        );
        {
            let mut data = shared.core.lock().await;
            if let Some(session) = data.sessions.get_mut(&channel_id) {
                session.history.push(HistoryItem {
                    item_type: HistoryType::User,
                    content: upload_record.clone(),
                });
                session.pending_uploads.push(upload_record);
            }
        }
    }

    Ok(())
}

/// Handle shell commands from raw text messages (! prefix)
async fn handle_shell_command_raw(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    text: &str,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    let cmd_str = text.strip_prefix('!').unwrap_or("").trim();
    if cmd_str.is_empty() {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(&ctx.http, "Usage: `!<command>`\nExample: `!ls -la`")
            .await;
        return Ok(());
    }

    let working_dir = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.current_path.clone())
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|h| h.display().to_string())
                    .unwrap_or_else(|| "/".to_string())
            })
    };

    let cmd_owned = cmd_str.to_string();
    let working_dir_clone = working_dir.clone();

    let result = tokio::task::spawn_blocking(move || {
        let child = crate::services::platform::shell::shell_command_builder(&cmd_owned)
            .current_dir(&working_dir_clone)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        match child {
            Ok(child) => child.wait_with_output(),
            Err(e) => Err(e),
        }
    })
    .await;

    let response = match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let exit_code = output.status.code().unwrap_or(-1);
            let mut parts = Vec::new();
            if !stdout.is_empty() {
                parts.push(format!("```\n{}\n```", stdout.trim_end()));
            }
            if !stderr.is_empty() {
                parts.push(format!("stderr:\n```\n{}\n```", stderr.trim_end()));
            }
            if parts.is_empty() {
                parts.push(format!("(exit code: {})", exit_code));
            } else if exit_code != 0 {
                parts.push(format!("(exit code: {})", exit_code));
            }
            parts.join("\n")
        }
        Ok(Err(e)) => format!("Failed to execute: {}", e),
        Err(e) => format!("Task error: {}", e),
    };

    send_long_message_raw(&ctx.http, channel_id, &response, shared).await?;
    Ok(())
}

/// Handle text-based commands (!start, !meeting, !stop, !clear, etc.).
/// Returns true if the command was handled, false otherwise.
async fn handle_text_command(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    data: &Data,
    channel_id: serenity::ChannelId,
    text: &str,
) -> Result<bool, Error> {
    let parts: Vec<&str> = text.splitn(3, char::is_whitespace).collect();
    let cmd = parts[0];
    let arg1 = parts.get(1).unwrap_or(&"");
    let arg2 = parts.get(2).unwrap_or(&"");

    match cmd {
        "!start" => {
            let path_str = if arg1.is_empty() { "." } else { arg1 };

            // Resolve path
            let effective_path = if path_str == "." || path_str.is_empty() {
                // Use workspace root or current directory
                let Some(workspace_dir) = runtime_store::workspace_root() else {
                    let _ = msg
                        .reply(&ctx.http, "Error: cannot determine workspace root.")
                        .await;
                    return Ok(true);
                };
                // Create a random workspace for this channel
                use rand::Rng;
                let random_name: String = rand::thread_rng()
                    .sample_iter(&rand::distributions::Alphanumeric)
                    .take(8)
                    .map(char::from)
                    .collect();
                let ch_name = resolve_channel_category(ctx, channel_id)
                    .await
                    .0
                    .unwrap_or_else(|| format!("ch-{}", channel_id));
                let dir = workspace_dir.join(format!("{}-{}", ch_name, random_name));
                std::fs::create_dir_all(&dir).ok();
                dir.to_string_lossy().to_string()
            } else if path_str.starts_with('~') {
                dirs::home_dir()
                    .map(|h| path_str.replacen('~', &h.to_string_lossy(), 1))
                    .unwrap_or_else(|| path_str.to_string())
            } else {
                path_str.to_string()
            };

            // Validate path exists
            if !std::path::Path::new(&effective_path).exists() {
                let _ = msg
                    .reply(
                        &ctx.http,
                        format!("Error: path `{}` does not exist.", effective_path),
                    )
                    .await;
                return Ok(true);
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ◀ [{}] !start path={}",
                msg.author.name, effective_path
            );

            // Create session
            let (ch_name, cat_name) = resolve_channel_category(ctx, channel_id).await;
            {
                let mut d = data.shared.core.lock().await;
                let session = d
                    .sessions
                    .entry(channel_id)
                    .or_insert_with(|| DiscordSession {
                        session_id: None,
                        current_path: None,
                        history: Vec::new(),
                        pending_uploads: Vec::new(),
                        cleared: false,
                        channel_name: None,
                        category_name: None,
                        remote_profile_name: None,
                        channel_id: Some(channel_id.get()),
                        last_active: tokio::time::Instant::now(),
                        worktree: None,

                        born_generation: runtime_store::load_generation(),
                    });
                session.current_path = Some(effective_path.clone());
                session.channel_name = ch_name;
                session.category_name = cat_name;
                session.last_active = tokio::time::Instant::now();
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ▶ Session started: {}", effective_path);
            let _ = msg
                .reply(
                    &ctx.http,
                    format!("Session started at `{}`.", effective_path),
                )
                .await;
            return Ok(true);
        }

        "!meeting" => {
            let action = if arg1.is_empty() { "start" } else { arg1 };
            let agenda = if arg2.is_empty() { arg1 } else { arg2 };

            match action {
                "start" => {
                    let agenda_text = if agenda.is_empty() || *agenda == "start" {
                        let _ = msg
                            .reply(
                                &ctx.http,
                                "사용법: `!meeting start <안건>` 또는 `!meeting <안건>`",
                            )
                            .await;
                        return Ok(true);
                    } else {
                        agenda
                    };

                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] ◀ [{}] !meeting start {}",
                        msg.author.name, agenda_text
                    );

                    let http = ctx.http.clone();
                    let shared = data.shared.clone();
                    let provider = data.provider.clone();
                    let agenda_owned = agenda_text.to_string();

                    let _ = msg
                        .reply(
                            &ctx.http,
                            format!(
                                "📋 회의를 시작할게. 진행 모델: {} / 교차검증: {}",
                                provider.display_name(),
                                provider.counterpart().display_name()
                            ),
                        )
                        .await;

                    tokio::spawn(async move {
                        match meeting::start_meeting(
                            &*http,
                            channel_id,
                            &agenda_owned,
                            provider,
                            &shared,
                        )
                        .await
                        {
                            Ok(Some(id)) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                println!("  [{ts}] ✅ Meeting completed: {id}");
                            }
                            Ok(None) => {}
                            Err(e) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                println!("  [{ts}] ❌ Meeting error: {e}");
                            }
                        }
                    });
                    return Ok(true);
                }
                "stop" => {
                    let _ = meeting::cancel_meeting(&ctx.http, channel_id, &data.shared).await;
                    return Ok(true);
                }
                "status" => {
                    let _ = meeting::meeting_status(&ctx.http, channel_id, &data.shared).await;
                    return Ok(true);
                }
                _ => {
                    // Treat unknown action as agenda text
                    let full_agenda = text.trim_start_matches("!meeting").trim();
                    if full_agenda.is_empty() {
                        let _ = msg.reply(&ctx.http, "사용법: `!meeting <안건>`").await;
                        return Ok(true);
                    }
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] ◀ [{}] !meeting {}", msg.author.name, full_agenda);

                    let http = ctx.http.clone();
                    let shared = data.shared.clone();
                    let provider = data.provider.clone();
                    let agenda_owned = full_agenda.to_string();

                    let _ = msg
                        .reply(
                            &ctx.http,
                            format!(
                                "📋 회의를 시작할게. 진행 모델: {} / 교차검증: {}",
                                provider.display_name(),
                                provider.counterpart().display_name()
                            ),
                        )
                        .await;

                    tokio::spawn(async move {
                        match meeting::start_meeting(
                            &*http,
                            channel_id,
                            &agenda_owned,
                            provider,
                            &shared,
                        )
                        .await
                        {
                            Ok(Some(id)) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                println!("  [{ts}] ✅ Meeting completed: {id}");
                            }
                            Ok(None) => {}
                            Err(e) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                println!("  [{ts}] ❌ Meeting error: {e}");
                            }
                        }
                    });
                    return Ok(true);
                }
            }
        }

        "!stop" => {
            let mut d = data.shared.core.lock().await;
            if let Some(token) = d.cancel_tokens.remove(&channel_id) {
                token.cancel_with_tmux_cleanup();
                drop(d);
                let _ = msg.reply(&ctx.http, "Turn cancelled.").await;
            } else {
                drop(d);
                let _ = msg.reply(&ctx.http, "No active turn to stop.").await;
            }
            return Ok(true);
        }

        "!clear" => {
            let mut d = data.shared.core.lock().await;
            if let Some(token) = d.cancel_tokens.remove(&channel_id) {
                token.cancel_with_tmux_cleanup();
            }
            // Build tmux session name from channel name
            let tmux_name = d
                .sessions
                .get(&channel_id)
                .and_then(|s| s.channel_name.as_ref())
                .map(|ch_name| data.provider.build_tmux_session_name(ch_name));
            if let Some(session) = d.sessions.get_mut(&channel_id) {
                session.history.clear();
                session.pending_uploads.clear();
                session.cleared = true;
                session.session_id = None;
            }
            d.intervention_queue.remove(&channel_id);
            drop(d);
            // Clear stored provider session_id from DB
            if let Some(key) =
                super::adk_session::build_adk_session_key(&data.shared, channel_id, &data.provider)
                    .await
            {
                super::adk_session::clear_provider_session_id(&key, data.shared.api_port).await;
            }
            // Send /clear to the actual Claude Code session via tmux
            #[cfg(unix)]
            if data.provider == crate::services::provider::ProviderKind::Claude {
                if let Some(ref name) = tmux_name {
                    let exact_target = tmux_exact_target(name);
                    let _ = tokio::task::spawn_blocking(move || {
                        std::process::Command::new("tmux")
                            .args(["send-keys", "-t", &exact_target, "/clear", "Enter"])
                            .output()
                    })
                    .await;
                }
            }
            let _ = msg.reply(&ctx.http, "Session cleared.").await;
            return Ok(true);
        }

        // ── Simple diagnostic / info commands ──
        "!pwd" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !pwd", msg.author.name);

            auto_restore_session(&data.shared, channel_id, ctx).await;

            let (current_path, remote_name) = {
                let d = data.shared.core.lock().await;
                let session = d.sessions.get(&channel_id);
                (
                    session.and_then(|s| s.current_path.clone()),
                    session.and_then(|s| s.remote_profile_name.clone()),
                )
            };
            let reply = match current_path {
                Some(path) => {
                    let remote_info = remote_name
                        .map(|n| format!(" (remote: **{}**)", n))
                        .unwrap_or_else(|| " (local)".to_string());
                    format!("`{}`{}", path, remote_info)
                }
                None => "No active session. Use `!start <path>` first.".to_string(),
            };
            let _ = msg.reply(&ctx.http, &reply).await;
            return Ok(true);
        }

        "!health" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !health", msg.author.name);

            let text =
                commands::build_health_report(&data.shared, &data.provider, channel_id).await;
            send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
            return Ok(true);
        }

        "!status" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !status", msg.author.name);

            let text =
                commands::build_status_report(&data.shared, &data.provider, channel_id).await;
            send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
            return Ok(true);
        }

        "!inflight" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !inflight", msg.author.name);

            let text =
                commands::build_inflight_report(&data.shared, &data.provider, channel_id).await;
            send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
            return Ok(true);
        }

        "!queue" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !queue", msg.author.name);

            let show_all = *arg1 == "all";
            let text =
                commands::build_queue_report(&data.shared, &data.provider, channel_id, show_all)
                    .await;
            send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
            return Ok(true);
        }

        "!metrics" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !metrics", msg.author.name);

            let metrics_data = if arg1.is_empty() {
                metrics::load_today()
            } else {
                metrics::load_date(arg1)
            };
            let label = if arg1.is_empty() { "today" } else { arg1 };
            let text = metrics::build_metrics_report(&metrics_data, label);
            send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
            return Ok(true);
        }

        "!debug" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !debug", msg.author.name);

            let new_state = claude::toggle_debug();
            let status = if new_state { "ON" } else { "OFF" };
            let _ = msg
                .reply(&ctx.http, format!("Debug logging: **{}**", status))
                .await;
            println!("  [{ts}] ▶ Debug logging toggled to {status}");
            return Ok(true);
        }

        "!help" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !help", msg.author.name);

            let provider_name = data.provider.display_name();
            let help = format!(
                "\
**AgentDesk Discord Bot**
Manage server files & chat with {p}.
Each channel gets its own independent {p} session.

**Session**
`!start <path>` — Start session at directory
`!pwd` — Show current working directory
`!health` — Show runtime health summary
`!status` — Show this channel session status
`!inflight` — Show saved inflight turn state
`!clear` — Clear AI conversation history
`!stop` — Stop current AI request

**File Transfer**
`!down <file>` — Download file from server
Send a file/photo — Upload to session directory

**Shell**
`!shell <command>` — Run shell command directly

**AI Chat**
Any other message is sent to {p}.

**Tool Management**
`!allowedtools` — Show currently allowed tools
`!allowed +name` — Add tool (e.g. `!allowed +Bash`)
`!allowed -name` — Remove tool

**Skills**
`!cc <skill>` — Run a provider skill

**Settings**
`/model` — Open the interactive model picker
`!debug` — Toggle debug logging
`!metrics [date]` — Show turn metrics
`!queue [all]` — Show pending queue

**User Management** (owner only)
`!allowall on|off|status` — Allow everyone or restrict to authorized users
`!adduser <user_id>` — Allow a user to use the bot
`!removeuser <user_id>` — Remove a user's access
`!help` — Show this help",
                p = provider_name
            );
            send_long_message_raw(&ctx.http, channel_id, &help, &data.shared).await?;
            return Ok(true);
        }

        "!allowedtools" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !allowedtools", msg.author.name);

            let tools = {
                let settings = data.shared.settings.read().await;
                settings.allowed_tools.clone()
            };

            let mut reply = String::from("**Allowed Tools**\n\n");
            for tool in &tools {
                let (desc, destructive) = super::formatting::tool_info(tool);
                let badge = super::formatting::risk_badge(destructive);
                if badge.is_empty() {
                    reply.push_str(&format!("`{}` — {}\n", tool, desc));
                } else {
                    reply.push_str(&format!("`{}` {} — {}\n", tool, badge, desc));
                }
            }
            reply.push_str(&format!(
                "\n{} = destructive\nTotal: {}",
                super::formatting::risk_badge(true),
                tools.len()
            ));
            send_long_message_raw(&ctx.http, channel_id, &reply, &data.shared).await?;
            return Ok(true);
        }

        // ── Commands with arguments ──
        "!model" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !model {} {}", msg.author.name, arg1, arg2);
            let _ = msg
                .reply(
                    &ctx.http,
                    "Model picker text commands are deprecated. Use `/model`.",
                )
                .await;
            return Ok(true);
        }

        "!allowed" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !allowed {}", msg.author.name, arg1);

            let arg = arg1.trim();
            let (op, raw_name) = if let Some(name) = arg.strip_prefix('+') {
                ('+', name.trim())
            } else if let Some(name) = arg.strip_prefix('-') {
                ('-', name.trim())
            } else {
                let _ = msg.reply(&ctx.http, "Use `+toolname` to add or `-toolname` to remove.\nExample: `!allowed +Bash`").await;
                return Ok(true);
            };

            if raw_name.is_empty() {
                let _ = msg.reply(&ctx.http, "Tool name cannot be empty.").await;
                return Ok(true);
            }

            let Some(tool_name) =
                super::formatting::canonical_tool_name(raw_name).map(str::to_string)
            else {
                let _ = msg
                    .reply(
                        &ctx.http,
                        format!(
                            "Unknown tool `{}`. Use `!allowedtools` to see valid tool names.",
                            raw_name
                        ),
                    )
                    .await;
                return Ok(true);
            };

            let response_msg = {
                let mut settings = data.shared.settings.write().await;
                match op {
                    '+' => {
                        if settings.allowed_tools.iter().any(|t| t == &tool_name) {
                            format!("`{}` is already in the list.", tool_name)
                        } else {
                            settings.allowed_tools.push(tool_name.clone());
                            save_bot_settings(&data.token, &settings);
                            format!("Added `{}`", tool_name)
                        }
                    }
                    '-' => {
                        let before_len = settings.allowed_tools.len();
                        settings.allowed_tools.retain(|t| t != &tool_name);
                        if settings.allowed_tools.len() < before_len {
                            save_bot_settings(&data.token, &settings);
                            format!("Removed `{}`", tool_name)
                        } else {
                            format!("`{}` is not in the list.", tool_name)
                        }
                    }
                    _ => unreachable!(),
                }
            };
            let _ = msg.reply(&ctx.http, &response_msg).await;
            return Ok(true);
        }

        "!adduser" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !adduser {}", msg.author.name, arg1);

            if !check_owner(msg.author.id, &data.shared).await {
                let _ = msg.reply(&ctx.http, "Only the owner can add users.").await;
                return Ok(true);
            }

            let raw_id = arg1
                .trim()
                .trim_start_matches("<@")
                .trim_end_matches('>')
                .trim_start_matches('!');
            let target_id: u64 = match raw_id.parse() {
                Ok(id) => id,
                Err(_) => {
                    let _ = msg
                        .reply(&ctx.http, "Usage: `!adduser <user_id>` or `!adduser @user`")
                        .await;
                    return Ok(true);
                }
            };

            {
                let mut settings = data.shared.settings.write().await;
                if settings.allowed_user_ids.contains(&target_id) {
                    let _ = msg
                        .reply(&ctx.http, format!("`{}` is already authorized.", target_id))
                        .await;
                    return Ok(true);
                }
                settings.allowed_user_ids.push(target_id);
                save_bot_settings(&data.token, &settings);
            }

            let _ = msg
                .reply(
                    &ctx.http,
                    format!("Added `{}` as authorized user.", target_id),
                )
                .await;
            println!("  [{ts}] ▶ Added user: {target_id}");
            return Ok(true);
        }

        "!allowall" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !allowall {}", msg.author.name, arg1);

            if !check_owner(msg.author.id, &data.shared).await {
                let _ = msg
                    .reply(&ctx.http, "Only the owner can change public access.")
                    .await;
                return Ok(true);
            }

            let action = arg1.trim().to_ascii_lowercase();
            if action.is_empty() || action == "status" {
                let enabled = {
                    let settings = data.shared.settings.read().await;
                    settings.allow_all_users
                };
                let message = if enabled {
                    "Public access is enabled. Any Discord user can talk to this bot in allowed channels."
                } else {
                    "Public access is disabled. Only the owner and authorized users can talk to this bot."
                };
                let _ = msg.reply(&ctx.http, message).await;
                return Ok(true);
            }

            let enabled = match action.as_str() {
                "on" | "true" | "enable" | "enabled" => true,
                "off" | "false" | "disable" | "disabled" => false,
                _ => {
                    let _ = msg
                        .reply(
                            &ctx.http,
                            "Usage: `!allowall on`, `!allowall off`, or `!allowall status`",
                        )
                        .await;
                    return Ok(true);
                }
            };

            let response = {
                let mut settings = data.shared.settings.write().await;
                settings.allow_all_users = enabled;
                save_bot_settings(&data.token, &settings);
                if enabled {
                    "Public access enabled. Any Discord user can talk to this bot in allowed channels."
                } else {
                    "Public access disabled. Only the owner and authorized users can talk to this bot."
                }
            };

            let _ = msg.reply(&ctx.http, response).await;
            println!("  [{ts}] ▶ {response}");
            return Ok(true);
        }

        "!removeuser" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !removeuser {}", msg.author.name, arg1);

            if !check_owner(msg.author.id, &data.shared).await {
                let _ = msg
                    .reply(&ctx.http, "Only the owner can remove users.")
                    .await;
                return Ok(true);
            }

            let raw_id = arg1
                .trim()
                .trim_start_matches("<@")
                .trim_end_matches('>')
                .trim_start_matches('!');
            let target_id: u64 = match raw_id.parse() {
                Ok(id) => id,
                Err(_) => {
                    let _ = msg
                        .reply(
                            &ctx.http,
                            "Usage: `!removeuser <user_id>` or `!removeuser @user`",
                        )
                        .await;
                    return Ok(true);
                }
            };

            {
                let mut settings = data.shared.settings.write().await;
                let before_len = settings.allowed_user_ids.len();
                settings.allowed_user_ids.retain(|&id| id != target_id);
                if settings.allowed_user_ids.len() == before_len {
                    let _ = msg
                        .reply(
                            &ctx.http,
                            format!("`{}` is not in the authorized list.", target_id),
                        )
                        .await;
                    return Ok(true);
                }
                save_bot_settings(&data.token, &settings);
            }

            let _ = msg
                .reply(
                    &ctx.http,
                    format!("Removed `{}` from authorized users.", target_id),
                )
                .await;
            println!("  [{ts}] ▶ Removed user: {target_id}");
            return Ok(true);
        }

        "!down" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let file_arg = text.strip_prefix("!down").unwrap_or("").trim();
            println!("  [{ts}] ◀ [{}] !down {}", msg.author.name, file_arg);

            if file_arg.is_empty() {
                let _ = msg
                    .reply(
                        &ctx.http,
                        "Usage: `!down <filepath>`\nExample: `!down /home/user/file.txt`",
                    )
                    .await;
                return Ok(true);
            }

            // Resolve relative path
            let resolved_path = if std::path::Path::new(file_arg).is_absolute() {
                file_arg.to_string()
            } else {
                let current_path = {
                    let d = data.shared.core.lock().await;
                    d.sessions
                        .get(&channel_id)
                        .and_then(|s| s.current_path.clone())
                };
                match current_path {
                    Some(base) => format!("{}/{}", base.trim_end_matches('/'), file_arg),
                    None => {
                        let _ = msg
                            .reply(
                                &ctx.http,
                                "No active session. Use absolute path or `!start <path>` first.",
                            )
                            .await;
                        return Ok(true);
                    }
                }
            };

            let path = std::path::Path::new(&resolved_path);
            if !path.exists() {
                let _ = msg
                    .reply(&ctx.http, format!("File not found: {}", resolved_path))
                    .await;
                return Ok(true);
            }
            if !path.is_file() {
                let _ = msg
                    .reply(&ctx.http, format!("Not a file: {}", resolved_path))
                    .await;
                return Ok(true);
            }

            let attachment = CreateAttachment::path(path).await?;
            rate_limit_wait(&data.shared, channel_id).await;
            let _ = channel_id
                .send_message(&ctx.http, CreateMessage::new().add_file(attachment))
                .await;
            return Ok(true);
        }

        "!shell" => {
            let cmd_str = text.strip_prefix("!shell").unwrap_or("").trim();
            let ts = chrono::Local::now().format("%H:%M:%S");
            let preview = truncate_str(cmd_str, 60);
            println!("  [{ts}] ◀ [{}] !shell {}", msg.author.name, preview);

            if cmd_str.is_empty() {
                let _ = msg
                    .reply(
                        &ctx.http,
                        "Usage: `!shell <command>`\nExample: `!shell ls -la`",
                    )
                    .await;
                return Ok(true);
            }

            let working_dir = {
                let d = data.shared.core.lock().await;
                d.sessions
                    .get(&channel_id)
                    .and_then(|s| s.current_path.clone())
                    .unwrap_or_else(|| {
                        dirs::home_dir()
                            .map(|h| h.display().to_string())
                            .unwrap_or_else(|| "/".to_string())
                    })
            };

            let cmd_owned = cmd_str.to_string();
            let working_dir_clone = working_dir.clone();

            let result = tokio::task::spawn_blocking(move || {
                let child = crate::services::platform::shell::shell_command_builder(&cmd_owned)
                    .current_dir(&working_dir_clone)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn();
                match child {
                    Ok(child) => child.wait_with_output(),
                    Err(e) => Err(e),
                }
            })
            .await;

            let response = match result {
                Ok(Ok(output)) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let exit_code = output.status.code().unwrap_or(-1);
                    let mut parts = Vec::new();
                    if !stdout.is_empty() {
                        parts.push(format!("```\n{}\n```", stdout.trim_end()));
                    }
                    if !stderr.is_empty() {
                        parts.push(format!("stderr:\n```\n{}\n```", stderr.trim_end()));
                    }
                    if parts.is_empty() {
                        parts.push(format!("(exit code: {})", exit_code));
                    } else if exit_code != 0 {
                        parts.push(format!("(exit code: {})", exit_code));
                    }
                    parts.join("\n")
                }
                Ok(Err(e)) => format!("Failed to execute: {}", e),
                Err(e) => format!("Task error: {}", e),
            };

            send_long_message_raw(&ctx.http, channel_id, &response, &data.shared).await?;
            return Ok(true);
        }

        "!cc" => {
            let skill = arg1.to_string();
            let args_str = text
                .strip_prefix("!cc")
                .unwrap_or("")
                .trim()
                .strip_prefix(&skill)
                .unwrap_or("")
                .trim();
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ◀ [{}] !cc {} {}",
                msg.author.name, skill, args_str
            );

            if skill.is_empty() {
                let _ = msg.reply(&ctx.http, "Usage: `!cc <skill> [args]`").await;
                return Ok(true);
            }

            // Handle built-in shortcuts
            match skill.as_str() {
                "clear" => {
                    let _ = msg.reply(&ctx.http, "Use `!clear` instead.").await;
                    return Ok(true);
                }
                "stop" => {
                    let mut d = data.shared.core.lock().await;
                    if let Some(token) = d.cancel_tokens.remove(&channel_id) {
                        token.cancel_with_tmux_cleanup();
                        drop(d);
                        let _ = msg.reply(&ctx.http, "Stopping...").await;
                    } else {
                        drop(d);
                        let _ = msg.reply(&ctx.http, "No active request to stop.").await;
                    }
                    return Ok(true);
                }
                "pwd" => {
                    // Delegate to !pwd
                    return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!pwd")).await;
                }
                "health" => {
                    return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!health"))
                        .await;
                }
                "status" => {
                    return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!status"))
                        .await;
                }
                "inflight" => {
                    return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!inflight"))
                        .await;
                }
                "help" => {
                    return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!help"))
                        .await;
                }
                _ => {}
            }

            // Auto-restore session
            auto_restore_session(&data.shared, channel_id, ctx).await;

            // Verify skill exists
            let skill_exists = {
                let skills = data.shared.skills_cache.read().await;
                skills.iter().any(|(name, _)| name == &skill)
            };

            if !skill_exists {
                let _ = msg
                    .reply(
                        &ctx.http,
                        format!(
                            "Unknown skill: `{}`. Use `!cc` to see available skills.",
                            skill
                        ),
                    )
                    .await;
                return Ok(true);
            }

            // Check session exists
            let has_session = {
                let d = data.shared.core.lock().await;
                d.sessions
                    .get(&channel_id)
                    .and_then(|s| s.current_path.as_ref())
                    .is_some()
            };

            if !has_session {
                let _ = msg
                    .reply(&ctx.http, "No active session. Use `!start <path>` first.")
                    .await;
                return Ok(true);
            }

            // Block if AI is in progress
            {
                let d = data.shared.core.lock().await;
                if d.cancel_tokens.contains_key(&channel_id) {
                    drop(d);
                    let _ = msg
                        .reply(&ctx.http, "AI request in progress. Use `!stop` to cancel.")
                        .await;
                    return Ok(true);
                }
            }

            // Build the prompt
            let skill_prompt = match &data.provider {
                ProviderKind::Claude => {
                    if args_str.is_empty() {
                        format!(
                            "Execute the skill `/{skill}` now. \
                             Use the Skill tool with skill=\"{skill}\"."
                        )
                    } else {
                        format!(
                            "Execute the skill `/{skill}` with arguments: {args_str}\n\
                             Use the Skill tool with skill=\"{skill}\", args=\"{args_str}\"."
                        )
                    }
                }
                ProviderKind::Codex => {
                    if args_str.is_empty() {
                        format!(
                            "Use the local Codex skill `/{skill}` now. \
                             Follow its SKILL.md instructions exactly and complete the task."
                        )
                    } else {
                        format!(
                            "Use the local Codex skill `/{skill}` now with this user request: {args_str}\n\
                             Follow its SKILL.md instructions exactly and adapt them to the request."
                        )
                    }
                }
                ProviderKind::Gemini => {
                    if args_str.is_empty() {
                        format!(
                            "Use the local Gemini skill `/{skill}` now. \
                             Follow its SKILL.md instructions exactly and complete the task."
                        )
                    } else {
                        format!(
                            "Use the local Gemini skill `/{skill}` now with this user request: {args_str}\n\
                             Follow its SKILL.md instructions exactly and adapt them to the request."
                        )
                    }
                }
                ProviderKind::Qwen => {
                    if args_str.is_empty() {
                        format!(
                            "Use the local Qwen skill `/{skill}` now. \
                             Follow its SKILL.md instructions exactly and complete the task."
                        )
                    } else {
                        format!(
                            "Use the local Qwen skill `/{skill}` now with this user request: {args_str}\n\
                             Follow its SKILL.md instructions exactly and adapt them to the request."
                        )
                    }
                }
                ProviderKind::Unsupported(name) => {
                    let _ = msg
                        .reply(&ctx.http, format!("Provider '{}' is not installed.", name))
                        .await;
                    return Ok(true);
                }
            };

            // Send confirmation and hand off to AI
            rate_limit_wait(&data.shared, channel_id).await;
            let confirm = channel_id
                .send_message(
                    &ctx.http,
                    CreateMessage::new().content(format!("Running skill: `/{skill}`")),
                )
                .await?;

            handle_text_message(
                ctx,
                channel_id,
                confirm.id,
                msg.author.id,
                &msg.author.name,
                &skill_prompt,
                &data.shared,
                &data.token,
                false,
                false,
                false,
                None,
            )
            .await?;
            return Ok(true);
        }

        _ => {}
    }

    Ok(false)
}

/// Look up the active_thread_id for a dispatch's kanban card via internal API.
/// Dispatch info returned by the card-thread internal API.
struct DispatchInfo {
    active_thread_id: Option<String>,
    dispatch_type: Option<String>,
    discord_channel_alt: Option<String>,
    /// #259: Dispatch context JSON — used to extract worktree_path for session CWD.
    context: Option<String>,
}

#[allow(dead_code)]
async fn lookup_card_thread(api_port: u16, dispatch_id: &str) -> Option<String> {
    let info = lookup_dispatch_info(api_port, dispatch_id).await?;
    info.active_thread_id
}

async fn lookup_dispatch_info(api_port: u16, dispatch_id: &str) -> Option<DispatchInfo> {
    let url = crate::config::local_api_url(
        api_port,
        &format!("/api/internal/card-thread?dispatch_id={dispatch_id}"),
    );
    let resp = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    Some(DispatchInfo {
        active_thread_id: body
            .get("active_thread_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        dispatch_type: body
            .get("dispatch_type")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        discord_channel_alt: body
            .get("discord_channel_alt")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        context: body
            .get("dispatch_context")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

/// Verify a thread is accessible and not locked via Discord API.
/// Returns true if the thread exists and is not locked.
async fn verify_thread_accessible(
    ctx: &poise::serenity_prelude::Context,
    thread_id: ChannelId,
) -> bool {
    match ctx.http.get_channel(thread_id).await {
        Ok(channel) => {
            if let Some(guild_channel) = channel.guild() {
                // Check if thread is locked
                if let Some(ref metadata) = guild_channel.thread_metadata {
                    if metadata.locked {
                        return false;
                    }
                    // Unarchive if needed — send will fail on archived threads via gateway
                    if metadata.archived {
                        let edit =
                            poise::serenity_prelude::builder::EditThread::new().archived(false);
                        if let Err(e) = thread_id.edit_thread(&ctx.http, edit).await {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            println!("  [{ts}] ⚠️ Failed to unarchive thread {thread_id}: {e}");
                            return false;
                        }
                    }
                }
                true
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

/// Link a newly created dispatch thread to the card's active_thread_id via internal API.
async fn link_dispatch_thread(api_port: u16, dispatch_id: &str, thread_id: u64, channel_id: u64) {
    let url = crate::config::local_api_url(api_port, "/api/internal/link-dispatch-thread");
    let _ = reqwest::Client::new()
        .post(&url)
        .timeout(std::time::Duration::from_secs(2))
        .json(&serde_json::json!({
            "dispatch_id": dispatch_id,
            "thread_id": thread_id.to_string(),
            "channel_id": channel_id.to_string(),
        }))
        .send()
        .await;
}

#[cfg(test)]
mod tests {
    use super::super::model_picker_interaction::build_model_picker_close_response;
    use super::{
        is_model_picker_component_custom_id, should_process_turn_message,
        should_skip_human_slash_message,
    };
    use poise::serenity_prelude::ChannelId;
    use serde_json::json;
    use serenity::model::channel::MessageType;

    #[test]
    fn turn_messages_allow_regular_and_inline_reply() {
        assert!(should_process_turn_message(MessageType::Regular));
        assert!(should_process_turn_message(MessageType::InlineReply));
    }

    #[test]
    fn system_messages_are_not_processed_as_turns() {
        assert!(!should_process_turn_message(MessageType::ThreadCreated));
        assert!(!should_process_turn_message(
            MessageType::ThreadStarterMessage
        ));
        assert!(!should_process_turn_message(MessageType::ChatInputCommand));
    }

    #[test]
    fn known_human_slash_messages_are_skipped() {
        let known = std::collections::HashSet::from([
            "help".to_string(),
            "clear".to_string(),
            "model".to_string(),
        ]);
        assert!(should_skip_human_slash_message("/help", Some(&known)));
        assert!(should_skip_human_slash_message("/clear now", Some(&known)));
    }

    #[test]
    fn unregistered_human_slash_messages_fall_through() {
        let known = std::collections::HashSet::from(["help".to_string()]);
        assert!(!should_skip_human_slash_message("/unknown", Some(&known)));
        assert!(!should_skip_human_slash_message("/", Some(&known)));
        assert!(!should_skip_human_slash_message("/   ", Some(&known)));
        assert!(!should_skip_human_slash_message(
            "/unknown arg",
            Some(&known)
        ));
        assert!(!should_skip_human_slash_message("/unknown", None));
    }

    /// mid:* cleanup should use the longer MSG_DEDUP_TTL (60s),
    /// while bot-specific entries (dispatch:*, msg:*) use INTAKE_DEDUP_TTL (30s).
    /// Verifies that bot cleanup does not prematurely evict mid:* entries.
    #[test]
    fn mid_entries_survive_bot_cleanup() {
        use std::time::{Duration, Instant};

        let map: dashmap::DashMap<String, (Instant, bool)> = dashmap::DashMap::new();
        let now = Instant::now();

        // Simulate: mid:* entry inserted 40s ago (within 60s TTL, outside 30s TTL)
        let mid_time = now - Duration::from_secs(40);
        map.insert("mid:123".to_string(), (mid_time, false));

        // Simulate: dispatch:* entry inserted 40s ago (outside 30s TTL)
        map.insert("dispatch:abc".to_string(), (mid_time, false));

        // Simulate: fresh bot entry inserted just now
        map.insert("msg:456".to_string(), (now, false));

        // Bot cleanup: retain non-mid entries only if within 30s TTL
        let intake_dedup_ttl = Duration::from_secs(30);
        map.retain(|k, v| {
            if k.starts_with("mid:") {
                true // preserved; cleaned by universal dedup cleanup
            } else {
                now.duration_since(v.0) < intake_dedup_ttl
            }
        });

        // mid:* should survive bot cleanup
        assert!(
            map.contains_key("mid:123"),
            "mid:* entry must survive bot cleanup"
        );
        // dispatch:* older than 30s should be removed
        assert!(
            !map.contains_key("dispatch:abc"),
            "expired dispatch:* should be removed"
        );
        // fresh msg:* should survive
        assert!(map.contains_key("msg:456"), "fresh msg:* should survive");

        // Universal mid:* cleanup with 60s TTL
        let msg_dedup_ttl = Duration::from_secs(60);
        map.retain(|k, v| {
            if k.starts_with("mid:") {
                now.duration_since(v.0) < msg_dedup_ttl
            } else {
                true
            }
        });

        // mid:* at 40s should still survive (within 60s)
        assert!(
            map.contains_key("mid:123"),
            "mid:* within TTL must survive universal cleanup"
        );

        // Now simulate mid:* at 65s ago (outside 60s TTL)
        let old_mid_time = now - Duration::from_secs(65);
        map.insert("mid:old".to_string(), (old_mid_time, false));
        map.retain(|k, v| {
            if k.starts_with("mid:") {
                now.duration_since(v.0) < msg_dedup_ttl
            } else {
                true
            }
        });
        assert!(
            !map.contains_key("mid:old"),
            "expired mid:* must be cleaned by universal cleanup"
        );
    }

    /// Thread-preference dedup: once a message is processed as thread context,
    /// subsequent thread duplicates (e.g. gateway reconnection) must be blocked.
    /// Only parent→thread promotion is allowed, not thread→thread re-processing.
    #[test]
    fn thread_dedup_blocks_duplicate_thread_context() {
        use std::time::{Duration, Instant};

        let map: dashmap::DashMap<String, (Instant, bool)> = dashmap::DashMap::new();
        let now = Instant::now();
        let msg_dedup_ttl = Duration::from_secs(60);

        // Case 1: First seen as parent context, then thread arrives → allow
        map.insert("mid:100".to_string(), (now, false)); // was_thread = false
        let entry = map.get("mid:100").unwrap();
        let (ts, was_thread) = *entry;
        drop(entry);
        // is_thread_context=true, was_thread=false → should allow
        let allow = now.duration_since(ts) < msg_dedup_ttl && !was_thread; // this is the "allow" condition for thread promotion
        assert!(allow, "thread should be allowed when previous was parent");

        // Case 2: First seen as thread context, then thread arrives again → block
        map.insert("mid:200".to_string(), (now, true)); // was_thread = true
        let entry = map.get("mid:200").unwrap();
        let (ts2, was_thread2) = *entry;
        drop(entry);
        // is_thread_context=true, was_thread=true → should block
        let allow2 = now.duration_since(ts2) < msg_dedup_ttl && !was_thread2;
        assert!(!allow2, "duplicate thread context must be blocked");

        // Case 3: First seen as thread context, then parent arrives → block
        let entry = map.get("mid:200").unwrap();
        let (ts3, _was_thread3) = *entry;
        drop(entry);
        // is_thread_context=false → always blocked by the main branch
        let is_dup = now.duration_since(ts3) < msg_dedup_ttl;
        assert!(is_dup, "parent duplicate after thread must be blocked");
    }

    #[test]
    fn model_picker_component_dispatch_matches_all_actions() {
        let channel_id = ChannelId::new(42);
        let custom_ids = [
            format!("agentdesk:model-picker:{}", channel_id.get()),
            format!("agentdesk:model-submit:{}", channel_id.get()),
            format!("agentdesk:model-reset:{}", channel_id.get()),
            format!("agentdesk:model-cancel:{}", channel_id.get()),
        ];

        for custom_id in custom_ids {
            assert!(
                is_model_picker_component_custom_id(&custom_id, channel_id),
                "expected model picker dispatch for {custom_id}"
            );
        }

        assert!(!is_model_picker_component_custom_id(
            "agentdesk:other:42",
            channel_id
        ));
    }

    #[test]
    fn model_picker_close_response_acknowledges_component_close() {
        let payload = serde_json::to_value(build_model_picker_close_response())
            .expect("close response should serialize");

        assert_eq!(payload["type"], json!(6));
        assert_eq!(payload["data"], json!(null));
    }
}
