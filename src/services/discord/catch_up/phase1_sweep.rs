use std::ops::ControlFlow;

use super::*;

// Run the checkpoint phase before the unanswered-message phase shares its retry state.
pub(super) async fn run<A: CatchUpDiscordApi + ?Sized>(
    context: &SweepContext<'_, A>,
    candidates: &BTreeMap<u64, CatchUpChannelCandidate>,
    state: &mut SweepState,
) {
    let CatchUpDeps {
        api,
        shared,
        provider,
        retry_checkpoints,
        pending_retry_channels,
        ..
    } = context.deps;
    let SweepState {
        consumed_retry_states_this_cycle,
        incomplete_recent_channels,
        ttl_retired_channels,
        open_barriers,
    } = state;
    let mut total_recovered = 0usize;
    // Pace successive per-channel REST scans so a many-channel sweep doesn't
    // fire as one tight burst (see `catch_up_scan_pace`). The first eligible
    // channel runs immediately; subsequent scans wait the configured gap.
    let mut paced_scan = false;
    for candidate in candidates.values() {
        let channel_id = candidate.channel_id;

        // #429: skip channels this bot cannot access.  Utility bots
        // (notify/announce) share the claude provider checkpoint dir but
        // have no channel read permissions → every API call fails slowly.
        {
            let settings = shared.settings.read().await;
            if !catch_up_candidate_allowed_for_bot(&settings, provider, candidate) {
                continue;
            }
        }

        if should_pace_before_scan(paced_scan) {
            catch_up_scan_pace_gap().await;
        }

        let Some(mut channel_scan) = try_channel_scan(
            shared,
            channel_id,
            retry_checkpoints.get(&channel_id).copied(),
        ) else {
            incomplete_recent_channels.insert(channel_id);
            continue;
        };
        let retry_state = match consume_catch_up_retry_state_for_scan(
            shared,
            channel_id,
            retry_checkpoints,
            pending_retry_channels,
        ) {
            CatchUpRetryScanDecision::Proceed(retry_state) => retry_state,
            CatchUpRetryScanDecision::SkipConsumed => continue,
        };
        // #4156: remember the budget this scan consumed so phase-2 can carry it
        // forward even when phase-1 does not itself re-arm (which would leave the
        // pending map empty by phase-2 time).
        if let Some(state) = retry_state {
            consumed_retry_states_this_cycle.insert(channel_id, state);
        }
        paced_scan = true;
        let retry_checkpoint = retry_state.map(|state| state.checkpoint);
        let live_checkpoint = shared.last_message_ids.get(&channel_id).map(|entry| *entry);
        let fetch_mode = catch_up_fetch_mode_for_scan(candidate, live_checkpoint, retry_checkpoint);
        let scan_checkpoint = fetch_mode.checkpoint();

        match api.resolve_runtime_channel_binding_status(channel_id).await {
            RuntimeChannelBindingStatus::Owned => {}
            RuntimeChannelBindingStatus::Unowned => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(target: "agentdesk::services::discord::catch_up",
                    "  [{ts}] ⏭ catch-up: dropping stale checkpoint for unowned channel {} ({})",
                    channel_id,
                    candidate
                        .checkpoint_path
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "no checkpoint".to_string())
                );
                if let Some(path) = candidate.checkpoint_path.as_ref() {
                    let _ = fs::remove_file(path);
                }
                continue;
            }
            RuntimeChannelBindingStatus::Unknown => {
                // An unread Recent or held range cannot yield a newer phase-2 settlement.
                // Normal After scans retain their independent phase-2 behavior.
                if matches!(fetch_mode, CatchUpFetchMode::Recent)
                    || retry_state.is_some_and(|state| state.is_exhausted_hold())
                {
                    incomplete_recent_channels.insert(channel_id);
                }
                if let Some(retry_state) = retry_state {
                    rearm_catch_up_retry_after_fetch_failure(shared, channel_id, retry_state);
                }
                continue;
            }
        }

        let messages = match fetch_messages(
            context,
            candidate,
            fetch_mode,
            retry_state,
            incomplete_recent_channels,
        )
        .await
        {
            ControlFlow::Continue(messages) => messages,
            ControlFlow::Break(()) => continue,
        };

        // Get bot's own user ID to filter out self-messages
        // Collect existing message IDs in queue for dedup
        let known_snapshot = mailbox_snapshot(shared, channel_id).await;
        let (mut known_arms, mut existing_ids) = recovery_known_arms_and_ids(&known_snapshot);
        // The completed-turn ledger prevents DLQ for delivered ids and restores absorbed arms.
        let ledger = settled_ledger_consult::read(provider, channel_id);
        let settled_ids = ledger.settle(&known_snapshot, &mut known_arms, &mut existing_ids);
        let injected = inject_disposition::scan_view(provider);
        injected.merge(&mut known_arms, &mut existing_ids);
        injected.log_hits(
            "phase1",
            channel_id,
            messages.iter().map(|msg| msg.id.get()),
        );
        let consumed = consumed_commands::read(provider, channel_id);

        let allowed_bot_ids: Vec<u64> = {
            let settings = shared.settings.read().await;
            settings.allowed_bot_ids.clone()
        };
        let (announce_resolution, notify_resolution) = api.utility_bot_user_ids(shared).await;
        let announce_bot_id = announce_resolution.user_id();
        let notify_bot_id = notify_resolution.user_id();
        let (frontier, retry_exhausted, stats, actionable_too_old_count) = recover_messages(
            context,
            channel_id,
            retry_state,
            scan_checkpoint,
            &messages,
            MessageEvidence {
                known_snapshot: &known_snapshot,
                known_arms: &known_arms,
                existing_ids: &existing_ids,
                settled_ids: &settled_ids,
                consumed: &consumed,
                allowed_bot_ids: &allowed_bot_ids,
                announce_resolution,
                notify_resolution,
                announce_bot_id,
                notify_bot_id,
            },
            ttl_retired_channels,
        )
        .await;

        // #1227: emit a breakdown line for EVERY scanned channel — including
        // 0-recovery — so operator can distinguish "no missed messages" from
        // "limit too small" / "filter ate them" without re-reading the code.
        let ts = chrono::Local::now().format("%H:%M:%S");
        if stats.recovered > 0 {
            total_recovered += stats.recovered;
            tracing::info!(target: "agentdesk::services::discord::catch_up",
                "  [{ts}] 🔍 CATCH-UP: recovered {} message(s) for channel {} \
                 (returned={} self={} dup={} too_old={} settled={} empty={} not_allowed={} system={})",
                stats.recovered,
                channel_id,
                stats.returned,
                stats.self_authored,
                stats.duplicate,
                stats.too_old,
                stats.settled,
                stats.empty,
                stats.not_allowed,
                stats.system_kind,
            );
        } else {
            tracing::info!(target: "agentdesk::services::discord::catch_up",
                "  [{ts}] 🔍 catch-up scan: channel={} returned={} bot={} dup={} \
                 too_old={} settled={} empty={} not_allowed={} system={} recovered=0",
                channel_id,
                stats.returned,
                stats.self_authored,
                stats.duplicate,
                stats.too_old,
                stats.settled,
                stats.empty,
                stats.not_allowed,
                stats.system_kind,
            );
        }

        // Persist the contiguous settled frontier even when a retry is pending.
        // The current cap/defer item was deliberately not folded in, so this
        // retires permanent skips without crossing work that still needs retry.
        let retained = frontier.retained_barrier(scan_checkpoint, retry_exhausted);
        if let Some(newest) = frontier.newest() {
            advance_last_message_checkpoint(shared, provider, channel_id, MessageId::new(newest));
            if retry_checkpoint.is_some()
                && shared
                    .catch_up_retry_pending
                    .get(&channel_id)
                    .is_none_or(|state| state.is_exhausted_hold())
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                let outcome = match retained {
                    Some(retained) if retained.exhausted => {
                        format!("exhausted at barrier {}", retained.barrier)
                    }
                    Some(retained) => format!("retained at barrier {}", retained.barrier),
                    None => "completed".to_owned(),
                };
                tracing::info!(target: "agentdesk::services::discord::catch_up",
                    "  [{ts}] 🔁 catch-up: retry {outcome} for channel {} at checkpoint {}",
                    channel_id,
                    newest
                );
            }
        }

        channel_scan.settled(frontier.newest());
        finish_hold_scan(shared, channel_id, retry_state, frontier.newest(), retained);
        if actionable_too_old_count > 0 {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(target: "agentdesk::services::discord::catch_up",
                channel_id = channel_id.get(),
                too_old = actionable_too_old_count,
                "  [{ts}] ⚠ catch-up: dropped too-old message(s)"
            );
        }
        if let Some(retained) = retained {
            open_barriers.insert(channel_id, retained);
        }
    }

    if total_recovered > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(target: "agentdesk::services::discord::catch_up",
            "  [{ts}] 🔍 CATCH-UP: total {total_recovered} message(s) recovered across channels"
        );
    }
}

// Fetch a complete channel range while returning the caller's channel-skip decision.
async fn fetch_messages<A: CatchUpDiscordApi + ?Sized>(
    context: &SweepContext<'_, A>,
    candidate: &CatchUpChannelCandidate,
    fetch_mode: CatchUpFetchMode,
    retry_state: Option<CatchUpRetryState>,
    incomplete_recent_channels: &mut HashSet<ChannelId>,
) -> ControlFlow<(), Vec<serenity::Message>> {
    let api = context.deps.api;
    let shared = context.deps.shared;
    let scan_wall_time = context.scan_wall_time;
    let max_age = context.max_age;
    let channel_id = candidate.channel_id;
    // Fetch messages after the saved cursor when one exists. Configured
    // channels can legitimately have no last_message checkpoint yet (for
    // example after `/clear` or stale-checkpoint pruning), so fall back to a
    // bounded recent-message scan instead of silently skipping the channel.
    // #1227: limit was 10 — channels with bursty bot activity (streaming
    // replies + many short turns) routinely fill that window with bot
    // messages, pushing user messages outside the page. Discord applies
    // `limit` BEFORE author filtering; 50 keeps the single-page contract with
    // headroom for the realistic
    // bot:user ratio. Discord per-channel rate limit (5 req / 5 sec)
    // has plenty of margin for this 5x cost.
    let mut request = CatchUpFetchRequest::new(CATCH_UP_FETCH_LIMIT);
    if let CatchUpFetchMode::After(last_id) = fetch_mode {
        request = request.after(last_id);
    }
    let mut messages = match api.fetch_messages(channel_id, request).await {
        Ok(msgs) => msgs,
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(target: "agentdesk::services::discord::catch_up",
                "  [{ts}] ⚠ catch-up: failed to fetch messages for channel {channel_id}: {e}"
            );
            // A failed first page leaves the Recent or held range unproven.
            // Phase 2 must not replace that gap with a newer last-20 frontier.
            if matches!(fetch_mode, CatchUpFetchMode::Recent)
                || retry_state.is_some_and(|state| state.is_exhausted_hold())
            {
                incomplete_recent_channels.insert(channel_id);
            }
            // #429: permanent errors — remove checkpoint to avoid retrying every restart
            if e.contains("Missing Access") || e.contains("Unknown Channel") {
                if let Some(path) = candidate.checkpoint_path.as_ref() {
                    let _ = fs::remove_file(path);
                }
            } else if let Some(retry_state) = retry_state {
                rearm_catch_up_retry_after_fetch_failure(shared, channel_id, retry_state);
            }
            return ControlFlow::Break(());
        }
    };

    if matches!(fetch_mode, CatchUpFetchMode::After(_))
        && retry_state.is_some_and(|state| state.is_exhausted_hold())
    {
        messages = match held_range::complete(api, channel_id, messages).await {
            Ok(messages) => messages,
            Err(error) => {
                incomplete_recent_channels.insert(channel_id);
                tracing::warn!(target: "agentdesk::services::discord::catch_up",%channel_id, %error, "catch-up: held range unread; preserving hold");
                return ControlFlow::Break(());
            }
        };
    }

    // #3668 F3: `Recent` mode (no checkpoint) has no lower bound, so a single
    // newest-first page can be fully consumed by newer bot/system noise and
    // bury an age-window-eligible user message past page 1. Page backward
    // with `.before(oldest)` up to `CATCH_UP_RECENT_MAX_PAGES`, stopping as
    // soon as a page's oldest message exceeds the age window. The `After`
    // path keeps its precise single-page contract and is left untouched.
    let mut fetch_completeness = CatchUpFetchCompleteness::Complete;
    if matches!(fetch_mode, CatchUpFetchMode::Recent) {
        let mut pages_fetched: u8 = 1;
        loop {
            // The oldest message currently held is the smallest id (Discord
            // returns newest-first; ids are time-ordered snowflakes).
            let Some(oldest) = messages.iter().min_by_key(|msg| msg.id.get()) else {
                break;
            };
            let oldest_id = oldest.id;
            let oldest_age_secs = scan_wall_time
                .signed_duration_since(*oldest_id.created_at())
                .num_seconds();
            match recent_page_decision(
                pages_fetched,
                Some(oldest_age_secs),
                max_age.as_secs() as i64,
            ) {
                RecentPageDecision::Complete => break,
                RecentPageDecision::IncompleteBudget => {
                    fetch_completeness = CatchUpFetchCompleteness::Incomplete;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(target: "agentdesk::services::discord::catch_up",
                        "  [{ts}] ⚠ catch-up: Recent pagination budget exhausted for channel {channel_id}; preserving the unbounded gap"
                    );
                    break;
                }
                RecentPageDecision::FetchOlder => {}
            }
            if should_pace_before_scan(true) {
                catch_up_scan_pace_gap().await;
            }
            let older_request =
                CatchUpFetchRequest::new(CATCH_UP_FETCH_LIMIT).before(oldest_id.get());
            match api.fetch_messages(channel_id, older_request).await {
                Ok(mut older) if !older.is_empty() => {
                    // A non-advancing/overlapping REST page is not proof that
                    // the unbounded Recent gap is complete. Keep only ids
                    // below the requested cursor and retry on a later sweep
                    // if the cursor did not move at all.
                    older.retain(|message| message.id.get() < oldest_id.get());
                    if older.is_empty() {
                        fetch_completeness = CatchUpFetchCompleteness::Incomplete;
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(target: "agentdesk::services::discord::catch_up",
                            "  [{ts}] ⚠ catch-up: Recent pagination cursor did not advance for channel {channel_id}; preserving the unbounded gap"
                        );
                        break;
                    }
                    pages_fetched += 1;
                    messages.extend(older);
                }
                Ok(_) => break, // no older messages → done
                Err(e) => {
                    fetch_completeness = CatchUpFetchCompleteness::Incomplete;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(target: "agentdesk::services::discord::catch_up",
                        "  [{ts}] ⚠ catch-up: backward page fetch failed for channel {channel_id}: {e}"
                    );
                    break;
                }
            }
        }
    }

    // A partial newest-first Recent prefix is not contiguous with the
    // unknown lower bound. Processing it could durably checkpoint terminal
    // noise past an unseen recoverable message, so leave the whole batch to
    // the periodic Recent backstop instead.
    if fetch_completeness == CatchUpFetchCompleteness::Incomplete {
        incomplete_recent_channels.insert(channel_id);
        return ControlFlow::Break(());
    }

    if messages.is_empty() {
        return ControlFlow::Break(());
    }
    messages.sort_by_key(|msg| msg.id.get());
    ControlFlow::Continue(messages)
}

struct MessageEvidence<'a> {
    known_snapshot: &'a ChannelMailboxSnapshot,
    known_arms: &'a HashMap<u64, RecoveryKnownIdArm>,
    existing_ids: &'a HashSet<u64>,
    settled_ids: &'a HashSet<u64>,
    consumed: &'a consumed_commands::ConsumedCommands,
    allowed_bot_ids: &'a [u64],
    announce_resolution: health::UtilityBotUserIdResolution,
    notify_resolution: health::UtilityBotUserIdResolution,
    announce_bot_id: Option<u64>,
    notify_bot_id: Option<u64>,
}

// Recover one oldest-first batch without crossing its first unresolved frontier.
async fn recover_messages<A: CatchUpDiscordApi + ?Sized>(
    context: &SweepContext<'_, A>,
    channel_id: ChannelId,
    retry_state: Option<CatchUpRetryState>,
    scan_checkpoint: Option<u64>,
    messages: &[serenity::Message],
    evidence: MessageEvidence<'_>,
    ttl_retired_channels: &mut HashSet<ChannelId>,
) -> (SettledFrontier, bool, CatchUpScanStats, usize) {
    let CatchUpDeps {
        api,
        shared,
        provider,
        clock,
        ..
    } = context.deps;
    let scan_wall_time = context.scan_wall_time;
    let scan_instant = context.scan_instant;
    let current_bot_user_id = context.current_bot_user_id;
    let max_age = context.max_age;
    // Newest id this oldest-first scan durably settled; the first message it
    // leaves open (un-evidenced duplicate, cap, defer) seals it for the scan.
    let mut frontier = SettledFrontier::default();
    let mut retry_exhausted = false;
    let mut stats = CatchUpScanStats::default();
    stats.returned = messages.len();
    // Keep the human-drop warning count separate from all TooOld DLQ records.
    let mut actionable_too_old_count = 0usize;

    // Codex P2 on #1301: the 50-message fetch can exceed
    // `MAX_INTERVENTIONS_PER_CHANNEL` (30) on a long restart gap. Without
    // a cap `enqueue_intervention` would silently supersede older
    // queued entries while catch-up still advances the checkpoint to the
    // newest recovered id — meaning the evicted messages are lost. Cap
    // recovery to the queue's remaining capacity at scan-start; the
    // overflow stays unrecovered with the OLD checkpoint, so the next
    // catch-up cycle picks it up from the same `after` cursor.
    let queue_initial_len = mailbox_snapshot(shared, channel_id)
        .await
        .intervention_queue
        .len();
    let remaining_capacity = catch_up_remaining_queue_capacity(queue_initial_len);

    for msg in messages {
        let text = msg.content.trim().to_string();
        let intervention_text = catch_up_intervention_text(&text, msg.author.bot);
        let msg_ts = msg.id.created_at();
        let age_reference =
            catch_up_message_age_reference_time(scan_wall_time, scan_instant, retry_state);
        let age_secs = age_reference.signed_duration_since(*msg_ts).num_seconds();
        let view = CatchUpMessageView {
            message_id: msg.id.get(),
            author_id: msg.author.id.get(),
            author_is_bot: msg.author.bot,
            is_processable_kind: router::should_process_turn_message(msg.kind),
            age_secs,
            trimmed_text: intervention_text.clone(),
        };
        let outcome = match classify_catch_up_message_with_utility_resolution(
            &view,
            current_bot_user_id,
            evidence.existing_ids,
            evidence.settled_ids,
            max_age.as_secs() as i64,
            evidence.allowed_bot_ids,
            hold_utility_resolution(
                retry_state.is_some_and(|state| state.hold_expired_at(scan_instant))
                    && age_secs > max_age.as_secs() as i64,
                evidence.announce_resolution,
                (view.author_is_bot && view.trimmed_text.starts_with("DISPATCH:"))
                    .then_some(view.author_id),
            ),
            hold_utility_resolution(
                retry_state.is_some_and(|state| state.hold_expired_at(scan_instant))
                    && age_secs > max_age.as_secs() as i64,
                evidence.notify_resolution,
                None,
            ),
            discord_io::author_authorized(shared, msg.author.id.get()).await,
        ) {
            CatchUpClassificationDecision::Determinate(outcome) => outcome,
            CatchUpClassificationDecision::UtilityIdentityUnavailable => {
                let mid = msg.id.get();
                frontier.seal(mid);
                let retry_after = rearm_catch_up_retry_after_defer(
                    shared,
                    channel_id,
                    frontier.retry_after(scan_checkpoint, mid),
                    retry_state,
                    clock,
                );
                retry_exhausted |= retry_after.is_none();
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(target: "agentdesk::services::discord::catch_up",
                    channel_id = channel_id.get(),
                    message_id = mid,
                    retry_after,
                    "  [{ts}] 🔁 catch-up: utility bot identity unavailable; preserving checkpoint before ambiguous message"
                );
                break;
            }
        };
        let mid = msg.id.get();
        // A replied command was consumed live; replaying it would prompt the provider.
        let command =
            evidence
                .consumed
                .evidence(&intervention_text, msg.id, messages, current_bot_user_id);
        let outcome = match (outcome, command) {
            (CatchUpClassification::Recover, TextCommandEvidence::Consumed) => {
                CatchUpClassification::Settled
            }
            (CatchUpClassification::Recover, TextCommandEvidence::ReplierUnknown) => {
                let retry_after = defer_unrecognized_command(&mut frontier, scan_checkpoint, mid);
                retry_exhausted |= rearm_catch_up_retry_after_defer(
                    shared,
                    channel_id,
                    retry_after,
                    retry_state,
                    clock,
                )
                .is_none();
                break;
            }
            (outcome, _) => outcome,
        };
        // Check the cap before counting a recover so refused work is not tallied.
        // Keep the checkpoint at the last queued message so newer ids stay retryable.
        if outcome == CatchUpClassification::Recover && stats.recovered >= remaining_capacity {
            frontier.seal(mid);
            let retry_after = frontier.retry_after(scan_checkpoint, mid);
            let retry_after = arm_catch_up_retry_preserving_prior(
                shared,
                channel_id,
                retry_after,
                retry_state,
                clock,
            );
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(target: "agentdesk::services::discord::catch_up",
                "  [{ts}] 🔁 catch-up: queue cap reached for channel {}; retry armed after checkpoint {}",
                channel_id,
                retry_after
            );
            break;
        }
        if outcome != CatchUpClassification::Recover {
            if outcome == CatchUpClassification::TooOld {
                let hold_expired =
                    retry_state.is_some_and(|state| state.hold_expired_at(scan_instant));
                if hold_expired {
                    if frontier.retained_barrier(scan_checkpoint, false).is_some()
                        || !record_exhausted_hold_expiry(api, shared, retry_state, msg)
                    {
                        frontier.seal(mid);
                        break;
                    }
                    ttl_retired_channels.insert(channel_id);
                } else {
                    // Preserve every TooOld input as detached DLQ evidence.
                    let _ = api.record_too_old_dead_letter(
                        shared.pg_pool.as_ref(),
                        crate::db::relay_dead_letter::RelayDeadLetterRecord {
                            kind: crate::db::relay_dead_letter::KIND_CATCH_UP_TOO_OLD.to_string(),
                            channel_id: channel_id.to_string(),
                            author_id: Some(msg.author.id.get().to_string()),
                            message_id: Some(mid.to_string()),
                            content: text.clone(),
                            reason: format!(
                                "age_secs={age_secs} > max_age_secs={}",
                                max_age.as_secs()
                            ),
                        },
                    );
                }
                if too_old_is_actionable(
                    outcome,
                    msg.author.id.get(),
                    msg.author.bot,
                    evidence.allowed_bot_ids,
                    evidence.announce_bot_id,
                    evidence.notify_bot_id,
                ) {
                    actionable_too_old_count += 1;
                }
            }
            frontier.record_skipped(mid, outcome, evidence.known_arms.get(&mid).copied());
            stats.record(outcome);
            if ttl_retired_channels.contains(&channel_id) {
                break;
            }
            continue;
        }

        let queued_generation = crate::services::discord::runtime_store::process_generation();
        let source_generation = catch_up_source_generation(
            msg.id,
            queued_generation,
            msg.author.id.get(),
            msg.author.bot,
            evidence.allowed_bot_ids,
            evidence.announce_resolution,
        );
        let enqueue = A::enqueue_intervention(
            api,
            shared,
            provider,
            channel_id,
            Intervention {
                author_id: msg.author.id,
                author_is_bot: msg.author.bot,
                message_id: msg.id,
                queued_generation,
                source_message_ids: vec![msg.id],
                source_message_queued_generations: vec![source_generation],
                source_text_segments: Vec::new(),
                text: intervention_text.clone(),
                mode: InterventionMode::Soft,
                created_at: catch_up_intervention_created_at(scan_wall_time, scan_instant, msg.id),
                reply_context: None,
                has_reply_boundary: msg.message_reference.is_some(),
                merge_consecutive: !msg.author.bot
                    && !intervention_text.starts_with('!')
                    && !intervention_text.starts_with('/')
                    && !intervention_text.starts_with("DISPATCH:"),
                pending_uploads: Vec::new(),
                voice_announcement: None,
            },
            evidence.known_snapshot.claim_observation,
        );
        let enqueue = evidence
            .consumed
            .guard(enqueue, msg.id, &intervention_text)
            .await;
        match classify_phase2_enqueue_commit(&enqueue) {
            Phase2EnqueueCommit::Accepted => {
                stats.record(CatchUpClassification::Recover);
                api.cleanup_recovered_catch_up_hourglass(shared, channel_id, msg.id)
                    .await;
                frontier.settle(mid);
            }
            // Only the active-turn refusal is dispatch evidence (#6035); the rest seal.
            commit @ (Phase2EnqueueCommit::DuplicateActiveTurn
            | Phase2EnqueueCommit::DuplicateQueued
            | Phase2EnqueueCommit::NotYetEvidenced) => {
                stats.record(CatchUpClassification::Duplicate);
                frontier.record_duplicate_commit(mid, commit);
            }
            Phase2EnqueueCommit::LastItemDedup => {
                let snapshot = mailbox_snapshot(shared, channel_id).await;
                if catch_up_last_item_dedup_is_checkpoint_safe(
                    snapshot.intervention_queue.last(),
                    msg.id,
                ) {
                    stats.record(CatchUpClassification::Duplicate);
                    frontier.settle(mid);
                } else {
                    log_catch_up_enqueue_not_accepted("phase1", channel_id, msg.id, &enqueue);
                    frontier.seal(mid);
                    let retry_after = frontier.retry_after(scan_checkpoint, mid);
                    retry_exhausted |= rearm_catch_up_retry_after_defer(
                        shared,
                        channel_id,
                        retry_after,
                        retry_state,
                        clock,
                    )
                    .is_none();
                    break;
                }
            }
            Phase2EnqueueCommit::Deferred => {
                log_catch_up_enqueue_not_accepted("phase1", channel_id, msg.id, &enqueue);
                frontier.seal(mid);
                let retry_after = frontier.retry_after(scan_checkpoint, mid);
                retry_exhausted |= rearm_catch_up_retry_after_defer(
                    shared,
                    channel_id,
                    retry_after,
                    retry_state,
                    clock,
                )
                .is_none();
                break;
            }
        }
    }
    (frontier, retry_exhausted, stats, actionable_too_old_count)
}
