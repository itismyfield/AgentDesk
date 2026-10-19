//! Guarded stale-turn repair and finalized leak inflight cleanup.

use super::*;

use std::time::Instant;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct IdleTmuxStaleTurnInflightPin {
    pub(super) identity: discord::inflight::InflightTurnIdentity,
    turn_nonce: Option<String>,
    finalizer_turn_id: u64,
    updated_at: String,
    pub(super) save_generation: u64,
}

#[cfg(test)]
pub(super) type IdleTmuxStaleTurnInflightCandidateHook =
    Arc<dyn Fn(&discord::inflight::InflightTurnState) + Send + Sync>;

#[cfg(test)]
pub(super) type IdleTmuxStaleTurnPostClearHook =
    Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;

#[cfg(test)]
pub(super) fn idle_tmux_stale_turn_inflight_candidate_hook()
-> &'static std::sync::Mutex<Option<IdleTmuxStaleTurnInflightCandidateHook>> {
    static HOOK: std::sync::OnceLock<
        std::sync::Mutex<Option<IdleTmuxStaleTurnInflightCandidateHook>>,
    > = std::sync::OnceLock::new();
    HOOK.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
pub(super) fn idle_tmux_stale_turn_post_clear_hook()
-> &'static std::sync::Mutex<Option<IdleTmuxStaleTurnPostClearHook>> {
    static HOOK: std::sync::OnceLock<std::sync::Mutex<Option<IdleTmuxStaleTurnPostClearHook>>> =
        std::sync::OnceLock::new();
    HOOK.get_or_init(|| std::sync::Mutex::new(None))
}

fn load_idle_tmux_stale_turn_inflight_clear_candidate(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<discord::inflight::InflightTurnState> {
    let state = discord::inflight::load_inflight_state(provider, channel_id)?;
    if !discord::inflight::inflight_state_allows_idle_tmux_repair_state(&state) {
        return None;
    }
    #[cfg(test)]
    if let Some(hook) = idle_tmux_stale_turn_inflight_candidate_hook()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
    {
        hook(&state);
    }
    Some(state)
}

pub(super) fn capture_idle_tmux_stale_turn_inflight_pin(
    state: &discord::inflight::InflightTurnState,
) -> Option<IdleTmuxStaleTurnInflightPin> {
    let finalizer_turn_id = state.effective_finalizer_turn_id();
    (finalizer_turn_id != 0).then(|| IdleTmuxStaleTurnInflightPin {
        identity: discord::inflight::InflightTurnIdentity::from_state(state),
        turn_nonce: state.turn_nonce.clone(),
        finalizer_turn_id,
        updated_at: state.updated_at.clone(),
        save_generation: state.save_generation,
    })
}

pub(super) fn clear_idle_tmux_stale_turn_inflight_if_pinned(
    provider: &ProviderKind,
    channel_id: u64,
    pin: Option<&IdleTmuxStaleTurnInflightPin>,
) -> discord::inflight::GuardedClearOutcome {
    let Some(pin) = pin else {
        return discord::inflight::GuardedClearOutcome::Missing;
    };
    let outcome = discord::inflight::clear_inflight_state_if_matches_identity_generation(
        provider,
        channel_id,
        &pin.identity,
        pin.finalizer_turn_id,
        &pin.updated_at,
        pin.save_generation,
    );
    match outcome {
        discord::inflight::GuardedClearOutcome::Cleared => {}
        other => {
            let current = discord::inflight::load_inflight_state(provider, channel_id);
            tracing::warn!(
                target: "agentdesk::services::discord::health::recovery",
                provider = %provider.as_str(),
                channel_id,
                clear_outcome = ?other,
                expected_user_msg_id = pin.identity.user_msg_id,
                expected_finalizer_turn_id = pin.finalizer_turn_id,
                expected_updated_at = %pin.updated_at,
                expected_save_generation = pin.save_generation,
                current_user_msg_id = current.as_ref().map(|state| state.user_msg_id).unwrap_or(0),
                current_finalizer_turn_id = current
                    .as_ref()
                    .map(|state| state.effective_finalizer_turn_id())
                    .unwrap_or(0),
                current_updated_at = %current
                    .as_ref()
                    .map(|state| state.updated_at.as_str())
                    .unwrap_or("<missing>"),
                current_save_generation = current.as_ref().map(|state| state.save_generation).unwrap_or(0),
                "idle tmux stale-turn repair skipped persistent inflight clear because the readiness-time pin was not cleared"
            );
        }
    }
    outcome
}
pub async fn clear_idle_tmux_stale_turn(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: u64,
    tmux_session: &str,
    stop_source: &'static str,
) -> Option<IdleTmuxStaleTurnRepairResult> {
    let repair_started_at = Instant::now();
    let provider = ProviderKind::from_str(provider_name)?;
    let _recovery = discord::live_bridge::try_recovery(&provider, channel_id).ok()?;
    let inflight_clear_state =
        load_idle_tmux_stale_turn_inflight_clear_candidate(&provider, channel_id)?;
    if !crate::services::discord::relay_recovery::idle_tmux_repair_state_ready_for_input(
        &provider,
        channel_id,
        tmux_session,
        &inflight_clear_state,
    ) {
        return None;
    }
    if crate::services::discord::relay_recovery::idle_tmux_repair_has_unrelayed_tail_answer(
        &inflight_clear_state,
    ) {
        tracing::warn!(
            target: "agentdesk::services::discord::health::recovery",
            provider = %provider.as_str(),
            channel_id,
            tmux_session,
            "idle tmux stale-turn repair skipped mailbox/runtime teardown because helper-load found an unrelayed tail answer"
        );
        return None;
    }
    let inflight_pin = capture_idle_tmux_stale_turn_inflight_pin(&inflight_clear_state);

    let channel_id = ChannelId::new(channel_id);
    let shared = shared_for_provider(registry, &provider, channel_id).await?;
    let inflight_clear_outcome = clear_idle_tmux_stale_turn_inflight_if_pinned(
        &provider,
        channel_id.get(),
        inflight_pin.as_ref(),
    );
    if inflight_clear_outcome != discord::inflight::GuardedClearOutcome::Cleared {
        tracing::warn!(
            target: "agentdesk::services::discord::health::recovery",
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            clear_outcome = ?inflight_clear_outcome,
            "idle tmux stale-turn repair skipped mailbox/runtime teardown because guarded persistent inflight clear did not succeed"
        );
        return None;
    }
    let owned_role_override =
        discord::turn_finalizer::cleanup::snapshot_role_override(&shared, channel_id);
    #[cfg(test)]
    {
        // Bind the cloned hook first so the mutex guard (a non-Send temporary
        // in an `if let` scrutinee would live across the await) drops here.
        let hook = idle_tmux_stale_turn_post_clear_hook()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook().await;
        }
    }
    let expected_user_msg_id = inflight_pin
        .as_ref()
        .map(|pin| pin.identity.user_msg_id)
        .unwrap_or(0);
    let finish = if expected_user_msg_id != 0 {
        discord::mailbox_finish_turn_if_matches_episode_started_before(
            &shared,
            &provider,
            channel_id,
            MessageId::new(expected_user_msg_id),
            inflight_pin.as_ref().and_then(|pin| pin.turn_nonce.clone()),
            repair_started_at,
        )
        .await
    } else {
        let snapshot = discord::mailbox_snapshot(&shared, channel_id).await;
        discord::FinishTurnResult {
            removed_token: None,
            has_pending: !snapshot.intervention_queue.is_empty(),
            mailbox_online: shared.mailbox_peek(channel_id).is_some(),
            queue_exit_events: Vec::new(),
            persistence_error: None,
        }
    };
    let runtime_session_cleared = if finish.removed_token.is_some() {
        apply_runtime_hard_stop_cleanup(
            &shared,
            &provider,
            channel_id,
            &finish,
            owned_role_override,
            stop_source,
            false,
        )
        .await
    } else {
        false
    };

    Some(IdleTmuxStaleTurnRepairResult {
        had_active_turn: finish.removed_token.is_some(),
        has_pending_queue: finish.has_pending,
        persistent_inflight_cleared: true,
        runtime_session_cleared,
    })
}

/// #3925: finalize the inflight turn-state for a turn whose completed answer was
/// just delivered out-of-band by [`maybe_recover_completed_stale_leak`]. Mirrors
/// the identity-guarded clear the normal turn-completion path uses
/// (`turn_finalizer::do_finalize` step A / the startup recovery branches): a real
/// `user_msg_id` clears via `clear_inflight_state_if_matches` (a newer turn's row
/// yields `UserMsgMismatch` and is preserved); a zero-id-owned recovery /
/// TUI-direct turn clears its own zero-id row via the zero-owned guard.
pub(super) fn clear_recovered_leak_inflight(
    provider: &ProviderKind,
    channel_id: ChannelId,
    state: &discord::inflight::InflightTurnState,
) -> discord::inflight::GuardedClearOutcome {
    let Some(root) = discord::inflight::inflight_runtime_root() else {
        return discord::inflight::GuardedClearOutcome::Missing;
    };
    clear_recovered_leak_inflight_in_root(&root, provider, channel_id, state)
}

/// Root-explicit variant of [`clear_recovered_leak_inflight`] for hermetic unit
/// tests (mirrors inflight.rs's own `_in_root` test convention).
pub(super) fn clear_recovered_leak_inflight_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: ChannelId,
    state: &discord::inflight::InflightTurnState,
) -> discord::inflight::GuardedClearOutcome {
    if state.user_msg_id != 0 {
        discord::inflight::clear_inflight_state_if_matches_in_root(
            root,
            provider,
            channel_id.get(),
            state.user_msg_id,
        )
    } else {
        discord::inflight::clear_inflight_state_if_matches_zero_owned_in_root(
            root,
            provider,
            channel_id.get(),
        )
    }
}
