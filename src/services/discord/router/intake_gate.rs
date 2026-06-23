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

pub(super) fn should_skip_self_authored_turn_message(
    author_id: serenity::UserId,
    current_bot_id: serenity::UserId,
) -> bool {
    author_id == current_bot_id
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

fn strip_leading_bot_mention(text: &str) -> String {
    static BOT_MENTION_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^<@!?\d+>\s*").expect("static bot-mention regex is valid")
    });
    BOT_MENTION_RE.replace(text, "").to_string()
}

fn should_start_attachment_only_turn(text: &str, saved_attachment_count: usize) -> bool {
    saved_attachment_count > 0 && strip_leading_bot_mention(text).trim().is_empty()
}

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

pub(in crate::services::discord) fn bot_author_allowed_for_live_intake(
    allowed_bot_ids: &[u64],
    announce_bot_id: Option<u64>,
    author_id: u64,
) -> bool {
    allowed_bot_ids.contains(&author_id) || announce_bot_id.is_some_and(|id| id == author_id)
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

async fn claim_voice_transcript_announcement_for_queue(
    data: &Data,
    channel_id: serenity::ChannelId,
    message_id: serenity::MessageId,
    voice_announcement: &Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
    context: &'static str,
) -> bool {
    if voice_announcement.is_none() {
        return true;
    }
    let Some(pool) = data.shared.pg_pool.as_ref() else {
        return true;
    };
    match crate::voice::announce_meta::mark_voice_announcement_durable_consumed(pool, message_id)
        .await
    {
        Ok(true) => true,
        Ok(false) => {
            tracing::info!(
                channel_id = channel_id.get(),
                message_id = message_id.get(),
                context,
                "voice transcript announcement durable metadata already claimed before queue accept"
            );
            false
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                channel_id = channel_id.get(),
                message_id = message_id.get(),
                context,
                "voice transcript announcement durable metadata claim failed before queue accept"
            );
            false
        }
    }
}

fn build_soft_intervention(
    author_id: serenity::UserId,
    author_is_bot: bool,
    message_id: serenity::MessageId,
    text: &str,
    reply_context: Option<String>,
    has_reply_boundary: bool,
    merge_consecutive: bool,
    pending_uploads: Vec<String>,
    // #2266: when the intake-gate sees a voice-transcript announcement and
    // chooses to enqueue it (busy active turn, thread guard, dispatch
    // collision, drain mode, reconcile gate), the per-process
    // `voice::announce_meta` store entry can expire before the queued turn
    // runs (30s TTL vs. arbitrary queue dwell). Carrying the announcement
    // inside the Intervention keeps the queued payload self-contained so
    // every queued-dispatch entrypoint can reinsert it into the store and
    // recover the voice-transcript framing.
    voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
) -> Intervention {
    Intervention {
        author_id,
        author_is_bot,
        message_id,
        source_message_ids: vec![message_id],
        text: text.to_string(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context,
        has_reply_boundary,
        merge_consecutive,
        pending_uploads,
        voice_announcement,
    }
}

async fn enqueue_soft_intervention(
    data: &Data,
    channel_id: serenity::ChannelId,
    author_id: serenity::UserId,
    author_is_bot: bool,
    message_id: serenity::MessageId,
    text: &str,
    reply_context: Option<String>,
    has_reply_boundary: bool,
    merge_consecutive: bool,
    pending_uploads: Vec<String>,
    // #2266: pass-through for the voice-transcript payload (see
    // `build_soft_intervention` doc-comment).
    voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
) -> super::super::MailboxEnqueueOutcome {
    mailbox_enqueue_intervention(
        &data.shared,
        &data.provider,
        channel_id,
        build_soft_intervention(
            author_id,
            author_is_bot,
            message_id,
            text,
            reply_context,
            has_reply_boundary,
            merge_consecutive,
            pending_uploads,
            voice_announcement,
        ),
    )
    .await
}

/// Pick the queue-pending reaction emoji based on the enqueue outcome.
/// Standalone queue head entries get `📬`; merged-into-previous entries get
/// `➕` so users can tell merged from standalone at a glance (#1190 follow-up).
pub(super) fn queue_pending_reaction_for(outcome: super::super::MailboxEnqueueOutcome) -> char {
    if outcome.merged { '➕' } else { '📬' }
}

/// #3182: add the queue-pending reaction (📬 standalone / ➕ merged) and
/// self-heal the late-add race.
///
/// Every `intake_gate` enqueue path adds this reaction AFTER one or more
/// Discord `await`s (placeholder POST, queued-card render, dispatch-guard
/// bookkeeping). During that await the active turn can finish and the
/// queued-dispatch entrypoints (`DiscordGateway::dispatch_queued_turn` /
/// `kickoff_idle_queues`) can dequeue THIS message and run their 📬/➕ reaction
/// drain BEFORE the add lands — stranding the reaction on an
/// already-promoted/processed message (the gate-path analog of the
/// `intake_turn.rs` Surface-3 self-heal, #2036). So after the add await
/// resolves, re-snapshot the mailbox and strip the reaction if the message is
/// no longer queued.
///
/// Validity uses SOURCE MEMBERSHIP — head id OR any `source_message_ids`
/// entry — for BOTH standalone and merged. The head-only rule is correct only
/// for placeholder-card OWNERSHIP (one card per active turn); a merged source
/// `B` that is still queued under a newer head `C` keeps a valid `➕`, so a
/// head-only check would wrongly remove it. `remove_reaction_raw` is
/// best-effort (no-op when already cleared), and only the calling provider
/// bot's own @me reaction is removed.
async fn add_queue_pending_reaction_self_healing(
    ctx: &serenity::Context,
    data: &Data,
    channel_id: serenity::ChannelId,
    user_msg_id: serenity::MessageId,
    emoji: char,
) {
    add_reaction(&ctx.http, channel_id, user_msg_id, emoji).await;
    let still_queued = {
        let snapshot = mailbox_snapshot(&data.shared, channel_id).await;
        snapshot.intervention_queue.iter().any(|intervention| {
            intervention.message_id == user_msg_id
                || intervention.source_message_ids.contains(&user_msg_id)
        })
    };
    if !still_queued {
        super::super::formatting::remove_reaction_raw(&ctx.http, channel_id, user_msg_id, emoji)
            .await;
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔁 RACE: queue-pending {emoji} reacted after dequeue promotion (channel {}, msg {}); removed stale reaction",
            channel_id,
            user_msg_id
        );
    }
}

/// #1446 Layer 2 — load the thread's persisted inflight state and report
/// whether its `updated_at` is older than `INFLIGHT_STALENESS_THRESHOLD_SECS`.
/// Returns `false` when no state file exists (nothing to clean) or when
/// `updated_at` cannot be parsed (never infer staleness from missing data).
///
/// **Pure-classification helper only.** A stale `updated_at` is necessary
/// but not sufficient to force-clean a live thread — `updated_at` only
/// advances when `save_inflight_state` runs, so a healthy long Bash /
/// large Read / slow LLM stream can legitimately go silent for minutes.
/// `thread_guard_should_force_clean_stale_thread` adds the required
/// secondary signal (watcher snapshot's `desynced == true`).
#[allow(dead_code)] // #3034: #1446 Layer-2 classifier pinned by the intake-gate unit tests.
pub(super) fn thread_guard_inflight_is_stale(
    provider: &ProviderKind,
    thread_id: serenity::ChannelId,
    now_unix_secs: i64,
) -> bool {
    super::super::inflight::load_inflight_state(provider, thread_id.get())
        .map(|state| {
            super::super::inflight::inflight_state_is_stale(
                &state,
                now_unix_secs,
                super::super::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS,
            )
        })
        .unwrap_or(false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StaleActiveTurnProofClassification {
    LiveOrUnclear,
    RelayStalled,
    QueueBlockedOrphan,
    ExplicitBackgroundStatus,
}

fn classify_stale_active_turn_proof(
    inflight: &super::super::inflight::InflightTurnState,
    snapshot: &super::super::health::WatcherStateSnapshot,
    now_unix_secs: i64,
) -> StaleActiveTurnProofClassification {
    if !super::super::inflight::inflight_state_is_stale(
        inflight,
        now_unix_secs,
        super::super::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS,
    ) {
        return StaleActiveTurnProofClassification::LiveOrUnclear;
    }

    if inflight.long_running_placeholder_active {
        return StaleActiveTurnProofClassification::ExplicitBackgroundStatus;
    }

    if snapshot.desynced {
        return StaleActiveTurnProofClassification::RelayStalled;
    }

    if !snapshot.inflight_state_present {
        return StaleActiveTurnProofClassification::LiveOrUnclear;
    }

    if snapshot.mailbox_active_user_msg_id.is_some()
        && snapshot.mailbox_active_user_msg_id != Some(inflight.user_msg_id)
    {
        return StaleActiveTurnProofClassification::LiveOrUnclear;
    }

    if !snapshot.attached && snapshot.tmux_session_alive != Some(true) {
        return StaleActiveTurnProofClassification::QueueBlockedOrphan;
    }

    StaleActiveTurnProofClassification::LiveOrUnclear
}

async fn classify_channel_stale_active_turn_proof(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    now_unix_secs: i64,
) -> StaleActiveTurnProofClassification {
    let Some(inflight) = super::super::inflight::load_inflight_state(provider, channel_id.get())
    else {
        return StaleActiveTurnProofClassification::LiveOrUnclear;
    };
    let Some(registry) = shared.health_registry.upgrade() else {
        return StaleActiveTurnProofClassification::LiveOrUnclear;
    };
    let Some(snapshot) = registry
        .snapshot_watcher_state_for_provider(provider, channel_id.get())
        .await
    else {
        return StaleActiveTurnProofClassification::LiveOrUnclear;
    };
    classify_stale_active_turn_proof(&inflight, &snapshot, now_unix_secs)
}

/// #1446 / #1456 — full force-clean predicate. Requires stale persisted
/// inflight state plus either:
///   1. the watcher-state snapshot for the thread reports `desynced == true`
///      (capture-lag, cross-owner mismatch, or live-tmux orphan with no
///      relay heartbeat — the same conjunction the stall-watchdog uses), or
///   2. the mailbox active-turn proof has no live owner (`attached == false`
///      and no live tmux session), which is the queue-blocked fail-open path.
///
/// Without the snapshot's desync corroboration we would force-clean a
/// healthy long-running turn whose `updated_at` simply has not advanced
/// because no chunk hit the bridge in the last 5 minutes. The no-owner path
/// is intentionally narrower: live tmux sessions and explicit background
/// placeholder status are preserved. Returning `false` when the registry is
/// unreachable is the conservative default — a missing registry happens
/// during startup before the stall-watchdog would also be running, so
/// deferring cleanup costs nothing.
pub(super) async fn thread_guard_should_force_clean_stale_thread(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    thread_id: serenity::ChannelId,
    now_unix_secs: i64,
) -> bool {
    matches!(
        classify_channel_stale_active_turn_proof(shared, provider, thread_id, now_unix_secs).await,
        StaleActiveTurnProofClassification::RelayStalled
            | StaleActiveTurnProofClassification::QueueBlockedOrphan
    )
}

/// #1446 Layer 2 — perform the THREAD-GUARD's stale-thread cleanup:
///   1. drop the parent → thread mapping so subsequent intakes do not re-
///      trigger the guard,
///   2. delete the thread's inflight state file (releases the durable lock
///      whose presence convinced `mailbox_has_active_turn` the dispatch is
///      still live),
///   3. **clear** the thread's mailbox (cancel token + active turn anchor +
///      pending interventions). `cancel_active_turn` alone is insufficient
///      here — for a dead-dispatch case there is no live turn task to
///      observe the cancel signal and call `finish_turn`, so
///      `has_active_turn()` would stay `true` forever and the next bot
///      message would re-enter the THREAD-GUARD's queueing branch.
///      `mailbox_clear_channel` synchronously drops `active_request_owner`
///      / `active_user_message_id` and reports `has_active_turn() == false`
///      immediately on completion (see `ChannelMailboxMsg::Clear` handler
///      in `turn_orchestrator.rs`).
///   4. complete the bookkeeping that the missing `finish_turn` would
///      otherwise have done: cancel the orphaned token (kill any leftover
///      child / tmux session) and decrement `global_active`. Mirrors the
///      `placeholder_sweeper::finalize_abandoned_mailbox` cleanup
///      pattern so health and deferred-restart counters do not leak.
///
/// We never touch the parent channel's own mailbox — only the thread's.
/// This preserves the `watcher_owns_live_relay` invariant by leaving
/// parent-side relay state untouched.
pub(super) async fn thread_guard_force_clean_stale_thread(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    parent_channel_id: serenity::ChannelId,
    thread_id: serenity::ChannelId,
) {
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🔓 THREAD-GUARD: stale inflight detected for thread {}, cleaning up and proceeding",
        thread_id
    );
    shared.dispatch.thread_parents.remove(&parent_channel_id);
    super::super::inflight::delete_inflight_state_file(provider, thread_id.get());
    let cleared = mailbox_clear_channel(shared, provider, thread_id).await;
    super::super::stall_recovery::finalize_orphaned_clear(
        shared,
        thread_id,
        cleared.removed_token,
        "1446_thread_guard_stale_inflight",
    );
}

/// #2044 F7 (P3 — documentation): invariant note.
///
/// This recovery path delegates the `cancelled` flag + `global_active`
/// decrement to `stall_recovery::finalize_orphaned_clear`, which has
/// owned both side-effects since #1446 (see `stall_recovery.rs:65-89`):
///   1. it calls `turn_bridge::cancel_active_token` on the removed
///      token — that helper sets `token.cancelled = true` so any
///      watchdog/voice-barge-in holding an Arc to the same token sees
///      the cancellation;
///   2. it calls `saturating_decrement_global_active`, mirroring what
///      the normal `turn_bridge::mod.rs:3132-3141` and
///      `tmux.rs:2052-2061` cleanup sites do inline.
///
/// Therefore this site MUST NOT also poke `cancelled` / `global_active`
/// — doing so would double-decrement the counter (already saturating
/// in `finalize_orphaned_clear`, but the duplicate is still a smell)
/// and confuse audit logs. If a future change splits
/// `finalize_orphaned_clear` or makes either side-effect conditional,
/// this comment and the comments in the bridge/tmux peer sites must
/// move in lockstep.
async fn release_queue_blocked_stale_active_turn(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    now_unix_secs: i64,
) -> bool {
    let classification =
        classify_channel_stale_active_turn_proof(shared, provider, channel_id, now_unix_secs).await;
    if classification != StaleActiveTurnProofClassification::QueueBlockedOrphan {
        return false;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] 🔓 QUEUE-GUARD: stale active-turn proof for channel {} has no live owner; releasing mailbox and proceeding",
        channel_id
    );
    super::super::inflight::delete_inflight_state_file(provider, channel_id.get());
    super::super::clear_watchdog_deadline_override(channel_id.get()).await;
    let finish = mailbox_finish_turn(shared, provider, channel_id).await;
    // #2044 F7: `finalize_orphaned_clear` owns both `cancelled.store(true)`
    // and the saturating `global_active` decrement — do not duplicate them here.
    super::super::stall_recovery::finalize_orphaned_clear(
        shared,
        channel_id,
        finish.removed_token,
        "1456_queue_blocked_stale_proof",
    );
    shared
        .dispatch
        .thread_parents
        .retain(|_, thread_id| *thread_id != channel_id);
    if !finish.has_pending {
        shared.dispatch.role_overrides.remove(&channel_id);
    }
    true
}

async fn mailbox_has_live_active_turn_or_cleanup_stale_proof(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> bool {
    if !mailbox_has_active_turn(shared, channel_id).await {
        return false;
    }
    if release_queue_blocked_stale_active_turn(
        shared,
        provider,
        channel_id,
        chrono::Utc::now().timestamp(),
    )
    .await
    {
        return mailbox_has_active_turn(shared, channel_id).await;
    }
    true
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

/// #3009: when a follow-up message is *merged* into the previous queue head
/// (`MailboxEnqueueOutcome::merged == true`), the head intervention's
/// `message_id` is rewritten to this follow-up's id while the older ids are
/// retained as `source_message_ids` (see `turn_orchestrator::enqueue_intervention`).
/// The previous queue head already owns a `📬 메시지 대기 중` placeholder card
/// keyed under one of those older `source_message_ids`. Re-render and re-key
/// that single card under the new head id instead of POSTing a brand-new card,
/// so a burst of follow-ups collapses to ONE visible waiting placeholder.
///
/// Returns `true` when an existing merged-group placeholder was successfully
/// reused (no new card needed). Returns `false` when there was no existing
/// placeholder to reuse (e.g. the head's earlier POST failed), in which case
/// the caller falls back to creating a fresh card.
/// Pure decision helper for #3009 merged-placeholder reuse. Given the merged
/// head's `source_message_ids` and a lookup into the live `queued_placeholders`
/// map, return the `(prior_source_id, placeholder_msg_id)` of the existing
/// waiting card that should be re-keyed under the new head id. Returns `None`
/// when no earlier source id of the group owns a card yet (caller POSTs a fresh
/// one). The follow-up's own id is excluded because it has no card yet.
fn pick_reusable_merged_placeholder<F>(
    source_message_ids: &[serenity::MessageId],
    user_msg_id: serenity::MessageId,
    mut lookup: F,
) -> Option<(serenity::MessageId, serenity::MessageId)>
where
    F: FnMut(serenity::MessageId) -> Option<serenity::MessageId>,
{
    source_message_ids
        .iter()
        .copied()
        .filter(|id| *id != user_msg_id)
        .find_map(|id| lookup(id).map(|placeholder| (id, placeholder)))
}

/// Outcome of the #3009 merged-placeholder reuse attempt. Distinguishes the
/// "reuse handled it" cases (caller must NOT post a fresh card) from the
/// genuine "no reusable card, post a fresh one" case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MergedPlaceholderReuse {
    /// Reused / no-op success: a single waiting card already represents this
    /// merged backlog (either re-keyed here, or this follow-up is a stale
    /// non-head source whose content is already folded into the head's card).
    /// The caller must skip the fresh-card POST.
    Handled,
    /// No reusable card exists for the head and the head's own card render is
    /// still required — the caller should POST a fresh placeholder.
    PostFresh,
}

async fn reuse_merged_queued_placeholder(
    ctx: &serenity::Context,
    data: &Data,
    channel_id: serenity::ChannelId,
    user_msg_id: serenity::MessageId,
) -> MergedPlaceholderReuse {
    // Serialize the whole reuse (snapshot read → re-key → re-render) against
    // every other `queued_placeholders` mutation / queue-exit drain on this
    // channel. Without the lock a concurrent dispatch/queue-exit drain could
    // remove the source mapping between our snapshot read and our re-key,
    // leaking the card or resurrecting a stale mapping (#3009 TOCTOU guard).
    let persist_lock = data.shared.queued_placeholders_persist_lock(channel_id);
    let persist_guard = persist_lock.lock_owned().await;

    let snapshot = mailbox_snapshot(&data.shared, channel_id).await;
    // Locate the merged head whose source ids include this follow-up. The
    // enqueue path rewrote its `message_id` to `user_msg_id`, so match on the
    // source-id membership to stay robust even if the head id moved again.
    let Some(merged_head) = snapshot
        .intervention_queue
        .iter()
        .find(|intervention| intervention.source_message_ids.contains(&user_msg_id))
    else {
        // No longer queued (already dispatched / exited). The merge outcome
        // told us this id folded into a prior head, so its content is not
        // standalone — do NOT post a fresh card for it.
        return MergedPlaceholderReuse::Handled;
    };
    let already_started = snapshot.active_user_message_id == Some(user_msg_id);
    if already_started {
        // The merged head is being dispatched; its card transition is owned by
        // the dispatch hand-off. No fresh card needed.
        return MergedPlaceholderReuse::Handled;
    }

    // codex round-1 P2 (identity race) + round-2 P2 (stale-source duplicate):
    // only re-key the card when `user_msg_id` is STILL the merged head. Under
    // bursty concurrent merges a newer follow-up can have already advanced the
    // head past this id (rewriting `message_id`), leaving `user_msg_id` as a
    // non-head source. Re-keying to a non-head id would let the dispatch drain
    // delete the only queued card. AND posting a fresh card for this stale
    // non-head source would recreate the very duplicate this path prevents —
    // its content is already folded into the head's card. So treat a stale
    // non-head merged source as Handled (no new card); the head's own reuse
    // pass owns the single visible card.
    if merged_head.message_id != user_msg_id {
        return MergedPlaceholderReuse::Handled;
    }

    // codex round-3 P3: `enqueue_intervention` already appended this follow-up's
    // text to the head's accumulated `text` (e.g. "A\nB"). Render the FULL
    // merged request on the single reused card so the placeholder represents the
    // whole pending backlog, not just the latest line (`text` is only this
    // Discord message). Capture it before any further borrows of `snapshot`.
    let merged_request_text = merged_head.text.clone();

    // Find the existing placeholder card owned by any earlier source id of the
    // merged group (excluding this follow-up, which has no card yet).
    let existing =
        pick_reusable_merged_placeholder(&merged_head.source_message_ids, user_msg_id, |id| {
            data.shared
                .queued
                .queued_placeholders
                .get(&(channel_id, id))
                .map(|entry| *entry.value())
        });
    let Some((prior_source_id, placeholder_msg_id)) = existing else {
        // We are the current head but no earlier source owns a card yet (e.g.
        // the prior head's POST failed). A fresh card IS warranted here.
        return MergedPlaceholderReuse::PostFresh;
    };

    // Re-key the mapping so the head id (== `user_msg_id`) owns the card the
    // dispatch hand-off consumes, and the old source-id mapping is dropped so
    // a later merged-drain cannot delete this still-live card. The
    // placeholder_controller entry is keyed by the *placeholder* Discord
    // message id, which is unchanged, so no controller re-keying is needed.
    data.shared
        .remove_queued_placeholder_locked(channel_id, prior_source_id);
    data.shared
        .insert_queued_placeholder_locked(channel_id, user_msg_id, placeholder_msg_id);

    // Refresh the card body with merged request text; unchanged renders
    // coalesce, changed ones edit.
    let gateway = super::super::gateway::DiscordGateway::new(
        ctx.http.clone(),
        data.shared.clone(),
        data.provider.clone(),
        None,
    );
    let key = super::super::placeholder_controller::PlaceholderKey {
        provider: data.provider.clone(),
        channel_id,
        message_id: placeholder_msg_id,
    };
    let queued_input = super::super::placeholder_controller::PlaceholderActiveInput {
        reason: super::super::formatting::MonitorHandoffReason::Queued,
        started_at_unix: chrono::Utc::now().timestamp(),
        tool_summary: None,
        command_summary: None,
        reason_detail: None,
        context_line: None,
        request_line: Some(merged_request_text),
        progress_line: None,
    };
    let outcome = data
        .shared
        .ui
        .placeholder_controller
        .ensure_queued(&gateway, key, queued_input)
        .await;

    // codex round-1 P2 (edit-failure rollback): only commit the re-key after
    // `Edited`/`Coalesced`. On `EditFailed`/`Rejected`, restore the prior source
    // id and let the caller fall back to fresh POST, with the persistence lock
    // held so no drain observes the half-applied re-key.
    if !matches!(
        outcome,
        super::super::placeholder_controller::PlaceholderControllerOutcome::Edited
            | super::super::placeholder_controller::PlaceholderControllerOutcome::Coalesced
    ) {
        data.shared
            .remove_queued_placeholder_locked(channel_id, user_msg_id);
        data.shared.insert_queued_placeholder_locked(
            channel_id,
            prior_source_id,
            placeholder_msg_id,
        );
        drop(persist_guard);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ QUEUE-ACK: merged reuse of placeholder {} for message {} in channel {} failed to render ({:?}); kept prior mapping, falling back to fresh card",
            placeholder_msg_id,
            user_msg_id,
            channel_id,
            outcome,
        );
        return MergedPlaceholderReuse::PostFresh;
    }
    drop(persist_guard);

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] ➕ QUEUE-ACK: merged queued message {} into existing placeholder {} in channel {} (no new card)",
        user_msg_id,
        placeholder_msg_id,
        channel_id
    );
    MergedPlaceholderReuse::Handled
}

/// #3082 part A — pure decision helper: of the existing queued cards on a
/// channel (`(owner_msg_id, placeholder_msg_id)` pairs), pick the single card to
/// re-key onto `new_head` so the channel keeps AT MOST ONE visible queued card.
///
/// * Never reuse a card already owned by `new_head` (it has no card yet / would
///   be a self-reuse).
/// * Prefer a card whose owner is still present in the live queue (`queued_ids`)
///   — that is the canonical waiting card; fall back to any other channel card
///   (e.g. a stale owner whose intervention already drained but whose card has
///   not been cleaned up yet) so we coalesce rather than stack.
/// * `None` when there is no other channel card to reuse → caller posts fresh.
fn pick_channel_queued_placeholder(
    channel_cards: &[(serenity::MessageId, serenity::MessageId)],
    new_head: serenity::MessageId,
    queued_ids: &std::collections::HashSet<serenity::MessageId>,
) -> Option<(serenity::MessageId, serenity::MessageId)> {
    let mut fallback: Option<(serenity::MessageId, serenity::MessageId)> = None;
    for &(owner, placeholder) in channel_cards {
        if owner == new_head {
            continue;
        }
        if queued_ids.contains(&owner) {
            return Some((owner, placeholder));
        }
        fallback.get_or_insert((owner, placeholder));
    }
    fallback
}

/// #3082 part A — generalize "AT MOST ONE queued card per channel/active turn".
///
/// `reuse_merged_queued_placeholder` only collapses *merge-eligible*
/// follow-ups (same author, no reply boundary, within the merge rules). Messages
/// that skip merge — a different author, a reply-boundary message, or an
/// identical re-send outside the 10s dedup window — each used to POST their own
/// fresh `📬` card, leaving multiple waiting cards in the timeline. This helper
/// closes that gap: before POSTing a fresh card, reuse ANY existing queued card
/// already owned by this channel's active turn, re-keying it onto the new head
/// id and re-rendering it with the latest request text.
///
/// Returns:
/// * `Some(true)`  — an existing card was successfully re-keyed/re-rendered onto
///   `user_msg_id`. Caller must NOT post a fresh card.
/// * `Some(false)` — we found a card and tried to reuse it but the re-render
///   failed; the mapping was rolled back. Caller should fall through to the
///   fresh POST path (which deletes/replaces as needed).
/// * `None`        — there is no existing queued card to reuse on this channel.
///   Caller posts a fresh card as before.
async fn reuse_any_queued_placeholder_for_channel(
    ctx: &serenity::Context,
    data: &Data,
    channel_id: serenity::ChannelId,
    user_msg_id: serenity::MessageId,
    text: &str,
) -> Option<bool> {
    // Serialize against every other `queued_placeholders` mutation on this
    // channel (dispatch hand-off, queue-exit drain, merged reuse) — same TOCTOU
    // guard as `reuse_merged_queued_placeholder`.
    let persist_lock = data.shared.queued_placeholders_persist_lock(channel_id);
    let persist_guard = persist_lock.lock_owned().await;

    let snapshot = mailbox_snapshot(&data.shared, channel_id).await;
    // Only reuse while this message is genuinely still queued behind an active
    // turn. If it already started, the dispatch hand-off owns its card; if it is
    // no longer queued, there is nothing to represent.
    let still_queued = snapshot.intervention_queue.iter().any(|intervention| {
        intervention.message_id == user_msg_id
            || intervention.source_message_ids.contains(&user_msg_id)
    });
    if snapshot.active_user_message_id == Some(user_msg_id) || !still_queued {
        return None;
    }

    // Find ANY existing queued card on this channel that does not already belong
    // to `user_msg_id`. Prefer the card owned by a message still present in the
    // queue (the live waiting head) so we re-key the canonical card; fall back to
    // any other channel-scoped card otherwise. Either way the invariant is "one
    // visible card", so reusing whichever exists is correct.
    let queued_ids: std::collections::HashSet<serenity::MessageId> = snapshot
        .intervention_queue
        .iter()
        .flat_map(|intervention| {
            std::iter::once(intervention.message_id)
                .chain(intervention.source_message_ids.iter().copied())
        })
        .collect();

    let channel_cards: Vec<(serenity::MessageId, serenity::MessageId)> = data
        .shared
        .queued
        .queued_placeholders
        .iter()
        .filter_map(|entry| {
            let (ch, owner) = *entry.key();
            (ch == channel_id).then_some((owner, *entry.value()))
        })
        .collect();

    let Some((prior_owner, placeholder_msg_id)) =
        pick_channel_queued_placeholder(&channel_cards, user_msg_id, &queued_ids)
    else {
        return None;
    };

    // Re-key the single card onto this message id so the dispatch hand-off /
    // queue-exit drain track exactly one card per active turn.
    data.shared
        .remove_queued_placeholder_locked(channel_id, prior_owner);
    data.shared
        .insert_queued_placeholder_locked(channel_id, user_msg_id, placeholder_msg_id);

    let gateway = super::super::gateway::DiscordGateway::new(
        ctx.http.clone(),
        data.shared.clone(),
        data.provider.clone(),
        None,
    );
    let key = super::super::placeholder_controller::PlaceholderKey {
        provider: data.provider.clone(),
        channel_id,
        message_id: placeholder_msg_id,
    };
    let queued_input = super::super::placeholder_controller::PlaceholderActiveInput {
        reason: super::super::formatting::MonitorHandoffReason::Queued,
        started_at_unix: chrono::Utc::now().timestamp(),
        tool_summary: None,
        command_summary: None,
        reason_detail: None,
        context_line: None,
        request_line: Some(text.to_string()),
        progress_line: None,
    };
    let outcome = data
        .shared
        .ui
        .placeholder_controller
        .ensure_queued(&gateway, key, queued_input)
        .await;

    if !matches!(
        outcome,
        super::super::placeholder_controller::PlaceholderControllerOutcome::Edited
            | super::super::placeholder_controller::PlaceholderControllerOutcome::Coalesced
    ) {
        // Render failed — roll back the re-key so the prior owner keeps the card,
        // and let the caller post a fresh one (which handles deletion/cleanup).
        data.shared
            .remove_queued_placeholder_locked(channel_id, user_msg_id);
        data.shared
            .insert_queued_placeholder_locked(channel_id, prior_owner, placeholder_msg_id);
        drop(persist_guard);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ QUEUE-ACK: channel-wide reuse of placeholder {} for message {} in channel {} failed to render ({:?}); kept prior mapping, falling back to fresh card",
            placeholder_msg_id,
            user_msg_id,
            channel_id,
            outcome,
        );
        return Some(false);
    }
    drop(persist_guard);

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] ➕ QUEUE-ACK: coalesced queued message {} into existing channel placeholder {} in channel {} (one card per active turn)",
        user_msg_id,
        placeholder_msg_id,
        channel_id
    );
    Some(true)
}

async fn render_visible_queued_ack(
    ctx: &serenity::Context,
    data: &Data,
    channel_id: serenity::ChannelId,
    user_msg_id: serenity::MessageId,
    text: &str,
    merged: bool,
) -> bool {
    // #3009: a merged follow-up reuses the existing waiting placeholder instead
    // of stacking a duplicate card. Only fall through to a fresh POST when this
    // message is the current merged head AND no prior card exists to reuse
    // (e.g. the head's earlier POST failed). A stale non-head merged source is
    // already represented by the head's single card, so it is `Handled` (no new
    // card) — posting one would recreate the duplicate this path prevents.
    if merged {
        match reuse_merged_queued_placeholder(ctx, data, channel_id, user_msg_id).await {
            MergedPlaceholderReuse::Handled => return true,
            MergedPlaceholderReuse::PostFresh => {}
        }
    }
    // #3082 part A: even when this message did NOT merge (different author, a
    // reply-boundary message, or an identical re-send outside the dedup window),
    // enforce a single visible queued card per active turn by reusing any card
    // already owned by this channel's active turn instead of POSTing a second
    // one. `Some(true)` => handled (no fresh card); `Some(false)`/`None` => fall
    // through to the fresh POST below.
    if let Some(true) =
        reuse_any_queued_placeholder_for_channel(ctx, data, channel_id, user_msg_id, text).await
    {
        return true;
    }
    let post_result = super::super::gateway::send_intake_placeholder(
        ctx.http.clone(),
        data.shared.clone(),
        channel_id,
        None,
        // #3082 P2-3: this IS the queued-turn "📬" notice — wait behind any
        // in-flight multi-chunk answer flush so the card lands as a trailing
        // notice, never interleaved between answer chunks.
        true,
    )
    .await;
    let placeholder_msg_id = match post_result {
        Ok(msg_id) => msg_id,
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ QUEUE-ACK: placeholder POST failed for queued message {} in channel {}: {}",
                user_msg_id,
                channel_id,
                error
            );
            // #1984 (codex C — observation): record the failure without
            // changing user-visible behaviour. The user message is already
            // enqueued in the mailbox at this point — only the visible
            // "queued" card rendering is missing — so the recovery label
            // reflects that the message itself is not lost.
            crate::services::observability::emit_intake_placeholder_post_failed(
                data.provider.as_str(),
                channel_id.get(),
                Some(user_msg_id.get()),
                "queue_ack_visible",
                "queued_card_skipped",
                &error.to_string(),
            );
            // #2044 F13: best-effort user-visible fallback. Without
            // this, a Discord 5xx on the queued-card POST left users
            // with "no card / no reaction yet / message silently
            // queued" until the per-message 📬 reaction landed later
            // — surface a short text reply so the user knows their
            // message is in the queue even when the card path
            // failed. Reply is rate-limited and best-effort.
            send_reaction_control_reply(
                ctx,
                &data.shared,
                channel_id,
                user_msg_id,
                "📬 큐에 추가됨 — 카드 표시는 실패했지만 메시지는 큐잉되었습니다.",
            )
            .await;
            return false;
        }
    };

    let persist_lock = data.shared.queued_placeholders_persist_lock(channel_id);
    let persist_guard = persist_lock.lock_owned().await;
    let snapshot = mailbox_snapshot(&data.shared, channel_id).await;
    // codex round-3 P2: for a merged fallback (reuse returned `PostFresh`),
    // require this id to STILL be the queued head before committing a fresh
    // card. Re-snapshotting under the persist lock closes the burst race where
    // a newer follow-up merged (advancing the head past `user_msg_id`) while
    // our `send_intake_placeholder` POST was in flight — without this, a stale
    // non-head source could keep its own card and recreate the #3009 duplicate.
    // Non-merged (standalone) messages keep the original source-id membership
    // check: they are their own head, so the two conditions coincide.
    let still_queued = snapshot.intervention_queue.iter().any(|intervention| {
        if merged {
            intervention.message_id == user_msg_id
        } else {
            intervention.message_id == user_msg_id
                || intervention.source_message_ids.contains(&user_msg_id)
        }
    });
    let already_started = snapshot.active_user_message_id == Some(user_msg_id);
    if already_started || !still_queued {
        drop(persist_guard);
        // #2044 F13: route Discord 5xx/404 on delete_message into the
        // deferred clear queue instead of silently swallowing them.
        // Without this, a stale "📬 메시지 대기 중" card could remain
        // visible after the turn already started or the queue moved on.
        if let Err(error) = channel_id
            .delete_message(&ctx.http, placeholder_msg_id)
            .await
        {
            tracing::warn!(
                "  [{ts}] ⚠ QUEUE-ACK: delete placeholder {} in channel {} failed ({error}); deferring cleanup",
                placeholder_msg_id,
                channel_id,
                ts = chrono::Local::now().format("%H:%M:%S"),
            );
            data.shared
                .add_pending_queue_exit_placeholder_clear_one(
                    channel_id,
                    user_msg_id,
                    placeholder_msg_id,
                )
                .await;
        }
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔁 QUEUE-ACK: queued message {} in channel {} no longer needs a queued card",
            user_msg_id,
            channel_id
        );
        return false;
    }

    data.shared
        .insert_queued_placeholder_locked(channel_id, user_msg_id, placeholder_msg_id);
    let gateway = super::super::gateway::DiscordGateway::new(
        ctx.http.clone(),
        data.shared.clone(),
        data.provider.clone(),
        None,
    );
    let key = super::super::placeholder_controller::PlaceholderKey {
        provider: data.provider.clone(),
        channel_id,
        message_id: placeholder_msg_id,
    };
    let queued_input = super::super::placeholder_controller::PlaceholderActiveInput {
        reason: super::super::formatting::MonitorHandoffReason::Queued,
        started_at_unix: chrono::Utc::now().timestamp(),
        tool_summary: None,
        command_summary: None,
        reason_detail: None,
        context_line: None,
        request_line: Some(text.to_string()),
        progress_line: None,
    };
    let outcome = data
        .shared
        .ui
        .placeholder_controller
        .ensure_queued(&gateway, key, queued_input)
        .await;
    if matches!(
        outcome,
        super::super::placeholder_controller::PlaceholderControllerOutcome::Edited
            | super::super::placeholder_controller::PlaceholderControllerOutcome::Coalesced
    ) {
        drop(persist_guard);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 📬 QUEUE-ACK: rendered queued placeholder for message {} in channel {}",
            user_msg_id,
            channel_id
        );
        return true;
    }

    data.shared
        .remove_queued_placeholder_locked(channel_id, user_msg_id);
    drop(persist_guard);
    // #2044 F13: same deferred-cleanup fallback as the early-return
    // branch above — a delete_message error here is logged + enqueued
    // for the next `drain_pending_queue_exit_placeholder_clears` pass
    // instead of being silently discarded.
    if let Err(error) = channel_id
        .delete_message(&ctx.http, placeholder_msg_id)
        .await
    {
        tracing::warn!(
            "  [{ts}] ⚠ QUEUE-ACK: delete placeholder {} in channel {} failed after render miss ({error}); deferring cleanup",
            placeholder_msg_id,
            channel_id,
            ts = chrono::Local::now().format("%H:%M:%S"),
        );
        data.shared
            .add_pending_queue_exit_placeholder_clear_one(
                channel_id,
                user_msg_id,
                placeholder_msg_id,
            )
            .await;
    }
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ⚠ QUEUE-ACK: queued placeholder render failed for message {} in channel {}; deleted placeholder",
        user_msg_id,
        channel_id
    );
    false
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
            // Cache miss — fetch from Discord API to determine bot status.
            //
            // #2044 F4: on API failure (transient 5xx, timeout, or
            // restart-warmup with empty cache) the previous fallback
            // treated the user as a bot, which silently dropped
            // legitimate user stop/cancel reactions. The reaction event
            // already carried the user_id and we've passed the bot
            // self-check above, so fail-open + warn is the safer
            // default: a missed bot-self event will be re-filtered on
            // the next cache fill, but a missed human stop is
            // user-visible.
            match ctx.http.get_user(user_id).await {
                Ok(user) => user.bot,
                Err(err) => {
                    tracing::warn!(
                        "  [reaction-remove] failed to fetch user {} from API: {err}; defaulting to non-bot (#2044 F4 fail-open)",
                        user_id
                    );
                    false
                }
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
            // The 🚫 reaction added by `apply_queue_exit_feedback`
            // (see `mod.rs:queue_exit_feedback_emoji`) is the only feedback
            // we surface here — no extra reply, per operator preference.
            let removed = mailbox_cancel_soft_intervention(
                &data.shared,
                &data.provider,
                channel_id,
                removed_reaction.message_id,
            )
            .await;
            if removed.is_some() {
                super::super::advance_last_message_checkpoint(
                    &data.shared,
                    &data.provider,
                    channel_id,
                    removed_reaction.message_id,
                );
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 📭 QUEUE-CANCEL: removed queued message {} in channel {} via reaction removal",
                    removed_reaction.message_id,
                    channel_id
                );
            }
        }
        RemovedControlReaction::StopActiveTurn => {
            // #441: flows through cancel_text_stop_token_mailbox (mailbox_cancel_active_turn)
            // → cancel_active_token → token.cancelled triggers turn_bridge loop exit
            // → mailbox_finish_turn canonical cleanup
            //
            // #2044 F1 (TOCTOU): snapshot the cancel-token together with
            // the active user_message_id. Between this snapshot and the
            // cancel await, the mailbox actor may finish the current turn
            // and start a new one for a queued message — using the
            // snapshotted token identity via
            // `cancel_text_stop_token_mailbox_if_current` ensures we only
            // cancel if the mailbox is still on the same turn we just
            // observed. The inflight-file fallback intentionally does NOT
            // carry a token (it's only consulted when the mailbox snapshot
            // lacks an active turn), so in that branch we fall back to the
            // legacy unchecked cancel as before.
            let snapshot = mailbox_snapshot(&data.shared, channel_id).await;
            let (active_message_id, expected_token) = match snapshot.active_user_message_id {
                Some(active_id) => (Some(active_id), snapshot.cancel_token.clone()),
                None => {
                    // user_msg_id == 0 (e.g. a TUI-direct turn) anchors no
                    // Discord message that could carry a reaction, so it yields
                    // None (never matches `removed_reaction.message_id`);
                    // `MessageId::new(0)` would panic.
                    let inflight_id = super::super::inflight::load_inflight_state(
                        &data.provider,
                        channel_id.get(),
                    )
                    .and_then(|state| {
                        super::super::inflight::optional_message_id(state.user_msg_id)
                    });
                    (inflight_id, None)
                }
            };
            if active_message_id != Some(removed_reaction.message_id) {
                return Ok(());
            }

            let stop_lookup = if let Some(expected) = expected_token {
                super::message_handler::cancel_text_stop_token_mailbox_if_current(
                    &data.shared,
                    &data.provider,
                    channel_id,
                    expected,
                    "reaction remove ⏳ (if_current)",
                )
                .await
            } else {
                super::message_handler::cancel_text_stop_token_mailbox(
                    &data.shared,
                    &data.provider,
                    channel_id,
                )
                .await
            };
            match stop_lookup {
                super::message_handler::TextStopLookup::Stop(token) => {
                    // #1218: stop_active_turn sends the provider abort key
                    // (C-c) FIRST so the CLI sees the interrupt while its
                    // tmux pane is still alive, then flips the cooperative
                    // flag and SIGKILLs the wrapper. The previous order
                    // killed the tmux-wrapper first — tearing down the
                    // tmux session — which made the follow-up send-keys
                    // fail with "can't find pane". For Codex/Qwen TUIs and
                    // resumed runs (`child_pid = None`) the C-c is the
                    // only mechanism that actually stops the provider.
                    super::super::turn_bridge::stop_active_turn(
                        &data.provider,
                        &token,
                        super::super::turn_bridge::TmuxCleanupPolicy::PreserveSession,
                        "reaction remove ⏳",
                    )
                    .await;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 🛑 TURN-STOP: cancelled active turn for message {} in channel {} via reaction removal",
                        removed_reaction.message_id,
                        channel_id
                    );
                    // #3650: no separate notify-bot stop message — the in-place
                    // `[Stopped]` edit on the assistant message and the 🛑
                    // reaction already cover the stop signal.
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
                if super::super::idle_recap_interaction::is_idle_recap_clear_custom_id(
                    &component.data.custom_id,
                ) {
                    return super::super::idle_recap_interaction::handle_idle_recap_clear_interaction(
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
            // intake-gate so we can both (a) cheaply classify the message and
            // (b) embed the full announcement in any queued Intervention we
            // construct on the busy-channel/thread-guard/drain-mode paths
            // below. Resolution is non-consuming: local store, durable PG row,
            // then a short pending-key wait for the gateway-before-send-response
            // race. Legacy hidden metadata is deliberately not trusted here;
            // the durable/ref path is the authority for new runtime routing.
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
                add_reaction(
                    &ctx.http,
                    channel_id,
                    new_message.id,
                    super::super::queue_exit_feedback_emoji(stale.queue_exit_kind),
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
                if try_handle_pending_dm_reply(
                    None::<&crate::db::Db>,
                    data.shared.pg_pool.as_ref(),
                    new_message,
                )
                .await
                {
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
                upload_records_appended_to_session =
                    append_pending_uploads(&data.shared, channel_id, &upload_records).await;
                let handled = super::message_handler::handle_text_command(
                    ctx,
                    new_message,
                    &data,
                    channel_id,
                    &cmd_text,
                )
                .await?;
                if handled {
                    if !upload_records_appended_to_session {
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
                            if !claim_voice_transcript_announcement_for_queue(
                                data,
                                channel_id,
                                new_message.id,
                                &resolved_voice_announcement,
                                "thread_guard_queue",
                            )
                            .await
                            {
                                return Ok(());
                            }
                            let outcome = enqueue_soft_intervention(
                                data,
                                channel_id,
                                user_id,
                                new_message.author.bot,
                                new_message.id,
                                text,
                                None,
                                false,
                                false,
                                upload_records.clone(),
                                // #2266: thread-guard queue path — embed the
                                // voice payload so the eventual queued
                                // dispatch can reinsert it into the store
                                // even if the >30s TTL expires first.
                                resolved_voice_announcement.clone(),
                            )
                            .await;
                            add_queue_pending_reaction_self_healing(
                                ctx,
                                data,
                                channel_id,
                                new_message.id,
                                queue_pending_reaction_for(outcome),
                            )
                            .await;
                            // #2044 F12: use monotonic checkpoint helper
                            // so this hot intake path matches the cancel
                            // reaction path
                            // (`mod.rs:advance_last_message_checkpoint`)
                            // and never regresses the per-channel
                            // last-processed id.
                            super::super::advance_last_message_checkpoint(
                                &data.shared,
                                &data.provider,
                                channel_id,
                                new_message.id,
                            );
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
                    let outcome = enqueue_soft_intervention(
                        data,
                        channel_id,
                        user_id,
                        new_message.author.bot,
                        new_message.id,
                        text,
                        None,
                        false,
                        false,
                        upload_records.clone(),
                        // #2266: DISPATCH: collision guard — DISPATCH messages
                        // never carry voice transcripts, so this is always
                        // None. Explicit for clarity / future audits.
                        None,
                    )
                    .await;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 📬 DISPATCH-GUARD: queued dispatch message in channel {} (active turn in progress)",
                        channel_id
                    );
                    add_queue_pending_reaction_self_healing(
                        ctx,
                        data,
                        channel_id,
                        new_message.id,
                        queue_pending_reaction_for(outcome),
                    )
                    .await;
                    // #2044 F12: monotonic checkpoint (see comment above).
                    super::super::advance_last_message_checkpoint(
                        &data.shared,
                        &data.provider,
                        channel_id,
                        new_message.id,
                    );
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
                if !claim_voice_transcript_announcement_for_queue(
                    data,
                    channel_id,
                    new_message.id,
                    &resolved_voice_announcement,
                    "busy_active_turn_queue",
                )
                .await
                {
                    return Ok(());
                }
                let outcome = enqueue_soft_intervention(
                    data,
                    channel_id,
                    user_id,
                    new_message.author.bot,
                    new_message.id,
                    text,
                    reply_context.clone(),
                    has_reply_boundary,
                    merge_consecutive,
                    upload_records.clone(),
                    // #2266: main busy-active-turn queue path — voice transcripts that
                    // arrive while a previous turn is running flow through here. Embed the
                    // announcement so the queued dispatch reinserts it into the store even
                    // if the >30s in-memory TTL expires first.
                    resolved_voice_announcement.clone(),
                )
                .await;
                let is_shutting_down = data
                    .shared
                    .restart
                    .shutting_down
                    .load(std::sync::atomic::Ordering::Relaxed);

                if !outcome.enqueued {
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
                        outcome.merged,
                    )
                    .await;
                }

                // React 📬 (standalone queue head) or ➕ (merged into previous
                // head), self-healing against the #3182 late-add race (see
                // `add_queue_pending_reaction_self_healing`).
                add_queue_pending_reaction_self_healing(
                    ctx,
                    data,
                    channel_id,
                    new_message.id,
                    queue_pending_reaction_for(outcome),
                )
                .await;

                // Checkpoint: message successfully queued (#2044 F12 — monotonic).
                super::super::advance_last_message_checkpoint(
                    &data.shared,
                    &data.provider,
                    channel_id,
                    new_message.id,
                );
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
                .restart
                .reconcile_done
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                if !claim_voice_transcript_announcement_for_queue(
                    data,
                    channel_id,
                    new_message.id,
                    &resolved_voice_announcement,
                    "reconcile_gate_queue",
                )
                .await
                {
                    return Ok(());
                }
                let _ = enqueue_soft_intervention(
                    data,
                    channel_id,
                    user_id,
                    new_message.author.bot,
                    new_message.id,
                    text,
                    reply_context.clone(),
                    has_reply_boundary,
                    merge_consecutive,
                    upload_records.clone(),
                    // #2266: reconcile gate — startup-recovery queue path. Voice transcripts
                    // that arrive before recovery completes need the embedded payload too.
                    resolved_voice_announcement.clone(),
                )
                .await;
                // Checkpoint: track last processed message (#2044 F12 — monotonic).
                super::super::advance_last_message_checkpoint(
                    &data.shared,
                    &data.provider,
                    channel_id,
                    new_message.id,
                );
                formatting::add_reaction_raw(&ctx.http, channel_id, new_message.id, '🔄').await;
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

                if !claim_voice_transcript_announcement_for_queue(
                    data,
                    channel_id,
                    new_message.id,
                    &resolved_voice_announcement,
                    "drain_mode_queue",
                )
                .await
                {
                    return Ok(());
                }
                let outcome = enqueue_soft_intervention(
                    data,
                    channel_id,
                    user_id,
                    new_message.author.bot,
                    new_message.id,
                    text,
                    reply_context.clone(),
                    has_reply_boundary,
                    merge_consecutive,
                    upload_records.clone(),
                    // #2266: drain-mode queue path (restart pending) — pass the embedded voice
                    // payload so the post-restart dispatch path can reinsert it into the store.
                    resolved_voice_announcement.clone(),
                )
                .await;

                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏸ DRAIN: queued message from [{user_name}] in channel {} (restart pending)",
                    channel_id
                );

                // React 📬 (standalone) or ➕ (merged into previous queue head).
                add_queue_pending_reaction_self_healing(
                    ctx,
                    data,
                    channel_id,
                    new_message.id,
                    queue_pending_reaction_for(outcome),
                )
                .await;

                // Checkpoint: message successfully queued in drain mode (#2044 F12 — monotonic).
                super::super::advance_last_message_checkpoint(
                    &data.shared,
                    &data.provider,
                    channel_id,
                    new_message.id,
                );

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
                    if !claim_voice_transcript_announcement_for_queue(
                        data,
                        channel_id,
                        new_message.id,
                        &resolved_voice_announcement,
                        "idle_backlog_queue",
                    )
                    .await
                    {
                        return Ok(());
                    }
                    Some(
                        enqueue_soft_intervention(
                            data,
                            channel_id,
                            user_id,
                            new_message.author.bot,
                            new_message.id,
                            text,
                            reply_context.clone(),
                            has_reply_boundary,
                            merge_consecutive,
                            upload_records.clone(),
                            // #2266: queued-behind-idle-backlog path —
                            // FIFO ordering keeps voice transcripts behind
                            // pre-existing queue items, so embed the
                            // payload for the eventual dispatch reinsert.
                            resolved_voice_announcement.clone(),
                        )
                        .await,
                    )
                }
            };
            if let Some(outcome) = queued_behind_idle_backlog {
                let ts = chrono::Local::now().format("%H:%M:%S");
                if outcome.enqueued {
                    tracing::info!(
                        "  [{ts}] 📬 IDLE-QUEUE: queued message from [{user_name}] in channel {} behind pending backlog",
                        channel_id
                    );
                    add_queue_pending_reaction_self_healing(
                        ctx,
                        data,
                        channel_id,
                        new_message.id,
                        queue_pending_reaction_for(outcome),
                    )
                    .await;
                    // #2044 F12: monotonic checkpoint helper.
                    super::super::advance_last_message_checkpoint(
                        &data.shared,
                        &data.provider,
                        channel_id,
                        new_message.id,
                    );
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

            // Phase 4 of intake-node-routing: try to forward to a worker
            // node first. Only acts when a PG pool exists AND the global
            // mode env var is `observe` or `enforce`; otherwise this is
            // a no-op and the leader runs the intake locally as before.
            let route_decision = if !new_message.attachments.is_empty() {
                tracing::debug!(
                    channel_id = %channel_id,
                    user_msg_id = %new_message.id,
                    "[intake_router] Discord attachments are node-local — running locally"
                );
                None
            } else if let Some(pool) = data.shared.pg_pool.as_ref().as_ref() {
                let mode =
                    crate::services::cluster::intake_router_hook::IntakeRoutingMode::from_env();
                let leader_instance_id =
                    crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
                let channel_id_str = channel_id.get().to_string();
                let user_msg_id_str = new_message.id.get().to_string();
                let request_owner_id_str = user_id.get().to_string();
                let turn_kind_str = match turn_kind {
                    super::message_handler::TurnKind::Foreground => "foreground",
                    super::message_handler::TurnKind::BackgroundTrigger => "background_trigger",
                };
                let hook_ctx = crate::services::cluster::intake_router_hook::IntakeRouterContext {
                    mode,
                    leader_instance_id: &leader_instance_id,
                    channel_id: &channel_id_str,
                    user_msg_id: &user_msg_id_str,
                    request_owner_id: &request_owner_id_str,
                    request_owner_name: Some(user_name),
                    user_text: text,
                    reply_context: reply_context.as_deref(),
                    has_reply_boundary,
                    dm_hint: Some(is_dm),
                    turn_kind: turn_kind_str,
                    merge_consecutive,
                    reply_to_user_message: false,
                    defer_watcher_resume: false,
                    wait_for_completion: false,
                };
                Some(
                    crate::services::cluster::intake_router_hook::try_route_intake(pool, &hook_ctx)
                        .await,
                )
            } else {
                None
            };

            // Branch on the decision:
            // - `Forwarded` → worker has it, skip local.
            // - `SkippedDuplicate` → Discord redelivery, skip local
            //   (running locally would double-emit).
            // - `ObservedWouldForward` → log the would-be target,
            //   fall through to local (dark-launch).
            // - `RanLocal { reason }` → log the reason for Phase 5
            //   observability, then fall through to local.
            match &route_decision {
                Some(
                    crate::services::cluster::intake_router_hook::IntakeRouterDecision::Forwarded {
                        target_instance_id,
                        outbox_id,
                    },
                ) => {
                    tracing::info!(
                        target_instance_id = %target_instance_id,
                        outbox_id = outbox_id,
                        channel_id = %channel_id,
                        user_msg_id = %new_message.id,
                        "[intake_router] ENFORCE: forwarded intake to worker — skipping local execution"
                    );
                    return Ok(());
                }
                Some(crate::services::cluster::intake_router_hook::IntakeRouterDecision::SkippedDuplicate) => {
                    tracing::info!(
                        channel_id = %channel_id,
                        user_msg_id = %new_message.id,
                        "[intake_router] SKIPPED_DUPLICATE: Discord redelivered known message — skipping local execution"
                    );
                    return Ok(());
                }
                Some(
                    crate::services::cluster::intake_router_hook::IntakeRouterDecision::ObservedWouldForward {
                        target_instance_id,
                    },
                ) => {
                    tracing::info!(
                        target_instance_id = %target_instance_id,
                        channel_id = %channel_id,
                        user_msg_id = %new_message.id,
                        "[intake_router] OBSERVE: would forward — running locally"
                    );
                }
                Some(crate::services::cluster::intake_router_hook::IntakeRouterDecision::RanLocal { reason }) => {
                    if let crate::services::cluster::intake_router_hook::RanLocalReason::DbErrorFellBackToLocal { detail } = &reason {
                        tracing::warn!(
                            channel_id = %channel_id,
                            user_msg_id = %new_message.id,
                            detail = %detail,
                            "[intake_router] DB error during routing decision — falling back to local"
                        );
                    } else {
                        tracing::debug!(
                            ?reason,
                            channel_id = %channel_id,
                            user_msg_id = %new_message.id,
                            "[intake_router] ran locally"
                        );
                    }
                }
                None => {} // hook not active (no PG pool)
            }

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
            super::message_handler::handle_text_message(
                &deps,
                channel_id,
                new_message.id,
                user_id,
                user_name,
                text,
                false,
                false,
                false,
                merge_consecutive,
                reply_context,
                has_reply_boundary,
                Some(is_dm),
                turn_kind,
                preloaded_uploads,
            )
            .await?;
        }
        _ => {}
    }
    Ok(())
}

use super::super::model_picker_interaction::handle_model_picker_interaction;

/// #1446 Layer 2 — `thread_guard_inflight_is_stale` reads inflight files
/// via the runtime root override, so we keep the always-on slice that
/// only exercises the read+staleness classification (no `SharedData`
/// construction). The `thread_guard_force_clean_stale_thread` integration
/// test that drives mailbox cancel / dispatch_thread_parents removal is
/// gated on `legacy-sqlite-tests` because it depends on `TestHealthHarness`.
#[cfg(test)]
mod thread_guard_stale_pure_tests {
    use super::*;
    use chrono::TimeZone;
    use poise::serenity_prelude::ChannelId;

    /// Anchor `now` and produce a stale `updated_at` literal using the
    /// production `now_string` encoding.
    fn local_at_offset(now_unix: i64, offset_secs: i64) -> String {
        chrono::Local
            .timestamp_opt(now_unix + offset_secs, 0)
            .single()
            .expect("valid local time")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    fn seed_inflight_with_updated_at(provider: &ProviderKind, channel_id: u64, updated_at: &str) {
        let mut state = super::super::super::inflight::InflightTurnState::new(
            provider.clone(),
            channel_id,
            Some("test-thread-guard".to_string()),
            42,
            8_001,
            8_002,
            "test-input".to_string(),
            Some("test-session".to_string()),
            Some("test-tmux".to_string()),
            None,
            None,
            0,
        );
        state.updated_at = updated_at.to_string();
        state.started_at = updated_at.to_string();
        let root = super::super::super::inflight::inflight_runtime_root()
            .expect("inflight runtime root must be available under test override");
        let provider_dir = root.join(provider.as_str());
        std::fs::create_dir_all(&provider_dir).expect("create provider dir");
        let path = provider_dir.join(format!("{channel_id}.json"));
        let json = serde_json::to_string_pretty(&state).expect("serialize seeded inflight");
        std::fs::write(&path, json).expect("write seeded inflight");
    }

    fn inflight_with_updated_at(
        provider: &ProviderKind,
        channel_id: u64,
        updated_at: &str,
    ) -> super::super::super::inflight::InflightTurnState {
        let mut state = super::super::super::inflight::InflightTurnState::new(
            provider.clone(),
            channel_id,
            Some("test-thread-guard".to_string()),
            42,
            8_001,
            8_002,
            "test-input".to_string(),
            Some("test-session".to_string()),
            Some("stale-proof-tmux".to_string()),
            None,
            None,
            0,
        );
        state.updated_at = updated_at.to_string();
        state.started_at = updated_at.to_string();
        state
    }

    fn watcher_snapshot(
        provider: &ProviderKind,
        channel_id: u64,
        user_msg_id: u64,
        attached: bool,
        tmux_session_alive: Option<bool>,
        desynced: bool,
    ) -> super::super::super::health::WatcherStateSnapshot {
        let relay_health = super::super::super::relay_health::RelayHealthSnapshot {
            provider: provider.as_str().to_string(),
            channel_id,
            active_turn: super::super::super::relay_health::RelayActiveTurn::Foreground,
            tmux_session: Some("stale-proof-tmux".to_string()),
            tmux_alive: tmux_session_alive,
            watcher_attached: attached,
            watcher_attached_stale: false,
            watcher_owner_channel_id: attached.then_some(channel_id),
            watcher_owns_live_relay: false,
            bridge_inflight_present: true,
            bridge_current_msg_id: Some(8_002),
            mailbox_has_cancel_token: true,
            mailbox_active_user_msg_id: Some(user_msg_id),
            queue_depth: 0,
            pending_discord_callback_msg_id: Some(8_002),
            pending_thread_proof: false,
            parent_channel_id: None,
            thread_channel_id: None,
            last_relay_ts_ms: None,
            last_outbound_activity_ms: None,
            last_capture_offset: None,
            last_relay_offset: 0,
            unread_bytes: None,
            desynced,
            stale_thread_proof: false,
        };
        let relay_stall_state =
            super::super::super::relay_health::RelayStallClassifier::classify(&relay_health);
        super::super::super::health::WatcherStateSnapshot {
            provider: provider.as_str().to_string(),
            attached,
            tmux_session: Some("stale-proof-tmux".to_string()),
            watcher_owner_channel_id: attached.then_some(channel_id),
            last_relay_offset: 0,
            inflight_state_present: true,
            last_relay_ts_ms: 0,
            last_capture_offset: None,
            unread_bytes: None,
            desynced,
            reconnect_count: 0,
            inflight_started_at: None,
            inflight_updated_at: None,
            inflight_user_msg_id: Some(user_msg_id),
            inflight_current_msg_id: Some(8_002),
            tmux_session_alive,
            has_pending_queue: false,
            mailbox_active_user_msg_id: Some(user_msg_id),
            inflight_terminal_delivery_committed: false,
            relay_stall_state,
            relay_health,
        }
    }

    /// Scoped env-var override for `AGENTDESK_ROOT_DIR`. Restores the
    /// previous value (or removes the var) on drop. Used so the always-on
    /// test does not leak state into adjacent test runs that may also rely
    /// on the runtime root.
    ///
    /// #2444 follow-up: acquires `shared_test_env_lock()` so this writer
    /// serializes with every other AGENTDESK_ROOT_DIR mutator in the test
    /// binary (claude_tui::hook_relay, credential, integration tests etc),
    /// closing the cross-module env race that survived the wave-D fix.
    struct EnvRootGuard {
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl EnvRootGuard {
        fn set(path: &std::path::Path) -> Self {
            let lock = crate::config::shared_test_env_lock()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
            Self {
                previous,
                _lock: lock,
            }
        }
    }
    impl Drop for EnvRootGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    /// `thread_guard_inflight_is_stale` must:
    ///   1. report `true` for a stale on-disk inflight,
    ///   2. report `false` for a fresh on-disk inflight,
    ///   3. report `false` when the inflight file does not exist (nothing
    ///      to clean — never cleanup a thread we know nothing about).
    #[tokio::test]
    async fn thread_guard_inflight_is_stale_classifies_disk_state() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());

        let provider = ProviderKind::Codex;
        let now_unix = chrono::Utc::now().timestamp();

        // Missing inflight → not stale.
        assert!(
            !super::thread_guard_inflight_is_stale(
                &provider,
                ChannelId::new(900_000_000_000_900),
                now_unix
            ),
            "missing inflight must NOT be classified as stale"
        );

        // Stale inflight → stale.
        let stale_channel = 900_000_000_000_901u64;
        let stale_at = local_at_offset(
            now_unix,
            -(super::super::super::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64) - 5,
        );
        seed_inflight_with_updated_at(&provider, stale_channel, &stale_at);
        assert!(
            super::thread_guard_inflight_is_stale(
                &provider,
                ChannelId::new(stale_channel),
                now_unix
            ),
            "stale inflight (updated_at={stale_at}) must be classified as stale"
        );

        // Fresh inflight → not stale.
        let fresh_channel = 900_000_000_000_902u64;
        let fresh_at = local_at_offset(now_unix, -5);
        seed_inflight_with_updated_at(&provider, fresh_channel, &fresh_at);
        assert!(
            !super::thread_guard_inflight_is_stale(
                &provider,
                ChannelId::new(fresh_channel),
                now_unix
            ),
            "fresh inflight (updated_at={fresh_at}) must NOT be classified as stale"
        );
    }

    /// #1456: a stale active-turn proof with no attached watcher and no live
    /// tmux owner must be classified as queue-blocked orphan state. The intake
    /// gate uses this to release the mailbox before the new user message takes
    /// the normal streaming path instead of being queued forever.
    #[test]
    fn stale_active_turn_proof_classifies_no_owner_as_queue_blocked_orphan() {
        let provider = ProviderKind::Codex;
        let channel_id = 900_000_000_000_910u64;
        let now_unix = chrono::Utc::now().timestamp();
        let stale_at = local_at_offset(
            now_unix,
            -(super::super::super::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64) - 5,
        );
        let inflight = inflight_with_updated_at(&provider, channel_id, &stale_at);
        let snapshot = watcher_snapshot(
            &provider,
            channel_id,
            inflight.user_msg_id,
            false,
            Some(false),
            false,
        );

        assert_eq!(
            super::classify_stale_active_turn_proof(&inflight, &snapshot, now_unix),
            super::StaleActiveTurnProofClassification::QueueBlockedOrphan
        );
    }

    /// #1456: explicit background placeholders are a visible status surface,
    /// not disposable stale proof. Even if their inflight timestamp is old,
    /// the fail-open classifier must preserve them instead of taking the
    /// cleanup path that would cancel the owning session.
    #[test]
    fn stale_active_turn_proof_preserves_explicit_background_status() {
        let provider = ProviderKind::Codex;
        let channel_id = 900_000_000_000_911u64;
        let now_unix = chrono::Utc::now().timestamp();
        let stale_at = local_at_offset(
            now_unix,
            -(super::super::super::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64) - 5,
        );
        let mut inflight = inflight_with_updated_at(&provider, channel_id, &stale_at);
        inflight.long_running_placeholder_active = true;
        let snapshot = watcher_snapshot(
            &provider,
            channel_id,
            inflight.user_msg_id,
            false,
            Some(false),
            false,
        );

        assert_eq!(
            super::classify_stale_active_turn_proof(&inflight, &snapshot, now_unix),
            super::StaleActiveTurnProofClassification::ExplicitBackgroundStatus
        );
    }
}

/// #3009 — pure-logic tests for merged-placeholder reuse. These exercise the
/// `pick_reusable_merged_placeholder` decision (which card a merged follow-up
/// re-keys) without needing a live serenity ctx.
#[cfg(test)]
mod merged_placeholder_reuse_pure_tests {
    use super::*;
    use poise::serenity_prelude::MessageId;
    use std::collections::HashMap;

    #[test]
    fn merged_followup_reuses_prior_head_card() {
        // Queue head started as msg A (owns card CA); follow-up B merged in,
        // so the head's source ids are [A, B] and the head message_id == B.
        let a = MessageId::new(800_000_000_000_001);
        let b = MessageId::new(800_000_000_000_002);
        let card_a = MessageId::new(700_000_000_000_001);
        let map: HashMap<MessageId, MessageId> = [(a, card_a)].into_iter().collect();

        let picked = pick_reusable_merged_placeholder(&[a, b], b, |id| map.get(&id).copied());

        // The follow-up B must reuse A's existing card (no new card).
        assert_eq!(picked, Some((a, card_a)));
    }

    #[test]
    fn second_merged_followup_reuses_rekeyed_card() {
        // After B merged and re-keyed CA under B, a third follow-up C merges:
        // source ids [A, B, C], only B currently owns the card.
        let a = MessageId::new(800_000_000_000_001);
        let b = MessageId::new(800_000_000_000_002);
        let c = MessageId::new(800_000_000_000_003);
        let card = MessageId::new(700_000_000_000_001);
        let map: HashMap<MessageId, MessageId> = [(b, card)].into_iter().collect();

        let picked = pick_reusable_merged_placeholder(&[a, b, c], c, |id| map.get(&id).copied());

        assert_eq!(picked, Some((b, card)));
    }

    #[test]
    fn no_prior_card_falls_back_to_fresh_post() {
        // The head's earlier POST failed, so no source id owns a card yet.
        let a = MessageId::new(800_000_000_000_001);
        let b = MessageId::new(800_000_000_000_002);
        let map: HashMap<MessageId, MessageId> = HashMap::new();

        let picked = pick_reusable_merged_placeholder(&[a, b], b, |id| map.get(&id).copied());

        assert_eq!(picked, None);
    }

    #[test]
    fn own_id_card_is_never_reused() {
        // Defensive: even if the follow-up id somehow already mapped to a card,
        // it must be excluded so we never "reuse" the card we are about to
        // create / treat the new head as its own prior.
        let a = MessageId::new(800_000_000_000_001);
        let b = MessageId::new(800_000_000_000_002);
        let card_b = MessageId::new(700_000_000_000_002);
        let map: HashMap<MessageId, MessageId> = [(b, card_b)].into_iter().collect();

        let picked = pick_reusable_merged_placeholder(&[a, b], b, |id| map.get(&id).copied());

        assert_eq!(picked, None);
    }
}

/// #3082 part A pure decision tests for `pick_channel_queued_placeholder` — the
/// "one card per active turn" coalescing that closes the gaps merge cannot
/// (different author, reply-boundary, identical re-send outside the dedup
/// window). These don't need a live serenity ctx.
#[cfg(test)]
mod channel_queued_placeholder_pure_tests {
    use super::*;
    use poise::serenity_prelude::MessageId;
    use std::collections::HashSet;

    fn ids(items: &[MessageId]) -> HashSet<MessageId> {
        items.iter().copied().collect()
    }

    #[test]
    fn different_author_reuses_existing_channel_card() {
        // Author A queued first (card CA, still in queue). Author B's message —
        // which does NOT merge with A — must reuse CA, not POST a second card.
        let a = MessageId::new(900_000_000_000_001);
        let b = MessageId::new(900_000_000_000_002);
        let card_a = MessageId::new(710_000_000_000_001);
        let cards = vec![(a, card_a)];
        let queued = ids(&[a, b]);

        let picked = pick_channel_queued_placeholder(&cards, b, &queued);
        assert_eq!(picked, Some((a, card_a)), "B must coalesce onto A's card");
    }

    #[test]
    fn reply_boundary_message_reuses_existing_card() {
        // Same author, but the new message has a reply boundary so merge was
        // skipped. Still must coalesce onto the single channel card.
        let a = MessageId::new(900_000_000_000_010);
        let b = MessageId::new(900_000_000_000_011);
        let card_a = MessageId::new(710_000_000_000_010);
        let cards = vec![(a, card_a)];
        let queued = ids(&[a, b]);

        let picked = pick_channel_queued_placeholder(&cards, b, &queued);
        assert_eq!(picked, Some((a, card_a)));
    }

    #[test]
    fn identical_resend_outside_dedup_window_reuses_card() {
        // The identical-resend dedup only fires within 10s; a later identical
        // re-send is enqueued as a new head but must still coalesce.
        let a = MessageId::new(900_000_000_000_020);
        let b = MessageId::new(900_000_000_000_021);
        let card_a = MessageId::new(710_000_000_000_020);
        let cards = vec![(a, card_a)];
        let queued = ids(&[a, b]);

        let picked = pick_channel_queued_placeholder(&cards, b, &queued);
        assert_eq!(picked, Some((a, card_a)));
    }

    #[test]
    fn prefers_live_queue_owner_over_stale_card() {
        // Two cards exist on the channel: a stale one (owner drained) and the
        // live waiting head. The live head's card must win so we re-key the
        // canonical card; the stale one is left for normal cleanup.
        let stale = MessageId::new(900_000_000_000_030);
        let live = MessageId::new(900_000_000_000_031);
        let new = MessageId::new(900_000_000_000_032);
        let card_stale = MessageId::new(710_000_000_000_030);
        let card_live = MessageId::new(710_000_000_000_031);
        // Stale first so the loop would hit it before the live one — preference
        // logic must still return the live card.
        let cards = vec![(stale, card_stale), (live, card_live)];
        let queued = ids(&[live, new]);

        let picked = pick_channel_queued_placeholder(&cards, new, &queued);
        assert_eq!(picked, Some((live, card_live)));
    }

    #[test]
    fn falls_back_to_any_card_when_no_live_owner() {
        // No card owner is still in the queue (all drained) but a visible card
        // lingers — coalesce onto it rather than stacking a new one.
        let stale = MessageId::new(900_000_000_000_040);
        let new = MessageId::new(900_000_000_000_041);
        let card_stale = MessageId::new(710_000_000_000_040);
        let cards = vec![(stale, card_stale)];
        let queued = ids(&[new]);

        let picked = pick_channel_queued_placeholder(&cards, new, &queued);
        assert_eq!(picked, Some((stale, card_stale)));
    }

    #[test]
    fn no_existing_card_posts_fresh() {
        // Nothing on the channel yet → None → caller posts the first card.
        let new = MessageId::new(900_000_000_000_050);
        let cards: Vec<(MessageId, MessageId)> = Vec::new();
        let queued = ids(&[new]);

        assert_eq!(pick_channel_queued_placeholder(&cards, new, &queued), None);
    }

    #[test]
    fn own_card_is_never_self_reused() {
        // Defensive: a card already keyed to the new head must be ignored so we
        // never "reuse" the card we are about to create for it.
        let new = MessageId::new(900_000_000_000_060);
        let card_new = MessageId::new(710_000_000_000_060);
        let cards = vec![(new, card_new)];
        let queued = ids(&[new]);

        assert_eq!(pick_channel_queued_placeholder(&cards, new, &queued), None);
    }

    #[test]
    fn three_queued_messages_collapse_to_one_card() {
        // Acceptance: queueing 3 messages leaves exactly one visible card.
        // m1 posts CA. m2 reuses CA (re-keyed to m2). m3 reuses it again.
        let m1 = MessageId::new(900_000_000_000_070);
        let m2 = MessageId::new(900_000_000_000_071);
        let m3 = MessageId::new(900_000_000_000_072);
        let card = MessageId::new(710_000_000_000_070);

        // m2: only m1 owns the card.
        let picked_2 = pick_channel_queued_placeholder(&[(m1, card)], m2, &ids(&[m1, m2]));
        assert_eq!(picked_2, Some((m1, card)), "m2 coalesces onto m1's card");

        // After re-key, the card is owned by m2; m3 coalesces onto it.
        let picked_3 = pick_channel_queued_placeholder(&[(m2, card)], m3, &ids(&[m1, m2, m3]));
        assert_eq!(picked_3, Some((m2, card)), "m3 coalesces onto the one card");
    }
}

#[cfg(test)]
mod reply_context_tests {
    use super::{
        AttachmentReplyItem, format_attachment_reply_context, should_start_attachment_only_turn,
        strip_leading_bot_mention,
    };

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
