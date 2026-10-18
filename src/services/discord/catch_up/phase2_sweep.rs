use super::*;

// Run unanswered-message recovery after phase one has retained its barriers.
pub(super) async fn run<A: CatchUpDiscordApi + ?Sized>(
    context: &SweepContext<'_, A>,
    candidates: &BTreeMap<u64, CatchUpChannelCandidate>,
    state: &mut SweepState,
) {
    let CatchUpDeps {
        api,
        shared,
        provider,
        ..
    } = context.deps;
    let scan_wall_time = context.scan_wall_time;
    let scan_instant = context.scan_instant;
    let current_bot_user_id = context.current_bot_user_id;
    let SweepState {
        consumed_retry_states_this_cycle,
        incomplete_recent_channels,
        ttl_retired_channels,
        open_barriers,
    } = state;
    // Phase 2: Scan for unanswered messages since last bot response.
    // Catches messages that were queued in-memory but lost on restart. This
    // intentionally also scans configured channels that currently have no
    // checkpoint file, because `/clear` or stale-checkpoint pruning can leave
    // an otherwise valid channel without a disk cursor during a restart gap.
    let mut phase2_recovered = 0usize;
    let allowed_bot_ids_phase2: Vec<u64> = {
        let settings = shared.settings.read().await;
        settings.allowed_bot_ids.clone()
    };
    let (announce_resolution_phase2, notify_resolution_phase2) =
        api.utility_bot_user_ids(shared).await;

    // Same per-channel pacing as phase 1 (see `catch_up_scan_pace`).
    let mut paced_scan_phase2 = false;
    for candidate in candidates.values() {
        let channel_id = candidate.channel_id;

        if incomplete_recent_channels.contains(&channel_id)
            || ttl_retired_channels.contains(&channel_id)
        {
            continue;
        }

        {
            let settings = shared.settings.read().await;
            if !catch_up_candidate_allowed_for_bot(&settings, provider, candidate) {
                continue;
            }
        }

        if should_pace_before_scan(paced_scan_phase2) {
            catch_up_scan_pace_gap().await;
        }
        paced_scan_phase2 = true;

        match api.resolve_runtime_channel_binding_status(channel_id).await {
            RuntimeChannelBindingStatus::Owned => {}
            RuntimeChannelBindingStatus::Unowned | RuntimeChannelBindingStatus::Unknown => continue,
        }

        let Some(mut channel_scan) = try_channel_scan(shared, channel_id, None) else {
            continue;
        };
        // Fetch last 20 messages (newest first — default Discord order)
        let recent = match api
            .fetch_messages(channel_id, CatchUpFetchRequest::new(20))
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(target: "agentdesk::services::discord::catch_up",
                    "  [{ts}] ⚠ catch-up phase2: failed to fetch recent messages for channel {channel_id}: {e}"
                );
                if e.contains("Missing Access") || e.contains("Unknown Channel") {
                    if let Some(path) = candidate.checkpoint_path.as_ref() {
                        let _ = fs::remove_file(path);
                    }
                }
                continue;
            }
        };

        if recent.is_empty() {
            continue;
        }

        // Find the newest bot response (first bot message in newest-first order)
        let last_bot_idx = recent.iter().position(|m| {
            Some(m.author.id.get()) == current_bot_user_id && !m.content.trim().is_empty()
        });

        // Messages at indices 0..last_bot_idx are newer than the last bot response.
        let (last_bot_response_id, unanswered_slice) = match last_bot_idx {
            Some(0) => continue, // Latest message is from bot — nothing unanswered
            Some(idx) => (recent[idx].id.get(), &recent[..idx]),
            None => continue, // No bot response found — skip (new/inactive channel)
        };

        // Collect existing queue IDs for dedup from the same snapshot used for
        // capacity so the initial phase2 claim boundary is internally coherent.
        let mailbox = mailbox_snapshot(shared, channel_id).await;
        let remaining_capacity =
            catch_up_remaining_queue_capacity(mailbox.intervention_queue.len());
        // #5996: keep the arm that answered for each id — only the arm can say
        // whether a membership carries the evidence an advance must earn.
        let (mut known_arms, mut existing_ids) = recovery_known_arms_and_ids(&mailbox);
        // Phase 1's ledger consult, re-read per channel. A Settled outcome
        // skips (no enqueue): an answered message is never re-surfaced.
        let ledger = settled_ledger_consult::read(provider, channel_id);
        let settled_ids = ledger.settle(&mailbox, &mut known_arms, &mut existing_ids);
        let injected = inject_disposition::scan_view(provider);
        injected.merge(&mut known_arms, &mut existing_ids);
        injected.log_hits(
            "phase2",
            channel_id,
            unanswered_slice.iter().map(|m| m.id.get()),
        );
        let consumed = consumed_commands::read(provider, channel_id);
        let barrier = open_barriers.get(&channel_id).map(|r| r.barrier);
        let frontier = Phase2Frontier::new(barrier, last_bot_response_id);
        let live_checkpoint = shared.last_message_ids.get(&channel_id).map(|v| *v);
        let mut phase2_checkpoint = frontier.scan_checkpoint(live_checkpoint);
        let phase2_prior = catch_up_retry_prior(
            shared,
            channel_id,
            consumed_retry_states_this_cycle.get(&channel_id).copied(),
        );
        if let Some(hold) = phase2_prior.filter(|state| state.is_exhausted_hold()) {
            phase2_checkpoint = Some(hold.checkpoint);
        }
        let phase2_checkpoint_start = phase2_checkpoint;
        let phase2_age_reference =
            catch_up_message_age_reference_time(scan_wall_time, scan_instant, phase2_prior);
        let stats = Phase2RecoveryStats {
            returned: recent.len(),
            discovered: unanswered_slice.len(),
            ..Phase2RecoveryStats::default()
        };
        let phase2_retry_after: Option<u64> = None;

        let mut progress = RecoveryProgress {
            frontier,
            phase2_checkpoint,
            phase2_retry_after,
            stats,
        };
        recover_messages(
            context,
            channel_id,
            unanswered_slice,
            phase2_age_reference,
            phase2_prior,
            MessageEvidence {
                mailbox: &mailbox,
                remaining_capacity,
                known_arms: &known_arms,
                existing_ids: &mut existing_ids,
                settled_ids: &settled_ids,
                consumed: &consumed,
                allowed_bot_ids_phase2: &allowed_bot_ids_phase2,
                announce_resolution_phase2,
                notify_resolution_phase2,
                consumed_retry_states_this_cycle,
            },
            &mut progress,
        )
        .await;
        let RecoveryProgress {
            frontier,
            phase2_checkpoint,
            phase2_retry_after,
            stats,
        } = progress;

        if let Some(newest) = frontier.durable() {
            advance_last_message_checkpoint(shared, provider, channel_id, MessageId::new(newest));
        }
        channel_scan.settled(frontier.durable());
        frontier.retain_into(open_barriers, channel_id, phase2_checkpoint);
        finish_hold_scan(
            shared,
            channel_id,
            phase2_prior,
            frontier.durable(),
            open_barriers.get(&channel_id).copied(),
        );
        phase2_recovered += stats.enqueued;
        if stats.deferred > 0 && phase2_retry_after.is_none() {
            open_barriers
                .entry(channel_id)
                .and_modify(|r| r.exhausted = true);
        }

        if stats.enqueued > 0 {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(target: "agentdesk::services::discord::catch_up",
                "  [{ts}] 🔍 CATCH-UP phase2: recovered {} unanswered message(s) for channel {}",
                stats.enqueued,
                channel_id
            );
        }

        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(target: "agentdesk::services::discord::catch_up",
            "  [{ts}] 🔍 catch-up phase2 scan: channel={} returned={} discovered={} eligible={} duplicate={} skipped={} enqueued={} deferred={} checkpoint_start={:?} checkpoint_final={:?} retry_after={:?}",
            channel_id,
            stats.returned,
            stats.discovered,
            stats.eligible,
            stats.duplicate,
            stats.skipped,
            stats.enqueued,
            stats.deferred,
            phase2_checkpoint_start,
            phase2_checkpoint,
            phase2_retry_after,
        );
    }

    if phase2_recovered > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(target: "agentdesk::services::discord::catch_up",
            "  [{ts}] 🔍 CATCH-UP phase2: total {phase2_recovered} unanswered message(s) recovered"
        );
    }
}

struct MessageEvidence<'a> {
    mailbox: &'a ChannelMailboxSnapshot,
    remaining_capacity: usize,
    known_arms: &'a HashMap<u64, RecoveryKnownIdArm>,
    existing_ids: &'a mut HashSet<u64>,
    settled_ids: &'a HashSet<u64>,
    consumed: &'a consumed_commands::ConsumedCommands,
    allowed_bot_ids_phase2: &'a [u64],
    announce_resolution_phase2: health::UtilityBotUserIdResolution,
    notify_resolution_phase2: health::UtilityBotUserIdResolution,
    consumed_retry_states_this_cycle: &'a HashMap<ChannelId, CatchUpRetryState>,
}

struct RecoveryProgress {
    frontier: Phase2Frontier,
    phase2_checkpoint: Option<u64>,
    phase2_retry_after: Option<u64>,
    stats: Phase2RecoveryStats,
}

// Keep oldest-first message decisions together while updating the caller's frontier.
async fn recover_messages<A: CatchUpDiscordApi + ?Sized>(
    context: &SweepContext<'_, A>,
    channel_id: ChannelId,
    unanswered_slice: &[serenity::Message],
    phase2_age_reference: chrono::DateTime<chrono::Utc>,
    phase2_prior: Option<CatchUpRetryState>,
    evidence: MessageEvidence<'_>,
    progress: &mut RecoveryProgress,
) {
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
    // Iterate in reverse (oldest first) for chronological queue order
    for msg in unanswered_slice.iter().rev() {
        let mid = msg.id.get();
        let age_secs = match phase2_pre_gate(
            msg,
            current_bot_user_id,
            &*evidence.existing_ids,
            progress.phase2_checkpoint,
            phase2_age_reference,
        ) {
            Phase2PreGate::Skip => {
                progress.stats.skipped += 1;
                continue;
            }
            Phase2PreGate::Duplicate => {
                progress.stats.duplicate += 1;
                progress.phase2_checkpoint = progress.frontier.after_membership_skip(
                    progress.phase2_checkpoint,
                    evidence.known_arms,
                    mid,
                );
                continue;
            }
            Phase2PreGate::TooOld { age_secs } => {
                progress.stats.skipped += 1;
                log_catch_up_phase2_age_out(channel_id, msg.id, age_secs);
                if !phase2_prior.is_some_and(|state| state.hold_expired_at(scan_instant)) {
                    continue;
                }
                age_secs
            }
            Phase2PreGate::Proceed { age_secs } => age_secs,
        };
        let intervention_text = catch_up_intervention_text(msg.content.trim(), msg.author.bot);
        let utility_view = CatchUpMessageView {
            message_id: mid,
            author_id: msg.author.id.get(),
            author_is_bot: msg.author.bot,
            is_processable_kind: true,
            age_secs,
            trimmed_text: intervention_text.clone(),
        };
        match classify_catch_up_message_with_utility_resolution(
            &utility_view,
            current_bot_user_id,
            &*evidence.existing_ids,
            evidence.settled_ids,
            600,
            evidence.allowed_bot_ids_phase2,
            hold_utility_resolution(
                phase2_prior.is_some_and(|state| state.hold_expired_at(scan_instant))
                    && age_secs > 600,
                evidence.announce_resolution_phase2,
                (utility_view.author_is_bot && utility_view.trimmed_text.starts_with("DISPATCH:"))
                    .then_some(utility_view.author_id),
            ),
            hold_utility_resolution(
                phase2_prior.is_some_and(|state| state.hold_expired_at(scan_instant))
                    && age_secs > 600,
                evidence.notify_resolution_phase2,
                None,
            ),
            discord_io::author_authorized(shared, msg.author.id.get()).await,
        ) {
            CatchUpClassificationDecision::UtilityIdentityUnavailable => {
                progress.frontier.seal(mid);
                let retry_after = progress.frontier.retry_cursor(progress.phase2_checkpoint);
                progress.phase2_retry_after = rearm_catch_up_retry_after_defer(
                    shared,
                    channel_id,
                    retry_after,
                    evidence
                        .consumed_retry_states_this_cycle
                        .get(&channel_id)
                        .copied(),
                    clock,
                );
                progress.stats.deferred += 1;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(target: "agentdesk::services::discord::catch_up",
                    channel_id = channel_id.get(),
                    message_id = mid,
                    retry_after = progress.phase2_retry_after,
                    "  [{ts}] 🔁 catch-up phase2: utility bot identity unavailable; preserving checkpoint before ambiguous message"
                );
                break;
            }
            CatchUpClassificationDecision::Determinate(CatchUpClassification::TooOld) => {
                if !progress.frontier.can_durably_settle(mid)
                    || !record_exhausted_hold_expiry(api, shared, phase2_prior, msg)
                {
                    progress.frontier.seal(mid);
                    break;
                }
                progress.frontier.recovered(mid);
                progress.phase2_checkpoint =
                    advance_phase2_checkpoint(progress.phase2_checkpoint, mid);
                break;
            }
            CatchUpClassificationDecision::Determinate(CatchUpClassification::Recover)
                if !evidence.consumed.settles(&intervention_text, mid) => {}
            CatchUpClassificationDecision::Determinate(_) => {
                progress.stats.skipped += 1;
                continue;
            }
        }
        progress.stats.eligible += 1;
        debug_assert!(should_phase2_recover_message(
            mid,
            progress.phase2_checkpoint,
            &*evidence.existing_ids
        ));

        if progress.stats.enqueued >= evidence.remaining_capacity {
            progress.frontier.seal(mid);
            let retry_after = progress.frontier.retry_cursor(progress.phase2_checkpoint);
            let retry_after = arm_catch_up_retry_preserving_prior(
                shared,
                channel_id,
                retry_after,
                evidence
                    .consumed_retry_states_this_cycle
                    .get(&channel_id)
                    .copied(),
                clock,
            );
            progress.phase2_retry_after = Some(retry_after);
            progress.stats.deferred += 1;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(target: "agentdesk::services::discord::catch_up",
                "  [{ts}] 🔁 catch-up phase2: queue cap reached for channel {}; retry armed after checkpoint {}",
                channel_id,
                retry_after
            );
            break;
        }

        let queued_generation = crate::services::discord::runtime_store::process_generation();
        let source_generation = catch_up_source_generation(
            msg.id,
            queued_generation,
            msg.author.id.get(),
            msg.author.bot,
            evidence.allowed_bot_ids_phase2,
            evidence.announce_resolution_phase2,
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
            evidence.mailbox.claim_observation,
        );
        let enqueue = evidence
            .consumed
            .guard(enqueue, msg.id, &intervention_text)
            .await;
        match classify_phase2_enqueue_commit(&enqueue) {
            Phase2EnqueueCommit::Accepted => {
                evidence.existing_ids.insert(mid);
                progress.phase2_checkpoint =
                    advance_phase2_checkpoint(progress.phase2_checkpoint, mid);
                progress.frontier.recovered(mid);
                progress.stats.enqueued += 1;
            }
            // #5996/#6035: only the active-turn refusal advances (`FrontierEvidence`).
            commit @ (Phase2EnqueueCommit::DuplicateActiveTurn
            | Phase2EnqueueCommit::DuplicateQueued
            | Phase2EnqueueCommit::NotYetEvidenced) => {
                evidence.existing_ids.insert(mid);
                progress.phase2_checkpoint = progress.frontier.after_duplicate_commit(
                    commit,
                    progress.phase2_checkpoint,
                    mid,
                );
                progress.stats.duplicate += 1;
            }
            Phase2EnqueueCommit::LastItemDedup => {
                let snapshot = mailbox_snapshot(shared, channel_id).await;
                if catch_up_last_item_dedup_is_checkpoint_safe(
                    snapshot.intervention_queue.last(),
                    msg.id,
                ) {
                    evidence.existing_ids.insert(mid);
                    progress.phase2_checkpoint =
                        advance_phase2_checkpoint(progress.phase2_checkpoint, mid);
                    progress.stats.duplicate += 1;
                } else {
                    log_catch_up_enqueue_not_accepted("phase2", channel_id, msg.id, &enqueue);
                    progress.frontier.seal(mid);
                    let retry_after = progress.frontier.retry_cursor(progress.phase2_checkpoint);
                    progress.phase2_retry_after = rearm_catch_up_retry_after_defer(
                        shared,
                        channel_id,
                        retry_after,
                        // #4156: recover the budget phase-1 consumed this cycle
                        // (the pending map entry is gone by now); the helper
                        // merges it with any surviving map entry (most-exhausted).
                        evidence
                            .consumed_retry_states_this_cycle
                            .get(&channel_id)
                            .copied(),
                        clock,
                    );
                    progress.stats.deferred += 1;
                    break;
                }
            }
            Phase2EnqueueCommit::Deferred => {
                log_catch_up_enqueue_not_accepted("phase2", channel_id, msg.id, &enqueue);
                progress.frontier.seal(mid);
                let retry_after = progress.frontier.retry_cursor(progress.phase2_checkpoint);
                progress.phase2_retry_after = rearm_catch_up_retry_after_defer(
                    shared,
                    channel_id,
                    retry_after,
                    evidence
                        .consumed_retry_states_this_cycle
                        .get(&channel_id)
                        .copied(),
                    clock,
                );
                progress.stats.deferred += 1;
                break;
            }
        }
    }
}
