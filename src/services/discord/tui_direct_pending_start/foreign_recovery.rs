use super::*;
use crate::services::discord::tmux_watcher_registry::{
    TerminalDeliveryFence, WatcherIdentityFence, execution_identity_mode,
};

const STALE_FOREIGN_CANCEL_IDENTITY_SITE: &str = "tui_direct_stale_foreign_cancel";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ForeignRecoverySource {
    PendingStart,
    LeakedRowSweep,
}

pub(super) async fn submit_stale_foreign_inflight_cancel(
    shared: &Arc<SharedData>,
    provider: &crate::services::provider::ProviderKind,
    channel_id: poise::serenity_prelude::ChannelId,
    probe: &super::super::destructive_cancel_gate::DestructiveCancelProbeSnapshot,
    source: ForeignRecoverySource,
) -> bool {
    let finalizer_turn_id = probe.pin.finalizer_turn_id;
    if finalizer_turn_id == 0 {
        return false;
    }
    let mailbox_active_user_msg_id = super::super::mailbox_snapshot(shared, channel_id)
        .await
        .active_user_message_id
        .map(|id| id.get());
    if mailbox_active_user_msg_id != probe.pin.mailbox_active_user_msg_id {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            expected_mailbox_active_user_msg_id = probe.pin.mailbox_active_user_msg_id.unwrap_or(0),
            mailbox_active_user_msg_id = mailbox_active_user_msg_id.unwrap_or(0),
            "tui_direct_pending_start: stale FOREIGN cancel no-op; mailbox episode changed"
        );
        return false;
    }
    #[cfg(test)]
    retirement_recheck_tests::pause("leaked_row_cancel_after_mailbox", channel_id.get()).await;
    if source == ForeignRecoverySource::LeakedRowSweep
        && super::super::health::legacy_supervision::legacy_retired(
            provider.as_str(),
            channel_id.get(),
            "leaked_row_cancel_after_mailbox",
        )
    {
        return false;
    }

    // Bind the execution identity to the watcher incarnation before cancellation.
    let pinned = probe
        .pin
        .tmux_session_name
        .as_deref()
        .and_then(|tmux_session| {
            watcher_cancel::pin_watcher_for_tmux_session(&shared.tmux_watchers, tmux_session)
        })
        .map(|pinned| {
            let identity_fence = WatcherIdentityFence::capture(
                execution_identity_mode(),
                STALE_FOREIGN_CANCEL_IDENTITY_SITE,
                &pinned.tmux_session_name,
            )
            .with_pinned_binding(pinned.owner_channel_id, &pinned.output_path);
            (pinned, identity_fence)
        });
    // A live delivery lease is checked at watcher removal; a missing watcher has no lease CAS.
    let delivery_fence = TerminalDeliveryFence::capture(
        shared.delivery_lease(channel_id),
        probe.delivery_lease_key.clone(),
        STALE_FOREIGN_CANCEL_IDENTITY_SITE,
    );
    let pinned_watcher = pinned.is_some();
    let commit_outcome = super::super::inflight::commit_destructive_cancel_locked(
        provider,
        channel_id.get(),
        &probe.inflight_identity,
        &probe.updated_at,
        probe.save_generation,
        // Registry removal below owns cancellation; the flock callback only records evidence.
        move |_| {
            if pinned_watcher {
                Ok(super::super::inflight::CommitEvidence::CancelledWatcher)
            } else {
                Ok(super::super::inflight::CommitEvidence::NoWatcher)
            }
        },
    );
    if !matches!(
        commit_outcome,
        super::super::inflight::DestructiveCancelCommitOutcome::CommittedCancelled
            | super::super::inflight::DestructiveCancelCommitOutcome::CommittedNoWatcher
    ) {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            ?commit_outcome,
            "tui_direct_pending_start: stale FOREIGN cancel no-op; flock-held pin commit failed"
        );
        return false;
    }
    #[cfg(test)]
    run_destructive_cancel_post_gate_hook_for_tests(DestructiveCancelHookPoint::PreRegistryCas);
    // The flock is released before registry CAS; the two lock domains never overlap.
    if let Some((pinned, identity_fence)) = pinned {
        // A changed watcher stays registered and uncancelled when its identity CAS fails.
        if shared
            .tmux_watchers
            .under_identity_fence(identity_fence)
            .with_terminal_delivery_fence(delivery_fence)
            .remove_tmux_session_if_current(&pinned.tmux_session_name, &pinned.cancel)
            .is_none()
        {
            tracing::info!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session = pinned.tmux_session_name.as_str(),
                "tui_direct_pending_start: stale FOREIGN cancel committed but watcher incarnation changed; finalizer skipped"
            );
            return false;
        }
        pinned
            .cancel
            .store(true, std::sync::atomic::Ordering::Release);
    }
    let stale_identity = probe.inflight_identity.clone();
    let _ = shared
        .turn_finalizer
        .submit_terminal_with_claim_snapshot(
            super::super::turn_finalizer::TurnKey::new(
                channel_id,
                finalizer_turn_id,
                shared.restart.current_generation,
            ),
            provider.clone(),
            super::super::turn_finalizer::TerminalEvent::Cancel,
            stale_foreign_cancel_finalize_context(),
            Some(probe.finalizer_claim_snapshot.clone()),
            shared.clone(),
        )
        .await;

    let lifecycle_clear_outcome =
        super::super::inflight::clear_lifecycle_inflight_state_if_matches_identity_after_death_evidence(
            provider,
            channel_id.get(),
            &stale_identity,
            &probe.updated_at,
            probe.save_generation,
        );

    let gone_or_changed = !super::super::inflight::load_inflight_state(provider, channel_id.get())
        .is_some_and(|current| {
            stale_identity == super::super::inflight::InflightTurnIdentity::from_state(&current)
                && current.effective_finalizer_turn_id() == finalizer_turn_id
        });
    tracing::warn!(
        provider = %provider.as_str(),
        channel_id = channel_id.get(),
        finalizer_turn_id,
        lifecycle_clear_outcome = ?lifecycle_clear_outcome,
        gone_or_changed,
        "tui_direct_pending_start: stale FOREIGN finalizer cancel completed under death-evidence gate"
    );
    gone_or_changed
}

async fn submit_committed_foreign_inflight_complete(
    shared: &Arc<SharedData>,
    provider: &crate::services::provider::ProviderKind,
    channel_id: poise::serenity_prelude::ChannelId,
    probe: &super::super::destructive_cancel_gate::DestructiveCancelProbeSnapshot,
    restart_orphan_evidence: bool,
    source: ForeignRecoverySource,
) -> bool {
    let finalizer_turn_id = probe.pin.finalizer_turn_id;
    if finalizer_turn_id == 0 {
        return false;
    }
    let Some(current) = super::super::inflight::load_inflight_state(provider, channel_id.get())
    else {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            finalizer_turn_id,
            "tui_direct_pending_start: committed FOREIGN complete no-op; inflight disappeared before finalizer submit"
        );
        return false;
    };
    let mailbox_active_user_msg_id = super::super::mailbox_snapshot(shared, channel_id)
        .await
        .active_user_message_id
        .map(|id| id.get());
    let terminal_envelope_present =
        super::super::destructive_cancel_gate::terminal_envelope_present(provider, probe);
    if !current.terminal_delivery_committed
        || (!terminal_envelope_present && !restart_orphan_evidence)
        || !probe.pin.matches_state(&current)
        || mailbox_active_user_msg_id != probe.pin.mailbox_active_user_msg_id
        || current.updated_at != probe.updated_at
        || current.save_generation != probe.save_generation
    {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            expected_finalizer_turn_id = finalizer_turn_id,
            current_finalizer_turn_id = current.effective_finalizer_turn_id(),
            expected_mailbox_active_user_msg_id = probe.pin.mailbox_active_user_msg_id.unwrap_or(0),
            mailbox_active_user_msg_id = mailbox_active_user_msg_id.unwrap_or(0),
            expected_tmux_session = ?probe.pin.tmux_session_name,
            current_tmux_session = ?current.tmux_session_name,
            terminal_delivery_committed = current.terminal_delivery_committed,
            expected_updated_at = %probe.updated_at,
            current_updated_at = %current.updated_at,
            expected_save_generation = probe.save_generation,
            current_save_generation = current.save_generation,
            "tui_direct_pending_start: committed FOREIGN complete no-op; terminal envelope or identity pin no longer matches"
        );
        return false;
    }

    #[cfg(test)]
    retirement_recheck_tests::pause("leaked_row_complete_after_mailbox", channel_id.get()).await;
    if source == ForeignRecoverySource::LeakedRowSweep
        && super::super::health::legacy_supervision::legacy_retired(
            provider.as_str(),
            channel_id.get(),
            "leaked_row_complete_after_mailbox",
        )
    {
        return false;
    }

    let committed_identity = super::super::inflight::InflightTurnIdentity::from_state(&current);
    if restart_orphan_evidence {
        let archive_outcome =
            super::super::inflight::archive_inflight_state_if_matches_identity_generation(
                provider,
                channel_id.get(),
                &committed_identity,
                &probe.updated_at,
                probe.save_generation,
                "stuck-restart-orphan",
            );
        if archive_outcome != super::super::inflight::GuardedClearOutcome::Cleared {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                ?archive_outcome,
                "tui_direct_pending_start: restart-orphan archive failed; preserving committed FOREIGN inflight"
            );
            return false;
        }
    }
    let outcome = shared
        .turn_finalizer
        .submit_terminal_with_claim_snapshot(
            super::super::turn_finalizer::TurnKey::new(
                channel_id,
                finalizer_turn_id,
                shared.restart.current_generation,
            ),
            provider.clone(),
            super::super::turn_finalizer::TerminalEvent::Complete,
            committed_foreign_complete_finalize_context(),
            Some(probe.finalizer_claim_snapshot.clone()),
            shared.clone(),
        )
        .await;

    let gone_or_changed = !super::super::inflight::load_inflight_state(provider, channel_id.get())
        .is_some_and(|current| {
            committed_identity == super::super::inflight::InflightTurnIdentity::from_state(&current)
                && current.effective_finalizer_turn_id() == finalizer_turn_id
                && current.save_generation == probe.save_generation
        });
    tracing::warn!(
        provider = %provider.as_str(),
        channel_id = channel_id.get(),
        finalizer_turn_id,
        finalize_outcome = ?std::mem::discriminant(&outcome),
        gone_or_changed,
        restart_orphan_evidence,
        "tui_direct_pending_start: committed FOREIGN inflight cleared via finalizer Complete under terminal or restart-orphan evidence"
    );
    gone_or_changed
}

pub(in crate::services::discord) async fn demote_stale_foreign_inflight_if_current(
    shared: &Arc<SharedData>,
    record: &TuiDirectPendingStart,
) -> bool {
    demote_stale_foreign_inflight(shared, record, ForeignRecoverySource::PendingStart).await
}

pub(in crate::services::discord) async fn demote_leaked_foreign_inflight_if_current(
    shared: &Arc<SharedData>,
    record: &TuiDirectPendingStart,
) -> bool {
    demote_stale_foreign_inflight(shared, record, ForeignRecoverySource::LeakedRowSweep).await
}

async fn demote_stale_foreign_inflight(
    shared: &Arc<SharedData>,
    record: &TuiDirectPendingStart,
    source: ForeignRecoverySource,
) -> bool {
    let Some(provider) = crate::services::provider::ProviderKind::from_str(&record.provider) else {
        return false;
    };
    let channel = poise::serenity_prelude::ChannelId::new(record.channel_id);
    let Some(state) = super::super::inflight::load_inflight_state(&provider, record.channel_id)
    else {
        return false;
    };
    let capture_offset = output_capture_offset(&state);
    if committed_foreign_inflight_is_finalize_clearable(&state, record) {
        let mailbox_active_user_msg_id = super::super::mailbox_snapshot(shared, channel)
            .await
            .active_user_message_id
            .map(|id| id.get());
        let probe =
            super::super::destructive_cancel_gate::DestructiveCancelProbeSnapshot::from_state(
                shared.as_ref(),
                &state,
                mailbox_active_user_msg_id,
                channel,
            );
        let relay_frontier = probe.relay_frontier;
        let terminal_envelope_present =
            super::super::destructive_cancel_gate::terminal_envelope_present(&provider, &probe);
        let pane_ready_for_input = !terminal_envelope_present
            && restart_orphan_pane_ready_for_input(&provider, &state, &record.tmux_session_name);
        let restart_evidence = restart_orphan_evidence_at(
            &state,
            shared.restart.current_generation,
            chrono::Utc::now().timestamp(),
            pane_ready_for_input,
        );
        if !terminal_envelope_present && !restart_evidence.permits_finalize_clear() {
            tracing::warn!(
                provider = %record.provider,
                channel_id = record.channel_id,
                tmux_session_name = %record.tmux_session_name,
                anchor_message_id = record.anchor_message_id,
                committed_user_msg_id = state.user_msg_id,
                committed_started_at = %state.started_at,
                committed_updated_at = %state.updated_at,
                relay_frontier = ?relay_frontier,
                capture_offset = ?capture_offset,
                generation_crossed = restart_evidence.generation_crossed,
                committed_frozen_past_grace = restart_evidence.committed_frozen_past_grace,
                pane_ready_for_input = restart_evidence.pane_ready_for_input,
                "tui_direct_pending_start: skipped committed FOREIGN finalize-clear; terminal envelope and restart-orphan evidence missing"
            );
            return false;
        }
        let cleared = submit_committed_foreign_inflight_complete(
            shared,
            &provider,
            channel,
            &probe,
            !terminal_envelope_present,
            source,
        )
        .await;
        if cleared {
            tracing::warn!(
                provider = %record.provider,
                channel_id = record.channel_id,
                tmux_session_name = %record.tmux_session_name,
                anchor_message_id = record.anchor_message_id,
                committed_user_msg_id = state.user_msg_id,
                committed_started_at = %state.started_at,
                committed_updated_at = %state.updated_at,
                relay_frontier = ?relay_frontier,
                capture_offset = ?capture_offset,
                restart_orphan_evidence = !terminal_envelope_present,
                "tui_direct_pending_start: cleared committed FOREIGN inflight via finalizer Complete; re-evaluating before claiming (#4805)"
            );
        }
        return cleared;
    }
    if !stale_foreign_inflight_is_reclaimable_at(&state, record, chrono::Utc::now().timestamp()) {
        return false;
    }
    let mailbox_active_user_msg_id = super::super::mailbox_snapshot(shared, channel)
        .await
        .active_user_message_id
        .map(|id| id.get());
    let probe = stale_foreign_probe(shared.as_ref(), &state, mailbox_active_user_msg_id, channel);
    let relay_frontier = probe.relay_frontier;
    let gate = super::super::destructive_cancel_gate::evaluate(
        shared, &provider, channel, channel, &probe,
    )
    .await;
    if !gate.is_allowed() {
        tracing::warn!(
            provider = %record.provider,
            channel_id = record.channel_id,
            tmux_session_name = %record.tmux_session_name,
            anchor_message_id = record.anchor_message_id,
            stale_user_msg_id = state.user_msg_id,
            stale_started_at = %state.started_at,
            stale_updated_at = %state.updated_at,
            relay_frontier = ?relay_frontier,
            capture_offset = ?capture_offset,
            denied_reason = gate.denied_reason().unwrap_or("unknown"),
            "tui_direct_pending_start: skipped destructive stale FOREIGN demotion; death/identity gate did not pass (#4030)"
        );
        return false;
    }

    #[cfg(test)]
    run_destructive_cancel_post_gate_hook_for_tests(DestructiveCancelHookPoint::PostGate);
    let demoted =
        submit_stale_foreign_inflight_cancel(shared, &provider, channel, &probe, source).await;
    if demoted {
        tracing::warn!(
            provider = %record.provider,
            channel_id = record.channel_id,
            tmux_session_name = %record.tmux_session_name,
            anchor_message_id = record.anchor_message_id,
            stale_user_msg_id = state.user_msg_id,
            stale_started_at = %state.started_at,
            stale_updated_at = %state.updated_at,
            relay_frontier = ?relay_frontier,
            capture_offset = ?capture_offset,
            death_evidence = gate.allowed_reason().unwrap_or("unknown"),
            min_stale_age_secs = STALE_FOREIGN_INFLIGHT_MIN_AGE_SECS,
            "tui_direct_pending_start: demoted stale FOREIGN inflight with dead relay frontier via finalizer Cancel; re-evaluating before claiming (#4030)"
        );
    }
    demoted
}
