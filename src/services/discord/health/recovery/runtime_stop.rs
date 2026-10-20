//! Judged runtime turn stopping and provider interruption policy.

use super::*;

use crate::services::discord::mailbox_probe::wait_for_turn_end;

pub(super) fn preserve_cancel_should_skip_provider_interrupt_for_idle_tui(
    provider: &ProviderKind,
    cleanup_policy: discord::TmuxCleanupPolicy,
    tmux_ready_for_input: bool,
    inflight_safe_to_clear: bool,
) -> bool {
    !cleanup_policy.should_cleanup_tmux()
        && matches!(provider, ProviderKind::Claude)
        && tmux_ready_for_input
        && inflight_safe_to_clear
}

/// `legacy_name` is the session the stop judged legacy tmux; its host is not read again.
fn preserve_cancel_can_skip_provider_interrupt_for_idle_tui(
    provider: &ProviderKind,
    channel_id: ChannelId,
    legacy_name: Option<&str>,
    cleanup_policy: discord::TmuxCleanupPolicy,
) -> bool {
    let Some(tmux_session) = legacy_name.filter(|session| !session.trim().is_empty()) else {
        return false;
    };
    let tmux_ready_for_input = watchdog_decisions::idle_tmux_repair_ready_for_input(
        provider,
        channel_id.get(),
        tmux_session,
    );
    let inflight_safe_to_clear =
        discord::inflight_state_allows_idle_tmux_repair_for_channel(provider, channel_id.get())
            .unwrap_or(false);
    preserve_cancel_should_skip_provider_interrupt_for_idle_tui(
        provider,
        cleanup_policy,
        tmux_ready_for_input,
        inflight_safe_to_clear,
    )
}

fn runtime_stop_wait_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(3)
}

fn clear_persistent_inflight_for_stop(
    provider: &ProviderKind,
    channel_id: ChannelId,
    was_present_at_stop_start: bool,
) -> bool {
    let removed_now = discord::clear_inflight_state(provider, channel_id.get());
    let disappeared_during_stop = was_present_at_stop_start
        && !discord::inflight::inflight_state_file_exists(provider, channel_id.get());
    removed_now || disappeared_during_stop
}

/// A turn stop on `shared`'s channel, carried out on `stop`, the turn judged before any write;
/// its fallback finishes that turn only. `approved` is the force-kill verdict's session.
pub(super) async fn stop_judged_channel_runtime(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    stop: discord::turn_bridge::ChannelJudgement,
    reason: &str,
    cleanup_policy: discord::TmuxCleanupPolicy,
    _approved: Option<Option<&str>>,
) -> RuntimeTurnStopResult {
    // A turn that could not be read is kept as a refused host's is.
    let Ok(stop) = stop else {
        return host_guard_preserved(shared, channel_id).await;
    };
    let (shared, provider) = (shared.clone(), provider.clone());
    let cleanup_requested = cleanup_policy.should_cleanup_tmux();
    let should_clear_persistent_inflight = cleanup_policy.should_clear_inflight();
    let persistent_inflight_was_present = should_clear_persistent_inflight
        && discord::inflight::inflight_state_file_exists(&provider, channel_id.get());
    let mut skipped_idle_provider_interrupt = false;
    let mut judged_termination = false;

    if let Some(stop) = stop.as_ref() {
        // A refused host, or a turn that moved on since the verdict, is left unchanged.
        let native = if !cleanup_requested {
            stop.native(reason).await
        } else {
            None
        };
        let result = if let Some(native) = native {
            native.notice(&shared).await;
            Some(native.finish().await).filter(|result| result.token.is_some())
        } else {
            match stop.refused() {
                false => Some(stop.cancel().await).filter(|result| result.token.is_some()),
                true => None,
            }
        };
        let Some(result) = result else {
            return host_guard_preserved(&shared, channel_id).await;
        };
        let legacy_name = stop.legacy_name();
        let skip_provider_interrupt = preserve_cancel_can_skip_provider_interrupt_for_idle_tui(
            &provider,
            channel_id,
            legacy_name,
            cleanup_policy,
        );
        let outcome = if skip_provider_interrupt {
            tracing::info!(
                target: "agentdesk::services::discord::health::recovery",
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                reason,
                "preserve cancel skipped provider interrupt for idle Claude TUI turn"
            );
            skipped_idle_provider_interrupt = true;
            stop.unstopped()
        } else if !result.already_stopping || cleanup_requested {
            stop.stop(cleanup_policy, reason).await
        } else {
            stop.unstopped()
        };
        let termination_recorded = outcome.termination_recorded;
        judged_termination = termination_recorded;
        if wait_for_turn_end(&shared, channel_id, runtime_stop_wait_timeout()).await {
            let snapshot = shared.mailbox(channel_id).snapshot().await;
            let idle_inflight_cleared = if skipped_idle_provider_interrupt {
                discord::clear_inflight_state(&provider, channel_id.get())
            } else {
                false
            };
            return RuntimeTurnStopResult {
                lifecycle_path: "canonical",
                had_active_turn: true,
                queue_depth: snapshot.intervention_queue.len(),
                inflight: match outcome.may_clear_inflight() {
                    false => InflightDisposition::PreservedByHostGuard,
                    true => InflightDisposition::cleared_if(
                        idle_inflight_cleared
                            || (should_clear_persistent_inflight
                                && clear_persistent_inflight_for_stop(
                                    &provider,
                                    channel_id,
                                    persistent_inflight_was_present,
                                )),
                    ),
                },
                termination_recorded,
                // The turn ended on its own: the mailbox anchor is gone by the
                // canonical exit, which is the only path that never needed a
                // zombie release in the first place.
                mailbox_foreground_free: snapshot.cancel_token.is_none(),
            };
        }
        // Only a legacy stop's turn is finished and cleared by the fallback.
        if !outcome.may_clear_inflight() {
            return host_guard_preserved(&shared, channel_id).await;
        }
    }

    let owned_role_override =
        discord::turn_finalizer::cleanup::snapshot_role_override(&shared, channel_id);
    let judged = (stop.as_ref(), judged_termination);
    let finish = stop_judgement::finish_judged_turn(&shared, &provider, channel_id, judged);
    let finish = match finish.await {
        Ok(finish) => finish,
        Err(kept) => return kept,
    };
    let mut termination_recorded = false;
    // The finish removes only the judged token.
    if let (Some(_), Some(stop)) = (finish.removed_token.as_ref(), stop.as_ref()) {
        let skip_provider_interrupt = skipped_idle_provider_interrupt
            || preserve_cancel_can_skip_provider_interrupt_for_idle_tui(
                &provider,
                channel_id,
                stop.legacy_name(),
                cleanup_policy,
            );
        if skip_provider_interrupt {
            tracing::info!(
                target: "agentdesk::services::discord::health::recovery",
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                reason,
                "runtime fallback skipped provider interrupt for idle Claude TUI turn"
            );
            skipped_idle_provider_interrupt = true;
        } else {
            termination_recorded = stop.stop(cleanup_policy, reason).await.termination_recorded;
        }
    }
    apply_runtime_hard_stop_cleanup(
        &shared,
        &provider,
        channel_id,
        &finish,
        owned_role_override,
        "runtime_stop_fallback",
        cleanup_requested,
    )
    .await;
    let queue_depth = shared
        .mailbox(channel_id)
        .snapshot()
        .await
        .intervention_queue
        .len();
    discord::mailbox_clear_recovery_marker(&shared, channel_id).await;
    let idle_inflight_cleared = if skipped_idle_provider_interrupt {
        discord::clear_inflight_state(&provider, channel_id.get())
    } else {
        false
    };
    let persistent_inflight_cleared = idle_inflight_cleared
        || if should_clear_persistent_inflight {
            clear_persistent_inflight_for_stop(
                &provider,
                channel_id,
                persistent_inflight_was_present,
            )
        } else {
            false
        };

    // #5176 — the runtime fallback is exactly where the zombie survived. It
    // stamps the turn `cancelled` and returns, and when no turn-bridge loop
    // exists to run the canonical exit the mailbox keeps its foreground anchor,
    // so the channel stays permanently locked while the response reports a
    // successful cancel. Ask the single release authority whether the anchor may
    // go, and report what actually happened rather than what was attempted.
    let release = discord::zombie_foreground_release::release_zombie_foreground_turn(
        &shared,
        &provider,
        channel_id,
        "runtime_stop_fallback",
    )
    .await;
    let mailbox_foreground_free = shared
        .mailbox(channel_id)
        .snapshot()
        .await
        .cancel_token
        .is_none();
    let queue_depth = if release.released {
        release.queue_depth_after
    } else {
        queue_depth
    };

    RuntimeTurnStopResult {
        lifecycle_path: "runtime-fallback",
        had_active_turn: finish.removed_token.is_some() || release.released,
        queue_depth,
        inflight: InflightDisposition::cleared_if(persistent_inflight_cleared),
        termination_recorded,
        mailbox_foreground_free,
    }
}

pub async fn force_kill_provider_channel_runtime(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
    reason: &str,
    termination_reason_code: &'static str,
) -> Option<RuntimeTurnStopResult> {
    stop_provider_channel_runtime_with_policy(
        registry,
        provider_name,
        channel_id,
        reason,
        discord::TmuxCleanupPolicy::CleanupSession {
            termination_reason_code: Some(termination_reason_code),
        },
    )
    .await
}
