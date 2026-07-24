use super::*;

pub(super) async fn apply_relay_recovery_decision(
    registry: &HealthRegistry,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    decision: &RelayRecoveryDecision,
    episode: Option<&circuit_breaker::RelayReattachEpisode>,
    source: RelayRecoveryApplySource,
) -> RelayRecoveryApplyResult {
    match decision.action {
        RelayRecoveryActionKind::ClearStaleThreadProof => {
            let channel = ChannelId::new(decision.channel_id);
            let before = shared.dispatch.thread_parents.len();
            let mut removed_parents = Vec::new();
            shared.dispatch.thread_parents.retain(|parent, thread| {
                let remove = *parent == channel || *thread == channel;
                if remove {
                    removed_parents.push(*parent);
                }
                !remove
            });
            super::turn_finalizer::cleanup::kickoff_thread_parents_after_finalize(
                shared,
                provider,
                removed_parents,
            );
            RelayRecoveryApplyResult {
                status: "applied",
                removed_thread_proofs: before.saturating_sub(shared.dispatch.thread_parents.len()),
                removed_mailbox_token: false,
                post_mailbox_has_cancel_token: None,
                post_mailbox_queue_depth: None,
                reattach_watcher_spawned: None,
                reattach_watcher_replaced: None,
                reattach_initial_offset: None,
                reattach_error: None,
            }
        }
        RelayRecoveryActionKind::ClearOrphanPendingToken => {
            let channel = ChannelId::new(decision.channel_id);
            let cleared = mailbox_clear_channel(shared, provider, channel).await;
            if source.cleanup_session() {
                super::stall_recovery::finalize_orphaned_clear(
                    shared,
                    channel,
                    cleared.removed_token.clone(),
                    source.finalizer_reason(),
                );
            } else {
                super::stall_recovery::finalize_orphaned_clear_preserve_session(
                    shared,
                    channel,
                    cleared.removed_token.clone(),
                    source.finalizer_reason(),
                );
            }
            mailbox_clear_recovery_marker(shared, channel).await;
            let after = mailbox_snapshot(shared, channel).await;
            RelayRecoveryApplyResult {
                status: "applied",
                removed_thread_proofs: 0,
                removed_mailbox_token: cleared.removed_token.is_some(),
                post_mailbox_has_cancel_token: Some(after.cancel_token.is_some()),
                post_mailbox_queue_depth: Some(after.intervention_queue.len()),
                reattach_watcher_spawned: None,
                reattach_watcher_replaced: None,
                reattach_initial_offset: None,
                reattach_error: None,
            }
        }
        RelayRecoveryActionKind::ReattachWatcher => {
            let channel = ChannelId::new(decision.channel_id);
            // The durable automatic lane is deliberately non-destructive: its
            // exact episode is adopted by `rebind_inflight` below.  The legacy
            // manual lane keeps the idle-turn retirement behavior.
            if episode.is_none()
                && let Some(tmux_session) = decision.affected.tmux_session.as_deref()
                && decision.evidence.unread_bytes.unwrap_or(0) == 0
                // This branch intentionally does not route through
                // `destructive_cancel_gate`: the snapshot readiness check is
                // the turn-scope proof that the provider prompt has returned
                // (structured JSONL ready state, or tmux prompt fallback), and the
                // following inflight/tail guards prove there is no deliverable
                // assistant body left to preserve. The cleanup below only retires
                // stale mailbox/inflight bookkeeping for an already-idle turn.
                && let Some(inflight_clear_state) =
                    load_idle_tmux_reattach_inflight_clear_candidate(provider, decision.channel_id)
                && idle_tmux_repair_snapshot_ready_for_input(
                    provider,
                    decision.channel_id,
                    tmux_session,
                    &inflight_clear_state,
                    idle_tmux_repair_pane_ready_for_input,
                )
                // #3668 F2: never destructively clear when a final answer is
                // still persisted in JSONL after `last_offset` — fall through to
                // the non-destructive rebind path so normal relay delivers it.
                && !idle_tmux_repair_has_unrelayed_tail_answer(&inflight_clear_state)
            {
                let inflight_clear_pin =
                    capture_idle_tmux_reattach_inflight_clear_pin(&inflight_clear_state);
                let inflight_clear_outcome = clear_idle_tmux_reattach_inflight_if_pinned(
                    provider,
                    decision.channel_id,
                    inflight_clear_pin.as_ref(),
                );
                if !matches!(
                    inflight_clear_outcome,
                    super::inflight::GuardedClearOutcome::Cleared
                ) {
                    let after = mailbox_snapshot(shared, channel).await;
                    return RelayRecoveryApplyResult {
                        status: idle_tmux_reattach_clear_status(inflight_clear_outcome),
                        removed_thread_proofs: 0,
                        removed_mailbox_token: false,
                        post_mailbox_has_cancel_token: Some(after.cancel_token.is_some()),
                        post_mailbox_queue_depth: Some(after.intervention_queue.len()),
                        reattach_watcher_spawned: Some(false),
                        reattach_watcher_replaced: Some(false),
                        reattach_initial_offset: None,
                        reattach_error: None,
                    };
                }
                completion_footer::forget_if_message(
                    channel,
                    decision.affected.bridge_current_msg_id,
                );
                if let Some((_, watcher)) = shared.tmux_watchers.remove(&channel) {
                    watcher.cancel.store(true, Ordering::Relaxed);
                }
                // #4198: snapshot before the yielding finish/cleanup awaits so
                // the remove below cannot clobber a same-channel follow-up's
                // freshly inserted override.
                let owned_role_override =
                    super::turn_finalizer::cleanup::snapshot_role_override(shared, channel);
                let finish = mailbox_finish_turn(shared, provider, channel).await;
                if let Some(token) = finish.removed_token.as_ref() {
                    token.cancelled.store(true, Ordering::Relaxed);
                    super::saturating_decrement_global_active(shared);
                }
                super::clear_watchdog_deadline_override(channel.get()).await;
                let thread_parent_kickoffs =
                    super::turn_finalizer::cleanup::collect_and_clear_thread_parents(
                        shared, channel,
                    );
                super::turn_finalizer::cleanup::kickoff_thread_parents_after_finalize(
                    shared,
                    provider,
                    thread_parent_kickoffs,
                );
                shared.restart.recovering_channels.remove(&channel);
                shared.turn_start_times.remove(&channel);
                if !finish.has_pending {
                    super::turn_finalizer::cleanup::remove_owned_role_override(
                        shared,
                        channel,
                        owned_role_override,
                    );
                }
                mailbox_clear_recovery_marker(shared, channel).await;
                let after = mailbox_snapshot(shared, channel).await;
                return RelayRecoveryApplyResult {
                    status: idle_tmux_reattach_clear_status(inflight_clear_outcome),
                    removed_thread_proofs: 0,
                    removed_mailbox_token: finish.removed_token.is_some(),
                    post_mailbox_has_cancel_token: Some(after.cancel_token.is_some()),
                    post_mailbox_queue_depth: Some(after.intervention_queue.len()),
                    reattach_watcher_spawned: Some(false),
                    reattach_watcher_replaced: Some(matches!(
                        inflight_clear_outcome,
                        super::inflight::GuardedClearOutcome::Cleared
                    )),
                    reattach_initial_offset: None,
                    reattach_error: None,
                };
            }
            // Cancelling/finalizing before exact-episode rebind both destroys
            // the reserved live authority and makes the rebind reject its own
            // now-missing pin.  Keep this legacy destructive repair manual;
            // bounded automatic recovery only performs the pinned adoption.
            if episode.is_none()
                && let Some(owner_channel_id) = relay_frontier_dead_reattach_owner(decision)
            {
                match relay_recovery_probe_snapshot_for_owner(
                    shared.as_ref(),
                    provider,
                    owner_channel_id,
                    decision,
                ) {
                    Ok(probe) => {
                        let expected_watcher =
                            shared.tmux_watchers.get(&owner_channel_id).map(|watcher| {
                                (
                                    watcher.tmux_session_name.clone(),
                                    watcher.output_path.clone(),
                                    watcher.cancel.clone(),
                                )
                            });
                        let gate = super::destructive_cancel_gate::evaluate(
                            shared,
                            provider,
                            owner_channel_id,
                            owner_channel_id,
                            &probe,
                        )
                        .await;
                        if gate.is_allowed() {
                            let current = super::inflight::load_inflight_state(
                                provider,
                                owner_channel_id.get(),
                            );
                            let mailbox_active_user_msg_id =
                                mailbox_snapshot(shared, owner_channel_id)
                                    .await
                                    .active_user_message_id
                                    .map(|id| id.get());
                            let current_matches_probe = current.as_ref().is_some_and(|state| {
                                probe.pin.matches_state(state)
                                    && mailbox_active_user_msg_id
                                        == probe.pin.mailbox_active_user_msg_id
                                    && state.updated_at == probe.updated_at
                                    && state.save_generation == probe.save_generation
                            });
                            if !current_matches_probe {
                                tracing::warn!(
                                    target: "agentdesk::discord::relay_recovery",
                                    provider = provider.as_str(),
                                    channel_id = decision.channel_id,
                                    watcher_owner_channel_id = owner_channel_id.get(),
                                    death_evidence = gate.allowed_reason().unwrap_or("unknown"),
                                    expected_updated_at = %probe.updated_at,
                                    current_updated_at = %current.as_ref().map(|state| state.updated_at.as_str()).unwrap_or("<missing>"),
                                    expected_save_generation = probe.save_generation,
                                    current_save_generation = current.as_ref().map(|state| state.save_generation).unwrap_or(0),
                                    expected_mailbox_active_user_msg_id = probe.pin.mailbox_active_user_msg_id.unwrap_or(0),
                                    mailbox_active_user_msg_id = mailbox_active_user_msg_id.unwrap_or(0),
                                    "relay recovery skipped destructive watcher cancel after gate; owner row changed during death-evidence reprobe"
                                );
                            } else if let Some((tmux_session_name, output_path, cancel)) =
                                expected_watcher.as_ref()
                            {
                                let watcher_removed =
                                    shared.tmux_watchers.cancel_and_remove_channel_if_current(
                                        &owner_channel_id,
                                        tmux_session_name,
                                        output_path,
                                        cancel,
                                    );
                                if !watcher_removed {
                                    tracing::warn!(
                                        target: "agentdesk::discord::relay_recovery",
                                        provider = provider.as_str(),
                                        channel_id = decision.channel_id,
                                        watcher_owner_channel_id = owner_channel_id.get(),
                                        death_evidence = gate.allowed_reason().unwrap_or("unknown"),
                                        "relay recovery skipped destructive watcher cancel after gate; expected watcher was not current"
                                    );
                                } else {
                                    let current =
                                        current.expect("checked by current_matches_probe");
                                    let lifecycle_identity =
                                        super::inflight::InflightTurnIdentity::from_state(&current);
                                    let lifecycle_updated_at = current.updated_at.clone();
                                    let lifecycle_save_generation = current.save_generation;
                                    let finalize_outcome = finalize_cancelled_watcher_owner_turn(
                                        shared,
                                        provider,
                                        decision,
                                        owner_channel_id,
                                    )
                                    .await;
                                    let lifecycle_clear_outcome =
                                        super::inflight::clear_lifecycle_inflight_state_if_matches_identity_after_death_evidence(
                                            provider,
                                            owner_channel_id.get(),
                                            &lifecycle_identity,
                                            &lifecycle_updated_at,
                                            lifecycle_save_generation,
                                        );
                                    tracing::warn!(
                                        target: "agentdesk::discord::relay_recovery",
                                        provider = provider.as_str(),
                                        channel_id = decision.channel_id,
                                        watcher_owner_channel_id = owner_channel_id.get(),
                                        last_relay_offset = decision.evidence.last_relay_offset,
                                        last_capture_offset = ?decision.evidence.last_capture_offset,
                                        unread_bytes = ?decision.evidence.unread_bytes,
                                        death_evidence = gate.allowed_reason().unwrap_or("unknown"),
                                        watcher_removed,
                                        lifecycle_clear_outcome = ?lifecycle_clear_outcome,
                                        finalizer_outcome = match finalize_outcome {
                                            Some(super::turn_finalizer::FinalizeOutcome::Finalized { .. }) => "finalized",
                                            Some(super::turn_finalizer::FinalizeOutcome::AlreadyFinalized) => "already_finalized",
                                            Some(super::turn_finalizer::FinalizeOutcome::Deferred) => "deferred",
                                            None => "missing_identity",
                                        },
                                        "relay recovery cancelled watcher with death evidence before reattach"
                                    );
                                }
                            } else {
                                tracing::warn!(
                                    target: "agentdesk::discord::relay_recovery",
                                    provider = provider.as_str(),
                                    channel_id = decision.channel_id,
                                    watcher_owner_channel_id = owner_channel_id.get(),
                                    death_evidence = gate.allowed_reason().unwrap_or("unknown"),
                                    "relay recovery skipped destructive watcher cancel after gate; no expected watcher identity was captured"
                                );
                            }
                        } else {
                            tracing::warn!(
                                target: "agentdesk::discord::relay_recovery",
                                provider = provider.as_str(),
                                channel_id = decision.channel_id,
                                watcher_owner_channel_id = owner_channel_id.get(),
                                denied_reason = gate.denied_reason().unwrap_or("unknown"),
                                finalizer_turn_id = decision.affected.finalizer_turn_id.unwrap_or(0),
                                mailbox_active_user_msg_id = decision.affected.mailbox_active_user_msg_id.unwrap_or(0),
                                tmux_session = ?decision.affected.tmux_session,
                                "relay recovery skipped destructive watcher cancel; death/identity gate did not pass"
                            );
                        }
                    }
                    Err(reason) => {
                        tracing::warn!(
                            target: "agentdesk::discord::relay_recovery",
                            provider = provider.as_str(),
                            channel_id = decision.channel_id,
                            watcher_owner_channel_id = owner_channel_id.get(),
                            denied_reason = reason,
                            finalizer_turn_id = decision.affected.finalizer_turn_id.unwrap_or(0),
                            mailbox_active_user_msg_id = decision.affected.mailbox_active_user_msg_id.unwrap_or(0),
                            tmux_session = ?decision.affected.tmux_session,
                            "relay recovery skipped destructive watcher cancel; decision identity no longer matches owner row"
                        );
                    }
                }
            }
            reattach_apply::apply_rebind(registry, provider, decision, episode).await
        }
        RelayRecoveryActionKind::DrainPendingQueue => {
            let channel = ChannelId::new(decision.channel_id);
            let outcome = super::health::schedule_pending_queue_drain_after_cancel(
                registry,
                provider.as_str(),
                channel,
                "relay_recovery_queue_blocked",
            )
            .await;
            let after = mailbox_snapshot(shared, channel).await;
            RelayRecoveryApplyResult {
                status: if outcome.queue_depth_after > 0 {
                    "scheduled_pending_queue_drain"
                } else {
                    "pending_queue_empty"
                },
                removed_thread_proofs: 0,
                removed_mailbox_token: false,
                post_mailbox_has_cancel_token: Some(after.cancel_token.is_some()),
                post_mailbox_queue_depth: Some(after.intervention_queue.len()),
                reattach_watcher_spawned: None,
                reattach_watcher_replaced: None,
                reattach_initial_offset: None,
                reattach_error: None,
            }
        }
        RelayRecoveryActionKind::ObserveOnly => RelayRecoveryApplyResult {
            status: "skipped",
            removed_thread_proofs: 0,
            removed_mailbox_token: false,
            post_mailbox_has_cancel_token: None,
            post_mailbox_queue_depth: None,
            reattach_watcher_spawned: None,
            reattach_watcher_replaced: None,
            reattach_initial_offset: None,
            reattach_error: None,
        },
    }
}
