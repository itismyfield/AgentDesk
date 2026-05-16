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

/// Upper bound on the durable-probe window. The MESSAGE_CREATE event can
/// outrace the producing announcer's `INSERT … voice_transcript_announce_meta`
/// commit. Worst-case staleness in practice is well under a second, but
/// under load / DB contention we want a generous cap before the gate
/// gives up and emits `voice_announce_durable_miss` (#2209 finding #1).
///
/// Configurable via `VOICE_ANNOUNCE_DURABLE_PROBE_CAP_MS`. Clamped to
/// `[100ms, 5s]` so a misconfiguration cannot starve ordinary intake.
const VOICE_ANNOUNCE_DURABLE_PROBE_DEFAULT_MS: u64 = 2_000;
const VOICE_ANNOUNCE_DURABLE_PROBE_FLOOR_MS: u64 = 100;
const VOICE_ANNOUNCE_DURABLE_PROBE_CEIL_MS: u64 = 5_000;

fn resolve_durable_probe_cap_ms() -> u64 {
    let raw = std::env::var("VOICE_ANNOUNCE_DURABLE_PROBE_CAP_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(VOICE_ANNOUNCE_DURABLE_PROBE_DEFAULT_MS);
    raw.clamp(
        VOICE_ANNOUNCE_DURABLE_PROBE_FLOOR_MS,
        VOICE_ANNOUNCE_DURABLE_PROBE_CEIL_MS,
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DurableAnnounceProbe {
    /// Row found inside the probe window. Carries the full announcement
    /// so callers can both classify the message AND hydrate the typed
    /// `VoiceTranscriptAnnouncement` payload (#2266) for any queued
    /// `Intervention`.
    Found(Box<crate::voice::prompt::VoiceTranscriptAnnouncement>),
    /// Probe window expired without finding the row. Caller should
    /// drop the message and emit `voice_announce_durable_miss` instead
    /// of falling through to ordinary text intake.
    Miss,
    /// No pg pool configured at all (single-node dev). Caller may
    /// fall through to ordinary intake — there is no durable backing
    /// to fail closed on.
    NoPool,
    /// Pg pool IS configured but the probe query errored. Caller MUST
    /// fail closed and drop the message — proceeding to non-voice
    /// gates would mis-route a voice announce as ordinary text.
    QueryError,
}

async fn probe_durable_voice_transcript_announcement(
    pg_pool: Option<&sqlx::PgPool>,
    message_id: serenity::MessageId,
    channel_id: serenity::ChannelId,
) -> DurableAnnounceProbe {
    let Some(pool) = pg_pool else {
        return DurableAnnounceProbe::NoPool;
    };

    // Discord can dispatch the create event before the producing
    // announcer's INSERT commits. Use an exponentially-spaced probe
    // ladder capped at `resolve_durable_probe_cap_ms()` so ordinary
    // intake is not delayed but a slow durable write still resolves.
    let cap_ms = resolve_durable_probe_cap_ms();
    let mut delays_ms: Vec<u64> = vec![0, 25, 75, 150, 300, 600, 1_200, 2_400];
    delays_ms.retain(|d| *d <= cap_ms);
    if !delays_ms.contains(&cap_ms) {
        delays_ms.push(cap_ms);
    }
    delays_ms.sort_unstable();
    delays_ms.dedup();

    for delay_ms in delays_ms {
        if delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        match crate::voice::announce_meta::load_durable(pool, message_id).await {
            Ok(Some(announcement)) => return DurableAnnounceProbe::Found(Box::new(announcement)),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    channel_id = channel_id.get(),
                    message_id = message_id.get(),
                    "failed to load durable voice transcript announcement metadata — failing closed"
                );
                return DurableAnnounceProbe::QueryError;
            }
        }
    }
    DurableAnnounceProbe::Miss
}

/// #2209 + #2266: Convenience wrapper used by the intake-gate to hydrate the
/// typed `VoiceTranscriptAnnouncement` from the durable side store. Returns
/// the announcement on `Found`, `None` on `Miss` / `NoPool` / `QueryError`.
/// Callers that need to fail-closed on `Miss` and `QueryError` should use
/// [`probe_durable_voice_transcript_announcement`] directly.
async fn load_durable_voice_transcript_announcement(
    pg_pool: Option<&sqlx::PgPool>,
    message_id: serenity::MessageId,
    channel_id: serenity::ChannelId,
) -> Option<crate::voice::prompt::VoiceTranscriptAnnouncement> {
    match probe_durable_voice_transcript_announcement(pg_pool, message_id, channel_id).await {
        DurableAnnounceProbe::Found(announcement) => Some(*announcement),
        DurableAnnounceProbe::Miss
        | DurableAnnounceProbe::NoPool
        | DurableAnnounceProbe::QueryError => None,
    }
}

/// Back-compat thin wrapper for callers that only care about the
/// found/not-found bit. New call sites should use
/// [`probe_durable_voice_transcript_announcement`] so they can react
/// to the `Miss` state with a structured warn instead of silently
/// falling through to ordinary text intake.
async fn has_durable_voice_transcript_announcement(
    pg_pool: Option<&sqlx::PgPool>,
    message_id: serenity::MessageId,
    channel_id: serenity::ChannelId,
) -> bool {
    matches!(
        probe_durable_voice_transcript_announcement(pg_pool, message_id, channel_id).await,
        DurableAnnounceProbe::Found(_)
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

fn build_soft_intervention(
    author_id: serenity::UserId,
    message_id: serenity::MessageId,
    text: &str,
    reply_context: Option<String>,
    has_reply_boundary: bool,
    merge_consecutive: bool,
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
        message_id,
        source_message_ids: vec![message_id],
        text: text.to_string(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context,
        has_reply_boundary,
        merge_consecutive,
        voice_announcement,
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
            message_id,
            text,
            reply_context,
            has_reply_boundary,
            merge_consecutive,
            voice_announcement,
        ),
    )
    .await
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
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
        build_soft_intervention(author_id, message_id, text, None, false, false, None),
    )
    .await
    .enqueued
}

/// Pick the queue-pending reaction emoji based on the enqueue outcome.
/// Standalone queue head entries get `📬`; merged-into-previous entries get
/// `➕` so users can tell merged from standalone at a glance (#1190 follow-up).
pub(super) fn queue_pending_reaction_for(outcome: super::super::MailboxEnqueueOutcome) -> char {
    if outcome.merged { '➕' } else { '📬' }
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
    shared.dispatch_thread_parents.remove(&parent_channel_id);
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
    // and `global_active.fetch_sub(1)` — do not duplicate them here.
    super::super::stall_recovery::finalize_orphaned_clear(
        shared,
        channel_id,
        finish.removed_token,
        "1456_queue_blocked_stale_proof",
    );
    shared
        .dispatch_thread_parents
        .retain(|_, thread_id| *thread_id != channel_id);
    if !finish.has_pending {
        shared.dispatch_role_overrides.remove(&channel_id);
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

async fn render_visible_queued_ack(
    ctx: &serenity::Context,
    data: &Data,
    channel_id: serenity::ChannelId,
    user_msg_id: serenity::MessageId,
    text: &str,
) -> bool {
    let post_result = super::super::gateway::send_intake_placeholder(
        ctx.http.clone(),
        data.shared.clone(),
        channel_id,
        None,
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
    let still_queued = snapshot.intervention_queue.iter().any(|intervention| {
        intervention.message_id == user_msg_id
            || intervention.source_message_ids.contains(&user_msg_id)
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
                    let inflight_id = super::super::inflight::load_inflight_state(
                        &data.provider,
                        channel_id.get(),
                    )
                    .map(|state| serenity::MessageId::new(state.user_msg_id));
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
                    super::super::commands::notify_turn_stop(
                        &ctx.http,
                        &data.shared,
                        &data.provider,
                        channel_id,
                        "reaction remove ⏳",
                    )
                    .await;
                    // Removed `Turn cancelled.` reply — the in-place
                    // `[Stopped]` edit on the assistant message + the
                    // `🛑 현재 턴 중단` outbox lifecycle notice from
                    // `notify_turn_stop` already cover the same signal.
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
            }
        }
        serenity::FullEvent::ReactionRemove { removed_reaction } => {
            handle_reaction_remove(ctx, removed_reaction, data).await?;
        }
        serenity::FullEvent::Message { new_message } => {
            // PR #3b: clear any active idle-recap card for this channel —
            // the user is back, the notification has served its purpose.
            // Spawned so it never blocks turn dispatch; lookup is keyed by
            // channel_id, so bot messages and irrelevant channels are a
            // cheap no-op (one indexed SELECT that returns zero rows).
            if !new_message.author.bot
                && let Some(pool) = data.shared.pg_pool.as_ref().cloned()
            {
                let http_for_clear = ctx.http.clone();
                let channel_id_for_clear = new_message.channel_id.get();
                tokio::spawn(async move {
                    match crate::services::discord::idle_recap::lookup_active_recap_for_channel(
                        &pool,
                        channel_id_for_clear,
                    )
                    .await
                    {
                        Ok(Some((session_key, chan, msg))) => {
                            crate::services::discord::idle_recap::delete_previous_card(
                                &http_for_clear,
                                chan,
                                msg,
                            )
                            .await;
                            // Compare-and-clear: only nullify the pointer
                            // when the row still references the message we
                            // just deleted. Guards against a stale wake-up
                            // racing the next 5-min cycle's fresh card.
                            let _ = crate::services::discord::idle_recap::clear_recap_pointer(
                                &pool,
                                &session_key,
                                msg,
                            )
                            .await;
                        }
                        Ok(None) => {}
                        Err(e) => tracing::warn!(
                            error = %e,
                            channel_id = channel_id_for_clear,
                            "idle_recap clear lookup failed"
                        ),
                    }
                });
            }

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
                let is_dup = match data.shared.intake_dedup.entry(key.clone()) {
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
            let announce_bot_id = super::super::resolve_announce_bot_user_id(&data.shared).await;
            // #2266 + #2209: resolve the voice-transcript payload ONCE at the
            // intake-gate so we can both (a) cheaply classify the message and
            // (b) embed the full announcement in any queued Intervention we
            // construct on the busy-channel/thread-guard/drain-mode paths
            // below. Resolution chain, in order of preference:
            //   1. `peek_clone` from the per-process in-memory store (no
            //      consume — the active dispatch path still finds the entry
            //      when it later calls `take()`).
            //   2. Parse the announce-bot message body when the store has no
            //      entry but the content itself is a valid authorized
            //      announcement (post-restart hydrate / out-of-band publish).
            //   3. Probe the durable Postgres `voice_transcript_announce_meta`
            //      side store with a short retry (#2209). This guards against
            //      the gateway race (Discord MESSAGE_CREATE arrives before
            //      the announcer's send future resolved the id), cross-process
            //      intake workers, dcserver restart, and TTL eviction of the
            //      in-memory store. The probe is only attempted when the
            //      visible announce-marker is present so ordinary intake is
            //      not delayed.
            let is_announce_bot_message = announce_bot_id == Some(user_id.get());
            // Local + body resolution (cheap, no async). `peek_clone` returns
            // the typed announcement without consuming the entry so the
            // active dispatch path still finds it. Body parse covers
            // post-restart hydrate / out-of-band publish.
            let local_or_body_announcement: Option<
                crate::voice::prompt::VoiceTranscriptAnnouncement,
            > = if is_announce_bot_message {
                crate::voice::announce_meta::global_store()
                    .peek_clone(new_message.id)
                    .or_else(|| {
                        crate::voice::prompt::parse_authorized_voice_transcript_announcement(
                            &new_message.content,
                            user_id.get(),
                            announce_bot_id,
                        )
                    })
            } else {
                None
            };
            // High-confidence voice-announce candidate: announce-bot author
            // produced a visible transcript message but neither the in-process
            // store nor the legacy hidden-metadata body parser has it. Probe
            // the durable table so cross-process intakes can recover (#2209)
            // AND obtain the typed payload for downstream Intervention
            // embedding (#2266).
            let visible_announce_candidate = is_announce_bot_message
                && local_or_body_announcement.is_none()
                && crate::voice::prompt::is_visible_voice_transcript_announcement(
                    &new_message.content,
                );
            let durable_probe = if visible_announce_candidate {
                probe_durable_voice_transcript_announcement(
                    data.shared.pg_pool.as_ref(),
                    new_message.id,
                    channel_id,
                )
                .await
            } else {
                DurableAnnounceProbe::NoPool
            };
            // #2209 finding #1 + v5 review: for a high-confidence visible
            // voice-announce candidate, both `Miss` (probe expired) and
            // `QueryError` (DB probe failed) must drop the message rather
            // than fall through to ordinary text intake. Otherwise the
            // mention guard, dispatch-thread guard, mailbox-queue,
            // reconcile, drain, and idle-backlog gates downstream would
            // mis-route or re-queue it as a plain bot text message.
            //
            // #2266: when the probe finds the durable row we also lift the
            // typed announcement out of the Found variant so downstream
            // queue paths can embed it into the `Intervention` payload —
            // keeping the queued-dispatch self-contained across cross-
            // process intake workers, dcserver restart, and the 30s
            // in-memory `voice::announce_meta` TTL.
            let durable_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement> =
                match durable_probe {
                    DurableAnnounceProbe::Miss => {
                        tracing::warn!(
                            event = "voice_announce_durable_miss",
                            channel_id = channel_id.get(),
                            effective_channel_id = effective_channel_id.get(),
                            message_id = new_message.id.get(),
                            author_id = user_id.get(),
                            "announce-bot voice transcript message had no durable metadata after probe cap — dropping rather than routing as ordinary text"
                        );
                        return Ok(());
                    }
                    DurableAnnounceProbe::QueryError => {
                        tracing::warn!(
                            event = "voice_announce_durable_probe_query_error",
                            channel_id = channel_id.get(),
                            effective_channel_id = effective_channel_id.get(),
                            message_id = new_message.id.get(),
                            author_id = user_id.get(),
                            "durable announce-meta probe query errored on a visible voice-announce candidate — failing closed, dropping rather than routing as ordinary text"
                        );
                        return Ok(());
                    }
                    DurableAnnounceProbe::Found(announcement) => Some(*announcement),
                    DurableAnnounceProbe::NoPool => None,
                };
            // #2266 + #2209: feed downstream queue paths the typed payload
            // resolved from whichever layer hit (local store → body parse →
            // durable PG side store).
            let resolved_voice_announcement: Option<
                crate::voice::prompt::VoiceTranscriptAnnouncement,
            > = local_or_body_announcement.or(durable_announcement);
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

            let is_allowed_bot_sender = settings_snapshot.allowed_bot_ids.contains(&user_id.get())
                || announce_bot_id.is_some_and(|id| id == user_id.get());
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
            // #2044 F11: the pattern is constant — compile once via
            // `LazyLock` instead of paying the per-message compile cost
            // and exposing intake to a panic on a hypothetical compile
            // failure (the previous code used `.unwrap()` on a
            // `regex::Regex::new` result inside the hot path).
            let cmd_text = {
                static BOT_MENTION_RE: std::sync::LazyLock<regex::Regex> =
                    std::sync::LazyLock::new(|| {
                        regex::Regex::new(r"^<@!?\d+>\s*")
                            .expect("static bot-mention regex is valid")
                    });
                BOT_MENTION_RE.replace(text, "").to_string()
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
            //
            // #2209 v5 review — exclude voice-announcement messages so they
            // flow through handle_text_message where the atomic durable claim
            // is the first side-effect gate.
            if is_allowed_bot && !is_voice_transcript_announcement {
                // #1446 — copy the mapped thread_id and immediately drop the
                // DashMap ref. `thread_guard_force_clean_stale_thread`
                // re-acquires the same shard lock to call `.remove()`; if the
                // ref were still held we would deadlock on the shard's
                // RwLock. The narrow scope below releases the ref at the `}`.
                let thread_id_opt = {
                    data.shared
                        .dispatch_thread_parents
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
                            let outcome = enqueue_soft_intervention(
                                data,
                                channel_id,
                                user_id,
                                new_message.id,
                                text,
                                None,
                                false,
                                false,
                                // #2266: thread-guard queue path — embed the
                                // voice payload so the eventual queued
                                // dispatch can reinsert it into the store
                                // even if the >30s TTL expires first.
                                resolved_voice_announcement.clone(),
                            )
                            .await;
                            add_reaction(
                                &ctx.http,
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
                        new_message.id,
                        text,
                        None,
                        false,
                        false,
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
                    add_reaction(
                        &ctx.http,
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

            // Queue messages while AI is in progress (executed as next turn after current finishes).
            // #2209 v4 review — voice-announcement messages must never be queued
            // as ordinary soft interventions. They carry durable metadata that
            // is consumed under an atomic ownership claim inside
            // handle_text_message; routing them through the mailbox-queue path
            // would (a) lose the voice-utterance context, (b) leave the durable
            // row unclaimed for the active-turn duration, and (c) potentially
            // exceed the durable TTL before the queued execution runs. Let
            // voice announces fall through to handle_text_message even when
            // the channel has an active turn — that handler is responsible
            // for the queueing decision under voice-aware semantics.
            if !is_voice_transcript_announcement
                && mailbox_has_live_active_turn_or_cleanup_stale_proof(
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
                    new_message.id,
                    text,
                    reply_context.clone(),
                    has_reply_boundary,
                    merge_consecutive,
                    // #2266: main busy-active-turn queue path — voice
                    // transcripts that arrive while a previous turn is
                    // running flow through here. Embed the announcement
                    // so the queued dispatch reinserts it into the store
                    // even if the >30s in-memory TTL expires first.
                    resolved_voice_announcement.clone(),
                )
                .await;
                let is_shutting_down = data
                    .shared
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
                    render_visible_queued_ack(ctx, data, channel_id, new_message.id, text).await;
                }

                // React 📬 (standalone queue head) or ➕ (merged into previous head).
                add_reaction(
                    &ctx.http,
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
            // #2209 v4 review — same rationale as the mailbox-queue branch
            // above: voice announces must not be re-queued as plain
            // interventions; route them through handle_text_message where the
            // atomic durable claim runs as the first side-effect gate.
            if !is_voice_transcript_announcement
                && !data
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
                    // #2266: reconcile gate — startup-recovery queue path.
                    // Voice transcripts that arrive before recovery
                    // completes need the embedded payload too.
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
            // #2209 v4 review — voice announces must not flow through the
            // intervention-queue path; route them through handle_text_message
            // so the atomic durable claim runs as the first side-effect gate.
            if !is_voice_transcript_announcement
                && data
                    .shared
                    .restart_pending
                    .load(std::sync::atomic::Ordering::Relaxed)
            {
                let is_shutting_down = data
                    .shared
                    .shutting_down
                    .load(std::sync::atomic::Ordering::Relaxed);

                let outcome = enqueue_soft_intervention(
                    data,
                    channel_id,
                    user_id,
                    new_message.id,
                    text,
                    reply_context.clone(),
                    has_reply_boundary,
                    merge_consecutive,
                    // #2266: drain-mode queue path (restart pending) —
                    // pass the embedded voice payload so the post-restart
                    // dispatch path can reinsert it into the store.
                    resolved_voice_announcement.clone(),
                )
                .await;

                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏸ DRAIN: queued message from [{user_name}] in channel {} (restart pending)",
                    channel_id
                );

                // React 📬 (standalone) or ➕ (merged into previous queue head).
                add_reaction(
                    &ctx.http,
                    channel_id,
                    new_message.id,
                    queue_pending_reaction_for(outcome),
                )
                .await;

                // Checkpoint: message successfully queued in drain mode
                // (#2044 F12 — monotonic).
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
            //
            // #2209 v5 review — voice-announcement messages must bypass this
            // path so the atomic durable claim runs as the first side-effect
            // gate inside handle_text_message. Otherwise the durable row sits
            // unclaimed behind backlog and can exceed its 10-minute TTL,
            // leaving the visible 🎙 text to be processed as an ordinary
            // announce-bot turn.
            let queued_behind_idle_backlog = if is_voice_transcript_announcement {
                None
            } else {
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
                    add_reaction(
                        &ctx.http,
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
            let route_decision = if let Some(pool) = data.shared.pg_pool.as_ref().as_ref() {
                let mode =
                    crate::services::cluster::intake_router_hook::IntakeRoutingMode::from_env();
                let leader_instance_id =
                    crate::server::cluster::resolve_self_instance_id_without_config();
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
                    tracing::debug!(
                        ?reason,
                        channel_id = %channel_id,
                        user_msg_id = %new_message.id,
                        "[intake_router] ran locally"
                    );
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
            )
            .await?;
        }
        _ => {}
    }
    Ok(())
}

use super::super::model_picker_interaction::handle_model_picker_interaction;

#[cfg(test)]
mod voice_announce_meta_tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use poise::serenity_prelude::{ChannelId, MessageId};

    fn announcement() -> crate::voice::prompt::VoiceTranscriptAnnouncement {
        crate::voice::prompt::VoiceTranscriptAnnouncement {
            transcript: "status please".to_string(),
            user_id: "42".to_string(),
            utterance_id: "utt-gate".to_string(),
            language: "en-US".to_string(),
            verbose_progress: false,
            started_at: None,
            completed_at: None,
            samples_written: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_metadata_marks_announce_message_as_voice_intake() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let message_id = MessageId::new(44_001);

        crate::voice::announce_meta::persist_durable(&pool, message_id, &announcement())
            .await
            .expect("persist durable voice announcement metadata");

        assert!(
            has_durable_voice_transcript_announcement(
                Some(&pool),
                message_id,
                ChannelId::new(55_001)
            )
            .await
        );

        pool.close().await;
        pg_db.drop().await;
    }
}

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
            relay_stall_state,
            relay_health,
        }
    }

    /// Scoped env-var override for `AGENTDESK_ROOT_DIR`. Restores the
    /// previous value (or removes the var) on drop. Used so the always-on
    /// test does not leak state into adjacent test runs that may also rely
    /// on the runtime root.
    struct EnvRootGuard {
        previous: Option<std::ffi::OsString>,
    }
    impl EnvRootGuard {
        fn set(path: &std::path::Path) -> Self {
            let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
            Self { previous }
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod thread_guard_stale_tests {
    //! #1446 Layer 2 — verify the stall-deadlock recovery path inside the
    //! THREAD-GUARD. The full event-handler is not driven (it requires a
    //! live Discord ctx); instead we exercise the two extracted helpers
    //! `thread_guard_inflight_is_stale` + `thread_guard_force_clean_stale_thread`
    //! through the same `SharedData` they would see in production.
    use super::super::super::health::TestHealthHarness;
    use super::*;
    use chrono::TimeZone;
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    use std::sync::Arc;

    /// Anchor `now` and produce a stale `updated_at` literal (older than
    /// `INFLIGHT_STALENESS_THRESHOLD_SECS`) using the same encoding the
    /// production `now_string` writer uses.
    fn stale_local_updated_at(now_unix: i64) -> String {
        let stale_unix = now_unix
            - (super::super::super::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64)
            - 5;
        chrono::Local
            .timestamp_opt(stale_unix, 0)
            .single()
            .expect("valid local time")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    fn fresh_local_updated_at() -> String {
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
    }

    /// Write an inflight state file directly to disk for `(provider, channel)`
    /// with the supplied `updated_at`. Uses the inflight crate's
    /// `inflight_runtime_root` so the test follows the same
    /// `AGENTDESK_ROOT_DIR` / `set_test_runtime_root_override` plumbing as
    /// the production paths.
    fn seed_inflight_with_updated_at(
        provider: &ProviderKind,
        channel_id: u64,
        tmux_session_name: &str,
        updated_at: &str,
    ) {
        let mut state = super::super::super::inflight::InflightTurnState::new(
            provider.clone(),
            channel_id,
            Some("test-thread-guard".to_string()),
            42,
            8_001,
            8_002,
            "test-input".to_string(),
            Some("test-session".to_string()),
            Some(tmux_session_name.to_string()),
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

    /// #1446 Test 2 — when the thread's persisted inflight is stale, the
    /// THREAD-GUARD must classify it as such, force-clean the dispatch
    /// mapping + inflight file + mailbox active turn, and let the caller
    /// fall through to normal processing (no enqueue).
    #[tokio::test]
    async fn thread_guard_clears_stale_inflight_and_proceeds() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let prev_override = crate::config::current_test_runtime_root_override();
        crate::config::set_test_runtime_root_override(Some(temp.path().to_path_buf()));
        struct OverrideGuard(Option<std::path::PathBuf>);
        impl Drop for OverrideGuard {
            fn drop(&mut self) {
                crate::config::set_test_runtime_root_override(self.0.clone());
            }
        }
        let _guard = OverrideGuard(prev_override);

        let harness = TestHealthHarness::new_with_provider(ProviderKind::Codex).await;
        let shared = harness.shared();
        let provider = ProviderKind::Codex;

        let parent_channel_id = ChannelId::new(900_000_000_000_001);
        let thread_id = ChannelId::new(900_000_000_000_002);

        // 1. Wire up the parent → thread mapping (production sets this when
        //    a phase-gate dispatch opens).
        shared
            .dispatch_thread_parents
            .insert(parent_channel_id, thread_id);

        // 2. Pin the thread's mailbox as having an active turn (mirrors
        //    the on-disk inflight state convincing `mailbox_has_active_turn`
        //    that the dispatch is still live).
        let cancel_token = Arc::new(crate::services::provider::CancelToken::new());
        let started = shared
            .mailbox(thread_id)
            .try_start_turn(cancel_token.clone(), UserId::new(7), MessageId::new(8))
            .await;
        assert!(started, "test setup: mailbox must accept the active turn");
        assert!(
            shared.mailbox(thread_id).has_active_turn().await,
            "test setup: thread mailbox must report active turn"
        );

        // 3. Drop a stale inflight file on disk for the thread.
        let now_unix = chrono::Utc::now().timestamp();
        let stale_at = stale_local_updated_at(now_unix);
        seed_inflight_with_updated_at(&provider, thread_id.get(), "stale-tmux", &stale_at);

        // 4. THREAD-GUARD's stale check fires.
        let stale = super::thread_guard_inflight_is_stale(&provider, thread_id, now_unix);
        assert!(
            stale,
            "stale inflight (updated_at={stale_at}) must be classified as stale"
        );

        // 5. Force-clean and verify the three side-effects.
        super::thread_guard_force_clean_stale_thread(
            &shared,
            &provider,
            parent_channel_id,
            thread_id,
        )
        .await;

        assert!(
            !shared
                .dispatch_thread_parents
                .contains_key(&parent_channel_id),
            "parent → thread mapping must be cleared by force-clean"
        );
        assert!(
            super::super::super::inflight::load_inflight_state(&provider, thread_id.get())
                .is_none(),
            "stale inflight state file must be deleted by force-clean"
        );
        assert!(
            !shared.mailbox(thread_id).has_active_turn().await,
            "thread's mailbox active turn must be cancelled by force-clean (THREAD-GUARD will not re-queue subsequent bot messages)"
        );
    }

    /// #1446 Test 4 (regression guard) — when the thread's persisted
    /// inflight is FRESH, the THREAD-GUARD must NOT classify it as stale
    /// and must NOT touch any state. False-positive cleanup of a healthy
    /// long-running thread is much worse than slightly delayed recovery,
    /// so this test pins the safe-default contract.
    #[tokio::test]
    async fn thread_guard_preserves_active_thread_unchanged() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let prev_override = crate::config::current_test_runtime_root_override();
        crate::config::set_test_runtime_root_override(Some(temp.path().to_path_buf()));
        struct OverrideGuard(Option<std::path::PathBuf>);
        impl Drop for OverrideGuard {
            fn drop(&mut self) {
                crate::config::set_test_runtime_root_override(self.0.clone());
            }
        }
        let _guard = OverrideGuard(prev_override);

        let harness = TestHealthHarness::new_with_provider(ProviderKind::Codex).await;
        let shared = harness.shared();
        let provider = ProviderKind::Codex;

        let parent_channel_id = ChannelId::new(900_000_000_000_011);
        let thread_id = ChannelId::new(900_000_000_000_012);

        shared
            .dispatch_thread_parents
            .insert(parent_channel_id, thread_id);
        let cancel_token = Arc::new(crate::services::provider::CancelToken::new());
        let started = shared
            .mailbox(thread_id)
            .try_start_turn(cancel_token.clone(), UserId::new(7), MessageId::new(8))
            .await;
        assert!(started, "test setup: mailbox must accept the active turn");

        let fresh_at = fresh_local_updated_at();
        seed_inflight_with_updated_at(&provider, thread_id.get(), "fresh-tmux", &fresh_at);

        let now_unix = chrono::Utc::now().timestamp();
        assert!(
            !super::thread_guard_inflight_is_stale(&provider, thread_id, now_unix),
            "fresh inflight (updated_at={fresh_at}) must NOT be classified as stale"
        );

        // No call to `thread_guard_force_clean_stale_thread` happens in
        // production when the staleness check returns false. Verify all
        // pre-existing state is intact.
        assert!(
            shared
                .dispatch_thread_parents
                .contains_key(&parent_channel_id),
            "parent → thread mapping must survive a non-stale thread"
        );
        assert!(
            super::super::super::inflight::load_inflight_state(&provider, thread_id.get())
                .is_some(),
            "fresh inflight state file must survive"
        );
        assert!(
            shared.mailbox(thread_id).has_active_turn().await,
            "fresh thread's mailbox active turn must be preserved"
        );
    }

    /// Extra coverage for the missing-inflight branch — production lookup
    /// on `(provider, thread_id)` returns `None` when no state file exists,
    /// and the staleness helper must report `false` (nothing to clean).
    #[tokio::test]
    async fn thread_guard_treats_missing_inflight_as_not_stale() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let prev_override = crate::config::current_test_runtime_root_override();
        crate::config::set_test_runtime_root_override(Some(temp.path().to_path_buf()));
        struct OverrideGuard(Option<std::path::PathBuf>);
        impl Drop for OverrideGuard {
            fn drop(&mut self) {
                crate::config::set_test_runtime_root_override(self.0.clone());
            }
        }
        let _guard = OverrideGuard(prev_override);

        let _harness = TestHealthHarness::new_with_provider(ProviderKind::Codex).await;
        let provider = ProviderKind::Codex;
        let now_unix = chrono::Utc::now().timestamp();
        assert!(
            !super::thread_guard_inflight_is_stale(
                &provider,
                ChannelId::new(900_000_000_000_099),
                now_unix
            ),
            "missing inflight state must NOT be classified as stale"
        );
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod direct_session_tests {
    use super::*;

    fn session_with_path(path: Option<&str>) -> DiscordSession {
        DiscordSession {
            session_id: None,
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: path.map(str::to_string),
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: Some(1),
            channel_name: None,
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: 0,
            assistant_turns: 0,
        }
    }

    #[test]
    fn direct_session_routing_requires_nonblank_path() {
        assert!(!session_has_usable_path(None));
        assert!(!session_has_usable_path(Some(&session_with_path(None))));
        assert!(!session_has_usable_path(Some(&session_with_path(Some(
            "  "
        )))));
        assert!(session_has_usable_path(Some(&session_with_path(Some(
            "/Users/itismyfield"
        )))));
    }
}
