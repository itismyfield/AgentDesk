use super::*;

fn prune_interventions_at(queue: &mut Vec<Intervention>, now: Instant) {
    queue.retain(|i| now.duration_since(i.created_at) <= INTERVENTION_TTL);
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        let overflow = queue.len() - MAX_INTERVENTIONS_PER_CHANNEL;
        queue.drain(0..overflow);
    }
}

pub(super) fn prune_interventions(queue: &mut Vec<Intervention>) {
    prune_interventions_at(queue, Instant::now());
}

pub(super) fn channel_has_pending_soft_queue(
    intervention_queue: &mut HashMap<ChannelId, Vec<Intervention>>,
    channel_id: ChannelId,
) -> bool {
    channel_has_pending_soft_queue_at(intervention_queue, channel_id, Instant::now())
}

pub(super) fn channel_has_pending_soft_queue_at(
    intervention_queue: &mut HashMap<ChannelId, Vec<Intervention>>,
    channel_id: ChannelId,
    now: Instant,
) -> bool {
    let mut remove_queue = false;
    let has_pending = if let Some(queue) = intervention_queue.get_mut(&channel_id) {
        let has_pending = has_soft_intervention_at(queue, now);
        remove_queue = queue.is_empty();
        has_pending
    } else {
        false
    };
    if remove_queue {
        intervention_queue.remove(&channel_id);
    }
    has_pending
}

pub(super) fn watcher_should_kickoff_idle_queue(
    has_active_turn: bool,
    intervention_queue: &mut HashMap<ChannelId, Vec<Intervention>>,
    channel_id: ChannelId,
) -> bool {
    watcher_should_kickoff_idle_queue_at(
        has_active_turn,
        intervention_queue,
        channel_id,
        Instant::now(),
    )
}

pub(super) fn watcher_should_kickoff_idle_queue_at(
    has_active_turn: bool,
    intervention_queue: &mut HashMap<ChannelId, Vec<Intervention>>,
    channel_id: ChannelId,
    now: Instant,
) -> bool {
    if has_active_turn {
        return false;
    }
    channel_has_pending_soft_queue_at(intervention_queue, channel_id, now)
}

pub(super) fn has_soft_intervention(queue: &mut Vec<Intervention>) -> bool {
    has_soft_intervention_at(queue, Instant::now())
}

fn has_soft_intervention_at(queue: &mut Vec<Intervention>, now: Instant) -> bool {
    prune_interventions_at(queue, now);
    queue.iter().any(|item| item.mode == InterventionMode::Soft)
}

pub(super) fn dequeue_next_soft_intervention(
    queue: &mut Vec<Intervention>,
) -> Option<Intervention> {
    prune_interventions(queue);
    let index = queue
        .iter()
        .position(|item| item.mode == InterventionMode::Soft)?;
    Some(queue.remove(index))
}

pub(super) fn requeue_intervention_front(
    queue: &mut Vec<Intervention>,
    intervention: Intervention,
) {
    prune_interventions(queue);
    queue.insert(0, intervention);
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        queue.truncate(MAX_INTERVENTIONS_PER_CHANNEL);
    }
}

// ─── Pending queue persistence (write-through + SIGTERM) ─────────────────────

/// Serializable form of a queued intervention for disk persistence.
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct PendingQueueItem {
    pub(super) author_id: u64,
    pub(super) message_id: u64,
    pub(super) text: String,
}

/// Write-through: save a single channel's queue to disk.
/// If the queue is empty the file is removed.
/// This is designed to be called from `tokio::spawn` after every enqueue/dequeue.
pub(super) fn save_channel_queue(
    provider: &ProviderKind,
    channel_id: ChannelId,
    queue: &[Intervention],
) {
    let Some(root) = runtime_store::discord_pending_queue_root() else {
        return;
    };
    let dir = root.join(provider.as_str());
    let path = dir.join(format!("{}.json", channel_id.get()));
    if queue.is_empty() {
        let _ = fs::remove_file(&path);
        return;
    }
    let _ = fs::create_dir_all(&dir);
    let items: Vec<PendingQueueItem> = queue
        .iter()
        .map(|i| PendingQueueItem {
            author_id: i.author_id.get(),
            message_id: i.message_id.get(),
            text: i.text.clone(),
        })
        .collect();
    if let Ok(json) = serde_json::to_string_pretty(&items) {
        let _ = runtime_store::atomic_write(&path, &json);
    }
}

#[allow(dead_code)]
async fn catch_up_missed_messages(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    let Some(root) = runtime_store::last_message_root() else {
        return;
    };
    let dir = root.join(provider.as_str());
    if !dir.is_dir() {
        return;
    }

    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let mut total_recovered = 0usize;
    let now = Instant::now();
    let max_age = std::time::Duration::from_secs(300); // Only catch up messages within 5 minutes

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(channel_id_raw) = stem.parse::<u64>() else {
            continue;
        };
        let Ok(last_id_str) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(last_id) = last_id_str.trim().parse::<u64>() else {
            continue;
        };

        let channel_id = ChannelId::new(channel_id_raw);
        let after_msg = MessageId::new(last_id);

        // Fetch messages after last_id (Discord returns oldest first with after=)
        let messages = match channel_id
            .messages(
                http,
                serenity::builder::GetMessages::new()
                    .after(after_msg)
                    .limit(10),
            )
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                eprintln!(
                    "  [{ts}] ⚠ catch-up: failed to fetch messages for channel {channel_id}: {e}"
                );
                continue;
            }
        };

        if messages.is_empty() {
            continue;
        }

        // Get bot's own user ID to filter out self-messages
        let bot_user_id = {
            let settings = shared.settings.read().await;
            settings.owner_user_id
        };

        // Collect existing message IDs in queue for dedup
        let existing_ids: std::collections::HashSet<u64> = {
            let data = shared.core.lock().await;
            data.intervention_queue
                .get(&channel_id)
                .map(|q| q.iter().map(|i| i.message_id.get()).collect())
                .unwrap_or_default()
        };

        let allowed_bot_ids: Vec<u64> = {
            let settings = shared.settings.read().await;
            settings.allowed_bot_ids.clone()
        };

        let mut channel_recovered = 0usize;
        let mut max_recovered_id: Option<u64> = None;
        let mut data = shared.core.lock().await;
        let queue = data.intervention_queue.entry(channel_id).or_default();

        for msg in &messages {
            // Skip system messages (thread creation, slash commands, etc.)
            if !router::should_process_turn_message(msg.kind) {
                continue;
            }
            // Skip own messages
            if Some(msg.author.id.get()) == bot_user_id {
                continue;
            }
            // Skip if already in queue
            if existing_ids.contains(&msg.id.get()) {
                continue;
            }
            // Skip messages older than max_age (use message snowflake timestamp)
            let msg_ts = msg.id.created_at();
            let msg_age = chrono::Utc::now().signed_duration_since(*msg_ts);
            if msg_age.num_seconds() > max_age.as_secs() as i64 {
                continue;
            }
            let text = msg.content.trim();
            if text.is_empty() {
                continue;
            }
            // Only process messages from allowed bots or authorized users
            let is_allowed = !msg.author.bot || allowed_bot_ids.contains(&msg.author.id.get());
            if !is_allowed {
                continue;
            }

            queue.push(Intervention {
                author_id: msg.author.id,
                message_id: msg.id,
                text: text.to_string(),
                mode: InterventionMode::Soft,
                created_at: now,
            });
            channel_recovered += 1;
            // Track the newest actually-recovered message for checkpoint
            let mid = msg.id.get();
            if max_recovered_id.map(|m| mid > m).unwrap_or(true) {
                max_recovered_id = Some(mid);
            }
        }
        drop(data);

        if channel_recovered > 0 {
            total_recovered += channel_recovered;
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] 🔍 CATCH-UP: recovered {} message(s) for channel {}",
                channel_recovered, channel_id
            );
        }

        // Only advance checkpoint if we actually recovered messages
        if let Some(newest) = max_recovered_id {
            shared.last_message_ids.insert(channel_id, newest);
        }
    }

    if total_recovered > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}] 🔍 CATCH-UP: total {total_recovered} message(s) recovered across channels"
        );
    }

    // Phase 2: Scan for unanswered messages since last bot response.
    // Catches messages that were queued in-memory but lost on restart.
    let Ok(entries2) = fs::read_dir(&dir) else {
        return;
    };
    let mut phase2_recovered = 0usize;
    let bot_user_id_phase2 = {
        let settings = shared.settings.read().await;
        settings.owner_user_id
    };
    let allowed_bot_ids_phase2: Vec<u64> = {
        let settings = shared.settings.read().await;
        settings.allowed_bot_ids.clone()
    };

    for entry in entries2.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(channel_id_raw) = stem.parse::<u64>() else {
            continue;
        };
        let channel_id = ChannelId::new(channel_id_raw);

        // Fetch last 20 messages (newest first — default Discord order)
        let recent = match channel_id
            .messages(http, serenity::builder::GetMessages::new().limit(20))
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                eprintln!(
                    "  [{ts}] ⚠ catch-up phase2: failed to fetch recent messages for channel {channel_id}: {e}"
                );
                continue;
            }
        };

        if recent.is_empty() {
            continue;
        }

        // Find the newest bot response (first bot message in newest-first order)
        let last_bot_idx = recent.iter().position(|m| {
            Some(m.author.id.get()) == bot_user_id_phase2 && !m.content.trim().is_empty()
        });

        // Messages at indices 0..last_bot_idx are newer than the last bot response
        let unanswered_slice = match last_bot_idx {
            Some(0) => continue, // Latest message is from bot — nothing unanswered
            Some(idx) => &recent[..idx],
            None => continue, // No bot response found — skip (new/inactive channel)
        };

        // Collect existing queue IDs for dedup
        let existing_ids: std::collections::HashSet<u64> = {
            let data = shared.core.lock().await;
            data.intervention_queue
                .get(&channel_id)
                .map(|q| q.iter().map(|i| i.message_id.get()).collect())
                .unwrap_or_default()
        };

        let mut channel_recovered = 0usize;
        let mut data = shared.core.lock().await;
        let queue = data.intervention_queue.entry(channel_id).or_default();

        // Iterate in reverse (oldest first) for chronological queue order
        for msg in unanswered_slice.iter().rev() {
            if !router::should_process_turn_message(msg.kind) {
                continue;
            }
            if Some(msg.author.id.get()) == bot_user_id_phase2 {
                continue;
            }
            if existing_ids.contains(&msg.id.get()) {
                continue;
            }
            let text = msg.content.trim();
            if text.is_empty() {
                continue;
            }
            let is_allowed =
                !msg.author.bot || allowed_bot_ids_phase2.contains(&msg.author.id.get());
            if !is_allowed {
                continue;
            }
            // Skip messages older than 10 minutes (generous window for restart gap)
            let msg_age = chrono::Utc::now().signed_duration_since(*msg.id.created_at());
            if msg_age.num_seconds() > 600 {
                continue;
            }

            queue.push(Intervention {
                author_id: msg.author.id,
                message_id: msg.id,
                text: text.to_string(),
                mode: InterventionMode::Soft,
                created_at: now,
            });
            channel_recovered += 1;
        }
        drop(data);

        if channel_recovered > 0 {
            phase2_recovered += channel_recovered;
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] 🔍 CATCH-UP phase2: recovered {} unanswered message(s) for channel {}",
                channel_recovered, channel_id
            );
        }
    }

    if phase2_recovered > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}] 🔍 CATCH-UP phase2: total {phase2_recovered} unanswered message(s) recovered"
        );
    }
}

/// Execute durable handoff turns saved before a restart.
/// Runs after tmux watcher restore and pending queue restore, but before
/// restart report flush. Skips channels that already have pending queue messages
/// (user intent takes priority over automatic follow-up).
pub(super) async fn kickoff_idle_queues(
    ctx: &serenity::Context,
    shared: &Arc<SharedData>,
    token: &str,
    provider: &ProviderKind,
) {
    // Collect channels with queued items that are idle (no active turn)
    let channels_to_kick: Vec<(ChannelId, Intervention, bool)> = {
        let mut data = shared.core.lock().await;
        let mut result = Vec::new();
        let channel_ids: Vec<ChannelId> = data.intervention_queue.keys().cloned().collect();
        for channel_id in channel_ids {
            // Skip if active turn already running — it will dequeue when done
            if data.cancel_tokens.contains_key(&channel_id) {
                continue;
            }
            if let Some(queue) = data.intervention_queue.get_mut(&channel_id)
                && let Some(intervention) = dequeue_next_soft_intervention(queue)
            {
                let has_more = has_soft_intervention(queue);
                // Write-through: update disk after dequeue
                if queue.is_empty() {
                    save_channel_queue(provider, channel_id, &[]);
                    data.intervention_queue.remove(&channel_id);
                } else {
                    save_channel_queue(provider, channel_id, queue);
                }
                result.push((channel_id, intervention, has_more));
            }
        }
        result
    };

    if channels_to_kick.is_empty() {
        return;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts}] 🚀 KICKOFF: starting turns for {} idle channel(s) with queued messages",
        channels_to_kick.len()
    );

    for (channel_id, intervention, has_more) in channels_to_kick {
        let owner_name = if intervention.author_id.get() <= 1 {
            "system".to_string()
        } else {
            intervention
                .author_id
                .to_user(&ctx.http)
                .await
                .map(|u| u.name.clone())
                .unwrap_or_else(|_| format!("user-{}", intervention.author_id.get()))
        };

        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}] 🚀 KICKOFF: starting queued turn for channel {}",
            channel_id
        );

        if let Err(e) = router::handle_text_message(
            ctx,
            channel_id,
            intervention.message_id,
            intervention.author_id,
            &owner_name,
            &intervention.text,
            shared,
            token,
            true,     // reply_to_user_message
            has_more, // defer_watcher_resume
            false,    // wait_for_completion — don't block, let channels run concurrently
            None,     // reply_context
        )
        .await
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}]   ⚠ KICKOFF: failed to start turn for channel {}: {e}",
                channel_id
            );
            // Requeue so the message is not lost
            let mut data = shared.core.lock().await;
            let queue = data.intervention_queue.entry(channel_id).or_default();
            requeue_intervention_front(queue, intervention);
        }
    }
}

pub(super) fn schedule_deferred_idle_queue_kickoff(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
) {
    shared
        .deferred_hook_backlog
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        if let (Some(ctx), Some(tok)) = (
            shared.cached_serenity_ctx.get(),
            shared.cached_bot_token.get(),
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] 🚀 Deferred drain: kicking off idle queues for channel {} ({reason})",
                channel_id
            );
            kickoff_idle_queues(ctx, &shared, tok, &provider).await;
        } else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⚠ Deferred drain: missing cached context for channel {} ({reason})",
                channel_id
            );
        }
        shared
            .deferred_hook_backlog
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    });
}

// Additional queue helpers live above. Skill scanning remains in `bot_init.rs`.
