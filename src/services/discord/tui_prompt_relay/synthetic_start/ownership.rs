//! Synthetic turn ownership observations and matching predicates.

use super::*;

#[derive(Debug)]
pub(in crate::services::discord::tui_prompt_relay) struct TuiDirectSyntheticTurnClaim {
    pub(in crate::services::discord::tui_prompt_relay) relay_owner: ExternalInputRelayOwner,
    pub(in crate::services::discord::tui_prompt_relay) claimed: bool,
    // #3154 P1 (timestamp-anchor output loss): the post-drain EOF offset the claim
    // seeded into this turn's inflight `turn_start_offset`. The deferred-BridgeAdapter
    // worker anchors its bridge tail to THIS byte boundary instead of a `Utc::now()`
    // scan, which can skip bytes written during the deferred-claim wait window.
    pub(in crate::services::discord::tui_prompt_relay) turn_start_offset: u64,
}

/// #3358 — offset-authority handover for synthetic inflight creation.
///
/// A synthetic is born at the lagging `relay_last_offset()`; when that lags the
/// watcher's delivered frontier (#3017), a later same-identity re-claim re-seeds
/// the row backward → trips the monotonicity guards (the incident). CARRY-FORWARD
/// the frontier so the synthetic is born at/above every delivered byte.
///
/// #3358 round 2 — GATED: `committed_relay_offset` is `Some` ONLY when the gating
/// accessor proved the watermark belongs to the CURRENT wrapper. After a restart
/// the stream resets to 0; a stale PREVIOUS-generation watermark must NOT clamp
/// forward — that marks future bytes below it as delivered → CONTENT SKIP (worse
/// than the original ERROR-only bug). On mismatch the frontier is `None` and we
/// fall back to pre-fix seeding (`relay_last_offset` only): the rare monotonicity
/// ERROR beats a skip, and backward writes outside this handover stay guarded.
pub(in crate::services::discord) fn synthetic_start_offset_carry_forward(
    relay_last_offset: u64,
    committed_relay_offset: Option<u64>,
) -> u64 {
    relay_last_offset.max(committed_relay_offset.unwrap_or(0))
}

pub(in crate::services::discord::tui_prompt_relay) fn tui_direct_watcher_can_own_output(
    watchers: &super::super::super::TmuxWatcherRegistry,
    tmux_session_name: &str,
    output_path: Option<&Path>,
) -> bool {
    let watcher_alive = watchers
        .tmux_session_live_for_relay(tmux_session_name)
        .is_some_and(|live| live);
    if !watcher_alive {
        return false;
    }
    match output_path {
        Some(output_path) => watchers
            .watcher_output_path(tmux_session_name)
            .is_some_and(|watcher_path| Path::new(&watcher_path) == output_path),
        None => true,
    }
}

/// Owner stamped on a new TUI-direct synthetic row. The sink only gets a session-bound
/// turn's frames from a live tmux watcher reading this output, else the bridge tail delivers.
pub(in crate::services::discord::tui_prompt_relay) fn tui_direct_synthetic_relay_owner(
    watcher_can_own: bool,
    session_bound_discord_delivery_enabled: bool,
    live_producer_present: bool,
    covers_output: bool,
) -> ExternalInputRelayOwner {
    if watcher_can_own {
        ExternalInputRelayOwner::TmuxWatcher
    } else if session_bound_discord_delivery_enabled && live_producer_present && covers_output {
        ExternalInputRelayOwner::SessionBoundRelay
    } else {
        ExternalInputRelayOwner::BridgeAdapter
    }
}

pub(in crate::services::discord::tui_prompt_relay) fn tui_direct_synthetic_inflight_active_for_prompt(
    provider: &str,
    channel_id: ChannelId,
    tmux_session_name: &str,
) -> bool {
    let Some(provider) = ProviderKind::from_str(provider) else {
        return false;
    };
    tui_direct_synthetic_inflight_matches(
        super::super::super::inflight::load_inflight_state(&provider, channel_id.get()).as_ref(),
        tmux_session_name,
    )
}

pub(in crate::services::discord::tui_prompt_relay) fn tui_direct_synthetic_inflight_matches(
    state: Option<&InflightTurnState>,
    tmux_session_name: &str,
) -> bool {
    state.is_some_and(|state| {
        state.turn_source == TurnSource::ExternalInput
            && state.tmux_session_name.as_deref() == Some(tmux_session_name)
    })
}

fn tui_direct_watcher_synthetic_inflight_shape_matches(
    state: Option<&InflightTurnState>,
    tmux_session_name: &str,
) -> bool {
    state.is_some_and(|state| {
        state.turn_source == TurnSource::ExternalInput
            && state.tmux_session_name.as_deref() == Some(tmux_session_name)
            && state.effective_relay_owner_kind() == RelayOwnerKind::Watcher
    })
}

/// A passive synthetic claim has resolved away from the BridgeAdapter once
/// either the tmux watcher or the session-bound relay owns terminal delivery.
/// The Codex/Claude idle observers record their provisional BridgeAdapter lease
/// before the synthetic-start observer resolves this owner; they must wait for
/// BOTH non-bridge outcomes or they can spawn a second Discord surface beside a
/// live session-bound relay (#4455).
pub(in crate::services::discord::tui_prompt_relay) fn tui_direct_synthetic_non_bridge_owner_matches(
    state: Option<&InflightTurnState>,
    tmux_session_name: &str,
    session_bound_relay_has_live_producer: bool,
) -> bool {
    state.is_some_and(|state| {
        state.turn_source == TurnSource::ExternalInput
            && state.tmux_session_name.as_deref() == Some(tmux_session_name)
            && match state.effective_relay_owner_kind() {
                RelayOwnerKind::Watcher => true,
                RelayOwnerKind::SessionBoundRelay => session_bound_relay_has_live_producer,
                _ => false,
            }
    })
}

pub(in crate::services::discord) fn tui_direct_watcher_synthetic_inflight_matches(
    state: Option<&InflightTurnState>,
    tmux_session_name: &str,
    current_offset: u64,
) -> bool {
    state.is_some_and(|state| {
        tui_direct_watcher_synthetic_inflight_shape_matches(Some(state), tmux_session_name)
            && state.turn_start_offset.unwrap_or(state.last_offset) < current_offset
    })
}

#[cfg(unix)]
pub(in crate::services::discord::tui_prompt_relay) fn codex_ownerless_external_input_inflight_needs_rollout_recovery(
    state: &InflightTurnState,
    tmux_session_name: &str,
) -> bool {
    if state.turn_source != TurnSource::ExternalInput
        || state.runtime_kind != Some(RuntimeHandoffKind::CodexTui)
        || state.effective_relay_owner_kind() != RelayOwnerKind::None
        || state.tmux_session_name.as_deref() != Some(tmux_session_name)
        || state.injected_prompt_message_id.is_none()
        || state.current_msg_id != 0
        || state.response_sent_offset != 0
        || !state.full_response.trim().is_empty()
        || state.last_watcher_relayed_offset.is_some()
        || state.terminal_delivery_committed
    {
        return false;
    }
    // At this point the inflight is ownerless and no Discord delivery has ever
    // started. Recovery must run whether `output_path` is stale/missing or
    // already points at the live rollout: an earlier deploy can interrupt after
    // repairing the path but before the bridge posts a response.
    true
}

#[cfg(unix)]
pub(in crate::services::discord::tui_prompt_relay) async fn wait_for_tui_direct_synthetic_non_bridge_claim(
    watchers: &super::super::super::TmuxWatcherRegistry,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
) -> bool {
    let deadline = tokio::time::Instant::now() + TUI_DIRECT_SYNTHETIC_CLAIM_WAIT;
    loop {
        let state = super::super::super::inflight::load_inflight_state(provider, channel_id.get());
        let output = state
            .as_ref()
            .and_then(|row| row.output_path.as_deref())
            .map(Path::new);
        // The sink only hears frames a live tmux watcher reading the row's output forwards.
        let session_bound_relay_has_live_producer =
            crate::services::cluster::relay_producer_registry::global_relay_producer_registry()
                .get_live_producer(tmux_session_name)
                .is_some()
                && claim::tui_direct_watcher_covers_output(watchers, tmux_session_name, output);
        if tui_direct_synthetic_non_bridge_owner_matches(
            state.as_ref(),
            tmux_session_name,
            session_bound_relay_has_live_producer,
        ) {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep(TUI_DIRECT_SYNTHETIC_CLAIM_POLL.min(deadline - now)).await;
    }
}
