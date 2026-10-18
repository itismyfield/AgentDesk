//! Stall watchdog passes and explicit background cleanup.

use super::*;

use crate::services::discord::relay_recovery::AxisBSite;
use crate::services::discord::turn_view_reconciler::note_intake_turn_cleared_via_shared as tv_clear;

pub(super) fn stop_hard_stop_cleanup_watcher_if_current(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    pinned_tmux_session_name: Option<&str>,
) -> bool {
    let Some(pinned_tmux_session_name) = pinned_tmux_session_name else {
        tracing::info!(
            target: "agentdesk::services::discord::health::recovery",
            channel_id = channel_id.get(),
            "STALL-WATCHDOG: skipped hard-stop watcher cleanup because pinned tmux session is missing"
        );
        return false;
    };

    let guard = discord::lock_tmux_watcher_registry();
    let current_tmux_session_name = shared
        .tmux_watchers
        .channel_binding(&channel_id)
        .map(|binding| binding.tmux_session_name);
    if current_tmux_session_name.as_deref() != Some(pinned_tmux_session_name) {
        tracing::info!(
            target: "agentdesk::services::discord::health::recovery",
            channel_id = channel_id.get(),
            pinned_tmux_session = pinned_tmux_session_name,
            current_tmux_session = current_tmux_session_name.as_deref(),
            "STALL-WATCHDOG: skipped hard-stop watcher cleanup because channel watcher identity changed"
        );
        return false;
    }
    let Some((_, watcher)) = shared.tmux_watchers.remove_locked(&guard, &channel_id) else {
        return false;
    };
    watcher.cancel.store(true, Ordering::Relaxed);
    true
}

pub(super) async fn cleanup_then_submit_explicit_background_watchdog_cancel<Submit, SubmitFuture>(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    revalidated: &ExplicitBackgroundWatchdogClear,
    pinned_tmux_session_name: Option<&str>,
    submit_terminal: Submit,
) -> bool
where
    Submit: FnOnce() -> SubmitFuture,
    SubmitFuture: std::future::Future<Output = discord::turn_finalizer::FinalizeOutcome>,
{
    debug_assert_eq!(
        revalidated.clear_outcome,
        discord::inflight::GuardedClearOutcome::Cleared
    );
    let owned_role_override =
        discord::turn_finalizer::cleanup::snapshot_role_override(shared, channel_id);
    let pre_release_has_pending = !shared
        .mailbox(channel_id)
        .snapshot()
        .await
        .intervention_queue
        .is_empty();
    let _runtime_session_cleared = apply_runtime_hard_stop_finalizer_cleanup_pre_release(
        shared,
        provider,
        channel_id,
        pre_release_has_pending,
        owned_role_override,
        pinned_tmux_session_name,
        true,
    )
    .await;
    let outcome = submit_terminal().await;
    match outcome {
        discord::turn_finalizer::FinalizeOutcome::Finalized { has_pending, .. } => has_pending,
        discord::turn_finalizer::FinalizeOutcome::AlreadyFinalized
        | discord::turn_finalizer::FinalizeOutcome::Deferred => pre_release_has_pending,
    }
}

#[derive(Debug, Clone)]
pub(super) struct ExplicitBackgroundWatchdogClear {
    pub(super) clear_outcome: discord::inflight::GuardedClearOutcome,
    pub(super) pending_hourglass_user_msg_id: Option<u64>,
    pub(super) finalizer_turn_id: u64,
    pub(super) claim_snapshot: Option<discord::turn_finalizer::SyntheticClaimSnapshot>,
}

pub(super) fn revalidate_and_clear_explicit_background_inflight(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot_identity: Option<&discord::inflight::InflightTurnIdentity>,
    snapshot_finalizer_turn_id: Option<u64>,
) -> ExplicitBackgroundWatchdogClear {
    let Some(snapshot_identity) = snapshot_identity else {
        return ExplicitBackgroundWatchdogClear {
            clear_outcome: discord::inflight::GuardedClearOutcome::Missing,
            pending_hourglass_user_msg_id: None,
            finalizer_turn_id: 0,
            claim_snapshot: None,
        };
    };
    let finalizer_turn_id = snapshot_finalizer_turn_id.unwrap_or(snapshot_identity.user_msg_id);
    if finalizer_turn_id == 0 {
        return ExplicitBackgroundWatchdogClear {
            clear_outcome: discord::inflight::GuardedClearOutcome::UserMsgMismatch,
            pending_hourglass_user_msg_id: None,
            finalizer_turn_id,
            claim_snapshot: None,
        };
    }
    let (clear_outcome, removed_row) =
        discord::inflight::clear_inflight_state_if_matches_identity_returning_row(
            provider,
            channel_id.get(),
            snapshot_identity,
        );
    let claim_snapshot = removed_row
        .as_ref()
        .map(discord::turn_finalizer::SyntheticClaimSnapshot::from_row);
    ExplicitBackgroundWatchdogClear {
        clear_outcome,
        pending_hourglass_user_msg_id: (snapshot_identity.user_msg_id != 0)
            .then_some(snapshot_identity.user_msg_id),
        finalizer_turn_id,
        claim_snapshot,
    }
}

/// Run a single stall-watchdog pass against one provider+SharedData.
///
/// Iterates every attached watcher and cleans channels whose snapshot satisfies
/// the watchdog predicates. Returns the number of channels cleaned this pass.
pub(crate) async fn run_stall_watchdog_pass(
    registry: &HealthRegistry,
    provider: &ProviderKind,
) -> usize {
    let now_unix_secs = chrono::Utc::now().timestamp();
    stall_liveness::gc_stall_watchdog_liveness_state(now_unix_secs);
    watcher_respawn::gc_watcher_absence_state(now_unix_secs);
    let recovering = live_agent_recovery::reconcile_provider(registry, provider).await;

    // Sweep every same-provider runtime; name-only lookup would miss later
    // bots, so keep the runtime that exposed each watcher.
    let runtimes = registry.all_shared_for_provider(provider).await;
    if runtimes.is_empty() {
        return 0;
    }
    let mut candidate_channels: Vec<(ChannelId, Arc<SharedData>)> = Vec::new();
    let mut seen: std::collections::HashSet<ChannelId> = std::collections::HashSet::new();
    for runtime in &runtimes {
        let runtime_channels: Vec<ChannelId> = runtime
            .tmux_watchers
            .iter()
            .filter_map(|entry| {
                runtime
                    .tmux_watchers
                    .owner_channel_for_tmux_session(entry.key())
            })
            .collect();
        for channel_id in runtime_channels {
            if seen.insert(channel_id) {
                candidate_channels.push((channel_id, runtime.clone()));
            }
        }
    }
    // #3410 P1-a: no early return on empty candidates; trailing retry/dead-man
    // are keyed on watcher absence, not live-watcher candidates.
    let mut cleaned = 0usize;
    for (channel_id, shared) in candidate_channels {
        // Use the selected runtime so same-provider multi-bot snapshots target
        // the bot that owns this watcher.
        let snapshot = match registry
            .snapshot_watcher_state_for_shared(provider, shared.clone(), channel_id.get())
            .await
        {
            Some(snapshot) => snapshot,
            None => continue,
        };
        // A successful takeover owns this tick; the snapshot is now stale.
        if recovering.contains(&channel_id.get())
            || live_agent_recovery::observe_and_execute(registry, &snapshot).await
        {
            continue;
        }
        let now_mono_secs = super::super::liveness_authority::monotonic_now_secs();
        let tick_inflight = discord::inflight::load_inflight_state(provider, channel_id.get());
        let capture_assessment = super::super::liveness_authority::observe_and_publish_from_tick(
            provider,
            channel_id,
            &snapshot,
            tick_inflight.as_ref(),
            now_unix_secs,
            now_mono_secs,
        );
        let reattach_lane = relay_dead_reattach::try_apply(
            registry,
            shared.clone(),
            provider,
            channel_id,
            &snapshot,
            now_unix_secs,
        )
        .await;
        #[cfg(test)]
        retirement_await_tests::hold_reattach_return(channel_id.get()).await;
        if reattach_lane.handled_tick() {
            // A reused live incumbent owns the tick without repairing anything,
            // so it stops the remaining branches but adds nothing to `cleaned`.
            if reattach_lane.counts_as_cleaned() {
                cleaned += 1;
            }
            continue;
        }
        // #3668 F2: if JSONL still holds an unrelayed final answer after
        // `last_offset`, skip destructive watchdog branches this tick.
        if let Some(state) = discord::inflight::load_inflight_state(provider, channel_id.get())
            && crate::services::discord::relay_recovery::idle_tmux_repair_has_unrelayed_tail_answer(
                &state,
            )
        {
            continue;
        }
        // #2965: a ready-for-input TUI can still look "desynced" when the
        // capture file has bytes past relay offsets. Prefer the idle-safe
        // anchor cleanup before the destructive desynced force-clean branch.
        if stale_idle_foreground_queue_detected(
            snapshot.relay_health.active_turn,
            snapshot.relay_health.mailbox_has_cancel_token,
            snapshot.relay_health.queue_depth,
            snapshot.inflight_state_present,
            snapshot.inflight_updated_at.as_deref(),
            snapshot.tmux_session_alive,
            snapshot.relay_health.last_outbound_activity_ms,
            now_unix_secs,
            STALL_WATCHDOG_THRESHOLD_SECS,
        ) && let Some(tmux_session) = snapshot.tmux_session.clone()
            // #3169: same loop-mid-write liveness guard as the desynced force-
            // clean below — a freshly-written jsonl means this idle-foreground
            // anchor is a live loop turn, not a stranded one, so do not clear it.
            && !stall_liveness::stall_watchdog_jsonl_liveness_defers_force_clean(
                crate::services::dispatched_sessions::latest_runtime_activity_unix_nanos(
                    &tmux_session,
                ),
                now_unix_secs,
                STALL_WATCHDOG_LIVENESS_FRESHNESS_SECS,
            )
            && watchdog_decisions::idle_tmux_repair_ready_for_input(
                provider,
                channel_id.get(),
                &tmux_session,
            )
            && discord::inflight_state_allows_idle_tmux_repair_for_channel(
                provider,
                channel_id.get(),
            )
            .unwrap_or(false)
            && watchdog_axis_b_warrants(provider, &snapshot, AxisBSite::WatchdogStaleIdle)
        {
            let Some(result) = clear_idle_tmux_stale_turn(
                registry,
                provider.as_str(),
                channel_id.get(),
                &tmux_session,
                "2965_stale_idle_foreground_queue_watchdog",
            )
            .await
            else {
                continue;
            };
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                target: "agentdesk::services::discord::health::recovery",
                "  [{ts}] ⚡ STALL-WATCHDOG: cleared idle foreground TUI turn for channel {} (provider={}, pending_queue={}, inflight_cleared={})",
                channel_id,
                provider.as_str(),
                result.has_pending_queue,
                result.persistent_inflight_cleared
            );
            cleaned += 1;
            continue;
        }

        if stall_watchdog_should_force_clean_orphan_explicit_background_work(
            snapshot.relay_stall_state,
            snapshot.attached,
            snapshot.watcher_owner_channel_id,
            channel_id.get(),
            snapshot.desynced,
            snapshot.inflight_state_present,
            snapshot.inflight_updated_at.as_deref(),
            snapshot.tmux_session_alive,
            snapshot.unread_bytes,
            snapshot.relay_health.last_outbound_activity_ms,
            now_unix_secs,
            STALL_WATCHDOG_THRESHOLD_SECS,
        ) && watchdog_axis_b_warrants(provider, &snapshot, AxisBSite::WatchdogExplicitBackground)
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                target: "agentdesk::services::discord::health::recovery",
                "  [{ts}] ⚡ STALL-WATCHDOG: forced cleanup for orphan explicit background work in channel {}",
                channel_id
            );
            let revalidated = revalidate_and_clear_explicit_background_inflight(
                provider,
                channel_id,
                snapshot.inflight_identity.as_ref(),
                snapshot.inflight_finalizer_turn_id,
            );
            if revalidated.clear_outcome != discord::inflight::GuardedClearOutcome::Cleared {
                tracing::info!(
                    target: "agentdesk::services::discord::health::recovery",
                    provider = %provider.as_str(),
                    channel_id = channel_id.get(),
                    clear_outcome = ?revalidated.clear_outcome,
                    "STALL-WATCHDOG: skipped orphan explicit background cleanup because inflight identity changed before the destructive clear"
                );
                continue;
            }
            let finalizer_turn_id = revalidated.finalizer_turn_id;
            let generation = shared.restart.current_generation;
            let shared_for_submit = shared.clone();
            let provider_for_submit = provider.clone();
            let claim_snapshot = revalidated.claim_snapshot.clone();
            let has_pending = cleanup_then_submit_explicit_background_watchdog_cancel(
                &shared,
                provider,
                channel_id,
                &revalidated,
                snapshot.tmux_session.as_deref(),
                move || async move {
                    shared_for_submit
                        .turn_finalizer
                        .submit_terminal_with_claim_snapshot(
                            discord::turn_finalizer::TurnKey::new(
                                channel_id,
                                finalizer_turn_id,
                                generation,
                            ),
                            provider_for_submit,
                            discord::turn_finalizer::TerminalEvent::Cancel,
                            discord::turn_finalizer::FinalizeContext::monitor(),
                            claim_snapshot,
                            shared_for_submit.clone(),
                        )
                        .await
                },
            )
            .await;
            if !has_pending {
                let hydrate = hydrate_pending_queue_from_disk(&shared, provider, channel_id)
                    .await
                    .unwrap_or_else(legacy_restitution_refusal);
                if hydrate.queue_len_after > 0 && hydrate.persistence_error.is_none() {
                    discord::schedule_deferred_idle_queue_kickoff(
                        shared.clone(),
                        provider.clone(),
                        channel_id,
                        "2967_orphan_explicit_background_watchdog",
                    );
                }
            }
            crate::services::observability::emit_inflight_lifecycle_event(
                provider.as_str(),
                channel_id.get(),
                None,
                None,
                Some(&format!(
                    "discord:{}:{}",
                    channel_id.get(),
                    revalidated.finalizer_turn_id
                )),
                "cleared_by_explicit_background_watchdog",
                serde_json::json!({
                    "clear_outcome": format!("{:?}", revalidated.clear_outcome),
                    "finalizer_turn_id": revalidated.finalizer_turn_id,
                }),
            );
            if let Some(user_msg_id) = revalidated.pending_hourglass_user_msg_id {
                tv_clear(
                    &shared,
                    channel_id,
                    user_msg_id.into(),
                    generation,
                    "health_watchdog",
                )
                .await;
            }
            cleaned += 1;
            continue;
        }

        let should_clean = stall_watchdog_should_force_clean(
            snapshot.attached,
            snapshot.desynced,
            capture_assessment.advancing,
            snapshot.inflight_terminal_delivery_committed,
            snapshot.inflight_started_at.as_deref(),
            now_unix_secs,
            STALL_WATCHDOG_THRESHOLD_SECS,
            registry.started_at_unix(),
        );
        let judgment_basis = stall_liveness::StallWatchdogJudgmentBasis::from_snapshot(
            &snapshot,
            now_unix_secs,
            registry.started_at_unix(),
        );
        let mut force_clean_inflight = None;
        let mut liveness_decision = None;
        if should_clean {
            force_clean_inflight =
                discord::inflight::load_inflight_state(provider, channel_id.get());
            let decision = stall_liveness::evaluate_stall_watchdog_liveness(
                provider,
                channel_id,
                &snapshot,
                force_clean_inflight.as_ref(),
                now_unix_secs,
                stall_liveness::STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
                stall_liveness::STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            );
            if decision.should_defer() {
                stall_liveness::log_stall_watchdog_liveness_deferred(
                    provider,
                    channel_id,
                    &snapshot,
                    &judgment_basis,
                    &decision,
                    stall_liveness::STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
                    STALL_WATCHDOG_THRESHOLD_SECS,
                );
            }
            liveness_decision = Some(decision);
        } else {
            stall_liveness::clear_stall_watchdog_liveness_state_if_healthy(
                provider, channel_id, &snapshot,
            );
        }
        if !should_clean {
            // Detection-only sibling probe for "completed-stale" inflight
            // leaks: bridge handed off cleanup to the watcher (or the watcher
            // delivered the response itself), but the inflight file persisted
            // past the staleness threshold even though the relay is healthy
            // and the mailbox has no active turn. This is the silent gap
            // behind the deadlock-manager 30/45/60-min alarm pattern. We do
            // NOT clean here — the live tmux session may be waiting for the
            // user's next message. Emitting the structured event lets the
            // external monitor and counters detect each occurrence so the
            // root cause can be isolated.
            if inflight_completed_stale_leak_detected(
                snapshot.attached,
                snapshot.desynced,
                snapshot.inflight_state_present,
                snapshot.mailbox_active_user_msg_id,
                snapshot.inflight_updated_at.as_deref(),
                snapshot.tmux_session_alive,
                now_unix_secs,
                STALL_WATCHDOG_THRESHOLD_SECS,
            ) {
                let leak_inflight =
                    discord::inflight::load_inflight_state(provider, channel_id.get());
                let leak_turn_id = leak_inflight
                    .as_ref()
                    .filter(|s| s.user_msg_id != 0)
                    .map(|s| format!("discord:{}:{}", s.channel_id, s.user_msg_id));
                let leak_dispatch_id = leak_inflight.as_ref().and_then(|s| s.dispatch_id.clone());
                let leak_session_key = leak_inflight.as_ref().and_then(|s| s.session_key.clone());
                // Only a genuinely unrelayed answer is a leak. A stale-but-
                // delivered turn — e.g. one whose answer went out as a bridge
                // fallback message, which advances response_sent_offset to len —
                // must NOT raise the leaked-answer alarm or trigger recovery.
                let has_unrelayed_answer = leak_inflight.as_ref().is_some_and(|s| {
                    leak_recovery_unrelayed_range(&s.full_response, s.response_sent_offset)
                        .is_some()
                });
                if !has_unrelayed_answer {
                    // #3629: a completed-stale row with no unrelayed answer is
                    // either a delivered-and-idle turn (committed → preserve) or
                    // a never-committed NO_REPLY/empty orphan (clean). We only
                    // reach here when the mailbox has NO active turn and the
                    // relay is healthy. Only a real, NON-ZERO user_msg_id is
                    // cleaned — zero-id rows are never auto-cleaned because they
                    // can be a live recovery/TUI-direct turn or a newer pinned
                    // zero-id turn (codex #3629 review). See
                    // `completed_stale_no_answer_orphan_should_clean`.
                    let terminal_committed = leak_inflight
                        .as_ref()
                        .is_some_and(|s| s.terminal_delivery_committed);
                    let this_turn_user_msg_id =
                        leak_inflight.as_ref().map(|s| s.user_msg_id).unwrap_or(0);
                    if completed_stale_no_answer_orphan_should_clean(
                        terminal_committed,
                        this_turn_user_msg_id,
                    ) {
                        // Identity-guarded removal: a newer turn that has since
                        // written this channel's row yields UserMsgMismatch and
                        // is preserved; planned-restart / rebind-origin rows are
                        // skipped. We only ever delete THIS leaked turn's row.
                        let Ok(_recovery) =
                            discord::live_bridge::try_recovery(provider, channel_id.get())
                        else {
                            continue;
                        };
                        let clear_outcome = discord::inflight::clear_inflight_state_if_matches(
                            provider,
                            channel_id.get(),
                            this_turn_user_msg_id,
                        );
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            target: "agentdesk::services::discord::health::recovery",
                            "  [{ts}] 🧹 #3629 cleaned NO_REPLY/empty orphan inflight on channel {} (provider={}, outcome={:?})",
                            channel_id,
                            provider.as_str(),
                            clear_outcome
                        );
                        crate::services::observability::emit_inflight_lifecycle_event(
                            provider.as_str(),
                            channel_id.get(),
                            leak_dispatch_id.as_deref(),
                            leak_session_key.as_deref(),
                            leak_turn_id.as_deref(),
                            "leak_cleaned_noreply_orphan",
                            serde_json::json!({
                                "guarded_clear_outcome": format!("{clear_outcome:?}"),
                                "inflight_started_at": snapshot.inflight_started_at,
                                "inflight_updated_at": snapshot.inflight_updated_at,
                                "tmux_session_alive": snapshot.tmux_session_alive,
                            }),
                        );
                    }
                    continue;
                }
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    target: "agentdesk::services::discord::health::recovery",
                    "  [{ts}] 🔎 inflight leak suspected on channel {} (provider={}): completed-stale + healthy watcher; emitting telemetry only",
                    channel_id,
                    provider.as_str()
                );
                crate::services::observability::emit_inflight_lifecycle_event(
                    provider.as_str(),
                    channel_id.get(),
                    leak_dispatch_id.as_deref(),
                    leak_session_key.as_deref(),
                    leak_turn_id.as_deref(),
                    "leak_detected_completed_stale",
                    serde_json::json!({
                        "inflight_started_at": snapshot.inflight_started_at,
                        "inflight_updated_at": snapshot.inflight_updated_at,
                        "tmux_session": snapshot.tmux_session,
                        "tmux_session_alive": snapshot.tmux_session_alive,
                        "watcher_attached": snapshot.attached,
                        "has_pending_queue": snapshot.has_pending_queue,
                        "full_response_len": leak_inflight
                            .as_ref()
                            .map(|s| s.full_response.len()),
                        "response_sent_offset": leak_inflight
                            .as_ref()
                            .map(|s| s.response_sent_offset),
                        "last_watcher_relayed_offset": leak_inflight
                            .as_ref()
                            .and_then(|s| s.last_watcher_relayed_offset),
                        "watcher_owns_live_relay": leak_inflight
                            .as_ref()
                            .map(|s| s.watcher_owns_live_relay),
                    }),
                );

                // #2860 delivery-lease consolidation: upgrade detection ->
                // recovery. Scoped to the watcher-delegated-but-never-relayed
                // signature and gated on a live-message probe, so it can never
                // double-deliver (see maybe_recover_completed_stale_leak). The
                // recovery re-loads its own fresh inflight state.
                maybe_recover_completed_stale_leak(registry, provider, &shared, channel_id).await;
            }
            continue;
        }
        // #4460: DO NOT force-terminate the active turn here. The old branch-4
        // "desynced force-clean" deleted inflight state, released the mailbox
        // cancel token, cancelled the watcher, and cleared the turn view. The
        // incident that motivated #4460 was later traced to relay-recovery
        // redrive rather than this branch, but the operator-requested hardening
        // remains: page the owner and leave every turn authority untouched.
        //
        // #4460 follow-up: the W0 shadow classifier deliberately labels a
        // desynced control plane as `control_plane_desync` even when producer
        // bytes are fresh, so `shadow_verdict == producer_live` was unreachable
        // in this branch. Use the authoritative production liveness decision
        // instead: fresh producer evidence suppresses the page, while no
        // evidence still pages. No case cleans.
        if !stall_alert::should_page_suspected_stall(liveness_decision.as_ref()) {
            continue;
        }
        stall_liveness::log_stall_watchdog_page_judgment(
            provider,
            channel_id,
            &snapshot,
            &judgment_basis,
            liveness_decision.as_ref(),
            stall_liveness::STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_THRESHOLD_SECS,
        );
        // #4460: not classified live — surface a rate-limited mention so the
        // owner can decide, but NEVER terminate/cancel/delete anything. The
        // turn (inflight state, mailbox token, watcher, turn view) is left
        // fully intact.
        stall_alert::notify_suspected_stall_without_cleanup(
            &shared,
            provider,
            channel_id,
            force_clean_inflight.as_ref(),
        )
        .await;
        continue;
    }
    // A cancelled watcher and a failed respawn both leave the channel out of the
    // watcher-derived candidate loop above, so neither is reachable from it.
    watcher_respawn::sweep_and_retry_absences(registry, provider, &runtimes, &seen, now_unix_secs)
        .await;
    cleaned + relay_auto_heal::run_orphan_token_auto_heal_pass(registry, provider, &runtimes).await
}
