use super::*;

use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};

#[cfg(test)]
#[path = "tests/unread_tail_seed.rs"]
pub(crate) mod unread_tail_seed;

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
    probe: &super::destructive_cancel_gate::DestructiveCancelProbeSnapshot,
) -> Option<super::turn_finalizer::FinalizeOutcome> {
    let finalizer_turn_id = decision.affected.finalizer_turn_id?;
    if finalizer_turn_id == 0 {
        return None;
    }
    Some(
        shared
            .turn_finalizer
            .submit_terminal_with_claim_snapshot(
                super::turn_finalizer::TurnKey::new(
                    owner_channel_id,
                    finalizer_turn_id,
                    shared.restart.current_generation,
                ),
                provider.clone(),
                super::turn_finalizer::TerminalEvent::Cancel,
                relay_recovery_cancel_finalize_context(),
                Some(probe.finalizer_claim_snapshot.clone()),
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
    // Used only for providers without structured turn-state evidence.
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
    _channel_id: u64,
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
        if crate::services::tui_turn_state::provider_runtime_has_structured_jsonl_turn_state(
            provider,
            state.runtime_kind,
        ) {
            return false;
        }
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
        | crate::services::tui_turn_state::TuiReadyState::Unknown => false,
    }
}

/// #5071 relay-tail S2: how the destructive idle-tmux clear gates read
/// `unread_bytes`, shared by the `ReattachWatcher` legacy manual lane
/// (`apply.rs`) and the manual stale-mailbox repair route (`health_api.rs`) so
/// the two cannot drift — the latter's gate exists to stay aligned with the
/// former.
///
/// The field is three-valued and `None` means UNMEASURED, not measured-empty.
/// `SessionEnrichment::load` produces `None` whenever the tail could not be
/// counted against this row's relay frontier: the row carries no `output_path`,
/// `std::fs::metadata` on that path failed, or the row's tmux session and the
/// watcher binding's disagree so the frontier is not attributable to the row.
///
/// Folding `None` to `0` opened these destructive branches on no evidence at
/// all, because the companion tail guard
/// [`idle_tmux_repair_has_unrelayed_tail_answer`] is blind under the same
/// conditions — it returns `false` for an absent/empty `output_path` and for
/// any extract failure. Two blind witnesses do not compose into a proof, so
/// only `Some(0)` counts as one.
///
/// The name overstates the arithmetic. `Some(0)` says that
/// `capture.saturating_sub(last_relay_offset)` was 0 in
/// `SessionEnrichment::load`, which is also the answer whenever the relay
/// frontier RUNS AHEAD of the capture offset — a rotated or truncated
/// transcript reads drained by this measure without its tail having been
/// relayed. And it is a snapshot-time fact: that poll's `std::fs::metadata`
/// succeeded and its length did not exceed the frontier. A read or parse
/// failure at apply time is outside it and belongs to the companion tail guard,
/// which is the other reason the two prove "nothing left to preserve" only in
/// conjunction.
///
/// It does NOT prove attribution: `relay_state_matches_inflight`
/// (`health::session_enrichment::load`) compares session names only when the
/// row and the live watcher binding BOTH carry one (`_ => true` otherwise), so
/// a frontier left behind by another session's watcher can still surface here
/// as `Some(0)` when either side is unnamed.
pub(crate) fn unread_tail_is_proven_drained(unread_bytes: Option<u64>) -> bool {
    unread_bytes == Some(0)
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

/// The decision sites that read `unread_bytes` as destructive permission, as the refusal record names them.
pub(crate) const UNREAD_TAIL_SITE_MANUAL_REATTACH: &str = "manual_reattach_idle_clear";
pub(crate) const UNREAD_TAIL_SITE_STALE_MAILBOX: &str = "stale_mailbox_idle_tmux";
pub(crate) const UNREAD_TAIL_SITE_WATCHDOG_EXPLICIT_BACKGROUND: &str =
    "watchdog_explicit_background";

/// Why a tail is UNMEASURED, from the published coordinates; `None` when measured.
/// A consumer may name the cause but never rebuild a tail from it.
pub(crate) fn unmeasured_tail_reason(
    unread_bytes: Option<u64>,
    last_capture_offset: Option<u64>,
    last_relay_offset: u64,
) -> Option<&'static str> {
    if unread_bytes.is_some() {
        return None;
    }
    Some(match last_capture_offset {
        None => "tail_not_measured",
        Some(capture) if capture < last_relay_offset => "saturated_tail",
        Some(capture) if capture == last_relay_offset => "zero_not_attributable",
        Some(_) => "unattributed_tail",
    })
}

type UnmeasuredTailSite = (String, u64, &'static str);
/// A graded refusal's episode: the mailbox turn (user message id, nonce).
type UnmeasuredTailEpisode = (Option<u64>, Option<String>);

/// Recent episodes graded per channel and site, so a wedge a site keeps
/// refusing is recorded once per episode rather than once per call.
const UNMEASURED_TAIL_EPISODES_KEPT: usize = 8;
static UNMEASURED_TAIL_REFUSALS_GRADED: LazyLock<
    Mutex<HashMap<UnmeasuredTailSite, VecDeque<UnmeasuredTailEpisode>>>,
> = LazyLock::new(Default::default);

/// The manual reattach idle-clear's tail conjunct; an UNMEASURED refusal keeps
/// the turn and is recorded when every other conjunct admits.
pub(super) fn reattach_idle_clear_tail_admits(
    provider: &ProviderKind,
    decision: &RelayRecoveryDecision,
    tmux_session: &str,
) -> bool {
    let evidence = &decision.evidence;
    if evidence.unread_bytes.is_some() {
        return unread_tail_is_proven_drained(evidence.unread_bytes); // a backlog is no wedge
    }
    // Judged on a read-only load so the refusal writes nothing.
    let (channel, ready) = (decision.channel_id, idle_tmux_repair_pane_ready_for_input);
    let others_admit = super::inflight::load_inflight_state_read_only(provider, channel)
        .filter(super::inflight::inflight_state_allows_idle_tmux_repair_state)
        .filter(|row| {
            idle_tmux_repair_snapshot_ready_for_input(provider, channel, tmux_session, row, ready)
        })
        .is_some_and(|state| !idle_tmux_repair_has_unrelayed_tail_answer(&state));
    if !others_admit {
        return false;
    }
    record_unmeasured_tail_refusal(
        provider,
        decision.channel_id,
        UNREAD_TAIL_SITE_MANUAL_REATTACH,
        decision.affected.tmux_session.as_deref(),
        (
            evidence.unread_bytes,
            evidence.last_capture_offset,
            evidence.last_relay_offset,
        ),
        (evidence.watcher_attached, evidence.tmux_alive),
        // The row loaded here is not the observation the tail came from, so it never keys.
        (
            decision.affected.mailbox_active_user_msg_id,
            decision.affected.mailbox_active_turn_nonce.clone(),
        ),
    );
    false
}

/// The stale-mailbox repair route's tail conjunct, kept aligned with the
/// `ReattachWatcher` lane; records the refusal when `others_admit`.
pub(crate) fn stale_mailbox_idle_tail_admits(
    provider: &ProviderKind,
    snapshot: &super::health::WatcherStateSnapshot,
    others_admit: bool,
) -> bool {
    let admits = unread_tail_is_proven_drained(snapshot.unread_bytes);
    if !admits && others_admit {
        let channel_id = snapshot.relay_health.channel_id;
        let site = UNREAD_TAIL_SITE_STALE_MAILBOX;
        record_unmeasured_tail_refusal_for_snapshot(provider, channel_id, snapshot, site);
    }
    admits
}

/// [`unmeasured_tail_reason`] read off a watcher-state snapshot.
pub(crate) fn unmeasured_tail_of(
    snapshot: &super::health::WatcherStateSnapshot,
) -> Option<&'static str> {
    let relay = &snapshot.relay_health;
    unmeasured_tail_reason(
        relay.unread_bytes,
        relay.last_capture_offset,
        relay.last_relay_offset,
    )
}

/// The snapshot sites' record, for a caller that already knows the tail alone refused.
pub(crate) fn record_unmeasured_tail_refusal_for_snapshot(
    provider: &ProviderKind,
    channel_id: u64,
    snapshot: &super::health::WatcherStateSnapshot,
    site: &'static str,
) {
    let relay = &snapshot.relay_health;
    let mailbox = (
        snapshot.mailbox_active_user_msg_id,
        snapshot.mailbox_active_turn_nonce.clone(),
    );
    record_unmeasured_tail_refusal(
        provider,
        channel_id,
        site,
        snapshot.tmux_session.as_deref(),
        (
            relay.unread_bytes,
            relay.last_capture_offset,
            relay.last_relay_offset,
        ),
        (relay.watcher_attached, relay.tmux_alive),
        mailbox,
    );
}

fn record_unmeasured_tail_refusal(
    provider: &ProviderKind,
    channel_id: u64,
    site: &'static str,
    tmux_session: Option<&str>,
    (unread_bytes, last_capture_offset, last_relay_offset): (Option<u64>, Option<u64>, u64),
    (watcher_attached, tmux_alive): (bool, Option<bool>),
    mailbox: UnmeasuredTailEpisode,
) {
    // A measured backlog is the invariant working, not a wedge.
    let Some(decided_by) =
        unmeasured_tail_reason(unread_bytes, last_capture_offset, last_relay_offset)
    else {
        return;
    };
    let user_msg_id = mailbox.0;
    // Without a mailbox turn nothing names the episode, so the refusal is never folded.
    if mailbox != (None, None) {
        let mut graded = UNMEASURED_TAIL_REFUSALS_GRADED
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let recent = graded
            .entry((provider.as_str().to_string(), channel_id, site))
            .or_default();
        if recent.contains(&mailbox) {
            return;
        }
        if recent.len() == UNMEASURED_TAIL_EPISODES_KEPT {
            recent.pop_front();
        }
        recent.push_back(mailbox);
    }
    crate::services::observability::record_invariant_check(
        false,
        crate::services::observability::InvariantViolation {
            provider: Some(provider.as_str()),
            channel_id: Some(channel_id),
            dispatch_id: None,
            session_key: tmux_session,
            turn_id: None,
            invariant: crate::services::observability::LIVE_TURN_PROVEN_BY_PROGRESS_INVARIANT,
            code_location: "src/services/discord/relay_recovery/idle_tmux.rs:record_unmeasured_tail_refusal",
            message: "destructive idle clear refused because its unread tail was never measured",
            details: serde_json::json!({
                "decided_by": decided_by,
                "site": site,
                "last_capture_offset": last_capture_offset,
                "last_relay_offset": last_relay_offset,
                "watcher_attached": watcher_attached,
                "tmux_alive": tmux_alive,
                "mailbox_active_user_msg_id": user_msg_id,
                "retired": false,
            }),
        },
    );
}
