use super::*;

use crate::services::discord::tmux_watcher_registry::{
    TerminalDeliveryFence, WatcherIdentityFence, execution_identity_mode,
};

/// #5071 T3-A1 observation label for the dead-frontier automatic watcher cancel.
const DEAD_FRONTIER_CANCEL_IDENTITY_SITE: &str = "relay_recovery_dead_frontier_cancel";

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
                // #5071 relay-tail S2: `Some(0)`, never `None`. An unmeasured
                // tail must not open the destructive branch — see
                // `unread_tail_is_proven_drained`.
                && unread_tail_is_proven_drained(decision.evidence.unread_bytes)
                // This branch intentionally does not route through
                // `destructive_cancel_gate`: the snapshot readiness check is
                // the turn-scope proof that the provider prompt has returned
                // (structured JSONL ready state, or tmux prompt fallback), and
                // the following inflight/tail guards then rule out a
                // deliverable assistant body. Their reach is bounded by what
                // they can read: `idle_tmux_repair_has_unrelayed_tail_answer`
                // proves an empty tail only for a row whose `output_path`
                // resolves and extracts, and returns `false` — no objection —
                // when it cannot read one. The `Some(0)` above is what excludes
                // that blind case, so the two guards prove "nothing left to
                // preserve" only in conjunction — and only as far as `Some(0)`
                // reaches, which is one snapshot's stat against the relay
                // frontier and not a rotated/truncated transcript that reads
                // drained by that measure (see
                // `unread_tail_is_proven_drained`). The cleanup below only
                // retires stale mailbox/inflight bookkeeping for an already-idle
                // turn.
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
                        let expected_watcher = shared
                            .tmux_watchers
                            .get(&owner_channel_id)
                            .map(|watcher| {
                                (
                                    watcher.tmux_session_name.clone(),
                                    watcher.output_path.clone(),
                                    watcher.cancel.clone(),
                                )
                            })
                            // #5071 T3-A1: pin the live execution identity in the
                            // same breath as the pointer/output pin, so the
                            // registry CAS below re-reads it across the whole
                            // gate -> commit -> CAS window. Captured outside the
                            // closure above because the marker read is file I/O
                            // and must not run under the dashmap ref.
                            .map(|(tmux_session_name, output_path, cancel)| {
                                let identity_fence = WatcherIdentityFence::capture(
                                    execution_identity_mode(),
                                    DEAD_FRONTIER_CANCEL_IDENTITY_SITE,
                                    &tmux_session_name,
                                );
                                (tmux_session_name, output_path, cancel, identity_fence)
                            });
                        // #5071 relay-tail S4 (I-1): pin the delivery-lease
                        // coordinate in the same breath as the execution
                        // identity. Both re-read live state inside the registry
                        // CAS below; what is pinned HERE is only which cell and
                        // which turn key to re-read, taken from the probe so it
                        // cannot name a different turn than the rest of the gate.
                        let delivery_fence = TerminalDeliveryFence::capture(
                            shared.delivery_lease(owner_channel_id),
                            probe.delivery_lease_key.clone(),
                            DEAD_FRONTIER_CANCEL_IDENTITY_SITE,
                        );
                        let gate = super::destructive_cancel_gate::evaluate(
                            shared,
                            provider,
                            owner_channel_id,
                            owner_channel_id,
                            &probe,
                        )
                        .await;
                        if gate.is_allowed() {
                            #[cfg(test)]
                            super::run_destructive_cancel_post_gate_hook_for_tests();
                            let mailbox_active_user_msg_id =
                                mailbox_snapshot(shared, owner_channel_id)
                                    .await
                                    .active_user_message_id
                                    .map(|id| id.get());
                            // #5071 T3-A1 retired the #5067 in-flight emission
                            // read that used to sit here. The registry CAS below
                            // now carries the identity conjunct, which is a
                            // different guarantee: it re-compares the pinned
                            // VALUES (owner channel, session, output path,
                            // cancel pointer, `.spawn_nonce`) against the live
                            // row, so a replaced or respawned row is refused. It
                            // does NOT establish a row generation — see
                            // `WatcherIdentityFence` for the A -> B -> A
                            // readmission it cannot see — and it says nothing on
                            // its own about a terminal POST the SAME incarnation
                            // may have in flight.
                            //
                            // #5071 relay-tail S4 (I-1) narrowed that second
                            // gap rather than closing it: the CAS below also
                            // carries `TerminalDeliveryFence`, which refuses
                            // while THIS turn's delivery lease is still `Leased`
                            // with an unelapsed deadline. What stays a declared
                            // non-guarantee is a same-incarnation terminal POST
                            // that holds NO delivery lease, or one whose holder
                            // stopped renewing — the fence is a lease read, not
                            // an HTTP-in-flight observation.
                            if mailbox_active_user_msg_id != probe.pin.mailbox_active_user_msg_id {
                                tracing::warn!(
                                    target: "agentdesk::discord::relay_recovery",
                                    provider = provider.as_str(),
                                    channel_id = decision.channel_id,
                                    watcher_owner_channel_id = owner_channel_id.get(),
                                    death_evidence = gate.allowed_reason().unwrap_or("unknown"),
                                    expected_mailbox_active_user_msg_id = probe.pin.mailbox_active_user_msg_id.unwrap_or(0),
                                    mailbox_active_user_msg_id = mailbox_active_user_msg_id.unwrap_or(0),
                                    "relay recovery skipped destructive watcher cancel after gate; mailbox episode changed"
                                );
                            } else if let Some((
                                tmux_session_name,
                                output_path,
                                cancel,
                                identity_fence,
                            )) = expected_watcher
                            {
                                let expected_identity = probe.inflight_identity.clone();
                                let commit_outcome =
                                    super::inflight::commit_destructive_cancel_locked(
                                        provider,
                                        owner_channel_id.get(),
                                        &expected_identity,
                                        &probe.updated_at,
                                        probe.save_generation,
                                        // #5071 T3-A1: the flock callback no
                                        // longer stores `cancel`. This helper
                                        // cancels inside its own CAS below, so
                                        // storing here would leave a cancelled
                                        // watcher that the registry still lists
                                        // whenever that CAS fails.
                                        |_| Ok(super::inflight::CommitEvidence::CancelledWatcher),
                                    );
                                if commit_outcome
                                    != super::inflight::DestructiveCancelCommitOutcome::CommittedCancelled
                                {
                                    tracing::warn!(
                                        target: "agentdesk::discord::relay_recovery",
                                        provider = provider.as_str(),
                                        channel_id = decision.channel_id,
                                        watcher_owner_channel_id = owner_channel_id.get(),
                                        death_evidence = gate.allowed_reason().unwrap_or("unknown"),
                                        ?commit_outcome,
                                        "relay recovery skipped destructive watcher cancel after gate; flock-held pin commit failed"
                                    );
                                } else {
                                    // The flock is released before registry CAS; the two lock domains never overlap.
                                    // #5071 T3-A1: this helper stores `cancel`
                                    // itself, inside the CAS, so `true` already
                                    // means the watcher was cancelled and
                                    // `false` means nothing was written.
                                    let watcher_removed = shared
                                        .tmux_watchers
                                        .under_identity_fence(identity_fence)
                                        .with_terminal_delivery_fence(delivery_fence)
                                        .cancel_and_remove_channel_if_current(
                                            &owner_channel_id,
                                            &tmux_session_name,
                                            &output_path,
                                            &cancel,
                                        );
                                    if !watcher_removed {
                                        tracing::warn!(
                                            target: "agentdesk::discord::relay_recovery",
                                            provider = provider.as_str(),
                                            channel_id = decision.channel_id,
                                            watcher_owner_channel_id = owner_channel_id.get(),
                                            death_evidence = gate.allowed_reason().unwrap_or("unknown"),
                                            "relay recovery skipped finalizer after committed cancel; expected watcher was not current"
                                        );
                                    } else {
                                        let finalize_outcome =
                                            finalize_cancelled_watcher_owner_turn(
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
                                                &expected_identity,
                                                &probe.updated_at,
                                                probe.save_generation,
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
        // #5071 T4-B6: `ReportRelayUnreachable` shares `ObserveOnly`'s apply
        // path — "skipped", nothing written — because 4987 §7.1 / I15 keeps the
        // reachability tier out of every destructive step. The two actions are
        // separate KINDS so the operator-facing decision says which observation
        // it is; they are deliberately not separate BEHAVIOURS here, and this
        // arm is spelled beside its twin rather than merged so that giving the
        // T4-B6 action an effect is a visible edit.
        RelayRecoveryActionKind::ObserveOnly | RelayRecoveryActionKind::ReportRelayUnreachable => {
            RelayRecoveryApplyResult {
                status: "skipped",
                removed_thread_proofs: 0,
                removed_mailbox_token: false,
                post_mailbox_has_cancel_token: None,
                post_mailbox_queue_depth: None,
                reattach_watcher_spawned: None,
                reattach_watcher_replaced: None,
                reattach_initial_offset: None,
                reattach_error: None,
            }
        }
    }
}
