//! Discord [`RelaySink`] for the session-bound `StreamRelay` path.
//!
//! `tmux_watcher` remains the tmux file reader / producer, but when the
//! supervisor has a matched session, this sink performs the terminal Discord
//! write. Inflight state only selects placeholder-edit metadata; a missing
//! inflight is still a valid pane-bound new-message route. The watcher then
//! treats terminal delivery as delegated instead of sending directly.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serenity::model::id::{ChannelId, MessageId};

use super::delivery_lease_cell::source_epoch_observer;
use super::formatting::{self, ReplaceLongMessageOutcome};
use super::health::HealthRegistry;
use super::inflight::{InflightTurnState, RelayOwnerKind, TurnSource};
use super::outbound::delivery_record as dr;
use super::outbound::turn_output_controller as toc;
#[cfg(test)]
use super::placeholder_controller::PlaceholderLifecycle;
use super::replace_outcome_policy::edit_fail_fallback_disposition;
#[cfg(test)]
use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::cluster::stream_relay::{
    RelaySink, RelaySinkError, RelaySinkOutcome, StreamFrame,
};
use crate::services::cluster::watcher_supervisor::{SupervisorConfig, run_watcher_supervisor_loop};
use crate::services::provider::ProviderKind;
use tracing::Instrument;

#[cfg(test)]
pub(in crate::services::discord) const PURE_SUBAGENT_ZERO_DELIVERY_PAYLOAD: &str = concat!(
    "{\"type\":\"system\",\"subtype\":\"task_started\",\"task_id\":\"sub-1\",\"task_type\":\"local_agent\"}\n",
    "{\"type\":\"system\",\"subtype\":\"task_notification\",\"task_id\":\"sub-1\",\"status\":\"completed\",\"summary\":\"Subagent finished\"}\n",
    "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}\n"
);
mod delivery_commit;
mod delivery_frontier;
mod delivery_outcome_classify;
mod idle_jsonl;
pub(in crate::services::discord) mod journal;
mod short_controller;
// #3960: orphaned `SessionBoundRelay` TUI-direct reclaim (producer-liveness TOCTOU).
mod orphan_reclaim;
mod relay_format;
mod task_notification_context;
mod terminal_handoff;
mod turn_parser;
use self::idle_jsonl::{
    IdleJsonlSessionInitRearm, IdleJsonlSuppression, IdleRelayRangeAction,
    idle_jsonl_apply_active_inflight_gate,
    idle_jsonl_clear_session_init_on_generation_signature_change, idle_jsonl_consume_offset,
    idle_jsonl_current_eof, idle_jsonl_payload_contains_init_event,
    idle_jsonl_payload_contains_schedule_wakeup_setup, idle_jsonl_payload_contains_user_event,
    idle_jsonl_prepare_dedup_shared, idle_jsonl_relay_source_for_matched,
    idle_jsonl_session_has_init, idle_jsonl_suppressed_range_action, idle_relay_range_action,
    prune_idle_jsonl_session_state, read_jsonl_range,
};
use self::task_notification_context::ensure_card_and_route;
use self::terminal_handoff::SessionRelayDeliveryOutcome;
use self::turn_parser::{SessionRelayDelivery, SessionRelayParser};
use super::task_notification_delivery::{ResponseDeliveryClaim, ResponseDeliveryClaimOutcome};

static SESSION_BOUND_DISCORD_DELIVERY_ENABLED: AtomicBool = AtomicBool::new(false);
const IDLE_JSONL_RELAY_POLL_INTERVAL: Duration = Duration::from_millis(500);
const IDLE_JSONL_RELAY_RECENT_INFLIGHT_GRACE: Duration = Duration::from_secs(10);
const IDLE_JSONL_RELAY_MAX_BYTES_PER_TICK: u64 = 1_048_576;

pub(in crate::services::discord) fn session_bound_discord_delivery_enabled() -> bool {
    SESSION_BOUND_DISCORD_DELIVERY_ENABLED.load(Ordering::Acquire)
}

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
enum SessionBoundTerminalDeliveryRoute {
    NewMessage,
    PlaceholderEdit(MessageId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionBoundTerminalDeliveryRouteDecision {
    Route(SessionBoundTerminalDeliveryRoute),
    Skipped,
}

fn session_bound_terminal_delivery_route(
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

fn session_bound_terminal_delivery_route_or_skip(
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

#[allow(dead_code)] // #3034: #3041 lease-free route-decision pinned by unit tests (prod uses `_with_lease`).
fn session_bound_terminal_delivery_route_decision(
    inflight: Option<&InflightTurnState>,
    tmux_session_name: &str,
    provider: &ProviderKind,
    channel_id: u64,
) -> SessionBoundTerminalDeliveryRouteDecision {
    // #3041 P1-4 codex (TOCTOU close): single lease read threaded into both the block
    // decision and the guard (see `deliver_response`'s prod path).
    let observed_lease = crate::services::tui_prompt_dedupe::external_input_relay_lease(
        provider.as_str(),
        tmux_session_name,
        channel_id,
    );
    session_bound_terminal_delivery_route_decision_with_lease(
        inflight,
        tmux_session_name,
        provider,
        channel_id,
        observed_lease.as_ref(),
    )
}

fn session_bound_terminal_delivery_route_decision_with_lease(
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
struct SessionBoundExternalInputLeaseGuard {
    provider: ProviderKind,
    tmux_session_name: String,
    channel_id: u64,
    /// `generation` of the recorded lease this guard armed with. Drop clears ONLY
    /// this exact generation.
    generation: u64,
}

impl SessionBoundExternalInputLeaseGuard {
    /// Arm a guard IFF the route-observed lease (`observed_lease`, a SINGLE shared
    /// read) is an `Unassigned`/`SessionBoundRelay` input lease for this target.
    /// Foreign-owner leases (not ours) and no-lease deliveries return `None` (inert).
    /// Capturing the generation from the SAME read closes the arm-time TOCTOU.
    fn arm_with_observed_lease(
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

    /// Test-only convenience: read the current lease for this target and arm with
    /// it (the production path threads in the route's single read instead).
    #[cfg(test)]
    fn arm_if_present(
        provider: &ProviderKind,
        channel_id: u64,
        tmux_session_name: &str,
    ) -> Option<Self> {
        let observed = crate::services::tui_prompt_dedupe::external_input_relay_lease(
            provider.as_str(),
            tmux_session_name,
            channel_id,
        );
        Self::arm_with_observed_lease(provider, channel_id, tmux_session_name, observed.as_ref())
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
struct SinkDeliveryLeaseGuard {
    cell: Arc<super::DeliveryLeaseCell>,
    key: super::DeliveryLeaseKey,
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
    fn acquire(
        cell: &Arc<super::DeliveryLeaseCell>,
        key: super::DeliveryLeaseKey,
        start: u64,
        end: u64,
    ) -> Option<Self> {
        // Mirror the watcher's self-healing acquire (tmux_watcher.rs:8594): reclaim an
        // EXPIRED prior holder so a stale dead lease can't lose this acquire.
        cell.reclaim_if_expired(super::lease_now_ms());
        let acquired = cell.try_acquire(
            key.clone(),
            super::LeaseHolder::Sink,
            start,
            end,
            super::lease_now_ms().saturating_add(super::DELIVERY_LEASE_DEADLINE_MS),
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
    fn commit(&self, outcome: super::LeaseOutcome) {
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
struct SinkPostHeartbeat {
    cell: Arc<super::DeliveryLeaseCell>,
}

impl toc::PostHeartbeat for SinkPostHeartbeat {
    fn start(
        &self,
        holder: super::LeaseHolder,
        key: super::DeliveryLeaseKey,
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

fn session_bound_should_send_new_chunks_for_placeholder(response_text: &str) -> bool {
    super::formatting::needs_multiple_messages(response_text)
}

/// Pick the one ordered coordinate used by every terminal sink path. Strict
/// commit-fenced frames take precedence; inflight-less idle/catch-up frames use
/// their carried range; legacy no-range frames still return a degenerate
/// zero-width coordinate so no terminal POST can bypass the shared lease.
fn sink_delivery_lease_coordinate(delivery: &SessionRelayDelivery) -> ((u64, u64), Option<u64>) {
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
struct SessionRelayTraceContext {
    turn_id: Option<String>,
    dispatch_id: Option<String>,
    session_key: Option<String>,
    relay_owner: Option<String>,
    runtime_kind: Option<String>,
}

impl SessionRelayTraceContext {
    fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }

    fn dispatch_id(&self) -> Option<&str> {
        self.dispatch_id.as_deref()
    }

    fn session_key(&self) -> Option<&str> {
        self.session_key.as_deref()
    }

    fn relay_owner(&self) -> &str {
        self.relay_owner.as_deref().unwrap_or("none")
    }

    fn runtime_kind(&self) -> &str {
        self.runtime_kind.as_deref().unwrap_or("unknown")
    }
}

fn session_relay_trace_context(
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

#[cfg(test)]
struct SinkLeaseTestProbe {
    acquired: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

pub(in crate::services::discord) struct SessionBoundDiscordRelaySink {
    health_registry: Arc<HealthRegistry>,
    frames_total: AtomicU64,
    delivered_total: AtomicU64,
    by_session: Mutex<HashMap<String, SessionRelayParser>>,
    // #5071 T1 S3a: borrowed from the process-wide observer rather than owned, so
    // the sink direct family and the watcher family serialise onto one actor.
    journal: &'static journal::JournalObserver,
    #[cfg(test)]
    lease_test_probe: Option<Arc<SinkLeaseTestProbe>>,
    #[cfg(test)]
    test_gateway: Option<Arc<dyn super::gateway::TurnGateway>>,
    #[cfg(test)]
    test_replace_anchor: Option<formatting::ReplaceLastChunkAnchor>,
    #[cfg(test)]
    test_delivery_outcomes: Option<Arc<Mutex<Vec<SessionRelayDeliveryOutcome>>>>,
    #[cfg(test)]
    test_force_legacy_replace: bool,
}

impl SessionBoundDiscordRelaySink {
    pub(in crate::services::discord) fn new(health_registry: Arc<HealthRegistry>) -> Self {
        Self {
            health_registry,
            frames_total: AtomicU64::new(0),
            delivered_total: AtomicU64::new(0),
            by_session: Mutex::new(HashMap::new()),
            journal: journal::process_observer(),
            #[cfg(test)]
            lease_test_probe: None,
            #[cfg(test)]
            test_gateway: None,
            #[cfg(test)]
            test_replace_anchor: None,
            #[cfg(test)]
            test_delivery_outcomes: None,
            #[cfg(test)]
            test_force_legacy_replace: false,
        }
    }

    #[cfg(test)]
    fn with_lease_test_probe(
        health_registry: Arc<HealthRegistry>,
        lease_test_probe: Arc<SinkLeaseTestProbe>,
    ) -> Self {
        let mut sink = Self::new(health_registry);
        sink.lease_test_probe = Some(lease_test_probe);
        sink
    }

    fn ingest_frame(&self, frame: &StreamFrame) -> Vec<SessionRelayDelivery> {
        self.frames_total.fetch_add(1, Ordering::AcqRel);
        let Ok(mut sessions) = self.by_session.lock() else {
            return Vec::new();
        };
        sessions
            .entry(frame.session_name.clone())
            .or_default()
            .ingest_frame(frame)
    }

    /// Commit a confirmed idle/catch-up delivery in the frame's ordered JSONL
    /// coordinate space. The wrapper generation and current EOF are rechecked
    /// after transport before either durable or in-memory authority advances.
    pub(in crate::services::discord) fn advance_idle_range_after_confirmed_post(
        &self,
        shared: &super::SharedData,
        provider: &ProviderKind,
        channel_id: u64,
        session_name: &str,
        delivery: &SessionRelayDelivery,
    ) -> bool {
        let Some((start, end)) = delivery.relay_range.filter(|(start, end)| end > start) else {
            return false;
        };
        let Some(frame_generation) = delivery
            .relay_generation_mtime_ns
            .filter(|generation| *generation != 0)
        else {
            return false;
        };
        let current_generation = dr::current_generation_mtime_ns(session_name);
        let current_eof = idle_jsonl_current_eof(provider, session_name);
        if current_generation == 0
            || current_generation != frame_generation
            || current_eof.is_none_or(|eof| end > eof)
        {
            return false;
        }
        if !matches!(
            dr::commit_ordered_jsonl_range(
                provider,
                ChannelId::new(channel_id),
                session_name,
                (start, end),
                frame_generation,
            ),
            Ok(true)
        ) {
            return false;
        }
        super::tmux::advance_watcher_confirmed_end(
            shared,
            provider,
            ChannelId::new(channel_id),
            session_name,
            end,
            "src/services/discord/session_relay_sink.rs:idle_range_confirmed_advance",
        );
        dr::record_delivered_content_fingerprint(
            provider,
            ChannelId::new(channel_id),
            session_name,
            &delivery.response_text,
        );
        true
    }

    pub(in crate::services::discord) fn idle_range_already_committed_before_transport(
        &self,
        shared: &super::SharedData,
        provider: &ProviderKind,
        channel_id: u64,
        session_name: &str,
        delivery: &SessionRelayDelivery,
    ) -> bool {
        let Some((_, end)) = delivery.relay_range else {
            return false;
        };
        let Some(frame_generation) = delivery.relay_generation_mtime_ns else {
            return false;
        };
        let current_generation = dr::current_generation_mtime_ns(session_name);
        let current_eof = idle_jsonl_current_eof(provider, session_name);
        if frame_generation == 0 || current_generation != frame_generation || current_eof.is_none()
        {
            return false;
        }
        dr::effective_committed_offset(
            shared,
            provider,
            ChannelId::new(channel_id),
            session_name,
            current_eof,
        )
        .max(dr::delivered_frontier_end_current_generation(
            provider,
            ChannelId::new(channel_id),
            session_name,
            current_eof,
        )) >= end
    }

    fn advance_after_confirmed_post(
        &self,
        shared: &super::SharedData,
        provider: &ProviderKind,
        channel_id: u64,
        session_name: &str,
        delivery: &SessionRelayDelivery,
        sink_lease_guard: Option<&SinkDeliveryLeaseGuard>,
    ) {
        let advanced = if delivery.relay_range.is_some() {
            self.advance_idle_range_after_confirmed_post(
                shared,
                provider,
                channel_id,
                session_name,
                delivery,
            )
        } else {
            let fresh_inflight = super::inflight::load_inflight_state(provider, channel_id);
            self.advance_offset_for_confirmed_delegated_terminal(
                shared,
                provider,
                channel_id,
                session_name,
                delivery,
                fresh_inflight.as_ref(),
            )
        };
        // #3151 CLEAR: advance committed FIRST, THEN commit the marker. #3159 BUG 1: the
        // commit outcome MUST reflect whether the advance fired — see
        // `SinkDeliveryLeaseGuard::commit` (a refused-advance `Delivered` is a black-hole).
        if let Some(guard) = sink_lease_guard {
            guard.commit(if advanced {
                super::LeaseOutcome::Delivered
            } else {
                super::LeaseOutcome::NotDelivered
            });
        }
    }

    async fn deliver_response(
        &self,
        delivery: SessionRelayDelivery,
    ) -> Result<SessionRelayDeliveryOutcome, RelaySinkError> {
        #[cfg(test)]
        let gateway: Option<&dyn super::gateway::TurnGateway> = self.test_gateway.as_deref();
        #[cfg(not(test))]
        let gateway: Option<&dyn super::gateway::TurnGateway> = None;
        let channel_id = delivery.channel_id;
        let provider = delivery.provider.clone();
        let inflight = super::inflight::load_inflight_state(&provider, channel_id);
        // #3041 P1-3 (Part a, B1 — frame-carried): this pre-POST `inflight` is for the
        // route + trace ONLY; the advance gate re-loads FRESH after the POST
        // (`advance_after_confirmed_post`, codex P1-3 issue 3) so a turn cleared/replaced
        // during the POST can't authorize a wrong-turn advance.
        let trace = session_relay_trace_context(
            &provider,
            channel_id,
            &delivery.session_name,
            inflight.as_ref(),
        );
        // #3041 P1-4 codex (TOCTOU close): read the external-input lease ONCE and
        // thread the SAME snapshot into both the route decision and the RAII release
        // guard (guard generation == lease the route observed; no `.await` between).
        let observed_external_lease =
            crate::services::tui_prompt_dedupe::external_input_relay_lease(
                provider.as_str(),
                &delivery.session_name,
                channel_id,
            );
        let route = match session_bound_terminal_delivery_route_decision_with_lease(
            inflight.as_ref(),
            &delivery.session_name,
            &provider,
            channel_id,
            observed_external_lease.as_ref(),
        ) {
            SessionBoundTerminalDeliveryRouteDecision::Route(route) => route,
            SessionBoundTerminalDeliveryRouteDecision::Skipped => {
                tracing::debug!(
                    provider = provider.as_str(),
                    channel_id,
                    tmux_session = %delivery.session_name,
                    turn_id = trace.turn_id().unwrap_or(""),
                    dispatch_id = trace.dispatch_id().unwrap_or(""),
                    session_key = trace.session_key().unwrap_or(""),
                    relay_owner = trace.relay_owner(),
                    runtime_kind = trace.runtime_kind(),
                    "session-bound relay sink skipped bridge-owned or mismatched inflight"
                );
                crate::services::observability::emit_relay_delivery(
                    provider.as_str(),
                    channel_id,
                    trace.dispatch_id(),
                    trace.session_key(),
                    trace.turn_id(),
                    None,
                    "session_relay_sink",
                    "skip",
                    None,
                    None,
                    false,
                    Some("bridge-owned or mismatched inflight"),
                );
                // #3041 P1-5: the SOLE sink-local decline (foreign-owner block or
                // bridge-owned/mismatched inflight). `NotDelivered`, NOT `Unknown` (the
                // sink KNOWS it did not post) → §3.2 reconciliation SendFull if uncommitted.
                return Ok(SessionRelayDeliveryOutcome::NotDelivered);
            }
        };
        // #3041 P1-4 (§4-④): arm the RAII release-on-all-paths guard now this sink owns
        // the delivery — see `SessionBoundExternalInputLeaseGuard`.
        let _external_input_lease_guard =
            SessionBoundExternalInputLeaseGuard::arm_with_observed_lease(
                &provider,
                channel_id,
                &delivery.session_name,
                observed_external_lease.as_ref(),
            );
        let shared = self
            .health_registry
            .shared_for_provider(&provider)
            .await
            .ok_or_else(|| {
                RelaySinkError::Transient(format!(
                    "discord shared state unavailable for provider {}",
                    provider.as_str()
                ))
            })?;
        let (raw_response_text, relay_text) =
            relay_format::session_bound_relay_bodies(&shared, &provider, &delivery);
        let channel = ChannelId::new(channel_id);

        let cutover_range = match (
            delivery.frame_turn_start_offset,
            delivery.terminal_consumed_end,
        ) {
            (Some(start), Some(end)) if end > start => Some((start, end)),
            _ => None,
        };
        // Task-context delivery may confirm a card and then promote the response
        // route from PlaceholderEdit to NewMessage. Keep that entire operation on
        // the outer lease so both the card transport and the final answer remain
        // guarded even though the route mutates inside `ensure_card_and_route`.
        let cutover_short_replace = delivery.task_notification_context.is_none()
            && cutover_range.is_some()
            && !relay_text.is_empty()
            && matches!(route, SessionBoundTerminalDeliveryRoute::PlaceholderEdit(_))
            && !session_bound_should_send_new_chunks_for_placeholder(&relay_text)
            && {
                #[cfg(test)]
                {
                    !self.test_force_legacy_replace
                }
                #[cfg(not(test))]
                {
                    true
                }
            };
        let (lease_range, lease_fallback_start) = sink_delivery_lease_coordinate(&delivery);
        let sink_lease_key = delivery_lease_key_for_frame(
            channel,
            shared.restart.current_generation,
            &delivery,
            lease_fallback_start,
        );
        let sink_delivery_authority = delivery_frontier::capture_sink_delivery_authority(
            &shared,
            channel,
            &delivery,
            &sink_lease_key,
            lease_range,
        );
        let sink_delivery_ctx = delivery_frontier::SinkDeliveryCtx {
            shared: &shared,
            provider: &provider,
            channel,
            delivery: &delivery,
            authority: sink_delivery_authority,
        };
        let sink_lease_guard = if cutover_short_replace {
            None
        } else {
            let cell = shared.delivery_lease(channel);
            let Some(guard) = SinkDeliveryLeaseGuard::acquire(
                &cell,
                sink_lease_key.clone(),
                lease_range.0,
                lease_range.1,
            ) else {
                tracing::debug!(
                    provider = provider.as_str(),
                    channel_id,
                    tmux_session = %delivery.session_name,
                    lease_start = lease_range.0,
                    lease_end = lease_range.1,
                    "session-bound relay sink deferred terminal delivery because another owner holds the lease"
                );
                return Ok(SessionRelayDeliveryOutcome::NotDelivered);
            };
            #[cfg(test)]
            if let Some(probe) = &self.lease_test_probe {
                probe.acquired.notify_one();
                probe.release.notified().await;
            }
            Some(guard)
        };

        if delivery.relay_range.is_some() {
            if self.idle_range_already_committed_before_transport(
                &shared,
                &provider,
                channel_id,
                &delivery.session_name,
                &delivery,
            ) {
                if let Some(guard) = sink_lease_guard.as_ref() {
                    guard.commit(super::LeaseOutcome::Delivered);
                }
                return Ok(SessionRelayDeliveryOutcome::Delivered);
            }
            let frame_generation = delivery.relay_generation_mtime_ns.unwrap_or(0);
            let current_generation = dr::current_generation_mtime_ns(&delivery.session_name);
            let current_eof = idle_jsonl_current_eof(&provider, &delivery.session_name);
            if frame_generation == 0
                || current_generation != frame_generation
                || current_eof.is_none_or(|eof| lease_range.1 > eof)
            {
                return Ok(SessionRelayDeliveryOutcome::NotDelivered);
            }
        }

        let http = if gateway.is_some() {
            Arc::new(serenity::http::Http::new("test-gateway"))
        } else {
            shared.serenity_http_or_token_fallback().ok_or_else(|| {
                RelaySinkError::Transient(format!(
                    "discord http unavailable for provider {}",
                    provider.as_str()
                ))
            })?
        };
        let (route, task_card_message_id, task_response_claim_outcome) =
            ensure_card_and_route(&self.health_registry, &shared, &delivery, route).await?;
        let (task_response_claim, task_response_already_delivered): (
            Option<ResponseDeliveryClaim>,
            bool,
        ) = match task_response_claim_outcome {
            Some(ResponseDeliveryClaimOutcome::Owned(claim)) => (Some(claim), false),
            Some(ResponseDeliveryClaimOutcome::Wait) => {
                tracing::warn!(
                    provider = provider.as_str(),
                    channel_id,
                    tmux_session = %delivery.session_name,
                    "task response is deferred to the watcher or owned by another live claimant"
                );
                return Ok(SessionRelayDeliveryOutcome::NotDelivered);
            }
            Some(ResponseDeliveryClaimOutcome::Delivered { .. }) => (None, true),
            Some(ResponseDeliveryClaimOutcome::SentUncommitted { card_message_id }) => {
                tracing::error!(
                    provider = provider.as_str(),
                    channel_id,
                    tmux_session = %delivery.session_name,
                    task_card_message_id = card_message_id,
                    "task response was already sent but its final delivery CAS is uncommitted; refusing a duplicate POST"
                );
                (None, true)
            }
            None => (None, false),
        };

        if task_response_already_delivered {
            self.advance_after_confirmed_post(
                &shared,
                &provider,
                channel_id,
                &delivery.session_name,
                &delivery,
                sink_lease_guard.as_ref(),
            );
            return Ok(SessionRelayDeliveryOutcome::Delivered);
        }

        if let SessionBoundTerminalDeliveryRoute::PlaceholderEdit(msg_id) = route {
            if let Some((start, end)) = cutover_range.filter(|_| cutover_short_replace) {
                let live_gateway = super::gateway::DiscordGateway::new(
                    http.clone(),
                    shared.clone(),
                    provider.clone(),
                    None,
                );
                return short_controller::deliver_short_replace_via_controller(
                    gateway.unwrap_or(&live_gateway),
                    short_controller::SinkShortReplaceCtx {
                        shared: &shared,
                        provider: &provider,
                        channel,
                        channel_id,
                        msg_id,
                        relay_text: &relay_text,
                        delivered_fingerprint_body: &raw_response_text,
                        delivery: &delivery,
                        sink_lease_key,
                        sink_delivery_authority,
                        trace: &trace,
                        range: (start, end),
                        delivered_total: &self.delivered_total,
                    },
                )
                .await;
            }
            let mut direct_journal_attempt =
                if journal::journals_sink_direct(&route, cutover_short_replace) {
                    self.journal.begin_fresh(&shared, &delivery)
                } else {
                    None
                };
            if session_bound_should_send_new_chunks_for_placeholder(&relay_text) {
                let (message_ids, chunk_anchor_receipt) =
                    journal::send_long_chunks_with_anchor_receipt(
                        gateway,
                        &http,
                        channel,
                        msg_id,
                        &relay_text,
                        &shared,
                    )
                    .await?;
                if let Some(gateway) = gateway {
                    let _ = gateway.delete_message(channel, msg_id).await;
                } else {
                    let _ = super::http::delete_channel_message(&http, channel, msg_id).await;
                }
                self.delivered_total.fetch_add(1, Ordering::AcqRel);
                tracing::info!(
                    provider = provider.as_str(),
                    channel_id,
                    message = msg_id.get(),
                    tmux_session = %delivery.session_name,
                    turn_id = trace.turn_id().unwrap_or(""),
                    dispatch_id = trace.dispatch_id().unwrap_or(""),
                    session_key = trace.session_key().unwrap_or(""),
                    relay_owner = trace.relay_owner(),
                    runtime_kind = trace.runtime_kind(),
                    chars = relay_text.chars().count(),
                    "session-bound relay sink delivered long terminal response as ordered new chunks"
                );
                crate::services::observability::emit_relay_delivery(
                    provider.as_str(),
                    channel_id,
                    trace.dispatch_id(),
                    trace.session_key(),
                    trace.turn_id(),
                    None,
                    "session_relay_sink",
                    "post",
                    None,
                    None,
                    true,
                    Some("long response sent as ordered chunks"),
                );
                let proof = delivery_frontier::finish_sink_delivery(
                    sink_delivery_ctx,
                    message_ids.last().map(|message_id| message_id.get()),
                    &raw_response_text,
                    sink_lease_guard.as_ref(),
                    "src/services/discord/session_relay_sink.rs:sink_long_chunks_advance",
                );
                journal::settle(
                    self.journal,
                    &mut direct_journal_attempt,
                    chunk_anchor_receipt,
                    proof,
                );
                return Ok(SessionRelayDeliveryOutcome::from_proof(proof));
            }
            #[cfg(test)]
            let mut last_chunk_anchor = self.test_replace_anchor.clone();
            #[cfg(not(test))]
            let mut last_chunk_anchor = None;
            let mut edit_anchor_receipt = None;
            let replace_outcome = if let Some(gateway) = gateway {
                gateway
                    .replace_message_with_outcome(channel, msg_id, &relay_text)
                    .await
            } else {
                formatting::replace_long_message_raw_with_outcome_returning_receipt(
                    &http,
                    channel,
                    msg_id,
                    &relay_text,
                    &shared,
                    &mut last_chunk_anchor,
                    &mut edit_anchor_receipt,
                )
                .await
                .map_err(|error| error.to_string())
            };
            match replace_outcome {
                Ok(ReplaceLongMessageOutcome::EditedOriginal) => {
                    self.delivered_total.fetch_add(1, Ordering::AcqRel);
                    tracing::info!(
                        provider = provider.as_str(),
                        channel_id,
                        message = msg_id.get(),
                        tmux_session = %delivery.session_name,
                        turn_id = trace.turn_id().unwrap_or(""),
                        dispatch_id = trace.dispatch_id().unwrap_or(""),
                        session_key = trace.session_key().unwrap_or(""),
                        relay_owner = trace.relay_owner(),
                        runtime_kind = trace.runtime_kind(),
                        chars = relay_text.chars().count(),
                        "session-bound relay sink delivered terminal response via placeholder edit"
                    );
                    crate::services::observability::emit_relay_delivery(
                        provider.as_str(),
                        channel_id,
                        trace.dispatch_id(),
                        trace.session_key(),
                        trace.turn_id(),
                        Some(msg_id.get()),
                        "session_relay_sink",
                        "edit",
                        None,
                        None,
                        true,
                        Some("placeholder edit"),
                    );
                    let anchor = formatting::watcher_completion_footer_anchor(
                        last_chunk_anchor.as_ref(),
                        msg_id,
                        &relay_text,
                    )
                    .0;
                    let proof = delivery_frontier::finish_sink_delivery(
                        sink_delivery_ctx,
                        Some(anchor.get()),
                        &raw_response_text,
                        sink_lease_guard.as_ref(),
                        "src/services/discord/session_relay_sink.rs:sink_legacy_short_edit_advance",
                    );
                    journal::settle(
                        self.journal,
                        &mut direct_journal_attempt,
                        edit_anchor_receipt.take(),
                        proof,
                    );
                    Ok(SessionRelayDeliveryOutcome::from_proof(proof))
                }
                Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                    edit_error,
                    replacement_anchor,
                }) => {
                    // #2757 (A0 #3089): never delete msg_id — it is the bridge's
                    // current_msg_id, possibly holding streamed content a transient edit
                    // failure would vacuum. The shared policy pins this preserve decision.
                    let preserve_original = !edit_fail_fallback_disposition().deletes_original();
                    debug_assert!(preserve_original, "#2757: must preserve original");
                    self.delivered_total.fetch_add(1, Ordering::AcqRel);
                    tracing::warn!(
                        provider = provider.as_str(),
                        channel_id,
                        message = msg_id.get(),
                        tmux_session = %delivery.session_name,
                        turn_id = trace.turn_id().unwrap_or(""),
                        dispatch_id = trace.dispatch_id().unwrap_or(""),
                        session_key = trace.session_key().unwrap_or(""),
                        relay_owner = trace.relay_owner(),
                        runtime_kind = trace.runtime_kind(),
                        chars = relay_text.chars().count(),
                        error = %edit_error,
                        "session-bound relay sink delivered terminal response via fallback; preserving original msg_id (#2757)"
                    );
                    crate::services::observability::emit_relay_delivery(
                        provider.as_str(),
                        channel_id,
                        trace.dispatch_id(),
                        trace.session_key(),
                        trace.turn_id(),
                        Some(msg_id.get()),
                        "session_relay_sink",
                        "post",
                        None,
                        None,
                        true,
                        Some("fallback after edit failure"),
                    );
                    let proof = delivery_frontier::finish_sink_delivery(
                        sink_delivery_ctx,
                        replacement_anchor.map(|anchor| anchor.get()),
                        &raw_response_text,
                        sink_lease_guard.as_ref(),
                        "src/services/discord/session_relay_sink.rs:sink_legacy_short_fallback_advance",
                    );
                    journal::settle(
                        self.journal,
                        &mut direct_journal_attempt,
                        edit_anchor_receipt.take(),
                        proof,
                    );
                    Ok(SessionRelayDeliveryOutcome::from_proof(proof))
                }
                Ok(ReplaceLongMessageOutcome::PartialContinuationFailure { error, .. }) => {
                    Err(RelaySinkError::Transient(
                        super::replace_outcome_policy::strip_watcher_send_failure_class_marker(
                            &error,
                        )
                        .to_string(),
                    ))
                }
                Err(error) => {
                    let error = error.to_string();
                    Err(RelaySinkError::Transient(
                        super::replace_outcome_policy::strip_watcher_send_failure_class_marker(
                            &error,
                        )
                        .to_string(),
                    ))
                }
            }
        } else {
            self.deliver_new_message_with_task_authority(
                gateway,
                &shared,
                &provider,
                channel_id,
                &delivery,
                &relay_text,
                task_card_message_id,
                task_response_claim,
                &trace,
                sink_lease_guard.as_ref(),
                sink_delivery_ctx,
            )
            .await
        }
    }
}

pub(crate) async fn run_session_bound_discord_relay_supervisor(
    health_registry: Option<Arc<HealthRegistry>>,
    shutdown: Arc<AtomicBool>,
) {
    let Some(health_registry) = health_registry else {
        tracing::warn!(
            "session-bound Discord relay sink unavailable: missing HealthRegistry; using metrics-only sink"
        );
        crate::services::cluster::registry_adapter_sink::run_with_registry_adapter_sink(shutdown)
            .await;
        return;
    };

    SESSION_BOUND_DISCORD_DELIVERY_ENABLED.store(true, Ordering::Release);
    let idle_health_registry = health_registry.clone();
    let sink: Arc<dyn RelaySink> = Arc::new(SessionBoundDiscordRelaySink::new(health_registry));
    let idle_shutdown = shutdown.clone();
    super::task_supervisor::spawn_observed(
        "session_bound_idle_jsonl_relay",
        async move {
            run_idle_jsonl_relay_loop(idle_shutdown, idle_health_registry).await;
        }
        .instrument(tracing::info_span!("session_bound_idle_jsonl_relay")),
    );
    run_watcher_supervisor_loop(SupervisorConfig::default(), sink, shutdown).await;
    SESSION_BOUND_DISCORD_DELIVERY_ENABLED.store(false, Ordering::Release);
}

async fn run_idle_jsonl_relay_loop(
    shutdown: Arc<AtomicBool>,
    health_registry: Arc<HealthRegistry>,
) {
    let registry = crate::services::cluster::session_registry::global_session_registry();
    let producers =
        crate::services::cluster::relay_producer_registry::global_relay_producer_registry();
    let mut offsets: HashMap<String, u64> = HashMap::new();
    let mut pending_ends: HashMap<String, u64> = HashMap::new();
    let mut first_seen_at: HashMap<String, Instant> = HashMap::new();
    let mut last_inflight_seen_at: HashMap<String, Instant> = HashMap::new();
    let mut session_init_seen: HashSet<String> = HashSet::new();
    let mut session_generation_signatures: HashMap<String, i64> = HashMap::new();

    while !shutdown.load(Ordering::Acquire) {
        let mut seen_sessions = HashSet::new();
        for entry in registry.list_matched() {
            let matched = entry.matched;
            let session_name = matched.expected_session_name.clone();
            let relay_source = idle_jsonl_relay_source_for_matched(&matched);
            seen_sessions.insert(session_name.clone());
            let first_seen = *first_seen_at
                .entry(session_name.clone())
                .or_insert_with(Instant::now);
            let Ok(channel_id) = matched.channel_id.parse::<u64>() else {
                continue;
            };
            let Ok(metadata) = std::fs::metadata(&relay_source.path) else {
                continue;
            };
            let len = metadata.len();
            let offset = offsets.entry(session_name.clone()).or_insert(len);
            if len < *offset {
                *offset = 0;
                pending_ends.remove(&session_name);
                session_init_seen.remove(&session_name);
            }
            let source_marker = source_epoch_observer::marker_if_enabled(&session_name);
            let current_generation_signature =
                super::tmux::read_generation_file_mtime_ns(&session_name);
            if idle_jsonl_clear_session_init_on_generation_signature_change(
                &mut session_init_seen,
                &mut session_generation_signatures,
                &session_name,
                current_generation_signature,
            ) {
                pending_ends.remove(&session_name);
            }
            let channel = ChannelId::new(channel_id);
            let shared_for_dedup = idle_jsonl_prepare_dedup_shared(
                &health_registry,
                &matched,
                channel,
                &session_name,
                len,
                &mut session_init_seen,
            )
            .await;
            let Some(shared) = shared_for_dedup else {
                continue;
            };
            let committed = dr::effective_committed_offset(
                &shared,
                &matched.provider,
                channel,
                &session_name,
                Some(len),
            )
            .max(dr::delivered_frontier_end_current_generation(
                &matched.provider,
                channel,
                &session_name,
                Some(len),
            ));

            macro_rules! consume_idle_offset {
                ($to:expr, $rearm:expr) => {
                    idle_jsonl_consume_offset(
                        &mut session_init_seen,
                        &session_name,
                        offset,
                        $to,
                        $rearm,
                    )
                };
            }

            if let Some(mut inflight) =
                super::inflight::load_inflight_state(&matched.provider, channel_id)
            {
                if orphan_reclaim::reclaim_orphaned_session_bound_relay_if_dead(
                    &health_registry,
                    &producers,
                    &matched.provider,
                    channel_id,
                    &session_name,
                    &inflight,
                )
                .await
                {
                    inflight.set_relay_owner_kind(super::inflight::RelayOwnerKind::None);
                }
                if !super::inflight::ownerless_external_input_inflight_is_stale(&inflight) {
                    let decision = idle_jsonl_apply_active_inflight_gate(
                        &mut last_inflight_seen_at,
                        &matched,
                        channel_id,
                        &inflight,
                    );
                    if matches!(
                        decision,
                        idle_jsonl::IdleJsonlInflightGateDecision::DeferUntilCommitted
                    ) {
                        let pending_end = pending_ends.entry(session_name.clone()).or_insert(len);
                        *pending_end = (*pending_end).max(len);
                        if matches!(
                            idle_jsonl_suppressed_range_action(
                                committed,
                                *offset,
                                *pending_end,
                                IdleJsonlSuppression::DeferUntilCommitted,
                            ),
                            IdleRelayRangeAction::AdvanceCommitted
                        ) {
                            consume_idle_offset!(*pending_end, IdleJsonlSessionInitRearm::Keep);
                            pending_ends.remove(&session_name);
                        }
                    }
                    continue;
                }
                last_inflight_seen_at.remove(&session_name);
            }

            let in_recent_inflight_grace = last_inflight_seen_at
                .get(&session_name)
                .is_some_and(|seen_at| seen_at.elapsed() < IDLE_JSONL_RELAY_RECENT_INFLIGHT_GRACE);
            let in_new_session_grace =
                first_seen.elapsed() < IDLE_JSONL_RELAY_RECENT_INFLIGHT_GRACE;
            if in_recent_inflight_grace || in_new_session_grace {
                let pending_end = pending_ends.entry(session_name.clone()).or_insert(len);
                *pending_end = (*pending_end).max(len);
                match idle_jsonl_suppressed_range_action(
                    committed,
                    *offset,
                    *pending_end,
                    IdleJsonlSuppression::DeferUntilCommitted,
                ) {
                    IdleRelayRangeAction::AdvanceCommitted => {
                        consume_idle_offset!(*pending_end, IdleJsonlSessionInitRearm::Keep);
                        pending_ends.remove(&session_name);
                    }
                    IdleRelayRangeAction::HoldPending => {}
                    _ => unreachable!("deferred suppression returns only hold/advance"),
                }
                continue;
            }
            if len <= *offset {
                pending_ends.remove(&session_name);
                continue;
            }

            let start = *offset;
            let was_deferred = pending_ends.contains_key(&session_name);
            let pending_end = pending_ends.remove(&session_name).unwrap_or(len).max(len);
            let end = pending_end.min(start.saturating_add(IDLE_JSONL_RELAY_MAX_BYTES_PER_TICK));
            let Ok(opened_range) = read_jsonl_range(&relay_source.path, start, end) else {
                continue;
            };
            let payload = &opened_range.payload;
            if payload.is_empty() {
                consume_idle_offset!(end, IdleJsonlSessionInitRearm::Keep);
                continue;
            }
            if idle_jsonl_payload_contains_schedule_wakeup_setup(payload) {
                consume_idle_offset!(end, IdleJsonlSessionInitRearm::Keep);
                continue;
            }
            if !was_deferred && idle_jsonl_payload_contains_user_event(payload) {
                consume_idle_offset!(end, IdleJsonlSessionInitRearm::Clear);
                continue;
            }
            let session_has_init =
                idle_jsonl_session_has_init(&mut session_init_seen, &session_name, payload);
            let action = idle_relay_range_action(
                payload,
                start,
                end,
                committed,
                relay_source.allow_continued_session_without_init,
                session_has_init,
                was_deferred,
            );
            match action {
                IdleRelayRangeAction::DropAndConsume => {
                    consume_idle_offset!(end, IdleJsonlSessionInitRearm::Keep);
                }
                IdleRelayRangeAction::AdvanceCommitted => {
                    consume_idle_offset!(end, IdleJsonlSessionInitRearm::Keep);
                }
                IdleRelayRangeAction::SendPendingSuffixFrom(from) => {
                    let Some(producer) = producers.get_producer(&session_name) else {
                        continue;
                    };
                    let Ok(suffix) = opened_range.suffix(&relay_source.path, from) else {
                        continue;
                    };
                    if suffix.payload.is_empty() {
                        continue;
                    }
                    let source_stamp = source_marker.and_then(|marker| {
                        source_epoch_observer::source_stamp(
                            &session_name,
                            marker,
                            suffix.file_identity,
                        )
                    });
                    if producer.try_send_frame_for_range_with_source(
                        String::from_utf8_lossy(&suffix.payload).into_owned(),
                        from,
                        end,
                        current_generation_signature,
                        source_stamp,
                    ) {
                        pending_ends.insert(session_name.clone(), end);
                    }
                }
                IdleRelayRangeAction::HoldPending => {
                    pending_ends.insert(session_name.clone(), end);
                }
            }
        }

        prune_idle_jsonl_session_state(
            &seen_sessions,
            &mut offsets,
            &mut first_seen_at,
            &mut last_inflight_seen_at,
            &mut session_init_seen,
            &mut session_generation_signatures,
            &mut pending_ends,
        );
        tokio::time::sleep(IDLE_JSONL_RELAY_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod relay_state_contract_refs {
    #[test]
    fn relay_state_contract_symbol_references_compile() {
        let _ = super::turn_parser::SessionRelayParser::ingest_frame;
        let _ = super::SessionBoundDiscordRelaySink::deliver_response;
        let _ = super::SessionBoundDiscordRelaySink::advance_idle_range_after_confirmed_post;
        let _ = crate::services::cluster::stream_relay::RelaySinkOutcome::terminal_fresh_delivered;
        let _ = super::idle_jsonl::idle_jsonl_suppressed_range_action;
        let _ = crate::services::discord::outbound::delivery_record::commit_ordered_jsonl_range;
    }
}

fn delivery_lease_key_for_frame(
    channel: ChannelId,
    generation: u64,
    delivery: &SessionRelayDelivery,
    relay_range_start: Option<u64>,
) -> super::DeliveryLeaseKey {
    super::DeliveryLeaseKey::new_for_site_with_fallback_offset(
        channel,
        generation,
        delivery.frame_turn_user_msg_id,
        Some(&delivery.frame_turn_started_at),
        delivery.frame_turn_start_offset,
        relay_range_start,
        "sink",
    )
}

#[cfg(test)]
mod delivery_orchestration_tests;
#[cfg(test)]
mod tests;
