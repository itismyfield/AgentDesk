//! Terminal delivery routing, lease guards, and trace context.

use super::*;

pub(in crate::services::discord) fn session_bound_discord_relay_can_own_terminal_delivery(
    inflight: Option<&InflightTurnState>,
    tmux_session_name: &str,
) -> bool {
    if tmux_session_name.trim().is_empty() {
        return false;
    }
    let Some(state) = inflight else {
        return true;
    };
    if state.tmux_session_name.as_deref() != Some(tmux_session_name) {
        return false;
    }
    // A normal Discord-origin inflight already has the watcher as terminal owner;
    // letting the sink (still attached to the same JSONL) deliver would double-post.
    // Only rebind/adopted rows are no real foreground turn; scheduled wakeups / idle
    // background output reach this path with no inflight at all.
    matches!(
        state.effective_relay_owner_kind(),
        RelayOwnerKind::SessionBoundRelay
    ) || state.rebind_origin
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionBoundTerminalDeliveryRoute {
    NewMessage,
    PlaceholderEdit(MessageId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionBoundTerminalDeliveryRouteDecision {
    Route(SessionBoundTerminalDeliveryRoute),
    Skipped,
}

pub(super) fn session_bound_terminal_delivery_route(
    inflight: Option<&InflightTurnState>,
    tmux_session_name: &str,
) -> Option<SessionBoundTerminalDeliveryRoute> {
    if tmux_session_name.trim().is_empty() {
        return None;
    }
    let Some(state) = inflight else {
        return Some(SessionBoundTerminalDeliveryRoute::NewMessage);
    };
    if !session_bound_discord_relay_can_own_terminal_delivery(Some(state), tmux_session_name) {
        return None;
    }
    if matches!(
        state.effective_relay_owner_kind(),
        RelayOwnerKind::SessionBoundRelay
    ) && matches!(state.turn_source, TurnSource::ExternalInput)
    {
        return Some(SessionBoundTerminalDeliveryRoute::NewMessage);
    }
    if !state.rebind_origin && state.current_msg_id != 0 {
        return Some(SessionBoundTerminalDeliveryRoute::PlaceholderEdit(
            MessageId::new(state.current_msg_id),
        ));
    }
    Some(SessionBoundTerminalDeliveryRoute::NewMessage)
}

pub(super) fn session_bound_terminal_delivery_route_or_skip(
    inflight: Option<&InflightTurnState>,
    tmux_session_name: &str,
    provider: &ProviderKind,
    channel_id: u64,
) -> Result<SessionBoundTerminalDeliveryRoute, String> {
    session_bound_terminal_delivery_route(inflight, tmux_session_name).ok_or_else(|| {
        format!(
            "session-bound terminal delivery route skipped for provider={} channel={} tmux_session={}",
            provider.as_str(),
            channel_id,
            tmux_session_name
        )
    })
}

pub(super) fn session_bound_terminal_delivery_route_decision_with_lease(
    inflight: Option<&InflightTurnState>,
    tmux_session_name: &str,
    provider: &ProviderKind,
    channel_id: u64,
    observed_lease: Option<&crate::services::tui_prompt_dedupe::ExternalInputRelayLease>,
) -> SessionBoundTerminalDeliveryRouteDecision {
    if session_bound_external_lease_blocks_delivery(observed_lease) {
        return SessionBoundTerminalDeliveryRouteDecision::Skipped;
    }
    match session_bound_terminal_delivery_route_or_skip(
        inflight,
        tmux_session_name,
        provider,
        channel_id,
    ) {
        Ok(route) => SessionBoundTerminalDeliveryRouteDecision::Route(route),
        Err(_) => SessionBoundTerminalDeliveryRouteDecision::Skipped,
    }
}

fn session_bound_external_lease_blocks_delivery(
    observed_lease: Option<&crate::services::tui_prompt_dedupe::ExternalInputRelayLease>,
) -> bool {
    let Some(lease) = observed_lease else {
        return false;
    };
    // #3041 P1-4 / §4-④: the external_input lease is now "input dedup only". A
    // FOREIGN-owner lease (BridgeAdapter/TuiPromptRelay/TmuxWatcher) names the
    // OTHER subsystem owning this terminal delivery → still defer (routing, not the
    // self-block behind the ~10min stall). An `Unassigned`/`SessionBoundRelay` lease
    // is THIS sink's own marker → must NOT block our delivery (serialization now
    // belongs to the `DeliveryLeaseCell` B2 gate + per-sequence ACK + reconciliation).
    !matches!(
        lease.relay_owner,
        crate::services::tui_prompt_dedupe::ExternalInputRelayOwner::Unassigned
            | crate::services::tui_prompt_dedupe::ExternalInputRelayOwner::SessionBoundRelay
    )
}

/// RAII guard releasing the session-bound `external_input_relay_lease` on EVERY exit of
/// `deliver_response` (Ok/Err/`?`/503/panic). #3041 P1-4 (§4-④, fixes the #2955 leak:
/// pre-P1-4 only Ok branches cleared, so an Err/`?`/503 stranded the lease for the 600s
/// TTL → blocked the next delivery ~10min). NO-CLOBBER: captures the UNIQUE `generation`
/// of the route-observed lease and clears via the generation-matched helper, so a newer
/// turn re-taking the key (even a value-identical `Unassigned`) survives (mirrors
/// `TuiDirectExternalInputLeaseGuard`).
pub(super) struct SessionBoundExternalInputLeaseGuard {
    provider: ProviderKind,
    tmux_session_name: String,
    channel_id: u64,
    /// `generation` of the recorded lease this guard armed with. Drop clears ONLY
    /// this exact generation.
    pub(super) generation: u64,
}

impl SessionBoundExternalInputLeaseGuard {
    /// Arm a guard IFF the route-observed lease (`observed_lease`, a SINGLE shared
    /// read) is an `Unassigned`/`SessionBoundRelay` input lease for this target.
    /// Foreign-owner leases (not ours) and no-lease deliveries return `None` (inert).
    /// Capturing the generation from the SAME read closes the arm-time TOCTOU.
    pub(super) fn arm_with_observed_lease(
        provider: &ProviderKind,
        channel_id: u64,
        tmux_session_name: &str,
        observed_lease: Option<&crate::services::tui_prompt_dedupe::ExternalInputRelayLease>,
    ) -> Option<Self> {
        let lease = observed_lease?;
        if !matches!(
            lease.relay_owner,
            crate::services::tui_prompt_dedupe::ExternalInputRelayOwner::Unassigned
                | crate::services::tui_prompt_dedupe::ExternalInputRelayOwner::SessionBoundRelay
        ) {
            return None;
        }
        Some(Self {
            provider: provider.clone(),
            tmux_session_name: tmux_session_name.to_string(),
            channel_id,
            generation: lease.generation,
        })
    }
}

impl Drop for SessionBoundExternalInputLeaseGuard {
    fn drop(&mut self) {
        // Compare-and-clear by generation: release only if the CURRENT lease for this
        // key is STILL the one we armed with. A newer turn's lease that re-took this key
        // — even a value-identical `Unassigned` — has a different generation → survives.
        crate::services::tui_prompt_dedupe::clear_external_input_relay_lease_if_generation_matches(
            self.provider.as_str(),
            &self.tmux_session_name,
            self.channel_id,
            self.generation,
        );
    }
}

/// #3151: RAII in-flight sink-delivery marker on the per-channel
/// [`super::DeliveryLeaseCell`], acquired as [`super::LeaseHolder::Sink`] for the SAME
/// `(channel, turn, [start,end))` the watcher's §3.2 reconciliation computes, BEFORE the
/// POST; a [`super::DeliveryLeaseHeartbeat`] renews the deadline so the watcher reads
/// `Leased{Sink, fresh}` and WAITS instead of re-sending (slow-sink dup). RECLAIMABLE: a
/// crashed sink stops renewing → the watcher reclaims within ~one deadline (no black-hole).
/// CLEAR ordering (SUCCESS): advance committed FIRST (`advance_after_confirmed_post`) THEN
/// [`Self::commit`] → watcher reads `committed >= end` → Skip. EVERY exit Drop RELEASES
/// (full-identity → stale no-ops); a never-committed failure leaves `Unleased`, committed
/// NOT advanced → watcher SendFull.
pub(super) struct SinkDeliveryLeaseGuard {
    cell: Arc<super::DeliveryLeaseCell>,
    key: super::super::DeliveryLeaseKey,
    start: u64,
    end: u64,
    /// The in-flight heartbeat; aborted on Drop (mirrors the watcher's RAII).
    _heartbeat: super::DeliveryLeaseHeartbeat,
}

impl SinkDeliveryLeaseGuard {
    /// Self-heal a dead PRIOR holder, then CAS-acquire as `LeaseHolder::Sink` for
    /// `(turn, [start,end))`. `Some` (spawning the heartbeat) only when the acquire wins;
    /// `None` means another holder owns the range and the caller must return NotDelivered
    /// without reaching transport.
    pub(super) fn acquire(
        cell: &Arc<super::DeliveryLeaseCell>,
        key: super::super::DeliveryLeaseKey,
        start: u64,
        end: u64,
    ) -> Option<Self> {
        // Mirror the watcher's self-healing acquire (tmux_watcher.rs:8594): reclaim an
        // EXPIRED prior holder so a stale dead lease can't lose this acquire.
        cell.reclaim_if_expired(super::super::lease_now_ms());
        let acquired = cell.try_acquire(
            key.clone(),
            super::LeaseHolder::Sink,
            start,
            end,
            super::super::lease_now_ms().saturating_add(super::super::DELIVERY_LEASE_DEADLINE_MS),
        );
        if !acquired {
            return None;
        }
        let heartbeat = super::DeliveryLeaseHeartbeat::spawn(
            cell.clone(),
            super::LeaseHolder::Sink,
            key.clone(),
        );
        Some(Self {
            cell: cell.clone(),
            key,
            start,
            end,
            _heartbeat: heartbeat,
        })
    }

    /// Terminal-decision commit, AFTER the advance was attempted: `outcome` reflects
    /// whether it ACTUALLY happened — `Delivered` only when the offset advanced (so the
    /// watcher reads `committed >= end` → Skip), else `NotDelivered` (offset `< end` →
    /// the watcher re-sends → SendFull, no black-hole). Full-identity compare-and-X →
    /// a stale older-turn clear no-ops. Drop still releases.
    pub(super) fn commit(&self, outcome: super::super::LeaseOutcome) {
        self.cell.commit(
            super::LeaseHolder::Sink,
            self.key.clone(),
            self.start,
            self.end,
            outcome,
        );
    }
}

impl Drop for SinkDeliveryLeaseGuard {
    fn drop(&mut self) {
        // Release on EVERY exit. `release` is valid from `Leased` (failure) and `Committed`
        // (success) and full-identity-gated, so it clears ONLY our marker — a newer turn
        // that re-leased this cell survives. (`_heartbeat` Drop aborts the renew task.)
        self.cell.release(
            super::LeaseHolder::Sink,
            self.key.clone(),
            self.start,
            self.end,
        );
    }
}

/// #3089 A2b: adapts the sink's `DeliveryLeaseHeartbeat` to [`toc::PostHeartbeat`]. Holds the
/// `Arc` (the controller drives the lease behind a borrowed `&cell`) and spawns the SAME
/// `DeliveryLeaseHeartbeat::spawn` the legacy guard used (#3151 — identical renew); the guard
/// Drop aborts the renew task BEFORE the inline commit.
pub(super) struct SinkPostHeartbeat {
    pub(super) cell: Arc<super::DeliveryLeaseCell>,
}

impl toc::PostHeartbeat for SinkPostHeartbeat {
    fn start(
        &self,
        holder: super::LeaseHolder,
        key: super::super::DeliveryLeaseKey,
    ) -> Box<dyn toc::PostHeartbeatGuard> {
        Box::new(SinkPostHeartbeatGuard {
            _heartbeat: super::DeliveryLeaseHeartbeat::spawn(self.cell.clone(), holder, key),
        })
    }
}

struct SinkPostHeartbeatGuard {
    _heartbeat: super::DeliveryLeaseHeartbeat,
}

impl toc::PostHeartbeatGuard for SinkPostHeartbeatGuard {}

pub(super) fn session_bound_should_send_new_chunks_for_placeholder(response_text: &str) -> bool {
    super::super::formatting::needs_multiple_messages(response_text)
}

/// Pick the one ordered coordinate used by every terminal sink path. Strict
/// commit-fenced frames take precedence; inflight-less idle/catch-up frames use
/// their carried range; legacy no-range frames still return a degenerate
/// zero-width coordinate so no terminal POST can bypass the shared lease.
pub(super) fn sink_delivery_lease_coordinate(
    delivery: &SessionRelayDelivery,
) -> ((u64, u64), Option<u64>) {
    if let (Some(start), Some(end)) = (
        delivery.frame_turn_start_offset,
        delivery.terminal_consumed_end,
    ) && end > start
    {
        return ((start, end), Some(start));
    }
    if let Some((start, end)) = delivery.relay_range
        && end > start
    {
        return ((start, end), Some(start));
    }

    // A terminal delivery without an ordered byte range must still serialize on
    // the channel cell. The zero-width coordinate and absent fallback retain the
    // legacy degenerate id-0 key instead of allowing a lease-free POST.
    ((0, 0), None)
}

#[derive(Clone, Debug, Default)]
pub(super) struct SessionRelayTraceContext {
    turn_id: Option<String>,
    dispatch_id: Option<String>,
    session_key: Option<String>,
    relay_owner: Option<String>,
    runtime_kind: Option<String>,
}

impl SessionRelayTraceContext {
    pub(super) fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }

    pub(super) fn dispatch_id(&self) -> Option<&str> {
        self.dispatch_id.as_deref()
    }

    pub(super) fn session_key(&self) -> Option<&str> {
        self.session_key.as_deref()
    }

    pub(super) fn relay_owner(&self) -> &str {
        self.relay_owner.as_deref().unwrap_or("none")
    }

    pub(super) fn runtime_kind(&self) -> &str {
        self.runtime_kind.as_deref().unwrap_or("unknown")
    }
}

pub(super) fn session_relay_trace_context(
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session_name: &str,
    inflight: Option<&InflightTurnState>,
) -> SessionRelayTraceContext {
    let lease = crate::services::tui_prompt_dedupe::external_input_relay_lease(
        provider.as_str(),
        tmux_session_name,
        channel_id,
    );
    SessionRelayTraceContext {
        turn_id: inflight
            .and_then(inflight_turn_id)
            .or_else(|| lease.as_ref().and_then(|lease| lease.turn_id.clone())),
        dispatch_id: inflight.and_then(|state| state.dispatch_id.clone()),
        session_key: inflight
            .and_then(|state| state.session_key.clone())
            .or_else(|| lease.as_ref().and_then(|lease| lease.session_key.clone())),
        relay_owner: inflight
            .map(|state| state.effective_relay_owner_kind().as_str().to_string())
            .or_else(|| {
                lease
                    .as_ref()
                    .map(|lease| lease.relay_owner.as_str().to_string())
            }),
        runtime_kind: inflight
            .and_then(|state| state.runtime_kind.map(|kind| kind.as_str().to_string()))
            .or_else(|| {
                lease
                    .as_ref()
                    .and_then(|lease| lease.runtime_kind.map(|kind| kind.as_str().to_string()))
            }),
    }
}

fn inflight_turn_id(state: &InflightTurnState) -> Option<String> {
    (state.user_msg_id != 0).then(|| format!("discord:{}:{}", state.channel_id, state.user_msg_id))
}
