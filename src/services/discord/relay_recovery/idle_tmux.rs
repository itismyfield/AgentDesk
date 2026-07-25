use super::*;

#[derive(Clone, Debug)]
pub(super) struct RelayRecoveryInflightClearPin {
    identity: super::inflight::InflightTurnIdentity,
    finalizer_turn_id: u64,
    updated_at: String,
    save_generation: u64,
}

pub(super) fn load_idle_tmux_reattach_inflight_clear_candidate(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<super::inflight::InflightTurnState> {
    let state = super::inflight::load_inflight_state(provider, channel_id)?;
    if !super::inflight::inflight_state_allows_idle_tmux_repair_state(&state) {
        return None;
    }
    #[cfg(test)]
    if let Some(hook) = idle_tmux_reattach_inflight_candidate_hook()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
    {
        hook(&state);
    }
    Some(state)
}

pub(super) fn capture_idle_tmux_reattach_inflight_clear_pin(
    state: &super::inflight::InflightTurnState,
) -> Option<RelayRecoveryInflightClearPin> {
    let finalizer_turn_id = state.effective_finalizer_turn_id();
    (finalizer_turn_id != 0).then(|| RelayRecoveryInflightClearPin {
        identity: super::inflight::InflightTurnIdentity::from_state(state),
        finalizer_turn_id,
        updated_at: state.updated_at.clone(),
        save_generation: state.save_generation,
    })
}

pub(super) fn clear_idle_tmux_reattach_inflight_if_pinned(
    provider: &ProviderKind,
    channel_id: u64,
    pin: Option<&RelayRecoveryInflightClearPin>,
) -> super::inflight::GuardedClearOutcome {
    let Some(pin) = pin else {
        return super::inflight::GuardedClearOutcome::Missing;
    };
    let outcome = super::inflight::clear_inflight_state_if_matches_identity_generation(
        provider,
        channel_id,
        &pin.identity,
        pin.finalizer_turn_id,
        &pin.updated_at,
        pin.save_generation,
    );
    match outcome {
        super::inflight::GuardedClearOutcome::Cleared
        | super::inflight::GuardedClearOutcome::Missing => {}
        other => warn_idle_tmux_reattach_inflight_clear_refused(provider, channel_id, pin, other),
    }
    outcome
}

fn warn_idle_tmux_reattach_inflight_clear_refused(
    provider: &ProviderKind,
    channel_id: u64,
    pin: &RelayRecoveryInflightClearPin,
    outcome: super::inflight::GuardedClearOutcome,
) {
    let current = super::inflight::load_inflight_state(provider, channel_id);
    tracing::warn!(
        provider = %provider.as_str(),
        channel_id,
        clear_outcome = ?outcome,
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
        "idle tmux stale-turn repair skipped persistent inflight clear because the readiness-time pin no longer matches"
    );
}

pub(super) fn idle_tmux_reattach_clear_status(
    outcome: super::inflight::GuardedClearOutcome,
) -> &'static str {
    match outcome {
        super::inflight::GuardedClearOutcome::Cleared => "cleared_idle_tmux_stale_turn",
        super::inflight::GuardedClearOutcome::IoError => "skipped_idle_tmux_stale_turn_io_error",
        super::inflight::GuardedClearOutcome::Missing => "skipped_idle_tmux_stale_turn_missing",
        super::inflight::GuardedClearOutcome::UserMsgMismatch
        | super::inflight::GuardedClearOutcome::PlannedRestartSkipped
        | super::inflight::GuardedClearOutcome::RebindOriginSkipped => {
            "skipped_idle_tmux_stale_turn_pin_mismatch"
        }
    }
}

fn relay_recovery_cancel_finalize_context() -> super::turn_finalizer::FinalizeContext {
    super::turn_finalizer::FinalizeContext {
        clear_inflight: true,
        allow_completion_cleanup: false,
        drain_voice: false,
        kickoff_queue: true,
        expected_idempotent_guard_miss: false,
    }
}

fn relay_recovery_destructive_cancel_pin(
    decision: &RelayRecoveryDecision,
) -> Option<super::destructive_cancel_gate::DestructiveCancelIdentityPin> {
    Some(
        super::destructive_cancel_gate::DestructiveCancelIdentityPin {
            finalizer_turn_id: decision.affected.finalizer_turn_id?,
            mailbox_active_user_msg_id: decision.affected.mailbox_active_user_msg_id,
            tmux_session_name: decision.affected.tmux_session.clone(),
        },
    )
}

pub(super) fn relay_recovery_probe_snapshot_for_owner(
    shared: &super::SharedData,
    provider: &ProviderKind,
    owner_channel_id: ChannelId,
    decision: &RelayRecoveryDecision,
) -> Result<super::destructive_cancel_gate::DestructiveCancelProbeSnapshot, &'static str> {
    let Some(pin) = relay_recovery_destructive_cancel_pin(decision) else {
        return Err("missing_decision_identity_pin");
    };
    let Some(state) = super::inflight::load_inflight_state(provider, owner_channel_id.get()) else {
        return Err("inflight_missing_before_cancel");
    };
    if !pin.matches_state(&state) {
        return Err("identity_mismatch_before_cancel");
    }
    Ok(
        super::destructive_cancel_gate::DestructiveCancelProbeSnapshot::from_pinned_state(
            shared,
            &state,
            pin,
            owner_channel_id,
        ),
    )
}

pub(super) async fn finalize_cancelled_watcher_owner_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    decision: &RelayRecoveryDecision,
    owner_channel_id: ChannelId,
) -> Option<super::turn_finalizer::FinalizeOutcome> {
    let finalizer_turn_id = decision.affected.finalizer_turn_id?;
    if finalizer_turn_id == 0 {
        return None;
    }
    Some(
        shared
            .turn_finalizer
            .submit_terminal(
                super::turn_finalizer::TurnKey::new(
                    owner_channel_id,
                    finalizer_turn_id,
                    shared.restart.current_generation,
                ),
                provider.clone(),
                super::turn_finalizer::TerminalEvent::Cancel,
                relay_recovery_cancel_finalize_context(),
                shared.clone(),
            )
            .await,
    )
}

pub(in crate::services::discord) fn idle_tmux_repair_ready_for_input(
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session: &str,
) -> bool {
    idle_tmux_repair_ready_for_input_with_pane_probe(
        provider,
        channel_id,
        tmux_session,
        idle_tmux_repair_pane_ready_for_input,
    )
}

pub(in crate::services::discord) fn idle_tmux_repair_state_ready_for_input(
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session: &str,
    state: &super::inflight::InflightTurnState,
) -> bool {
    idle_tmux_repair_snapshot_ready_for_input(
        provider,
        channel_id,
        tmux_session,
        state,
        idle_tmux_repair_pane_ready_for_input,
    )
}

pub(super) fn idle_tmux_repair_pane_ready_for_input(
    tmux_session: &str,
    provider: &ProviderKind,
) -> bool {
    // Pre-existing recovery override for long-frozen Busy JSONL. This is
    // intentionally not `FallbackPaneReadiness`: the override is scoped by
    // `frozen_busy_jsonl_allows_pane_fallback` below.
    crate::services::platform::tmux::capture_pane(tmux_session, -80)
        .map(|pane| {
            crate::services::provider::tmux_capture_indicates_ready_for_input(&pane, provider)
        })
        .unwrap_or(false)
}

pub(super) fn idle_tmux_repair_ready_for_input_with_pane_probe(
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session: &str,
    pane_ready_for_input: impl Fn(&str, &ProviderKind) -> bool,
) -> bool {
    let Some(state) = super::inflight::load_inflight_state(provider, channel_id) else {
        return pane_ready_for_input(tmux_session, provider);
    };
    idle_tmux_repair_snapshot_ready_for_input(
        provider,
        channel_id,
        tmux_session,
        &state,
        pane_ready_for_input,
    )
}

pub(super) fn idle_tmux_repair_snapshot_ready_for_input(
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session: &str,
    state: &super::inflight::InflightTurnState,
    pane_ready_for_input: impl Fn(&str, &ProviderKind) -> bool,
) -> bool {
    let Some(output_path) = state
        .output_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    else {
        return pane_ready_for_input(tmux_session, provider);
    };
    let output_path = Path::new(output_path);
    let Some(structured_ready) = crate::services::tui_turn_state::jsonl_ready_for_input(
        provider,
        state.runtime_kind,
        output_path,
        Some(state.last_offset),
    ) else {
        return pane_ready_for_input(tmux_session, provider);
    };

    match structured_ready {
        crate::services::tui_turn_state::TuiReadyState::Ready => true,
        crate::services::tui_turn_state::TuiReadyState::Busy
            if frozen_busy_jsonl_allows_pane_fallback(output_path) =>
        {
            let pane_ready = pane_ready_for_input(tmux_session, provider);
            if pane_ready {
                tracing::warn!(
                    target: "agentdesk::discord::relay_recovery",
                    provider = provider.as_str(),
                    channel_id,
                    tmux_session,
                    output_path = %output_path.display(),
                    stale_secs = FROZEN_BUSY_JSONL_READY_FALLBACK_AGE.as_secs(),
                    "idle-tmux repair accepted pane-ready fallback for frozen Busy JSONL"
                );
            }
            pane_ready
        }
        crate::services::tui_turn_state::TuiReadyState::Busy
        | crate::services::tui_turn_state::TuiReadyState::Unknown => false,
    }
}

fn frozen_busy_jsonl_allows_pane_fallback(output_path: &Path) -> bool {
    output_file_quiescent_for_duration(output_path, FROZEN_BUSY_JSONL_READY_FALLBACK_AGE)
}

fn output_file_quiescent_for_duration(output_path: &Path, min_age: Duration) -> bool {
    output_file_quiescent_for_duration_at(output_path, min_age, SystemTime::now())
}

fn output_file_quiescent_for_duration_at(
    output_path: &Path,
    min_age: Duration,
    now: SystemTime,
) -> bool {
    let Ok(metadata) = std::fs::metadata(output_path) else {
        return false;
    };
    if !metadata.is_file() || metadata.len() == 0 {
        return false;
    }
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    now.duration_since(modified).is_ok_and(|age| age >= min_age)
}

/// Channel-scoped entry for callers outside the `discord` subtree (e.g. the
/// manual stale-mailbox repair route) that cannot reach the `pub(super)`
/// inflight loader: loads the current row and delegates to the state-based
/// guard below. Absent row → no tail answer to lose → false.
pub(crate) fn channel_has_unrelayed_idle_tmux_tail_answer(
    provider: &ProviderKind,
    channel_id: u64,
) -> bool {
    super::inflight::load_inflight_state(provider, channel_id)
        .is_some_and(|state| idle_tmux_repair_has_unrelayed_tail_answer(&state))
}

/// #3668 F2: detect tail answer text that the destructive idle-tmux clear would
/// permanently lose.
///
/// `idle_tmux_repair_ready_for_input` returns Ready when the JSONL has a
/// terminal envelope after `last_offset` (the offset-behind path in
/// `tui_turn_state::jsonl_ready_for_input`), which means a final answer is
/// already persisted past the inflight watermark. The companion inflight guard
/// (`inflight_state_allows_idle_tmux_repair`) only inspects the streaming
/// `full_response`, so an empty-stream + JSONL-terminal-answer row passes both
/// guards and reaches `clear_inflight_state`, dropping text that
/// `extract_response_from_output_pub(output_path, last_offset)` could still
/// recover. The recovery_engine normal path (extract → relay → clear) never has
/// this asymmetry. This guard reads the same offset slice read-only: if it
/// yields non-empty relayable text, the caller skips the destructive clear and
/// falls through to the non-destructive rebind path (which preserves the
/// inflight/output so normal relay/recovery delivers the text). On extract
/// failure / IO error the function returns false → existing behavior (only the
/// genuinely-empty tail still clears), so this is behavior-preserving.
pub(crate) fn idle_tmux_repair_has_unrelayed_tail_answer(
    state: &super::inflight::InflightTurnState,
) -> bool {
    let Some(output_path) = state
        .output_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    else {
        return false;
    };
    // #3668 codex r3: only treat this as an answer worth preserving when there
    // is TERMINAL completion evidence — a *successful* `result` record after
    // `last_offset`. A hung / desynced turn with only partial assistant text and
    // no terminal result must NOT suppress the destructive idle-clear / force-
    // clean: otherwise the watchdog would skip it every tick forever, since
    // #3645 far-backstop / normal recovery only advance `last_offset` on a
    // terminal success. Requiring the success-result record keeps the guard to
    // genuinely-deliverable, complete-but-unrelayed answers.
    if super::recovery::success_result_end_offset_after_offset(output_path, state.last_offset)
        .is_none()
    {
        return false;
    }
    let tail = super::recovery::extract_response_from_output_pub(output_path, state.last_offset);
    !tail.trim().is_empty()
}
