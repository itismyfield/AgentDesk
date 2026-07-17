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

use async_trait::async_trait;
use serenity::model::id::{ChannelId, MessageId};

use super::formatting::{self, ReplaceLongMessageOutcome};
use super::health::HealthRegistry;
use super::inflight::{InflightTurnState, RelayOwnerKind, TurnSource};
use super::outbound::delivery_record as dr;
use super::outbound::turn_output_controller as toc;
use super::placeholder_controller::{PlaceholderKey, PlaceholderLifecycle};
use super::replace_outcome_policy::edit_fail_fallback_disposition;
use super::tmux::{WatcherToolState, process_watcher_lines};
use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::cluster::stream_relay::{
    RelaySink, RelaySinkError, RelaySinkOutcome, StreamFrame,
};
use crate::services::cluster::watcher_supervisor::{SupervisorConfig, run_watcher_supervisor_loop};
use crate::services::provider::ProviderKind;
use crate::services::session_backend::StreamLineState;
use tracing::Instrument;

mod delivery_outcome_classify;
mod idle_jsonl;
// #3960: orphaned `SessionBoundRelay` TUI-direct reclaim (producer-liveness TOCTOU).
mod orphan_reclaim;
mod relay_format;
mod task_notification_context;
use self::idle_jsonl::{
    IdleJsonlSessionInitRearm, IdleRelayRangeAction, idle_jsonl_apply_active_inflight_gate,
    idle_jsonl_clear_session_init_on_generation_signature_change, idle_jsonl_consume_offset,
    idle_jsonl_payload_contains_init_event, idle_jsonl_payload_contains_schedule_wakeup_setup,
    idle_jsonl_payload_contains_user_event, idle_jsonl_prepare_dedup_shared,
    idle_jsonl_relay_source_for_matched, idle_jsonl_session_has_init,
    idle_jsonl_should_retry_without_dedup_shared, idle_relay_range_action,
    prune_idle_jsonl_session_state, read_jsonl_range,
};
use self::task_notification_context::{ensure_card_and_route, merge_task_notification_kind};
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
    /// `(turn, [start,end))`. `Some` (spawning the heartbeat) only when the acquire wins; a
    /// FAILED acquire (another holder owns the range) → `None` → markerless heartbeat-less
    /// POST that never blocks delivery (single-winner CAS — no dup, no self-black-hole).
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
    response_text.len() > super::DISCORD_MSG_LIMIT
}

/// #3089 A2b (review-fix Medium-1): pure `SinkDeliveryLeaseGuard` acquire decision — legacy
/// branches acquire ONE `Leased{Sink}` marker over `cutover_range`; the cut-over short-replace
/// branch is EXCLUDED (controller owns the single lease). Extracted so the no-double-acquire
/// invariant is testable: dropping `!cutover` fails `cutover_skips_sink_guard_acquire`.
fn sink_guard_lease_range(
    cutover_range: Option<(u64, u64)>,
    cutover_short_replace: bool,
) -> Option<(u64, u64)> {
    cutover_range.filter(|_| !cutover_short_replace)
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

pub(in crate::services::discord) struct SessionBoundDiscordRelaySink {
    health_registry: Arc<HealthRegistry>,
    frames_total: AtomicU64,
    delivered_total: AtomicU64,
    by_session: Mutex<HashMap<String, SessionRelayParser>>,
}

impl SessionBoundDiscordRelaySink {
    pub(in crate::services::discord) fn new(health_registry: Arc<HealthRegistry>) -> Self {
        Self {
            health_registry,
            frames_total: AtomicU64::new(0),
            delivered_total: AtomicU64::new(0),
            by_session: Mutex::new(HashMap::new()),
        }
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

    fn finish_terminal_candidate(&self, session_name: &str) {
        let Ok(mut sessions) = self.by_session.lock() else {
            return;
        };
        if let Some(parser) = sessions.get_mut(session_name) {
            parser.reset_turn();
        }
    }

    /// #3041 P1-3 (Part a, B1 — FRAME-CARRIED commit fence): the post-POST wrapper around
    /// the identity-gated advance. Re-loads inflight FRESH AFTER the POST (codex P1-3 issue
    /// 3 — a pre-POST snapshot could authorize a wrong-turn advance if the turn was
    /// cleared/replaced during the POST), runs the pure gate
    /// (`advance_offset_for_confirmed_delegated_terminal`), and commits the #3151 marker.
    fn advance_after_confirmed_post(
        &self,
        shared: &super::SharedData,
        provider: &ProviderKind,
        channel_id: u64,
        session_name: &str,
        delivery: &SessionRelayDelivery,
        sink_lease_guard: Option<&SinkDeliveryLeaseGuard>,
    ) {
        let fresh_inflight = super::inflight::load_inflight_state(provider, channel_id);
        let advanced = self.advance_offset_for_confirmed_delegated_terminal(
            shared,
            provider,
            channel_id,
            session_name,
            delivery,
            fresh_inflight.as_ref(),
        );
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

    fn advance_offset_for_confirmed_delegated_terminal(
        &self,
        shared: &super::SharedData,
        provider: &ProviderKind,
        channel_id: u64,
        session_name: &str,
        delivery: &SessionRelayDelivery,
        inflight: Option<&super::inflight::InflightTurnState>,
    ) -> bool {
        let Some(end) = delivery.terminal_consumed_end.filter(|end| *end > 0) else {
            return false;
        };
        // IDENTITY GATE: the frame's pinned turn identity must still match the
        // channel's current inflight. A delayed frame from an already-replaced
        // turn (or a cleared inflight) is ignored — never advances a wrong turn.
        let Some(inflight) = inflight else {
            tracing::debug!(
                provider = provider.as_str(),
                channel_id,
                tmux_session = %session_name,
                frame_user_msg_id = delivery.frame_turn_user_msg_id,
                "session-bound sink: terminal frame carried a commit fence but inflight is gone; identity gate blocks advance"
            );
            return false;
        };
        // #3041 P1-3 (codex P1-3 issue 2 R4): STRICT `turn_start_offset` identity — a
        // REQUIRED gate part with NO None fallback (two `user_msg_id == 0` turns in the same
        // second collide on the weak `(user_msg_id, started_at)` pair). A fenced frame is
        // GUARANTEED a real offset by the producer, so `None`/mismatch is a stale/wrong-turn
        // frame → MUST NOT advance (the watcher's SendFull delivers — no black-hole).
        let identity_matches = inflight.user_msg_id == delivery.frame_turn_user_msg_id
            && inflight.started_at == delivery.frame_turn_started_at
            && delivery.frame_turn_start_offset.is_some()
            && inflight.turn_start_offset == delivery.frame_turn_start_offset;
        if !identity_matches {
            tracing::debug!(
                provider = provider.as_str(),
                channel_id,
                tmux_session = %session_name,
                frame_user_msg_id = delivery.frame_turn_user_msg_id,
                inflight_user_msg_id = inflight.user_msg_id,
                frame_turn_start_offset = delivery.frame_turn_start_offset,
                inflight_turn_start_offset = inflight.turn_start_offset,
                "session-bound sink: terminal frame identity != current inflight; identity gate blocks advance (delayed/wrong-turn frame)"
            );
            return false;
        }
        super::tmux::advance_watcher_confirmed_end(
            shared,
            provider,
            ChannelId::new(channel_id),
            session_name,
            end,
            "src/services/discord/session_relay_sink.rs:sink_confirmed_terminal_advance",
        );
        // #3976: stamp the durable per-row delivered marker ONLY here — past the
        // identity gate, after the `confirmed_end_offset` watermark advance fired
        // (so a refused/identity-mismatched advance, which returned above, never
        // marks the row). The watermark is resettable and writes nothing else to
        // the row, so without this durable marker a delivered-but-unmirrored row is
        // indistinguishable from a never-delivered black-hole and orphan-reclaim
        // would re-emit its tail on a watermark reset. The flock RMW re-gates the
        // identity under the lock, so a turn replaced during the POST is never
        // marked. Best-effort: a residual crash between the POST and this write
        // reverts the row to orphan shape on reboot (same at-most-once residual the
        // #3918 marker bounds) — acceptable and no worse than today.
        super::inflight::mark_session_bound_relay_delivered_locked(
            provider,
            channel_id,
            &super::inflight::InflightTurnIdentity::from_state(inflight),
            session_name,
        );
        true
    }

    /// #3089 A2b: short-replace via the turn-output controller, behaviourally equal to legacy
    /// `replace_long_message_raw_with_outcome` — SAME transport + `LeaseHolder::Sink` cell (one
    /// acquire/commit/release, no double-acquire), #3151 heartbeat, #2757 `PreserveAlways`,
    /// `CommitOnFallback`, identity-gated advance (FRESH post-POST reload): confirmed POST →
    /// `Delivered`, ambiguous → `Err(Transient)` (I2); `Replace { Active }` → `post_send_finalize`
    /// no-op. `gateway` seam (review-fix Medium-1): live = real gateway, test fakes it.
    #[allow(clippy::too_many_arguments)]
    async fn deliver_short_replace_via_controller<G: super::gateway::TurnGateway + ?Sized>(
        &self,
        gateway: &G,
        shared: &Arc<super::SharedData>,
        provider: &ProviderKind,
        channel: ChannelId,
        channel_id: u64,
        msg_id: MessageId,
        relay_text: &str,
        delivered_fingerprint_body: &str,
        delivery: &SessionRelayDelivery,
        trace: &SessionRelayTraceContext,
        start: u64,
        end: u64,
    ) -> Result<SessionRelayDeliveryOutcome, RelaySinkError> {
        let sink_turn = super::turn_finalizer::TurnKey::new(
            channel,
            delivery.frame_turn_user_msg_id,
            shared.restart.current_generation,
        );
        let sink_lease_key =
            delivery_lease_key_for_frame(channel, shared.restart.current_generation, delivery);
        let cell = shared.delivery_lease(channel);
        // Self-heal (`SinkDeliveryLeaseGuard::acquire`): reclaim an EXPIRED prior holder before acquire (a stale dead lease must not force a markerless POST).
        cell.reclaim_if_expired(super::lease_now_ms());
        let heartbeat = SinkPostHeartbeat { cell: cell.clone() };
        // Identity-gated advance: INLINE before any post-send await (I1), SAME FRESH-reload gate as `advance_after_confirmed_post` (`true`→`Delivered`, `false`→`NotDelivered`).
        let advance = |_range: (u64, u64)| -> bool {
            let fresh = super::inflight::load_inflight_state(provider, channel_id);
            self.advance_offset_for_confirmed_delegated_terminal(
                shared,
                provider,
                channel_id,
                &delivery.session_name,
                delivery,
                fresh.as_ref(),
            )
        };
        let outcome = toc::deliver_turn_output(
            gateway,
            toc::TurnOutputCtx {
                turn: sink_turn,
                lease_key: Some(sink_lease_key),
                owner: RelayOwnerKind::SessionBoundRelay,
                holder: super::LeaseHolder::Sink,
                lease: &*cell,
                channel_id: channel,
                placeholder_controller: &shared.ui.placeholder_controller,
                placeholder: toc::PlaceholderSlot::Active {
                    message_id: msg_id,
                    key: PlaceholderKey {
                        provider: provider.clone(),
                        channel_id: channel,
                        message_id: msg_id,
                    },
                },
                body: relay_text,
                send_range: (start, end),
                // `Replace { Active }` → non-terminal → `post_send_finalize` no-ops (no
                // placeholder transition), matching the legacy edit-in-place.
                plan: toc::OutputPlan::Replace {
                    lifecycle: PlaceholderLifecycle::Active,
                },
                edit_fail_policy: toc::EditFailPlaceholderPolicy::PreserveAlways,
                fallback_commit_policy: toc::FallbackCommitPolicy::CommitOnFallback,
                acquire_failure_mode: toc::AcquireFailureMode::ProceedMarkerless,
                advance: Some(&advance),
                heartbeat: Some(&heartbeat),
            },
        )
        .await;

        // #3089 B1: shadow-mirror durable delivered frontier — flag-gated, observe-only, Delivered-only (I2), OFF=no-op.
        // #3610 PR-1: anchor msg = `msg_id` (current_msg_id — true terminal anchor, not status_message_id). PR-1b: anchor channel = `channel` (same-channel path).
        dr::shadow_mirror_same_channel_frontier_with_body(
            shared,
            provider,
            channel,
            (start, end),
            dr::outcome_is_shadow_delivered(&outcome),
            msg_id.get(),
            delivered_fingerprint_body,
        );

        match outcome {
            // Confirmed POST (edit OR #2757 fallback): controller already ran advance + commit;
            // BOTH map to sink-local `Delivered` (POST landed; lease outcome only steers the watcher). Emit legacy side-effects.
            toc::DeliveryOutcome::Delivered { .. } | toc::DeliveryOutcome::NotDelivered { .. } => {
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
                    "session-bound relay sink delivered terminal response via placeholder edit (controller #3089 A2b)"
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
                    Some("placeholder edit (controller)"),
                );
                Ok(SessionRelayDeliveryOutcome::Delivered)
            }
            // #4046 S1r-1 P2: FreshDelivered is a confirmed cross-verb POST (dormant
            // here); its `Permanent` mapping is a non-retry INTENT marker only and does
            // NOT itself prevent a duplicate POST — the sink consumer is
            // error-variant-blind (see `delivery_outcome_classify` doc + #4623). Others
            // are uncommitted → retriable. Classified out-of-line (frozen #3016 giant).
            non_delivery @ (toc::DeliveryOutcome::FreshDelivered { .. }
            | toc::DeliveryOutcome::Transient { .. }
            | toc::DeliveryOutcome::Unknown { .. }
            | toc::DeliveryOutcome::Skipped) => Err(
                delivery_outcome_classify::short_replace_non_delivery_error(&non_delivery),
            ),
        }
    }

    async fn deliver_response(
        &self,
        delivery: SessionRelayDelivery,
    ) -> Result<SessionRelayDeliveryOutcome, RelaySinkError> {
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
        let http = shared.serenity_http_or_token_fallback().ok_or_else(|| {
            RelaySinkError::Transient(format!(
                "discord http unavailable for provider {}",
                provider.as_str()
            ))
        })?;

        let (raw_response_text, relay_text) =
            relay_format::session_bound_relay_bodies(&shared, &provider, &delivery);
        let channel = ChannelId::new(channel_id);
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

        // #3089 A2b/#3998 S1-f2: structurally eligible short-replace
        // (PlaceholderEdit + single-message body) routes to the controller
        // unconditionally. The controller owns the single lease, so the sink skips
        // `SinkDeliveryLeaseGuard` (no double-acquire). ONLY a real ordered
        // `[start,end)` is eligible; degenerate stays legacy. EMPTY `relay_text`
        // ALSO stays legacy (review-fix M2): legacy
        // `replace_long_message_raw_with_outcome` treats zero chunks as
        // `EditedOriginal` (delivered/advance, `formatting.rs:2063`) but the
        // controller returns `Skipped` (no-advance) for `body.is_empty()` —
        // diverting would flip → Transient.
        let cutover_range = match (
            delivery.frame_turn_start_offset,
            delivery.terminal_consumed_end,
        ) {
            (Some(start), Some(end)) if end > start => Some((start, end)),
            _ => None,
        };
        let cutover_short_replace = cutover_range.is_some()
            && !relay_text.is_empty()
            && matches!(route, SessionBoundTerminalDeliveryRoute::PlaceholderEdit(_))
            && !session_bound_should_send_new_chunks_for_placeholder(&relay_text);

        // #3151: acquire the in-flight `Leased{Sink}` marker BEFORE the POST (see
        // `SinkDeliveryLeaseGuard`). The legacy long-chunk + new-message branches acquire
        // ONE marker over the real ordered `cutover_range`; the cut-over short-replace branch
        // is EXCLUDED (the CONTROLLER owns its lease — no double-acquire). The pure
        // `sink_guard_lease_range` encodes that (review-fix Medium-1).
        let sink_lease_guard = sink_guard_lease_range(cutover_range, cutover_short_replace)
            .and_then(|(start, end)| {
                let sink_lease_key = delivery_lease_key_for_frame(
                    channel,
                    shared.restart.current_generation,
                    &delivery,
                );
                let cell = shared.delivery_lease(channel);
                SinkDeliveryLeaseGuard::acquire(&cell, sink_lease_key, start, end)
            });

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
                // Live path: the real `DiscordGateway` (the seam the ON-path test fakes).
                let gateway = super::gateway::DiscordGateway::new(
                    http.clone(),
                    shared.clone(),
                    provider.clone(),
                    None,
                );
                return self
                    .deliver_short_replace_via_controller(
                        &gateway,
                        &shared,
                        &provider,
                        channel,
                        channel_id,
                        msg_id,
                        &relay_text,
                        &raw_response_text,
                        &delivery,
                        &trace,
                        start,
                        end,
                    )
                    .await;
            }
            if session_bound_should_send_new_chunks_for_placeholder(&relay_text) {
                formatting::send_long_message_raw_with_rollback(
                    &http,
                    channel,
                    msg_id,
                    &relay_text,
                    &shared,
                )
                .await
                .map_err(|error| RelaySinkError::Transient(error.to_string()))?;
                let _ = super::http::delete_channel_message(&http, channel, msg_id).await;
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
                // #3041 P1-4 (§4-④): external_input lease released by the RAII guard
                // on exit. #3041 P1-3 (Part a, B1, codex issue 3): couple the confirmed
                // POST to the advance, re-checking the gate against fresh-reloaded inflight.
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
            match formatting::replace_long_message_raw_with_outcome(
                &http,
                channel,
                msg_id,
                &relay_text,
                &shared,
                // #3805 P1: the session-bound relay sink does not append a
                // completion footer, so the last-chunk anchor is unused here.
                &mut None,
            )
            .await
            {
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
                    // #3041 P1-4 (§4-④) lease released by RAII guard. #3041 P1-3 (Part a,
                    // B1, codex issue 3): commit fence — post-POST fresh re-check before advance.
                    self.advance_after_confirmed_post(
                        &shared,
                        &provider,
                        channel_id,
                        &delivery.session_name,
                        &delivery,
                        sink_lease_guard.as_ref(),
                    );
                    Ok(SessionRelayDeliveryOutcome::Delivered)
                }
                Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                    edit_error, ..
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
                    // #3041 P1-4 (§4-④) lease released by RAII guard. #3041 P1-3 (Part a,
                    // B1, codex issue 3): the fallback POST delivered → advance too, after
                    // a post-POST fresh re-check.
                    self.advance_after_confirmed_post(
                        &shared,
                        &provider,
                        channel_id,
                        &delivery.session_name,
                        &delivery,
                        sink_lease_guard.as_ref(),
                    );
                    Ok(SessionRelayDeliveryOutcome::Delivered)
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
                &http,
                &shared,
                &provider,
                channel_id,
                &delivery,
                &relay_text,
                task_card_message_id,
                task_response_claim,
                &trace,
                sink_lease_guard.as_ref(),
            )
            .await
        }
    }
}

/// #3041 P1-5: the SINK-LOCAL terminal outcome stays deliberately 2-way — the sink
/// always KNOWS its result: confirmed POST/edit → `Delivered`; deterministic
/// route decline (foreign-owner block / bridge-owned / mismatched inflight) →
/// `NotDelivered`; transport/format failure → `Err`. NO sink-local `Unknown` (that
/// is the cross-actor relay-ring + watcher state). `NotDelivered` (former `Skipped`)
/// maps to `RelaySinkOutcome::TerminalNotDelivered`, routed through §3.2
/// reconciliation — never a blind skip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionRelayDeliveryOutcome {
    Delivered,
    SentButUncommitted,
    NotDelivered,
}

#[async_trait]
impl RelaySink for SessionBoundDiscordRelaySink {
    async fn deliver(&self, frame: &StreamFrame) -> Result<RelaySinkOutcome, RelaySinkError> {
        // #3041 P1-3 R5 (codex — REVERT R4 fence-gating of the outcome): a result-bearing
        // delivery reports Delivered/NotDelivered REGARDLESS of a fence on this frame
        // (R4's gate BLACK-HOLED the legitimate no-inflight terminal — no fence but a real
        // terminal → `FrameAccepted` → watcher timed out). The co-chunked confusion is now
        // handled by the per-sequence ACK. The fence still ONLY gates the OFFSET ADVANCE
        // (inline in `deliver_response`) — outcome and advance are decoupled.
        let deliveries = self.ingest_frame(frame);
        let mut terminal_delivered = false;
        let mut terminal_not_delivered = false;
        for delivery in deliveries {
            let session_name = delivery.session_name.clone();
            match self.deliver_response(delivery).await {
                Ok(SessionRelayDeliveryOutcome::Delivered) => {
                    // #3041 P1-3 (B1 CLOSED): the offset advance is owned INLINE by
                    // `deliver_response` — see `advance_after_confirmed_post`.
                    terminal_delivered = true;
                    self.finish_terminal_candidate(&session_name);
                }
                Ok(SessionRelayDeliveryOutcome::SentButUncommitted) => {
                    self.finish_terminal_candidate(&session_name);
                    return Ok(RelaySinkOutcome::TerminalUnknown);
                }
                Ok(SessionRelayDeliveryOutcome::NotDelivered) => {
                    terminal_not_delivered = true;
                    self.finish_terminal_candidate(&session_name);
                }
                Err(error) => {
                    self.finish_terminal_candidate(&session_name);
                    return Err(error);
                }
            }
        }
        // #3041 P1-3 R5: surface the outcome on THIS frame's sequence (the watcher
        // resolves its own terminal ACK on its exact seq, so a co-chunked tail can't
        // satisfy another turn's ACK); no result-bearing delivery → `FrameAccepted`.
        // #3041 P1-5: NO `TerminalUnknown` (the sink always KNOWS its result).
        if terminal_delivered {
            Ok(RelaySinkOutcome::TerminalDelivered)
        } else if terminal_not_delivered {
            Ok(RelaySinkOutcome::TerminalNotDelivered)
        } else {
            Ok(RelaySinkOutcome::FrameAccepted)
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
                session_init_seen.remove(&session_name);
            }
            let current_generation_signature =
                super::tmux::read_generation_file_mtime_ns(&session_name);
            idle_jsonl_clear_session_init_on_generation_signature_change(
                &mut session_init_seen,
                &mut session_generation_signatures,
                &session_name,
                current_generation_signature,
            );
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
                // #3960: if a SessionBoundRelay claim lost its producer before
                // commit, downgrade it to the ownerless backstop after fresh
                // liveness/offset checks; the send point still re-gates delivery.
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
                    let _decision = idle_jsonl_apply_active_inflight_gate(
                        &mut last_inflight_seen_at,
                        &matched,
                        channel_id,
                        &inflight,
                        len,
                        offset,
                    );
                    continue;
                }
                last_inflight_seen_at.remove(&session_name);
                tracing::debug!(
                    provider = matched.provider.as_str(),
                    channel_id,
                    tmux_session = %session_name,
                    user_msg_id = inflight.user_msg_id,
                    updated_at = %inflight.updated_at,
                    "idle JSONL relay ignored stale ownerless TUI-direct inflight blocker"
                );
            }
            if last_inflight_seen_at
                .get(&session_name)
                .is_some_and(|seen_at| seen_at.elapsed() < IDLE_JSONL_RELAY_RECENT_INFLIGHT_GRACE)
            {
                consume_idle_offset!(len, IdleJsonlSessionInitRearm::Keep);
                continue;
            }
            if len <= *offset {
                continue;
            }

            let start = *offset;
            let end = len.min(start.saturating_add(IDLE_JSONL_RELAY_MAX_BYTES_PER_TICK));
            let Ok(payload) = read_jsonl_range(&relay_source.path, start, end) else {
                continue;
            };
            if payload.is_empty() {
                consume_idle_offset!(end, IdleJsonlSessionInitRearm::Keep);
                continue;
            }
            // Classify the WHOLE payload first so the dedup/trim run on an
            // already-classified turn (mirrors `idle_relay_range_action`'s ordering;
            // the per-reason debug logs below stay for observability).
            let in_new_session_grace =
                first_seen.elapsed() < IDLE_JSONL_RELAY_RECENT_INFLIGHT_GRACE;
            if in_new_session_grace {
                consume_idle_offset!(end, IdleJsonlSessionInitRearm::Keep);
                tracing::debug!(
                    provider = matched.provider.as_str(),
                    channel_id,
                    tmux_session = %session_name,
                    bytes = payload.len(),
                    "idle JSONL relay skipped new-session grace payload"
                );
                continue;
            }
            if idle_jsonl_payload_contains_user_event(&payload) {
                consume_idle_offset!(end, IdleJsonlSessionInitRearm::Clear);
                tracing::debug!(
                    provider = matched.provider.as_str(),
                    channel_id,
                    tmux_session = %session_name,
                    bytes = payload.len(),
                    "idle JSONL relay skipped active-turn payload with user/tool-result event"
                );
                continue;
            }
            if idle_jsonl_payload_contains_schedule_wakeup_setup(&payload) {
                consume_idle_offset!(end, IdleJsonlSessionInitRearm::Keep);
                tracing::debug!(
                    provider = matched.provider.as_str(),
                    channel_id,
                    tmux_session = %session_name,
                    bytes = payload.len(),
                    "idle JSONL relay skipped ScheduleWakeup setup payload"
                );
                continue;
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
            let session_has_init =
                idle_jsonl_session_has_init(&mut session_init_seen, &session_name, &payload);
            if !relay_source.allow_continued_session_without_init && !session_has_init {
                consume_idle_offset!(end, IdleJsonlSessionInitRearm::Keep);
                tracing::debug!(
                    provider = matched.provider.as_str(),
                    channel_id,
                    tmux_session = %session_name,
                    bytes = payload.len(),
                    "idle JSONL relay skipped non-init active-session payload"
                );
                continue;
            }
            let Some(producer) = producers.get_producer(&session_name) else {
                tracing::debug!(
                    tmux_session = %session_name,
                    "idle JSONL relay found new bytes but no session-bound producer"
                );
                continue;
            };
            if idle_jsonl_should_retry_without_dedup_shared(shared_for_dedup.as_ref()) {
                tracing::debug!(
                    provider = matched.provider.as_str(),
                    channel_id,
                    tmux_session = %session_name,
                    range_start = start,
                    range_end = end,
                    "idle JSONL relay skipped range because dedup shared data is unavailable; will retry without consuming"
                );
                continue;
            }
            // #3017 single output-offset authority: this idle path and the watcher both read
            // the SAME JSONL (E-13: an inflight-less wake relayed twice). The watcher is PRIMARY
            // and advances `confirmed_end_offset`; this backstop only CONSULTS it read-only
            // (committed >= range end → skip), no advance (codex P1: `try_send_frame` only QUEUES).
            if let Some(shared) = shared_for_dedup {
                // #3089 B2b: durable-frontier dedup authority (flag OFF → in-memory).
                let committed = dr::effective_committed_offset(
                    &shared,
                    &matched.provider,
                    channel,
                    &session_name,
                    Some(len),
                );
                // Classification passed above → consults ONLY the offset-authority dedup branch.
                match idle_relay_range_action(
                    &payload,
                    start,
                    end,
                    committed,
                    false,
                    relay_source.allow_continued_session_without_init,
                    session_has_init,
                ) {
                    IdleRelayRangeAction::SkipAlreadyRelayed => {
                        // Whole range already delivered by the watcher → skip.
                        consume_idle_offset!(end, IdleJsonlSessionInitRearm::Keep);
                        tracing::debug!(
                            provider = matched.provider.as_str(),
                            channel_id,
                            tmux_session = %session_name,
                            committed_relay_offset = committed,
                            end,
                            "idle JSONL relay skipped range already relayed by watcher (offset authority dedup)"
                        );
                        continue;
                    }
                    IdleRelayRangeAction::SendSuffixFrom(from) => {
                        // Codex r5 P2 + codex r6 P1 (black-hole): PARTIAL overlap — the watcher
                        // delivered the `[start, committed)` prefix; deliver ONLY the uncommitted
                        // suffix THIS pass (a next-tick bounce would re-read a suffix that lost the
                        // `system/init` event → re-classified and DROPPED). See `SendSuffixFrom`.
                        let suffix = match read_jsonl_range(&relay_source.path, from, end) {
                            Ok(suffix) => suffix,
                            Err(_) => continue,
                        };
                        if suffix.is_empty() {
                            consume_idle_offset!(end, IdleJsonlSessionInitRearm::Keep);
                            continue;
                        }
                        if producer.try_send_frame(String::from_utf8_lossy(&suffix).into_owned()) {
                            consume_idle_offset!(end, IdleJsonlSessionInitRearm::Keep);
                            tracing::debug!(
                                provider = matched.provider.as_str(),
                                channel_id,
                                tmux_session = %session_name,
                                committed_relay_offset = committed,
                                start,
                                end,
                                bytes = suffix.len(),
                                "idle JSONL relay sent un-committed suffix after trimming already-relayed prefix (offset authority dedup, no black-hole)"
                            );
                        }
                        continue;
                    }
                    // `committed <= start` → nothing covered → fall through to the full-range send.
                    IdleRelayRangeAction::SendFull => {}
                    // Unreachable here: `in_new_session_grace = false` and the payload already
                    // passed the init gate (classification happened above).
                    IdleRelayRangeAction::SkipClassified => {}
                }
            }
            if producer.try_send_frame(String::from_utf8_lossy(&payload).into_owned()) {
                consume_idle_offset!(end, IdleJsonlSessionInitRearm::Keep);
                tracing::debug!(
                    provider = matched.provider.as_str(),
                    channel_id,
                    tmux_session = %session_name,
                    bytes = payload.len(),
                    "idle JSONL relay forwarded background session output"
                );
            }
        }

        prune_idle_jsonl_session_state(
            &seen_sessions,
            &mut offsets,
            &mut first_seen_at,
            &mut last_inflight_seen_at,
            &mut session_init_seen,
            &mut session_generation_signatures,
        );
        tokio::time::sleep(IDLE_JSONL_RELAY_POLL_INTERVAL).await;
    }
}

struct SessionRelayParser {
    buffer: String,
    stream_state: StreamLineState,
    full_response: String,
    tool_state: WatcherToolState,
    task_notification_kind: Option<TaskNotificationKind>,
    task_notification_context: Option<super::task_notification_delivery::TaskNotificationContext>,
    assistant_text_seen: bool,
    frames_observed: u64,
    last_sequence: u64,
}

impl Default for SessionRelayParser {
    fn default() -> Self {
        Self {
            buffer: String::new(),
            stream_state: StreamLineState::new(),
            full_response: String::new(),
            tool_state: WatcherToolState::new(),
            task_notification_kind: None,
            task_notification_context: None,
            assistant_text_seen: false,
            frames_observed: 0,
            last_sequence: 0,
        }
    }
}

impl SessionRelayParser {
    fn ingest_frame(&mut self, frame: &StreamFrame) -> Vec<SessionRelayDelivery> {
        self.frames_observed = self.frames_observed.saturating_add(1);
        self.last_sequence = frame.sequence;
        self.buffer.push_str(&frame.payload);

        let channel_id = match frame.binding.channel_id.parse::<u64>() {
            Ok(channel_id) => channel_id,
            Err(error) => {
                tracing::warn!(
                    channel_id = %frame.binding.channel_id,
                    error = %error,
                    "session-bound relay sink skipped frame with invalid channel id"
                );
                return Vec::new();
            }
        };

        let mut deliveries = Vec::new();
        loop {
            let outcome = process_watcher_lines(
                &mut self.buffer,
                &mut self.stream_state,
                &mut self.full_response,
                &mut self.tool_state,
            );
            if let Some(kind) = outcome.task_notification_kind {
                self.task_notification_kind =
                    merge_task_notification_kind(self.task_notification_kind, kind);
            }
            if let Some(context) = outcome.task_notification_context {
                self.task_notification_context = super::task_notification_delivery::merge_context(
                    self.task_notification_context.take(),
                    context,
                );
            }
            self.assistant_text_seen |= outcome.assistant_text_seen;
            if !outcome.found_result {
                break;
            }

            let task_kind_allows_delivery = task_notification_context::allows_delivery(
                self.task_notification_kind,
                self.assistant_text_seen,
            );
            let has_user_visible_response =
                !self.full_response.trim().is_empty() && task_kind_allows_delivery;
            if has_user_visible_response {
                deliveries.push(SessionRelayDelivery {
                    provider: frame.binding.provider.clone(),
                    channel_id,
                    session_name: frame.session_name.clone(),
                    response_text: self.full_response.clone(),
                    task_notification_kind: self.task_notification_kind,
                    task_notification_context: self.task_notification_context.clone(),
                    // #3041 P1-3 (Part a, B1): the RESULT frame carries the commit fence;
                    // copying it onto the delivery keeps the POST and the identity-gated
                    // advance atomic per-frame.
                    terminal_consumed_end: frame.terminal_consumed_end,
                    frame_turn_user_msg_id: frame.turn_user_msg_id,
                    frame_turn_started_at: frame.turn_started_at.clone(),
                    frame_turn_start_offset: frame.turn_start_offset,
                });
                break;
            } else {
                self.reset_turn();
            }
            if self.buffer.trim().is_empty() {
                break;
            }
        }

        deliveries
    }

    fn reset_turn(&mut self) {
        self.stream_state = StreamLineState::new();
        self.full_response.clear();
        self.tool_state = WatcherToolState::new();
        self.task_notification_kind = None;
        self.task_notification_context = None;
        self.assistant_text_seen = false;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionRelayDelivery {
    provider: ProviderKind,
    channel_id: u64,
    session_name: String,
    response_text: String,
    task_notification_kind: Option<TaskNotificationKind>,
    task_notification_context: Option<super::task_notification_delivery::TaskNotificationContext>,
    /// #3041 P1-3 (Part a, B1 — frame-carried commit fence): the producer's authoritative
    /// consumed END on this RESULT frame (`None` = no delegate). A CONFIRMED delivery
    /// advances `confirmed_end_offset` to it, gated by the carried turn identity.
    terminal_consumed_end: Option<u64>,
    /// Pinned turn identity (`user_msg_id`, `started_at`) the IDENTITY GATE compares to
    /// the current inflight before advancing — a delayed/wrong-turn frame is ignored.
    frame_turn_user_msg_id: u64,
    frame_turn_started_at: String,
    /// #3041 P1-3 (codex P1-3 issue 2): the pinned `turn_start_offset`, a REQUIRED gate
    /// part — two `user_msg_id == 0` turns in the same second share `(0, started_at)`; the
    /// monotonic offset disambiguates. `None` on legacy/non-fence frames.
    frame_turn_start_offset: Option<u64>,
}

fn delivery_lease_key_for_frame(
    channel: ChannelId,
    generation: u64,
    delivery: &SessionRelayDelivery,
) -> super::DeliveryLeaseKey {
    super::DeliveryLeaseKey::new_for_site(
        channel,
        generation,
        delivery.frame_turn_user_msg_id,
        Some(&delivery.frame_turn_started_at),
        delivery.frame_turn_start_offset,
        "sink",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::cluster::session_matcher::{MatchedChannel, expected_rollout_path_for};
    use crate::services::discord::inflight::{RelayOwnerKind, TurnSource};
    use crate::services::tui_prompt_dedupe::{ExternalInputRelayLease, ExternalInputRelayOwner};

    fn matched(channel_id: &str) -> MatchedChannel {
        let session = ProviderKind::Claude.build_tmux_session_name(channel_id);
        MatchedChannel {
            channel_id: channel_id.to_string(),
            agent_id: format!("agent-{channel_id}"),
            provider: ProviderKind::Claude,
            expected_session_name: session.clone(),
            expected_rollout_path: expected_rollout_path_for(&session),
        }
    }

    fn matched_codex(channel_id: &str) -> MatchedChannel {
        let session = ProviderKind::Codex.build_tmux_session_name(channel_id);
        MatchedChannel {
            channel_id: channel_id.to_string(),
            agent_id: format!("agent-{channel_id}"),
            provider: ProviderKind::Codex,
            expected_session_name: session.clone(),
            expected_rollout_path: expected_rollout_path_for(&session),
        }
    }

    fn frame(binding: &MatchedChannel, payload: &str, sequence: u64) -> StreamFrame {
        StreamFrame {
            session_name: binding.expected_session_name.clone(),
            binding: binding.clone(),
            payload: payload.to_string(),
            sequence,
            terminal_consumed_end: None,
            turn_user_msg_id: 0,
            turn_started_at: String::new(),
            turn_start_offset: None,
        }
    }

    fn terminal_frame(
        binding: &MatchedChannel,
        payload: &str,
        sequence: u64,
        consumed_end: u64,
        turn_user_msg_id: u64,
        turn_started_at: &str,
    ) -> StreamFrame {
        terminal_frame_offset(
            binding,
            payload,
            sequence,
            consumed_end,
            turn_user_msg_id,
            turn_started_at,
            Some(0),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn terminal_frame_offset(
        binding: &MatchedChannel,
        payload: &str,
        sequence: u64,
        consumed_end: u64,
        turn_user_msg_id: u64,
        turn_started_at: &str,
        turn_start_offset: Option<u64>,
    ) -> StreamFrame {
        StreamFrame {
            session_name: binding.expected_session_name.clone(),
            binding: binding.clone(),
            payload: payload.to_string(),
            sequence,
            terminal_consumed_end: Some(consumed_end),
            turn_user_msg_id,
            turn_started_at: turn_started_at.to_string(),
            turn_start_offset,
        }
    }

    #[test]
    fn idle_jsonl_payload_detects_user_tool_result_events() {
        let payload = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"ScheduleWakeup\"}]}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"scheduled\"}]}}\n",
            "{\"type\":\"result\",\"result\":\"setup complete\"}\n"
        );
        assert!(idle_jsonl_payload_contains_user_event(payload.as_bytes()));
    }

    #[test]
    fn idle_jsonl_payload_allows_external_wakeup_result() {
        let payload = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"[E2E:E13:WAKE]\"}]}}\n",
            "{\"type\":\"result\",\"result\":\"[E2E:E13:WAKE]\"}\n"
        );
        assert!(!idle_jsonl_payload_contains_user_event(payload.as_bytes()));
        assert!(idle_jsonl_payload_contains_init_event(payload.as_bytes()));
        assert!(!idle_jsonl_payload_contains_schedule_wakeup_setup(
            payload.as_bytes()
        ));
    }

    #[test]
    fn idle_jsonl_payload_allows_wakeup_result_with_schedule_wakeup_tool_list() {
        let payload = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\",\"tools\":[\"ScheduleWakeup\",\"Bash\"]}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"[E2E:E13:WAKE]\"}]}}\n",
            "{\"type\":\"result\",\"result\":\"[E2E:E13:WAKE]\"}\n"
        );
        assert!(idle_jsonl_payload_contains_init_event(payload.as_bytes()));
        assert!(!idle_jsonl_payload_contains_schedule_wakeup_setup(
            payload.as_bytes()
        ));
    }

    #[test]
    fn idle_jsonl_payload_rejects_schedule_wakeup_setup_result() {
        let payload = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"E-13 setup. ScheduleWakeup 예약 완료.\"}]}}\n",
            "{\"type\":\"result\",\"result\":\"E-13 setup. ScheduleWakeup 예약 완료.\"}\n"
        );
        assert!(idle_jsonl_payload_contains_init_event(payload.as_bytes()));
        assert!(idle_jsonl_payload_contains_schedule_wakeup_setup(
            payload.as_bytes()
        ));
    }

    #[test]
    fn idle_jsonl_payload_rejects_schedule_wakeup_tool_use() {
        let payload = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"ScheduleWakeup\",\"input\":{\"delaySeconds\":20}}]}}\n",
            "{\"type\":\"result\",\"result\":\"scheduled\"}\n"
        );
        assert!(idle_jsonl_payload_contains_schedule_wakeup_setup(
            payload.as_bytes()
        ));
    }

    #[test]
    fn idle_jsonl_payload_rejects_steady_session_result_without_init() {
        let payload = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"[E2E:E1:OK]\"}]}}\n",
            "{\"type\":\"result\",\"result\":\"[E2E:E1:OK]\"}\n"
        );
        assert!(!idle_jsonl_payload_contains_init_event(payload.as_bytes()));
    }

    #[test]
    fn idle_jsonl_source_uses_codex_tui_runtime_output_path() {
        let _guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let temp = tempfile::tempdir().expect("tempdir");
        let rollout_path = temp.path().join("rollout.jsonl");
        std::fs::write(
            &rollout_path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s1\"}}\n",
        )
        .expect("write rollout");
        let matched = matched_codex("1479671301387059200");
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            &matched.expected_session_name,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
                output_path: rollout_path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("s1".to_string()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );

        let source = idle_jsonl_relay_source_for_matched(&matched);

        assert_eq!(source.path, rollout_path.display().to_string());
        assert!(
            source.allow_continued_session_without_init,
            "known Codex TUI runtime bindings may relay continued assistant-only suffixes"
        );
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
    }

    fn inflight_for(
        tmux_session_name: &str,
        relay_owner_kind: RelayOwnerKind,
        rebind_origin: bool,
    ) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            42,
            Some("relay-test".to_string()),
            7,
            9001,
            9002,
            "prompt".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        state.set_relay_owner_kind(relay_owner_kind);
        state.rebind_origin = rebind_origin;
        state
    }

    #[test]
    fn relay_ownership_uses_session_bound_inflight_shape() {
        let tmux = "AgentDesk-claude-relay-test";
        let bridge_owned = inflight_for(tmux, RelayOwnerKind::None, false);
        assert!(!session_bound_discord_relay_can_own_terminal_delivery(
            Some(&bridge_owned),
            tmux
        ));
        assert!(session_bound_discord_relay_can_own_terminal_delivery(
            None, tmux
        ));
        assert!(!session_bound_discord_relay_can_own_terminal_delivery(
            None, ""
        ));

        let watcher_owned = inflight_for(tmux, RelayOwnerKind::Watcher, false);
        assert!(!session_bound_discord_relay_can_own_terminal_delivery(
            Some(&watcher_owned),
            tmux
        ));

        let mut adopted = inflight_for(tmux, RelayOwnerKind::None, true);
        adopted.turn_source = super::super::inflight::TurnSource::ExternalAdopted;
        assert!(session_bound_discord_relay_can_own_terminal_delivery(
            Some(&adopted),
            tmux
        ));

        adopted.turn_source = super::super::inflight::TurnSource::Managed;
        assert!(session_bound_discord_relay_can_own_terminal_delivery(
            Some(&adopted),
            tmux
        ));

        let mut external_session_bound =
            inflight_for(tmux, RelayOwnerKind::SessionBoundRelay, false);
        external_session_bound.turn_source = TurnSource::ExternalInput;
        assert!(session_bound_discord_relay_can_own_terminal_delivery(
            Some(&external_session_bound),
            tmux
        ));
        assert!(!session_bound_discord_relay_can_own_terminal_delivery(
            Some(&watcher_owned),
            "AgentDesk-claude-other"
        ));
    }

    #[test]
    fn terminal_delivery_route_allows_missing_inflight_as_pane_bound_new_message() {
        let tmux = "AgentDesk-claude-relay-test";

        assert_eq!(
            session_bound_terminal_delivery_route(None, tmux),
            Some(SessionBoundTerminalDeliveryRoute::NewMessage)
        );
        assert_eq!(session_bound_terminal_delivery_route(None, ""), None);
    }

    #[test]
    fn placeholder_long_terminal_delivery_uses_ordered_new_chunks() {
        let body = format!(
            "[E2E:E15:BEGIN]\n{}\n[E2E:E15:MID]\n{}\n[E2E:E15:END]",
            "E15-LINE-010\n".repeat(90),
            "E15-LINE-150\n".repeat(90)
        );

        assert!(session_bound_should_send_new_chunks_for_placeholder(&body));
        assert!(!session_bound_should_send_new_chunks_for_placeholder(
            "[E2E:E15:BEGIN]\nE15-LINE-150\n[E2E:E15:END]"
        ));
    }

    #[test]
    fn terminal_delivery_route_preserves_active_inflight_skip_and_rebind_route() {
        let tmux = "AgentDesk-claude-relay-test";
        let bridge_owned = inflight_for(tmux, RelayOwnerKind::None, false);
        assert_eq!(
            session_bound_terminal_delivery_route(Some(&bridge_owned), tmux),
            None
        );

        let watcher_owned = inflight_for(tmux, RelayOwnerKind::Watcher, false);
        assert_eq!(
            session_bound_terminal_delivery_route(Some(&watcher_owned), tmux),
            None
        );

        let mut watcher_external = inflight_for(tmux, RelayOwnerKind::Watcher, false);
        watcher_external.user_msg_id = 0;
        watcher_external.current_msg_id = 0;
        assert_eq!(
            session_bound_terminal_delivery_route(Some(&watcher_external), tmux),
            None
        );

        let rebind_origin = inflight_for(tmux, RelayOwnerKind::Watcher, true);
        assert_eq!(
            session_bound_terminal_delivery_route(Some(&rebind_origin), tmux),
            Some(SessionBoundTerminalDeliveryRoute::NewMessage)
        );

        let mut external_session_bound =
            inflight_for(tmux, RelayOwnerKind::SessionBoundRelay, false);
        external_session_bound.turn_source = TurnSource::ExternalInput;
        external_session_bound.current_msg_id = 9002;
        assert_eq!(
            session_bound_terminal_delivery_route(Some(&external_session_bound), tmux),
            Some(SessionBoundTerminalDeliveryRoute::NewMessage),
            "TUI-direct external turns keep the prompt notification as an anchor; the sink posts a response message instead of editing it"
        );

        assert_eq!(
            session_bound_terminal_delivery_route(Some(&watcher_owned), "AgentDesk-claude-other"),
            None
        );
    }

    #[test]
    fn discord_and_tui_direct_have_explicit_terminal_owner_models() {
        let tmux = "AgentDesk-claude-relay-test";
        let discord_bridge_owned = inflight_for(tmux, RelayOwnerKind::None, false);
        assert_eq!(
            session_bound_terminal_delivery_route(Some(&discord_bridge_owned), tmux),
            None,
            "Discord-originated bridge-owned turns must stay out of the session-bound sink"
        );

        let mut tui_direct = inflight_for(tmux, RelayOwnerKind::SessionBoundRelay, false);
        tui_direct.turn_source = TurnSource::ExternalInput;
        assert_eq!(
            session_bound_terminal_delivery_route(Some(&tui_direct), tmux),
            Some(SessionBoundTerminalDeliveryRoute::NewMessage),
            "TUI-direct turns that select the session-bound owner converge on the same sink route without Discord intake resubmission"
        );
    }

    #[test]
    fn session_relay_trace_context_uses_external_input_lease_without_inflight() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        let tmux = "AgentDesk-codex-external-trace";
        crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            ProviderKind::Codex.as_str(),
            tmux,
            crate::services::tui_prompt_dedupe::ExternalInputRelayLease {
                channel_id: Some(4242),
                turn_id: Some("external:codex:4242:trace:1".to_string()),
                session_key: Some("host:AgentDesk-codex-external-trace".to_string()),
                relay_owner:
                    crate::services::tui_prompt_dedupe::ExternalInputRelayOwner::SessionBoundRelay,
                runtime_kind: Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui),
                generation:
                    crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
            },
        );

        let trace = session_relay_trace_context(&ProviderKind::Codex, 4242, tmux, None);

        assert_eq!(trace.turn_id(), Some("external:codex:4242:trace:1"));
        assert_eq!(
            trace.session_key(),
            Some("host:AgentDesk-codex-external-trace")
        );
        assert_eq!(trace.dispatch_id(), None);
        assert_eq!(trace.relay_owner(), "session_bound_relay");
        assert_eq!(trace.runtime_kind(), "codex_tui");
        assert!(
            crate::services::tui_prompt_dedupe::clear_external_input_relay_lease(
                ProviderKind::Codex.as_str(),
                tmux,
                4242,
            )
        );
    }

    #[test]
    fn terminal_delivery_route_skips_bridge_owned_external_lease_without_inflight() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        let tmux = "AgentDesk-codex-bridge-owned-direct";
        let lease = crate::services::tui_prompt_dedupe::ExternalInputRelayLease {
            channel_id: Some(4243),
            turn_id: Some("external:codex:4243:trace:1".to_string()),
            session_key: Some("host:AgentDesk-codex-bridge-owned-direct".to_string()),
            relay_owner: crate::services::tui_prompt_dedupe::ExternalInputRelayOwner::BridgeAdapter,
            runtime_kind: Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui),
            generation:
                crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        };
        // Capture the RECORDED lease (with its stamped generation) so the later
        // `_if_matches` clear compares against the exact stored identity.
        let recorded = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            ProviderKind::Codex.as_str(),
            tmux,
            lease.clone(),
        );

        assert_eq!(
            session_bound_terminal_delivery_route_decision(None, tmux, &ProviderKind::Codex, 4243,),
            SessionBoundTerminalDeliveryRouteDecision::Skipped
        );
        assert!(
            crate::services::tui_prompt_dedupe::clear_external_input_relay_lease_if_matches(
                ProviderKind::Codex.as_str(),
                tmux,
                4243,
                &recorded,
            )
        );
        assert_eq!(
            session_bound_terminal_delivery_route_decision(None, tmux, &ProviderKind::Codex, 4243,),
            SessionBoundTerminalDeliveryRouteDecision::Route(
                SessionBoundTerminalDeliveryRoute::NewMessage
            ),
            "after a bridge terminal path clears its lease, later normal session-bound output in the same pane must not be blocked"
        );
    }

    #[test]
    fn terminal_delivery_route_skip_is_not_sink_error_for_ack_fallback() {
        let tmux = "AgentDesk-claude-relay-test";
        let bridge_owned = inflight_for(tmux, RelayOwnerKind::None, false);
        let result = session_bound_terminal_delivery_route_or_skip(
            Some(&bridge_owned),
            tmux,
            &ProviderKind::Claude,
            42,
        );
        assert!(result.is_err());

        let watcher_owned = inflight_for(tmux, RelayOwnerKind::Watcher, false);
        let result = session_bound_terminal_delivery_route_or_skip(
            Some(&watcher_owned),
            "AgentDesk-claude-other",
            &ProviderKind::Claude,
            42,
        );
        assert!(result.is_err());
    }

    // #3041 P1-5: a route-layer `Skipped` decision (the route enum legitimately
    // keeps "Skipped" — a bridge-owned / mismatched inflight has no place to
    // route) is the SOLE producer of the sink-local `NotDelivered` outcome
    // (`deliver_response` returns `SessionRelayDeliveryOutcome::NotDelivered` at
    // the route-decline arm), which `deliver` surfaces as
    // `RelaySinkOutcome::TerminalNotDelivered`. The watcher then routes
    // `NotDelivered` through committed-offset reconciliation (§3.2) — never a
    // blind skip. This pins the route-`Skipped` → `NotDelivered` mapping origin.
    #[test]
    fn terminal_delivery_route_skip_maps_to_not_delivered_outcome() {
        let tmux = "AgentDesk-claude-relay-test";
        let bridge_owned = inflight_for(tmux, RelayOwnerKind::None, false);
        assert_eq!(
            session_bound_terminal_delivery_route_decision(
                Some(&bridge_owned),
                tmux,
                &ProviderKind::Claude,
                42,
            ),
            SessionBoundTerminalDeliveryRouteDecision::Skipped,
            "a bridge-owned / mismatched inflight declines → route-layer Skipped, \
             which deliver_response maps to the sink-local NotDelivered outcome \
             (surfaced as RelaySinkOutcome::TerminalNotDelivered)"
        );
    }

    /// #3017 / E-13 dedup invariant: the single output-offset authority makes
    /// the idle-JSONL relay (and the watcher) relay a given inflight-less wake
    /// byte-range EXACTLY ONCE.
    ///
    /// The tmux watcher is the PRIMARY relay and the SOLE committer of the
    /// authoritative offset (via `advance_watcher_confirmed_end`, simulated here
    /// by writing `confirmed_end_offset`). The idle relay and the watcher's
    /// own no-inflight relay gate are read-only CONSUMERS: each skips a range
    /// the committed offset already covers (`committed_relay_offset(channel) >=
    /// end`). This test exercises that exact decision:
    ///   1. Before any relay, the committed offset is 0 → a wake range
    ///      `[0, end)` is NOT yet covered → the actor relays it.
    ///   2. After the watcher commits at `end`, a second actor (idle relay or a
    ///      re-observing watcher pass) sees the range covered → it SKIPS, so the
    ///      wake body is never relayed twice (the
    ///      `duplicate Discord relay body: '[E2E:E13:WAKE]'` failure).
    ///   3. A genuinely-NEW wake output at a higher offset is past the committed
    ///      offset → NOT covered → relayed exactly once (dedup, not blanket
    ///      suppression - E-13 also asserts the wake text is present).
    #[test]
    fn offset_authority_relays_wake_range_exactly_once() {
        use std::sync::atomic::Ordering;

        // The dedup decision both consumers make: skip iff the committed offset
        // already covers this range end. Mirrors the read-only check in
        // `run_idle_jsonl_relay_loop` and the watcher's no-inflight gate.
        fn already_relayed(committed_end: u64, range_end: u64) -> bool {
            committed_end >= range_end
        }

        let shared = super::super::make_shared_data_for_tests();
        let channel = ChannelId::new(7_013);
        let coord = shared.tmux_relay_coord(channel);

        // (1) Fresh wake turn ending at offset 128: nothing committed yet → the
        // first actor relays it (not a duplicate).
        assert_eq!(shared.committed_relay_offset(channel), 0);
        assert!(
            !already_relayed(shared.committed_relay_offset(channel), 128),
            "a wake range past the committed offset must be relayed"
        );

        // The watcher (primary) commits the relay at 128, exactly as
        // `advance_watcher_confirmed_end` does after a confirmed delivery.
        coord.confirmed_end_offset.store(128, Ordering::Release);
        assert_eq!(shared.committed_relay_offset(channel), 128);

        // (2) The OTHER actor (idle relay, or a re-observing watcher) now sees
        // the same range covered → it SKIPS → the body is not relayed twice.
        assert!(
            already_relayed(shared.committed_relay_offset(channel), 128),
            "the second actor must skip a range the watcher already committed (E-13 dedup)"
        );
        // A sub-range / re-observation of the already-relayed range also skips.
        assert!(already_relayed(shared.committed_relay_offset(channel), 64));

        // (3) A genuinely-new wake output ending at 256 is PAST the committed
        // offset → not covered → relayed exactly once.
        assert!(
            !already_relayed(shared.committed_relay_offset(channel), 256),
            "a new wake output past the committed offset must be relayed (not suppressed)"
        );
        coord.confirmed_end_offset.store(256, Ordering::Release);
        assert!(
            already_relayed(shared.committed_relay_offset(channel), 256),
            "after it is committed the new range must not be relayed twice"
        );

        // The authority is per-channel: a different channel starts uncommitted,
        // so its own first wake range is relayed independently.
        let other = ChannelId::new(7_014);
        assert_eq!(shared.committed_relay_offset(other), 0);
        assert!(!already_relayed(shared.committed_relay_offset(other), 128));
    }

    /// #3017 / E-13 partial-overlap dedup (codex r5 P2): the idle relay must not
    /// re-send a PREFIX the watcher already delivered, and the watcher must
    /// compare against this turn's CONSUMED end (not the whole read batch end).
    #[test]
    fn offset_authority_handles_partial_and_batch_overlaps() {
        // Idle-relay decision for a read range `[start, end)` against the
        // committed watermark. Mirrors `run_idle_jsonl_relay_loop`:
        //   - committed >= end  → whole range already delivered → SKIP all.
        //   - committed > start → PREFIX delivered → trim: re-read from committed.
        //   - else              → nothing delivered → forward `[start, end)`.
        #[derive(Debug, PartialEq, Eq)]
        enum IdleDecision {
            SkipAll,
            TrimTo(u64),
            Forward,
        }
        fn idle_decision(committed: u64, start: u64, end: u64) -> IdleDecision {
            if committed >= end {
                IdleDecision::SkipAll
            } else if committed > start {
                IdleDecision::TrimTo(committed)
            } else {
                IdleDecision::Forward
            }
        }

        // Fully covered → skip.
        assert_eq!(idle_decision(200, 0, 128), IdleDecision::SkipAll);
        assert_eq!(idle_decision(128, 0, 128), IdleDecision::SkipAll);
        // Partial overlap: watcher delivered [0,128); file grew to 256 before the
        // idle poll → re-read only [128,256), never re-sending the wake prefix.
        assert_eq!(idle_decision(128, 0, 256), IdleDecision::TrimTo(128));
        // No overlap → forward the whole fresh range.
        assert_eq!(idle_decision(0, 128, 256), IdleDecision::Forward);
        assert_eq!(idle_decision(64, 128, 256), IdleDecision::Forward);

        // Watcher decision: dedup against the TURN's CONSUMED end, not the whole
        // read batch end. A batch read `[start=0, current_offset=256)` whose
        // result line ends at 128 (the trailing 128 bytes are a later turn's
        // buffered JSONL) consumes to 128. A prior commit at 128 (this same
        // terminal, already relayed) must suppress — comparing against 256 would
        // miss it and re-relay. `terminal_event_consumed_offset` = current_offset
        // - unprocessed_tail.len().
        let current_offset = 256u64;
        let unprocessed_tail_len = 128u64; // a later turn's buffered bytes
        let turn_consumed = current_offset - unprocessed_tail_len; // 128
        let committed_for_this_terminal = 128u64;
        assert!(
            committed_for_this_terminal >= turn_consumed,
            "a prior commit at the consumed terminal end must suppress the re-relay"
        );
        assert!(
            committed_for_this_terminal < current_offset,
            "comparing against the whole batch end (256) would WRONGLY miss the duplicate"
        );
    }

    /// #3017 / E-13 codex r6 P1 (black-hole regression): when the `system/init`
    /// event lives in the already-committed PREFIX and only the suffix is
    /// uncommitted, the idle relay must deliver the suffix EXACTLY ONCE in the
    /// SAME pass — it must NOT bounce to a next tick that re-classifies the
    /// init-less suffix as a "non-init active-session payload" and DROPS it.
    ///
    /// Unlike `offset_authority_handles_partial_and_batch_overlaps` (which only
    /// exercised a re-implemented `idle_decision` enum), this drives the REAL
    /// loop decision function `idle_relay_range_action` — the exact code the
    /// loop body runs — so it asserts the actual classification→dedup ORDERING.
    #[test]
    fn idle_relay_delivers_suffix_when_init_is_in_committed_prefix() {
        // A wake turn whose JSONL is: an `init` line (the prefix the watcher
        // already relayed) followed by the assistant body (the uncommitted
        // suffix the user still needs to see).
        let init_line = "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\"}\n";
        let body_line = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"the trailing answer\"}]}}\n";
        let init_bytes = init_line.len() as u64;
        let full = format!("{init_line}{body_line}");
        let full_bytes = full.as_bytes();
        let start = 0u64;
        let end = full_bytes.len() as u64;
        // The watcher already committed the init-line prefix; the file grew to
        // include the body before this idle poll → PARTIAL overlap.
        let committed = init_bytes;
        assert!(
            start < committed && committed < end,
            "test sets up a partial overlap"
        );

        // REAL loop decision on the WHOLE classified payload: relay only the
        // uncommitted suffix `[committed, end)` — NOT a re-classify-and-drop.
        let action =
            idle_relay_range_action(full_bytes, start, end, committed, false, false, false);
        assert_eq!(
            action,
            super::IdleRelayRangeAction::SendSuffixFrom(committed),
            "the loop must deliver the uncommitted suffix in the same pass (no black-hole)"
        );

        // The suffix that would be delivered is exactly the body line — the
        // already-relayed init prefix is skipped (no duplicate), the trailing
        // answer is sent (no black-hole).
        let suffix = &full_bytes[committed as usize..end as usize];
        assert_eq!(
            suffix,
            body_line.as_bytes(),
            "suffix is the uncommitted body, prefix skipped"
        );

        // REGRESSION WITNESS: the OLD code bounced (`*offset = committed;
        // continue;`) and re-read JUST this suffix on the next tick. Running the
        // classification on the init-less suffix proves that path BLACK-HOLES
        // it: classified as non-init → dropped. The fix avoids the bounce so the
        // suffix is never re-gated this way.
        let suffix_only_action =
            idle_relay_range_action(suffix, 0, suffix.len() as u64, 0, false, false, false);
        assert_eq!(
            suffix_only_action,
            super::IdleRelayRangeAction::SkipClassified,
            "re-gating the init-less suffix as a fresh payload WOULD black-hole it (the old bug)"
        );
        assert_eq!(
            idle_relay_range_action(suffix, 0, suffix.len() as u64, 0, false, true, false,),
            super::IdleRelayRangeAction::SendFull,
            "a known continued Codex TUI runtime binding may relay assistant-only suffixes without init"
        );

        // Whole range uncommitted → relay the full payload (control case).
        assert_eq!(
            idle_relay_range_action(full_bytes, start, end, 0, false, false, false),
            super::IdleRelayRangeAction::SendFull
        );
        // Whole range already committed → skip (control case).
        assert_eq!(
            idle_relay_range_action(full_bytes, start, end, end, false, false, false),
            super::IdleRelayRangeAction::SkipAlreadyRelayed
        );
        // New-session grace still wins over everything (ordering preserved).
        assert_eq!(
            idle_relay_range_action(full_bytes, start, end, committed, true, true, false),
            super::IdleRelayRangeAction::SkipClassified
        );
    }

    #[test]
    fn idle_relay_allows_later_large_backlog_chunk_after_session_init_seen() {
        let session_name = "AgentDesk-claude-large-backlog";
        let init_chunk = b"{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"large\"}\n";
        let continued_chunk = b"{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"continued answer\"}]}}\n";
        let start = IDLE_JSONL_RELAY_MAX_BYTES_PER_TICK;
        let end = start + continued_chunk.len() as u64;
        let mut offset = 0;
        let mut session_init_seen = HashSet::new();

        assert_eq!(
            idle_relay_range_action(continued_chunk, start, end, 0, false, false, false),
            super::IdleRelayRangeAction::SkipClassified,
            "without session-level init state, a later init-less chunk would be dropped"
        );
        assert!(idle_jsonl_session_has_init(
            &mut session_init_seen,
            session_name,
            init_chunk
        ));
        idle_jsonl_consume_offset(
            &mut session_init_seen,
            session_name,
            &mut offset,
            start,
            IdleJsonlSessionInitRearm::Keep,
        );
        assert!(
            session_init_seen.contains(session_name),
            "mid-backlog consumption must keep the init marker for later chunks in the same drain"
        );
        assert_eq!(
            idle_relay_range_action(
                continued_chunk,
                start,
                end,
                0,
                false,
                false,
                session_init_seen.contains(session_name)
            ),
            super::IdleRelayRangeAction::SendFull,
            "once the session has accepted an init chunk, later chunks are not re-gated by init"
        );
        idle_jsonl_consume_offset(
            &mut session_init_seen,
            session_name,
            &mut offset,
            end,
            IdleJsonlSessionInitRearm::Keep,
        );
        assert!(
            session_init_seen.contains(session_name),
            "EOF catch-up must keep the init marker for later chunks in the same growing file"
        );
    }

    #[test]
    fn idle_jsonl_generation_change_clears_session_init_seen() {
        let session_name = "AgentDesk-claude-generation-rearm";
        let mut session_init_seen = HashSet::from([session_name.to_string()]);

        idle_jsonl::idle_jsonl_clear_session_init_on_generation_reset(
            &mut session_init_seen,
            session_name,
            false,
        );
        assert!(
            session_init_seen.contains(session_name),
            "unchanged generations keep the init marker"
        );

        idle_jsonl::idle_jsonl_clear_session_init_on_generation_reset(
            &mut session_init_seen,
            session_name,
            true,
        );
        assert!(
            !session_init_seen.contains(session_name),
            "a generation-reset watermark event re-arms init detection"
        );
        let continued_chunk = b"{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"continued answer\"}]}}\n";
        assert_eq!(
            idle_relay_range_action(
                continued_chunk,
                0,
                continued_chunk.len() as u64,
                0,
                false,
                false,
                session_init_seen.contains(session_name)
            ),
            super::IdleRelayRangeAction::SkipClassified,
            "after generation reset, assistant-only bytes cannot inherit the prior wrapper's init marker"
        );
    }

    #[test]
    fn idle_jsonl_generation_signature_pre_commit_respawn_watermark_zero_clears_session_init_seen()
    {
        let session_name = "AgentDesk-claude-generation-signature-precommit";
        let pre_commit_watermark = 0_u64;
        let mut session_init_seen = HashSet::from([session_name.to_string()]);
        let mut session_generation_signatures = HashMap::from([(session_name.to_string(), 0_i64)]);

        assert_eq!(
            pre_commit_watermark, 0,
            "test models a respawn before any watcher commit"
        );
        assert!(
            idle_jsonl::idle_jsonl_clear_session_init_on_generation_signature_change(
                &mut session_init_seen,
                &mut session_generation_signatures,
                session_name,
                42,
            )
        );
        assert_eq!(session_generation_signatures.get(session_name), Some(&42));
        assert!(
            !session_init_seen.contains(session_name),
            "signature change must re-arm init detection even when watermark reset CAS would be false"
        );
    }

    #[test]
    fn idle_jsonl_unchanged_generation_signature_keeps_session_init_seen() {
        let session_name = "AgentDesk-claude-generation-signature-unchanged";
        let mut session_init_seen = HashSet::from([session_name.to_string()]);
        let mut session_generation_signatures = HashMap::from([(session_name.to_string(), 42_i64)]);

        assert!(
            !idle_jsonl::idle_jsonl_clear_session_init_on_generation_signature_change(
                &mut session_init_seen,
                &mut session_generation_signatures,
                session_name,
                42,
            )
        );
        assert_eq!(session_generation_signatures.get(session_name), Some(&42));
        assert!(
            session_init_seen.contains(session_name),
            "unchanged generation signatures keep the init marker across ticks"
        );
    }

    #[test]
    fn idle_jsonl_user_event_consumption_clears_session_init_seen() {
        let session_name = "AgentDesk-claude-user-event-rearm";
        let mut session_init_seen = HashSet::from([session_name.to_string()]);
        let mut offset = 128;
        let consumed_to = 256;

        idle_jsonl_consume_offset(
            &mut session_init_seen,
            session_name,
            &mut offset,
            consumed_to,
            IdleJsonlSessionInitRearm::Clear,
        );

        assert_eq!(offset, consumed_to);
        assert!(
            !session_init_seen.contains(session_name),
            "user-event-gated consumption starts a new active turn and clears stale init state"
        );
    }

    #[test]
    fn idle_jsonl_cross_tick_init_then_assistant_append_relays_and_keeps_session_init_seen() {
        let session_name = "AgentDesk-claude-cross-tick-continuation";
        let init_chunk = b"{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"split\"}\n";
        let assistant_chunk = b"{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"continued answer\"}]}}\n";
        let mut session_init_seen = HashSet::new();
        let mut offset = 0;
        let tick_a_end = init_chunk.len() as u64;

        let tick_a_session_has_init =
            idle_jsonl_session_has_init(&mut session_init_seen, session_name, init_chunk);
        assert!(tick_a_session_has_init);
        assert_eq!(
            idle_relay_range_action(
                init_chunk,
                0,
                tick_a_end,
                0,
                false,
                false,
                tick_a_session_has_init,
            ),
            super::IdleRelayRangeAction::SendFull,
            "tick A relays the init-only payload"
        );
        idle_jsonl_consume_offset(
            &mut session_init_seen,
            session_name,
            &mut offset,
            tick_a_end,
            IdleJsonlSessionInitRearm::Keep,
        );
        assert_eq!(offset, tick_a_end);
        assert!(
            session_init_seen.contains(session_name),
            "tick A reaching EOF must not clear the init marker for the growing file"
        );

        let tick_b_start = offset;
        let tick_b_end = tick_b_start + assistant_chunk.len() as u64;
        assert!(!idle_jsonl_payload_contains_init_event(assistant_chunk));
        assert!(!idle_jsonl_payload_contains_user_event(assistant_chunk));
        let tick_b_session_has_init =
            idle_jsonl_session_has_init(&mut session_init_seen, session_name, assistant_chunk);
        assert!(tick_b_session_has_init);
        assert_eq!(
            idle_relay_range_action(
                assistant_chunk,
                tick_b_start,
                tick_b_end,
                0,
                false,
                false,
                tick_b_session_has_init,
            ),
            super::IdleRelayRangeAction::SendFull,
            "tick B relays the assistant-only continuation without a fresh init/user event/inflight"
        );
        idle_jsonl_consume_offset(
            &mut session_init_seen,
            session_name,
            &mut offset,
            tick_b_end,
            IdleJsonlSessionInitRearm::Keep,
        );
        assert_eq!(offset, tick_b_end);
        assert!(
            session_init_seen.contains(session_name),
            "tick B must leave the init marker intact for later continuations"
        );
    }

    fn matched_with_session(channel_id: &str, session_name: &str) -> MatchedChannel {
        let mut matched = matched(channel_id);
        matched.expected_session_name = session_name.to_string();
        matched
    }

    /// #4116: when channel C has an inflight turn owned by tmux session X, the
    /// idle JSONL relay must not consume new bytes observed while iterating a
    /// different session Y for the same channel/provider. Leaving the offset
    /// unchanged lets the next no-inflight tick relay Y's wake output instead of
    /// permanently losing it.
    #[test]
    fn idle_jsonl_relay_resumes_after_mismatched_inflight_clears() {
        let owner_session = "AgentDesk-claude-channel-c-main-x";
        let background_session = "AgentDesk-claude-channel-c-bg-y";
        let background = matched_with_session("42", background_session);
        let inflight = inflight_for(owner_session, RelayOwnerKind::Watcher, false);
        let mut last_inflight_seen_at = HashMap::new();
        let mut offset = 128u64;
        let wake_payload = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"wake-y\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"wake answer from Y\"}]}}\n"
        );
        let end = offset + wake_payload.len() as u64;

        let decision = idle_jsonl::idle_jsonl_apply_active_inflight_gate(
            &mut last_inflight_seen_at,
            &background,
            42,
            &inflight,
            end,
            &mut offset,
        );
        assert_eq!(
            decision,
            idle_jsonl::IdleJsonlInflightGateDecision::SuppressWithoutConsuming,
            "cross-session inflight rows suppress this tick through the production gate"
        );
        assert_eq!(
            offset, 128,
            "cross-session inflight skip must leave Y's offset untouched"
        );

        assert_eq!(
            idle_relay_range_action(wake_payload.as_bytes(), offset, end, 0, false, false, false,),
            super::IdleRelayRangeAction::SendFull,
            "the preserved Y bytes relay on the next no-inflight tick"
        );
        offset = end;
        assert_eq!(
            offset, end,
            "Y's offset advances only after the inflight clears and relay eligibility is rechecked"
        );
    }

    #[test]
    fn idle_jsonl_should_not_skip_matching_inflight() {
        let matched = matched("42");
        let inflight = inflight_for(
            &matched.expected_session_name,
            RelayOwnerKind::Watcher,
            false,
        );
        let mut last_inflight_seen_at = HashMap::new();

        assert!(!idle_jsonl::idle_jsonl_should_skip_mismatched_inflight(
            &mut last_inflight_seen_at,
            &matched,
            42,
            &inflight,
        ));
    }

    #[test]
    fn idle_jsonl_should_not_skip_none_tmux_inflight_wildcard() {
        let matched = matched("42");
        let mut inflight = inflight_for("AgentDesk-claude-other", RelayOwnerKind::Watcher, false);
        inflight.tmux_session_name = None;
        let mut last_inflight_seen_at = HashMap::new();
        last_inflight_seen_at.insert(matched.expected_session_name.clone(), Instant::now());
        let mut offset = 128;
        let wake_payload = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"wake-none\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"wake answer\"}]}}\n"
        );
        let end = offset + wake_payload.len() as u64;

        assert!(idle_jsonl::idle_jsonl_inflight_mismatches_session(
            &inflight,
            &matched.expected_session_name,
        ));
        let decision = idle_jsonl::idle_jsonl_apply_active_inflight_gate(
            &mut last_inflight_seen_at,
            &matched,
            42,
            &inflight,
            end,
            &mut offset,
        );
        assert_eq!(
            decision,
            idle_jsonl::IdleJsonlInflightGateDecision::SuppressWithoutConsuming,
            "None-tmux inflight rows suppress the tick instead of taking the healthy consume branch"
        );
        assert_eq!(
            offset, 128,
            "None-tmux inflight suppression must not consume unrelated JSONL backlog"
        );
        assert!(
            last_inflight_seen_at.contains_key(&matched.expected_session_name),
            "suppression preserves any existing post-inflight grace marker"
        );
    }

    #[test]
    fn idle_jsonl_mismatched_skip_preserves_recent_inflight_grace_marker() {
        let owner_session = "AgentDesk-claude-channel-c-main-x";
        let background_session = "AgentDesk-claude-channel-c-bg-y";
        let background = matched_with_session("42", background_session);
        let inflight = inflight_for(owner_session, RelayOwnerKind::Watcher, false);
        let mut last_inflight_seen_at = HashMap::new();
        last_inflight_seen_at.insert(background_session.to_string(), Instant::now());

        assert!(idle_jsonl::idle_jsonl_should_skip_mismatched_inflight(
            &mut last_inflight_seen_at,
            &background,
            42,
            &inflight,
        ));
        assert!(
            last_inflight_seen_at.contains_key(background_session),
            "mismatched skip must not erase the session's existing post-inflight grace marker"
        );
    }

    #[tokio::test]
    async fn session_sink_frame_consumed_without_terminal_delivery_returns_frame_accepted() {
        let binding = matched("44");
        let sink = SessionBoundDiscordRelaySink::new(Arc::new(HealthRegistry::new()));
        let payload = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"partial only\"}]}}\n";

        let outcome = sink
            .deliver(&frame(&binding, payload, 1))
            .await
            .expect("frame without terminal delivery should be accepted");

        assert_eq!(outcome, RelaySinkOutcome::FrameAccepted);
    }

    // #3041 P1-3 R5 (codex P1-3 — fence gates the ADVANCE, not the ACK): a
    // FENCE-LESS frame that carries a COMPLETE result (e.g. turn B's result riding
    // turn A's trailing tail) produces a delivery whose `terminal_consumed_end` is
    // None — so the identity-gated OFFSET ADVANCE can NEVER fire for it (the advance
    // requires `Some(end > 0)`). After the R4 revert, `deliver` DOES surface this
    // frame's terminal outcome (the no-inflight terminal needs its ACK to resolve),
    // but that outcome is recorded keyed by THIS frame's own sequence, so it can
    // never satisfy ANOTHER turn's terminal-ACK (which queries its own sequence).
    // Driving a real terminal COMMIT needs Discord HTTP, so we assert the
    // load-bearing offset-advance invariant directly: a fence-less frame's delivery
    // carries no consumed_end → it can never advance the offset authority.
    #[test]
    fn fenceless_frame_with_complete_result_carries_no_commit_fence() {
        let binding = matched("44");
        let mut parser = SessionRelayParser::default();
        // A COMPLETE turn result on a fence-less frame (terminal_consumed_end=None).
        let payload = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"turn B done\"}]}}\n",
            "{\"type\":\"result\",\"result\":\"turn B done\"}\n"
        );
        let fenceless = frame(&binding, payload, 7);
        assert!(
            fenceless.terminal_consumed_end.is_none(),
            "precondition: this is a fence-less frame — its result is still delivered and its ACK still resolves, but it can never ADVANCE the offset"
        );

        let deliveries = parser.ingest_frame(&fenceless);
        assert_eq!(
            deliveries.len(),
            1,
            "the complete result on the fence-less frame is still delivered (B is mirrored, not black-holed)"
        );
        assert_eq!(
            deliveries[0].terminal_consumed_end, None,
            "a fence-less frame's delivery carries NO consumed_end → it can never advance the offset authority (no wrong-turn advance)"
        );
    }

    #[allow(dead_code)] // #3034: test helper superseded by `delivery_with_fence_offset`.
    fn delivery_with_fence(
        session_name: &str,
        consumed_end: Option<u64>,
        turn_user_msg_id: u64,
        turn_started_at: &str,
    ) -> SessionRelayDelivery {
        delivery_with_fence_offset(
            session_name,
            consumed_end,
            turn_user_msg_id,
            turn_started_at,
            None,
        )
    }

    fn delivery_with_fence_offset(
        session_name: &str,
        consumed_end: Option<u64>,
        turn_user_msg_id: u64,
        turn_started_at: &str,
        turn_start_offset: Option<u64>,
    ) -> SessionRelayDelivery {
        SessionRelayDelivery {
            provider: ProviderKind::Claude,
            channel_id: 8_041,
            session_name: session_name.to_string(),
            response_text: "answer".to_string(),
            task_notification_kind: None,
            task_notification_context: None,
            terminal_consumed_end: consumed_end,
            frame_turn_user_msg_id: turn_user_msg_id,
            frame_turn_started_at: turn_started_at.to_string(),
            frame_turn_start_offset: turn_start_offset,
        }
    }

    #[test]
    fn same_id0_turn_frame_and_inflight_derive_equal_delivery_lease_key() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        struct EnvReset(Option<std::ffi::OsString>);
        impl Drop for EnvReset {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                    None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
                }
            }
        }
        let _env_reset = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let temp = tempfile::TempDir::new().expect("temp runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        let channel = ChannelId::new(8_041);
        let generation = 17;
        let started_at = "2026-07-03T06:00:00Z";
        let start_offset = 128;
        let delivery = delivery_with_fence_offset(
            "AgentDesk-claude-lease-key",
            Some(256),
            0,
            started_at,
            Some(start_offset),
        );
        let inflight = inflight_with_identity_offset(
            channel.get(),
            "AgentDesk-claude-lease-key",
            0,
            started_at,
            Some(start_offset),
        );

        let frame_key = delivery_lease_key_for_frame(channel, generation, &delivery);
        let inflight_key =
            super::super::DeliveryLeaseKey::from_inflight_state(channel, generation, &inflight);

        assert_eq!(
            frame_key, inflight_key,
            "sink frame fence and bridge/watcher inflight state must derive the same lease identity for the same id-0 turn"
        );
    }

    // #3089 A2b: minimal `TurnGateway` fake for the short-replace controller-path
    // characterization. Only `replace_message_with_outcome` is exercised (the
    // `Replace { Active }` transport); `post_send_finalize` is a no-op for the
    // non-terminal `Active` lifecycle, so no edit/delete fires. Every other method
    // `panic!`s — reaching one would be a behaviour drift the test must catch.
    struct ShortReplaceFakeGateway {
        outcome: super::super::formatting::ReplaceLongMessageOutcome,
        ok: bool,
        replace_calls: std::sync::atomic::AtomicUsize,
    }

    impl super::super::gateway::TurnGateway for ShortReplaceFakeGateway {
        fn replace_message_with_outcome<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
            _content: &'a str,
        ) -> super::super::gateway::GatewayFuture<
            'a,
            Result<super::super::formatting::ReplaceLongMessageOutcome, String>,
        > {
            Box::pin(async move {
                self.replace_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if self.ok {
                    Ok(self.outcome.clone())
                } else {
                    Err("fake transport failure".to_string())
                }
            })
        }
        fn send_message<'a>(
            &'a self,
            _c: ChannelId,
            _x: &'a str,
        ) -> super::super::gateway::GatewayFuture<'a, Result<MessageId, String>> {
            panic!("short-replace path never sends a new message")
        }
        fn edit_message<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
            _x: &'a str,
        ) -> super::super::gateway::GatewayFuture<'a, Result<(), String>> {
            panic!("Active lifecycle → post_send_finalize no-op → no edit")
        }

        fn schedule_retry_with_history<'a>(
            &'a self,
            _c: ChannelId,
            _u: MessageId,
            _t: &'a str,
        ) -> super::super::gateway::GatewayFuture<'a, ()> {
            panic!("unused TurnGateway method on the short-replace path")
        }
        fn dispatch_queued_turn<'a>(
            &'a self,
            _c: ChannelId,
            _i: &'a super::super::Intervention,
            _o: &'a str,
            _h: bool,
        ) -> super::super::gateway::GatewayFuture<'a, Result<(), String>> {
            panic!("unused TurnGateway method on the short-replace path")
        }
        fn validate_live_routing<'a>(
            &'a self,
            _c: ChannelId,
        ) -> super::super::gateway::GatewayFuture<'a, Result<(), String>> {
            panic!("unused TurnGateway method on the short-replace path")
        }
        fn requester_mention(&self) -> Option<String> {
            None
        }
        fn can_chain_locally(&self) -> bool {
            false
        }
        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            None
        }
    }

    struct NoopHeartbeatGuard;
    impl toc::PostHeartbeatGuard for NoopHeartbeatGuard {}
    struct NoopHeartbeat;
    impl toc::PostHeartbeat for NoopHeartbeat {
        fn start(
            &self,
            _h: super::super::LeaseHolder,
            _k: super::super::DeliveryLeaseKey,
        ) -> Box<dyn toc::PostHeartbeatGuard> {
            Box::new(NoopHeartbeatGuard)
        }
    }

    // #3089 A2b: drive the SAME ctx `deliver_short_replace_via_controller` builds
    // (holder=Sink, ProceedMarkerless, Replace{Active}, PreserveAlways,
    // CommitOnFallback, identity-gated `advance`) on a FRESH per-channel cell, and
    // assert (1) the cell is acquired EXACTLY ONCE — the no-double-acquire invariant
    // (the sink never also runs `SinkDeliveryLeaseGuard::acquire` on this branch) —
    // and (2) the advance bool steers commit Delivered (advance true) vs NotDelivered
    // (advance false), matching the legacy `advance_after_confirmed_post` arm.
    async fn run_short_replace_controller(
        advance_returns: bool,
        outcome: super::super::formatting::ReplaceLongMessageOutcome,
        ok: bool,
    ) -> (toc::DeliveryOutcome, super::super::LeaseSnapshot, usize) {
        use std::sync::atomic::Ordering as O;
        let ch = ChannelId::new(8_041);
        let cell = Arc::new(super::super::DeliveryLeaseCell::new(ch));
        let turn = super::super::turn_finalizer::TurnKey::new(ch, 7, 0);
        let controller = super::super::placeholder_controller::PlaceholderController::default();
        let gateway = ShortReplaceFakeGateway {
            outcome,
            ok,
            replace_calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let hb = NoopHeartbeat;
        let advance = |_r: (u64, u64)| -> bool { advance_returns };
        let result = toc::deliver_turn_output(
            &gateway,
            toc::TurnOutputCtx {
                turn,
                lease_key: Some(super::super::DeliveryLeaseKey::from_turn_key(turn)),
                owner: RelayOwnerKind::SessionBoundRelay,
                holder: super::super::LeaseHolder::Sink,
                lease: &*cell,
                channel_id: ch,
                placeholder_controller: &controller,
                placeholder: toc::PlaceholderSlot::Active {
                    message_id: MessageId::new(99),
                    key: super::super::placeholder_controller::PlaceholderKey {
                        provider: ProviderKind::Claude,
                        channel_id: ch,
                        message_id: MessageId::new(99),
                    },
                },
                body: "answer",
                send_range: (10, 42),
                plan: toc::OutputPlan::Replace {
                    lifecycle: PlaceholderLifecycle::Active,
                },
                edit_fail_policy: toc::EditFailPlaceholderPolicy::PreserveAlways,
                fallback_commit_policy: toc::FallbackCommitPolicy::CommitOnFallback,
                acquire_failure_mode: toc::AcquireFailureMode::ProceedMarkerless,
                advance: Some(&advance),
                heartbeat: Some(&hb),
            },
        )
        .await;
        (result, cell.read(), gateway.replace_calls.load(O::SeqCst))
    }

    #[test]
    fn short_replace_controller_path_commits_once_and_advances_on_confirmed_edit() {
        let (outcome, lease, replace_calls) =
            futures::executor::block_on(run_short_replace_controller(
                true,
                super::super::formatting::ReplaceLongMessageOutcome::EditedOriginal,
                true,
            ));
        // EditedOriginal + advance true → Delivered; the cell saw exactly one acquire
        // (committed then released — `Unleased` after) and one transport call.
        assert!(matches!(
            outcome,
            toc::DeliveryOutcome::Delivered {
                committed_to: 42,
                ..
            }
        ));
        assert_eq!(replace_calls, 1, "exactly one transport POST");
        assert!(
            matches!(lease, super::super::LeaseSnapshot::Unleased),
            "controller committed then released the single lease (no leftover)"
        );
    }

    #[test]
    fn short_replace_controller_path_commits_not_delivered_when_advance_gate_refuses() {
        let (outcome, _lease, _calls) = futures::executor::block_on(run_short_replace_controller(
            false,
            super::super::formatting::ReplaceLongMessageOutcome::EditedOriginal,
            true,
        ));
        // advance gate refused (false) → NotDelivered (offset not advanced); mirrors the
        // legacy `advance_after_confirmed_post` `advanced == false` arm.
        assert!(matches!(
            outcome,
            toc::DeliveryOutcome::NotDelivered { committed_from: 10 }
        ));
    }

    #[test]
    fn short_replace_controller_path_fallback_advances_and_partial_is_unknown() {
        // #2757 fallback (SentFallbackAfterEditFailure) + CommitOnFallback → Delivered.
        let (fb, _l, _c) = futures::executor::block_on(run_short_replace_controller(
            true,
            super::super::formatting::ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                edit_error: "edit failed".to_string(),
                replacement_anchor: None,
            },
            true,
        ));
        assert!(matches!(fb, toc::DeliveryOutcome::Delivered { .. }));
        // PartialContinuationFailure → Unknown (I2: never advances); the sink maps it to
        // `Err(Transient)`.
        let (partial, _l2, _c2) = futures::executor::block_on(run_short_replace_controller(
            true,
            super::super::formatting::ReplaceLongMessageOutcome::PartialContinuationFailure {
                sent_chunks: 1,
                total_chunks: 2,
                failed_chunk_index: 1,
                sent_continuation_message_ids: vec![1],
                cleanup_errors: vec![],
                error: "mid-stream".to_string(),
            },
            true,
        ));
        // #3089 A5: the sink uses CommitOnFallback, so it never reaches the
        // fell_back=true arm; a partial continuation is fell_back=false — the
        // controller extension is byte-identical for this owner.
        assert!(matches!(
            partial,
            toc::DeliveryOutcome::Unknown { fell_back: false }
        ));
    }

    // #3089 A2b (review-fix Medium-1): drive the flag-ON `deliver_response`
    // short-replace branch through the PRODUCTION helper
    // (`deliver_short_replace_via_controller`) with an injectable
    // `TurnGateway` and REAL on-disk inflight, so the test exercises the actual
    // production wiring — the FRESH-reload identity-gated `advance` and the
    // no-double-acquire guard skip — not a local stub. This is the test the
    // earlier `run_short_replace_controller` characterization could not be: it
    // passed a stub `|_| advance_returns` closure, so replacing the production
    // advance with unconditional `true` (or dropping the guard-skip) did not
    // fail it.
    //
    // Mutation guards proven here:
    //  (a) advance = unconditional `true` (instead of the fresh identity gate):
    //      the MISMATCH case below would flip `NotDelivered`/no-advance into
    //      `Delivered`/advance and fail.
    //  (b) guard-skip removed (`&& !cutover_short_replace` dropped from
    //      `sink_guard_lease_range`): `cutover_skips_sink_guard_acquire` fails.
    #[test]
    fn cutover_short_replace_production_path_advance_is_fresh_identity_gated() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        struct EnvReset(Option<std::ffi::OsString>);
        impl Drop for EnvReset {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                    None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
                }
            }
        }
        let _env_reset = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let temp = tempfile::TempDir::new().unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        // The sink's `SinkPostHeartbeat` spawns a `DeliveryLeaseHeartbeat` task,
        // which needs a Tokio reactor. Drive the production helper on a local
        // current-thread runtime (sync `#[test]` keeps the env-lock guard valid).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let session = "AgentDesk-claude-8041";
        let channel = ChannelId::new(8_041);
        let provider = ProviderKind::Claude;
        // A real ordered [start,end) so the cut-over guard-skip applies and the
        // controller acquires the SINGLE lease.
        let (start, end) = (0u64, 256u64);
        let delivery =
            delivery_with_fence_offset(session, Some(end), 0, "2026-06-04T00:00:00Z", Some(0));

        // ---- MATCH: the on-disk inflight identity matches the frame ----------
        // The fresh-reload identity gate inside the production `advance` closure
        // returns true → Delivered AND the committed offset advances to `end`.
        let shared = super::super::make_shared_data_for_tests();
        let sink = SessionBoundDiscordRelaySink::new(Arc::new(HealthRegistry::new()));
        let matching = inflight_with_identity_offset(
            channel.get(),
            session,
            0,
            "2026-06-04T00:00:00Z",
            Some(0),
        );
        super::super::inflight::save_inflight_state(&matching).expect("persist matching inflight");
        assert_eq!(shared.committed_relay_offset(channel), 0);

        let gateway = ShortReplaceFakeGateway {
            outcome: super::super::formatting::ReplaceLongMessageOutcome::EditedOriginal,
            ok: true,
            replace_calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let trace = SessionRelayTraceContext::default();
        let outcome = rt
            .block_on(sink.deliver_short_replace_via_controller(
                &gateway,
                &shared,
                &provider,
                channel,
                channel.get(),
                MessageId::new(99),
                "answer",
                "answer",
                &delivery,
                &trace,
                start,
                end,
            ))
            .expect("matching-identity cut-over delivery is Ok");
        assert!(
            matches!(outcome, SessionRelayDeliveryOutcome::Delivered),
            "MATCH: the fresh identity gate advances → Delivered"
        );
        assert_eq!(
            shared.committed_relay_offset(channel),
            end,
            "MATCH: the production advance ran the FRESH identity gate and advanced to end"
        );
        assert_eq!(
            gateway
                .replace_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly ONE transport POST through the injected gateway"
        );

        // ---- MISMATCH: the on-disk inflight identity DIVERGES ----------------
        // A different turn now owns the channel (later turn_start_offset). The
        // production `advance` closure re-loads inflight FRESH and the gate
        // REFUSES → NotDelivered AND the committed offset does NOT advance.
        // A mutation that hard-codes advance=`true` would advance here and Deliver.
        let shared2 = super::super::make_shared_data_for_tests();
        let sink2 = SessionBoundDiscordRelaySink::new(Arc::new(HealthRegistry::new()));
        let mismatched = inflight_with_identity_offset(
            channel.get(),
            session,
            0,
            "2026-06-04T00:00:00Z",
            Some(4096),
        );
        super::super::inflight::save_inflight_state(&mismatched)
            .expect("persist mismatched inflight");
        assert_eq!(shared2.committed_relay_offset(channel), 0);

        let gateway2 = ShortReplaceFakeGateway {
            outcome: super::super::formatting::ReplaceLongMessageOutcome::EditedOriginal,
            ok: true,
            replace_calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let outcome2 = rt
            .block_on(sink2.deliver_short_replace_via_controller(
                &gateway2,
                &shared2,
                &provider,
                channel,
                channel.get(),
                MessageId::new(99),
                "answer",
                "answer",
                &delivery,
                &trace,
                start,
                end,
            ))
            .expect("mismatched-identity cut-over delivery is still Ok (POST landed)");
        // The POST landed (sink-local Delivered maps both lease outcomes), but the
        // committed offset must NOT advance — the gate refused. The decisive
        // mutation-sensitive assertion is the offset, not the sink-local outcome.
        assert!(matches!(outcome2, SessionRelayDeliveryOutcome::Delivered));
        assert_eq!(
            shared2.committed_relay_offset(channel),
            0,
            "MISMATCH: the FRESH identity gate must REFUSE the advance (offset stays 0). \
             A hard-coded advance=true mutation advances here and fails this assertion."
        );
        assert_eq!(
            gateway2
                .replace_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the mismatched run still POSTs once (markerless-equivalent), only the advance differs"
        );
    }

    // #4081 round-4: the session-bound short-replace controller sends formatted
    // Discord text, but its duplicate fingerprint must be recorded from the raw
    // terminal body. `# heading` is intentionally used because Discord formatting
    // rewrites that shape; recording `relay_text` would make the raw-body watcher
    // duplicate check miss.
    #[test]
    fn session_sink_short_replace_raw_body_fingerprint_refuses_watcher_rerelay_4081() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        struct EnvReset(Option<std::ffi::OsString>);
        impl Drop for EnvReset {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                    None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
                }
            }
        }
        let _env_reset = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let temp = tempfile::TempDir::new().expect("temp runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(8_041_4081);
        let session = "AgentDesk-claude-session-sink-raw-4081";
        let raw_body = "# heading\nbody line\n".to_string();
        let relay_text = formatting::format_for_discord_with_provider(&raw_body, &provider);
        assert_ne!(
            raw_body, relay_text,
            "the regression pin needs formatted/raw divergence"
        );
        let module_src = include_str!("session_relay_sink.rs");
        assert!(
            module_src.contains("let raw_response_text = delivery.response_text.clone();"),
            "session sink must preserve raw response bytes before Discord formatting"
        );
        assert!(
            module_src.contains("format_for_discord_with_provider(&raw_response_text"),
            "session sink must format from the preserved raw body, not derive raw from formatted text"
        );
        assert!(
            module_src.contains("format_for_discord_with_status_panel(&raw_response_text"),
            "session sink status-panel formatting must also use the preserved raw body"
        );
        assert!(
            module_src.contains("&relay_text,\n                        &raw_response_text,"),
            "session sink production cut-over call must thread the raw pre-format body into the fingerprint slot"
        );

        let gen_path = crate::services::tmux_common::session_temp_path(session, "generation");
        std::fs::create_dir_all(std::path::Path::new(&gen_path).parent().unwrap())
            .expect("generation dir");
        std::fs::write(&gen_path, b"1").expect("generation file");

        let start = 0;
        let end = raw_body.len() as u64;
        let mut delivery =
            delivery_with_fence_offset(session, Some(end), 0, "2026-07-04T00:00:00Z", Some(start));
        delivery.provider = provider.clone();
        delivery.channel_id = channel.get();
        delivery.response_text = raw_body.clone();

        let matching = inflight_with_identity_offset(
            channel.get(),
            session,
            0,
            "2026-07-04T00:00:00Z",
            Some(start),
        );
        super::super::inflight::save_inflight_state(&matching).expect("persist matching inflight");

        let shared = super::super::make_shared_data_for_tests();
        let sink = SessionBoundDiscordRelaySink::new(Arc::new(HealthRegistry::new()));
        let gateway = ShortReplaceFakeGateway {
            outcome: super::super::formatting::ReplaceLongMessageOutcome::EditedOriginal,
            ok: true,
            replace_calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let trace = SessionRelayTraceContext::default();

        let outcome = rt
            .block_on(sink.deliver_short_replace_via_controller(
                &gateway,
                &shared,
                &provider,
                channel,
                channel.get(),
                MessageId::new(99),
                &relay_text,
                &raw_body,
                &delivery,
                &trace,
                start,
                end,
            ))
            .expect("raw-fingerprint cut-over delivery is Ok");
        assert!(matches!(outcome, SessionRelayDeliveryOutcome::Delivered));
        assert!(
            dr::recent_delivered_content_matches(&provider, channel, session, &raw_body),
            "the watcher-direct duplicate-refusal lookup must match the raw terminal body"
        );
        assert!(
            !dr::recent_delivered_content_matches(&provider, channel, session, &relay_text),
            "formatted Discord relay text must not satisfy the raw-body refusal lookup"
        );
    }

    // #3089 A2b (review-fix Medium-1): the no-double-acquire invariant — a
    // cut-over short-replace turn must SKIP the legacy `SinkDeliveryLeaseGuard`
    // acquire so the controller owns the SINGLE lease. The pure
    // `sink_guard_lease_range` returns `None` for any cut-over turn; dropping the
    // `&& !cutover_short_replace` exclusion (the guard-skip mutation) makes it
    // return `Some(..)` and fails here, while the legacy (non-cutover) branches
    // still acquire over a real ordered range.
    #[test]
    fn cutover_skips_sink_guard_acquire() {
        // Cut-over ON + a real ordered range → guard MUST be skipped (no double-acquire).
        assert_eq!(
            sink_guard_lease_range(Some((0, 256)), true),
            None,
            "a cut-over short-replace turn must NOT acquire the legacy sink guard \
             (no double-acquire — the controller owns the single lease)"
        );
        // Cut-over OFF (legacy long-chunk / new-message) → guard acquires the range.
        assert_eq!(
            sink_guard_lease_range(Some((0, 256)), false),
            Some((0, 256)),
            "the legacy branches still acquire ONE sink guard over the ordered range"
        );
        // Absent range (degenerate / no fence) → no guard either way.
        assert_eq!(sink_guard_lease_range(None, false), None);
        assert_eq!(sink_guard_lease_range(None, true), None);
    }

    // #3089 A2b (review-fix M2): an EMPTY body diverges between the controller and
    // legacy, so the cut-over gate (`!relay_text.is_empty()`) MUST keep empty bodies
    // on the legacy path. This proves the divergence the gate guards against: the
    // controller returns `Skipped` (→ the sink maps it to `Err(Transient)`, no-advance)
    // for an empty body, whereas legacy `replace_long_message_raw_with_outcome` treats
    // zero chunks as `EditedOriginal` (delivered/advance, `formatting.rs:2063`). Without
    // the `!relay_text.is_empty()` gate a nonempty answer that sanitises to empty would
    // flip delivered/advance → Transient/no-advance.
    #[test]
    fn controller_skips_empty_body_so_cutover_gate_keeps_it_legacy() {
        let ch = ChannelId::new(8_041);
        let cell = Arc::new(super::super::DeliveryLeaseCell::new(ch));
        let turn = super::super::turn_finalizer::TurnKey::new(ch, 7, 0);
        let controller = super::super::placeholder_controller::PlaceholderController::default();
        // Gateway whose transport would PANIC if reached — the empty-body short-circuit
        // must return `Skipped` BEFORE any transport (the controller never POSTs empty).
        let gateway = ShortReplaceFakeGateway {
            outcome: super::super::formatting::ReplaceLongMessageOutcome::EditedOriginal,
            ok: true,
            replace_calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let hb = NoopHeartbeat;
        let advance = |_r: (u64, u64)| -> bool { true };
        let result = futures::executor::block_on(toc::deliver_turn_output(
            &gateway,
            toc::TurnOutputCtx {
                turn,
                lease_key: Some(super::super::DeliveryLeaseKey::from_turn_key(turn)),
                owner: RelayOwnerKind::SessionBoundRelay,
                holder: super::super::LeaseHolder::Sink,
                lease: &*cell,
                channel_id: ch,
                placeholder_controller: &controller,
                placeholder: toc::PlaceholderSlot::Active {
                    message_id: MessageId::new(99),
                    key: super::super::placeholder_controller::PlaceholderKey {
                        provider: ProviderKind::Claude,
                        channel_id: ch,
                        message_id: MessageId::new(99),
                    },
                },
                body: "",
                send_range: (10, 42),
                plan: toc::OutputPlan::Replace {
                    lifecycle: PlaceholderLifecycle::Active,
                },
                edit_fail_policy: toc::EditFailPlaceholderPolicy::PreserveAlways,
                fallback_commit_policy: toc::FallbackCommitPolicy::CommitOnFallback,
                acquire_failure_mode: toc::AcquireFailureMode::ProceedMarkerless,
                advance: Some(&advance),
                heartbeat: Some(&hb),
            },
        ));
        assert!(
            matches!(result, toc::DeliveryOutcome::Skipped),
            "the controller short-circuits an empty body to Skipped (legacy would advance) — \
             so the cut-over gate must keep empty bodies on the legacy path"
        );
        assert_eq!(
            gateway
                .replace_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an empty body never reaches transport on the controller path"
        );
        assert!(
            matches!(cell.read(), super::super::LeaseSnapshot::Unleased),
            "an empty-body Skip never touches the lease"
        );
    }

    #[test]
    fn structural_exclusion_gate_keeps_no_range_and_empty_body_on_legacy_path() {
        let module_src = include_str!("session_relay_sink.rs");

        let assignment = format!("let {} = ", "cutover_short_replace");
        let gate_start = module_src
            .find(&assignment)
            .expect("session sink cutover gate assignment");
        let gate_src = &module_src[gate_start
            ..gate_start
                + module_src[gate_start..]
                    .find(';')
                    .expect("session sink cutover gate terminator")];

        assert!(
            gate_src.contains(&format!("{}.is_some()", "cutover_range")),
            "A2b cutover_range=None / NoRange must stay on the legacy no-advance path"
        );
        assert!(
            gate_src.contains(&format!("&& !{}.is_empty()", "relay_text")),
            "A2b empty-body must stay legacy because controller Skipped would not advance"
        );
        assert!(
            gate_src.contains("SessionBoundTerminalDeliveryRoute::PlaceholderEdit(_)"),
            "A2b controller path is limited to anchored placeholder edits"
        );
        assert!(
            module_src.contains(&format!(
                "{}.filter(|_| {})",
                "cutover_range", "cutover_short_replace"
            )),
            "A2b controller branch must require both the ordered range and cutover gate"
        );
    }

    #[allow(dead_code)] // #3034: test helper superseded by `inflight_with_identity_offset`.
    fn inflight_with_identity(
        channel_id: u64,
        session_name: &str,
        user_msg_id: u64,
        started_at: &str,
    ) -> super::super::inflight::InflightTurnState {
        inflight_with_identity_offset(channel_id, session_name, user_msg_id, started_at, None)
    }

    fn inflight_with_identity_offset(
        channel_id: u64,
        session_name: &str,
        user_msg_id: u64,
        started_at: &str,
        turn_start_offset: Option<u64>,
    ) -> super::super::inflight::InflightTurnState {
        let mut state = super::super::inflight::InflightTurnState::new(
            ProviderKind::Claude,
            channel_id,
            None,
            1,
            user_msg_id,
            0,
            "hi".to_string(),
            None,
            Some(session_name.to_string()),
            None,
            None,
            0,
        );
        state.started_at = started_at.to_string();
        state.turn_start_offset = turn_start_offset;
        state
    }

    // #3041 P1-3 (Part a, BLOCKER B1 — FRAME-CARRIED): when the sink CONFIRMS a
    // terminal delivery, it advances `committed_relay_offset` to the producer's
    // authoritative consumed-terminal END carried ON the RESULT-bearing frame
    // (`SessionRelayDelivery::terminal_consumed_end`), identity-gated against the
    // channel's CURRENT inflight. This is the B1 "commit fence": the POST success
    // and the offset advance are coupled per-frame (no inflight-file read race), so
    // the watcher's §3.2 reconciliation (Part b) sees the delegated range as
    // delivered even if the terminal-commit ACK lags.
    #[tokio::test]
    async fn sink_confirmed_delivery_advances_committed_offset_to_frame_end() {
        let shared = super::super::make_shared_data_for_tests();
        let channel = ChannelId::new(8_041);
        let session = "AgentDesk-claude-8041";
        let sink = SessionBoundDiscordRelaySink::new(Arc::new(HealthRegistry::new()));
        // The channel's current inflight identity the frame must match to advance.
        // A fence ALWAYS carries a real `turn_start_offset` (producer guarantee),
        // and the STRICT gate requires it to match, so the inflight carries one too.
        let inflight = inflight_with_identity_offset(
            channel.get(),
            session,
            0,
            "2026-06-04T00:00:00Z",
            Some(0),
        );

        // Before any delivery the authority is at 0.
        assert_eq!(shared.committed_relay_offset(channel), 0);

        // A confirmed sink delivery for the producer-delegated range `[0, 256)`:
        // the frame carries end=256 + the matching identity, so the sink advances.
        sink.advance_offset_for_confirmed_delegated_terminal(
            &shared,
            &ProviderKind::Claude,
            channel.get(),
            session,
            &delivery_with_fence_offset(session, Some(256), 0, "2026-06-04T00:00:00Z", Some(0)),
            Some(&inflight),
        );
        assert_eq!(
            shared.committed_relay_offset(channel),
            256,
            "a confirmed sink delivery must advance committed_relay_offset to the frame end (B1 close)"
        );

        // Idempotent / no double-advance: re-confirming the SAME range is a
        // monotonic-CAS no-op (cannot regress or double-advance).
        sink.advance_offset_for_confirmed_delegated_terminal(
            &shared,
            &ProviderKind::Claude,
            channel.get(),
            session,
            &delivery_with_fence_offset(session, Some(256), 0, "2026-06-04T00:00:00Z", Some(0)),
            Some(&inflight),
        );
        assert_eq!(shared.committed_relay_offset(channel), 256);

        // When the producer did NOT delegate a range (None / zero end), the sink
        // does NOT advance — the watcher's own delivery path owns the advance.
        sink.advance_offset_for_confirmed_delegated_terminal(
            &shared,
            &ProviderKind::Claude,
            channel.get(),
            session,
            &delivery_with_fence_offset(session, None, 0, "2026-06-04T00:00:00Z", Some(0)),
            Some(&inflight),
        );
        sink.advance_offset_for_confirmed_delegated_terminal(
            &shared,
            &ProviderKind::Claude,
            channel.get(),
            session,
            &delivery_with_fence_offset(session, Some(0), 0, "2026-06-04T00:00:00Z", Some(0)),
            Some(&inflight),
        );
        assert_eq!(
            shared.committed_relay_offset(channel),
            256,
            "no delegated range (None/0) must not advance the authority"
        );

        // Monotonic guard: a later confirmed delivery at a LARGER end advances; a
        // stale smaller end never regresses the authority.
        sink.advance_offset_for_confirmed_delegated_terminal(
            &shared,
            &ProviderKind::Claude,
            channel.get(),
            session,
            &delivery_with_fence_offset(session, Some(128), 0, "2026-06-04T00:00:00Z", Some(0)),
            Some(&inflight),
        );
        assert_eq!(
            shared.committed_relay_offset(channel),
            256,
            "a smaller (stale) end must not regress the authority"
        );
        sink.advance_offset_for_confirmed_delegated_terminal(
            &shared,
            &ProviderKind::Claude,
            channel.get(),
            session,
            &delivery_with_fence_offset(session, Some(512), 0, "2026-06-04T00:00:00Z", Some(0)),
            Some(&inflight),
        );
        assert_eq!(shared.committed_relay_offset(channel), 512);
    }

    // #3041 P1-3 (Part a, B1 — IDENTITY GATE): a terminal frame whose pinned turn
    // identity does NOT match the channel's current inflight (a delayed/old frame,
    // or a newer turn already on the channel, or no inflight at all) must NOT
    // advance the authority — that would wrongly skip the new turn's reconciliation.
    #[tokio::test]
    async fn sink_identity_gate_blocks_advance_on_wrong_turn() {
        let shared = super::super::make_shared_data_for_tests();
        let channel = ChannelId::new(8_041);
        let session = "AgentDesk-claude-8041";
        let sink = SessionBoundDiscordRelaySink::new(Arc::new(HealthRegistry::new()));

        // Current inflight is turn started_at=NEW; the frame carries the OLD turn's
        // identity (same user_msg_id=0 external-input, different started_at). A fence
        // ALWAYS carries a real `turn_start_offset` (producer guarantee).
        let current_inflight = inflight_with_identity_offset(
            channel.get(),
            session,
            0,
            "2026-06-04T00:00:09Z",
            Some(4096),
        );
        let stale_frame =
            delivery_with_fence_offset(session, Some(256), 0, "2026-06-04T00:00:00Z", Some(0));

        sink.advance_offset_for_confirmed_delegated_terminal(
            &shared,
            &ProviderKind::Claude,
            channel.get(),
            session,
            &stale_frame,
            Some(&current_inflight),
        );
        assert_eq!(
            shared.committed_relay_offset(channel),
            0,
            "a stale/wrong-turn terminal frame must NOT advance the authority (identity gate)"
        );

        // No inflight at all (cleared by stop/cancel) → also no advance.
        sink.advance_offset_for_confirmed_delegated_terminal(
            &shared,
            &ProviderKind::Claude,
            channel.get(),
            session,
            &stale_frame,
            None,
        );
        assert_eq!(
            shared.committed_relay_offset(channel),
            0,
            "a terminal frame with no current inflight must NOT advance (identity gate)"
        );

        // Different user_msg_id (a managed turn replaced the external-input turn)
        // → no advance even if started_at coincidentally matched.
        let replaced_inflight = inflight_with_identity_offset(
            channel.get(),
            session,
            999,
            "2026-06-04T00:00:00Z",
            Some(0),
        );
        sink.advance_offset_for_confirmed_delegated_terminal(
            &shared,
            &ProviderKind::Claude,
            channel.get(),
            session,
            &stale_frame,
            Some(&replaced_inflight),
        );
        assert_eq!(
            shared.committed_relay_offset(channel),
            0,
            "a frame whose user_msg_id != current inflight must NOT advance (identity gate)"
        );

        // Sanity: the SAME identity (including matching turn_start_offset) DOES
        // advance, proving the gate isn't blocking everything.
        let matching_inflight = inflight_with_identity_offset(
            channel.get(),
            session,
            0,
            "2026-06-04T00:00:00Z",
            Some(0),
        );
        sink.advance_offset_for_confirmed_delegated_terminal(
            &shared,
            &ProviderKind::Claude,
            channel.get(),
            session,
            &stale_frame,
            Some(&matching_inflight),
        );
        assert_eq!(
            shared.committed_relay_offset(channel),
            256,
            "a matching-identity terminal frame DOES advance the authority"
        );
    }

    // #3041 P1-3 (codex P1-3 issue 2 — same-second identity collision close): two
    // consecutive TUI-direct turns with `user_msg_id == 0` started in the SAME
    // `now_string` second share an identical `(0, started_at)` pair. WITHOUT the
    // `turn_start_offset` discriminator a delayed OLD terminal frame would pass the
    // NEW turn's identity gate and wrongly advance. The gate now also compares
    // `turn_start_offset` (monotonic per turn), so the old frame is blocked.
    #[tokio::test]
    async fn sink_identity_gate_blocks_same_second_turn_by_turn_start_offset() {
        let shared = super::super::make_shared_data_for_tests();
        let channel = ChannelId::new(8_041);
        let session = "AgentDesk-claude-8041";
        let sink = SessionBoundDiscordRelaySink::new(Arc::new(HealthRegistry::new()));

        // Both turns: user_msg_id == 0 AND the SAME started_at (same second). They
        // differ ONLY by turn_start_offset (the JSONL byte offset they began at).
        let same_second = "2026-06-04T00:00:00Z";
        // The NEW turn now owns the channel (turn_start_offset = 4096).
        let new_inflight =
            inflight_with_identity_offset(channel.get(), session, 0, same_second, Some(4096));
        // A delayed OLD terminal frame from the PRIOR turn (turn_start_offset = 0)
        // arrives — its (user_msg_id, started_at) pair matches, but its offset does
        // not. Pre-fix this would have advanced the NEW turn's authority.
        let stale_same_second_frame =
            delivery_with_fence_offset(session, Some(256), 0, same_second, Some(0));

        sink.advance_offset_for_confirmed_delegated_terminal(
            &shared,
            &ProviderKind::Claude,
            channel.get(),
            session,
            &stale_same_second_frame,
            Some(&new_inflight),
        );
        assert_eq!(
            shared.committed_relay_offset(channel),
            0,
            "an old same-second frame (matching user_msg_id+started_at but different turn_start_offset) must NOT advance the new turn"
        );

        // The NEW turn's OWN terminal frame (matching turn_start_offset) DOES
        // advance, proving the offset gate is discriminating, not blocking all.
        let new_turn_frame =
            delivery_with_fence_offset(session, Some(512), 0, same_second, Some(4096));
        sink.advance_offset_for_confirmed_delegated_terminal(
            &shared,
            &ProviderKind::Claude,
            channel.get(),
            session,
            &new_turn_frame,
            Some(&new_inflight),
        );
        assert_eq!(
            shared.committed_relay_offset(channel),
            512,
            "the matching-offset frame for the current turn DOES advance the authority"
        );
    }

    // #3041 P1-3 (codex P1-3 issue 2 R4 — STRICT offset gate, no weak fallback): a
    // fenced terminal frame whose `turn_start_offset` is None must NOT advance, even
    // when its `(user_msg_id, started_at)` pair matches the current inflight. The
    // producer GUARANTEES every fence carries a real offset, so a None here is a
    // malformed/legacy frame and the strict gate refuses the advance — closing the
    // old `is_none_or` weak fallback that let a None authorize an advance and could
    // collide for consecutive same-second `user_msg_id == 0` turns.
    #[tokio::test]
    async fn sink_strict_gate_blocks_advance_when_frame_offset_is_none() {
        let shared = super::super::make_shared_data_for_tests();
        let channel = ChannelId::new(8_041);
        let session = "AgentDesk-claude-8041";
        let sink = SessionBoundDiscordRelaySink::new(Arc::new(HealthRegistry::new()));

        // Current inflight has a real turn_start_offset; the (user_msg_id, started_at)
        // pair matches the frame exactly. Only the frame's missing offset differs.
        let same_second = "2026-06-04T00:00:00Z";
        let inflight =
            inflight_with_identity_offset(channel.get(), session, 0, same_second, Some(4096));
        // A fenced frame whose turn_start_offset is None (the weak-fallback case).
        let none_offset_frame =
            delivery_with_fence_offset(session, Some(256), 0, same_second, None);

        sink.advance_offset_for_confirmed_delegated_terminal(
            &shared,
            &ProviderKind::Claude,
            channel.get(),
            session,
            &none_offset_frame,
            Some(&inflight),
        );
        assert_eq!(
            shared.committed_relay_offset(channel),
            0,
            "a fenced frame with turn_start_offset=None must NOT advance (no weak None fallback)"
        );

        // Two `user_msg_id == 0` turns differing ONLY by turn_start_offset: only the
        // frame whose offset matches the current inflight advances. The mismatched
        // one (the prior turn's offset) is blocked even though user_msg_id+started_at
        // are identical (same-second TUI-direct collision).
        let mismatched_offset_frame =
            delivery_with_fence_offset(session, Some(256), 0, same_second, Some(0));
        sink.advance_offset_for_confirmed_delegated_terminal(
            &shared,
            &ProviderKind::Claude,
            channel.get(),
            session,
            &mismatched_offset_frame,
            Some(&inflight),
        );
        assert_eq!(
            shared.committed_relay_offset(channel),
            0,
            "an offset-mismatched same-second frame must NOT advance"
        );

        let matching_offset_frame =
            delivery_with_fence_offset(session, Some(256), 0, same_second, Some(4096));
        sink.advance_offset_for_confirmed_delegated_terminal(
            &shared,
            &ProviderKind::Claude,
            channel.get(),
            session,
            &matching_offset_frame,
            Some(&inflight),
        );
        assert_eq!(
            shared.committed_relay_offset(channel),
            256,
            "the offset-matching same-second frame DOES advance"
        );
    }

    // #3041 P1-3 (codex P1-3 issue 3 — stale-snapshot close): the production
    // advance path re-loads the inflight FRESH after the confirmed POST. If the
    // turn was cleared/replaced DURING the slow POST, the post-POST re-check sees
    // the CURRENT (replaced/cleared) inflight and blocks the wrong-turn advance —
    // even though a snapshot taken before the POST would have matched.
    #[test]
    fn sink_post_post_recheck_blocks_advance_when_inflight_replaced_during_post() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        struct EnvReset(Option<std::ffi::OsString>);
        impl Drop for EnvReset {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                    None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
                }
            }
        }
        let _env_reset = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let temp = tempfile::TempDir::new().unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        let shared = super::super::make_shared_data_for_tests();
        let channel = ChannelId::new(8_041);
        let session = "AgentDesk-claude-8041";
        let sink = SessionBoundDiscordRelaySink::new(Arc::new(HealthRegistry::new()));

        // The frame was delegated for the ORIGINAL turn (turn_start_offset=0). A
        // pre-POST snapshot would carry this identity and authorize the advance.
        let delivery =
            delivery_with_fence_offset(session, Some(256), 0, "2026-06-04T00:00:00Z", Some(0));

        // DURING the (simulated) slow POST a NEW turn took the channel: same
        // user_msg_id==0 and same second, but a later turn_start_offset. We persist
        // THAT as the on-disk inflight so the fresh post-POST reload reads it.
        let replaced = inflight_with_identity_offset(
            channel.get(),
            session,
            0,
            "2026-06-04T00:00:00Z",
            Some(4096),
        );
        super::super::inflight::save_inflight_state(&replaced).expect("persist replaced inflight");

        // Post-POST advance: re-loads inflight from disk (the REPLACED turn) → the
        // identity gate (turn_start_offset 0 != 4096) blocks the advance.
        sink.advance_after_confirmed_post(
            &shared,
            &ProviderKind::Claude,
            channel.get(),
            session,
            &delivery,
            None,
        );
        assert_eq!(
            shared.committed_relay_offset(channel),
            0,
            "a turn replaced DURING the POST must block the advance (post-POST fresh re-check)"
        );

        // Control: if the original turn STILL owns the channel at post-POST time,
        // the fresh reload matches and the advance proceeds.
        let still_original = inflight_with_identity_offset(
            channel.get(),
            session,
            0,
            "2026-06-04T00:00:00Z",
            Some(0),
        );
        super::super::inflight::save_inflight_state(&still_original).expect("persist original");
        sink.advance_after_confirmed_post(
            &shared,
            &ProviderKind::Claude,
            channel.get(),
            session,
            &delivery,
            None,
        );
        assert_eq!(
            shared.committed_relay_offset(channel),
            256,
            "when the original turn still owns the channel at post-POST time, the advance proceeds"
        );

        // Cleared inflight (stop/cancel during POST) → no advance either.
        super::super::make_shared_data_for_tests();
        super::super::inflight::clear_inflight_state(&ProviderKind::Claude, channel.get());
        let advanced_delivery =
            delivery_with_fence_offset(session, Some(512), 0, "2026-06-04T00:00:00Z", Some(0));
        sink.advance_after_confirmed_post(
            &shared,
            &ProviderKind::Claude,
            channel.get(),
            session,
            &advanced_delivery,
            None,
        );
        assert_eq!(
            shared.committed_relay_offset(channel),
            256,
            "a cleared inflight at post-POST time must block the advance (identity gate, no inflight)"
        );
    }

    // #3041 P1-3 (Part a, B1): the RESULT-bearing terminal frame's commit fence
    // (consumed_end + identity) is copied onto the emitted `SessionRelayDelivery`,
    // so `deliver_response` advances the authority from frame data — not an inflight
    // file read. A non-terminal frame's delivery (none here) would carry None/0.
    #[test]
    fn parser_propagates_terminal_frame_commit_fence_onto_delivery() {
        let binding = matched("46");
        let mut parser = SessionRelayParser::default();
        // A non-terminal text frame: accumulates, emits nothing.
        let assistant = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"final answer\"}]}}\n";
        assert!(
            parser
                .ingest_frame(&frame(&binding, assistant, 1))
                .is_empty()
        );
        // The result-bearing TERMINAL frame carries the commit fence.
        let result = "{\"type\":\"result\",\"result\":\"final answer\"}\n";
        let deliveries = parser.ingest_frame(&terminal_frame(
            &binding,
            result,
            2,
            512,
            77,
            "2026-06-04T00:00:00Z",
        ));
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].response_text, "final answer");
        assert_eq!(
            deliveries[0].terminal_consumed_end,
            Some(512),
            "the delivery must carry the terminal frame's consumed_end"
        );
        assert_eq!(deliveries[0].frame_turn_user_msg_id, 77);
        assert_eq!(deliveries[0].frame_turn_started_at, "2026-06-04T00:00:00Z");
        // #3041 P1-3 (codex P1-3 issue 2): the delivery also carries the frame's
        // pinned turn_start_offset for the sink's identity gate.
        assert_eq!(deliveries[0].frame_turn_start_offset, Some(0));
    }

    // #3041 P1-3 (codex P1-3 issue 1 — multi-turn-chunk close): after the watcher
    // SPLITS a `result(A) + bytes(B)` physical chunk, the sink's parser receives
    // turn A's bytes on a TERMINAL frame (A's fence) and turn B's bytes on a
    // SEPARATE frame. A commits to A's end with A's identity; B is NOT black-holed —
    // it is mirrored and, when it completes, commits to B's end with B's identity.
    // Pre-fix, B's bytes rode A's terminal frame and a follow-up empty chunk emitted
    // no frame for B (black-hole) and could reuse A's ACK (mis-commit).
    #[test]
    fn split_multi_turn_chunk_commits_each_turn_with_its_own_fence() {
        let binding = matched("8051");
        let mut parser = SessionRelayParser::default();

        // The watcher split produced: terminal frame = turn A (result + A's fence),
        // then a separate non-terminal frame = turn B's leading bytes.
        let turn_a_result = "{\"type\":\"result\",\"result\":\"answer A\"}\n";
        let turn_a_deliveries = parser.ingest_frame(&terminal_frame_offset(
            &binding,
            turn_a_result,
            1,
            /*consumed_end=*/ 100,
            /*user_msg_id=*/ 0,
            "2026-06-04T00:00:00Z",
            /*turn_start_offset=*/ Some(0),
        ));
        // Turn A commits to A's end (100) with A's identity (offset 0).
        assert_eq!(turn_a_deliveries.len(), 1, "turn A must produce a delivery");
        assert_eq!(turn_a_deliveries[0].response_text, "answer A");
        assert_eq!(turn_a_deliveries[0].terminal_consumed_end, Some(100));
        assert_eq!(turn_a_deliveries[0].frame_turn_start_offset, Some(0));

        // The sink resets the parser after each terminal delivery (via
        // `finish_terminal_candidate`); model that so turn B starts a fresh turn.
        parser.reset_turn();

        // Turn B's leading bytes arrive on their OWN (non-terminal) frame — the
        // split-out tail. The parser accumulates them (B not yet complete → no
        // delivery), proving B is MIRRORED rather than black-holed.
        let turn_b_assistant = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer B\"}]}}\n";
        assert!(
            parser
                .ingest_frame(&frame(&binding, turn_b_assistant, 2))
                .is_empty(),
            "turn B's split-out bytes must accumulate (mirrored), not vanish"
        );

        // When turn B completes on a later pass, it gets its OWN terminal frame +
        // fence: B commits to B's end (250) with B's identity (offset 100) — NOT
        // turn A's. No shared-ACK reuse, no mis-commit.
        let turn_b_result = "{\"type\":\"result\",\"result\":\"answer B\"}\n";
        let turn_b_deliveries = parser.ingest_frame(&terminal_frame_offset(
            &binding,
            turn_b_result,
            3,
            /*consumed_end=*/ 250,
            /*user_msg_id=*/ 0,
            "2026-06-04T00:00:01Z",
            /*turn_start_offset=*/ Some(100),
        ));
        assert_eq!(
            turn_b_deliveries.len(),
            1,
            "turn B must commit (not black-holed)"
        );
        assert_eq!(turn_b_deliveries[0].response_text, "answer B");
        assert_eq!(
            turn_b_deliveries[0].terminal_consumed_end,
            Some(250),
            "turn B commits to B's end, not A's"
        );
        assert_eq!(
            turn_b_deliveries[0].frame_turn_start_offset,
            Some(100),
            "turn B commits with B's own identity (turn_start_offset), not A's"
        );
    }

    #[test]
    fn parser_keeps_stop_hook_summary_soft_until_late_assistant_text_and_result() {
        let binding = matched("45");
        let mut parser = SessionRelayParser::default();
        let stop_hook_candidate = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"early \"}]}}\n",
            "{\"type\":\"system\",\"subtype\":\"stop_hook_summary\",\"sessionId\":\"sess-tui\",\"hookCount\":1,\"hasOutput\":true}\n"
        );

        assert!(
            parser
                .ingest_frame(&frame(&binding, stop_hook_candidate, 1))
                .is_empty(),
            "stop_hook_summary is only a soft terminal candidate and must not reset the turn"
        );

        let late_tail_and_result = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"late tail\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}\n"
        );
        let deliveries = parser.ingest_frame(&frame(&binding, late_tail_and_result, 2));
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].response_text, "early late tail");
    }

    #[test]
    fn parser_emits_only_user_visible_task_notification_response() {
        let binding = matched("42");
        let mut parser = SessionRelayParser::default();

        let pure_subagent = concat!(
            "{\"type\":\"system\",\"subtype\":\"task_started\",\"task_id\":\"sub-1\",\"task_type\":\"local_agent\"}\n",
            "{\"type\":\"system\",\"subtype\":\"task_notification\",\"task_id\":\"sub-1\",\"status\":\"completed\",\"summary\":\"Subagent finished\"}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}\n"
        );
        assert!(
            parser
                .ingest_frame(&frame(&binding, pure_subagent, 1))
                .is_empty()
        );

        let parent_text = concat!(
            "{\"type\":\"system\",\"subtype\":\"task_started\",\"task_id\":\"sub-2\",\"task_type\":\"local_agent\"}\n",
            "{\"type\":\"system\",\"subtype\":\"task_notification\",\"task_id\":\"sub-2\",\"status\":\"completed\",\"summary\":\"Subagent finished\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"final answer\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}\n"
        );
        let deliveries = parser.ingest_frame(&frame(&binding, parent_text, 2));
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].response_text, "final answer");
        assert_eq!(
            deliveries[0].task_notification_kind,
            Some(TaskNotificationKind::Subagent)
        );
        assert!(
            deliveries[0].task_notification_context.is_some(),
            "the terminal delivery must retain sanitized subagent task context"
        );
    }

    #[test]
    fn parser_preserves_text_across_tool_uses_within_turn() {
        // #2749 Pattern A: [text1] → [tool_use, text2] → [tool_use, no post-text]
        // → result. The trailing tool_use without post-text used to clear
        // full_response and overwrite with result.result, dropping text1+text2.
        let binding = matched("44");
        let mut parser = SessionRelayParser::default();
        let payload = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"first chunk \"}]}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Bash\",\"input\":{\"command\":\"ls\"}},{\"type\":\"text\",\"text\":\"second chunk \"}]}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_1\",\"content\":\"ok\"}]}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_2\",\"name\":\"Bash\",\"input\":{\"command\":\"pwd\"}}]}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_2\",\"content\":\"/tmp\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"third chunk\"}\n"
        );
        let deliveries = parser.ingest_frame(&frame(&binding, payload, 1));
        assert_eq!(deliveries.len(), 1);
        // Exact equality guards against accidental duplication or chunk reorder.
        assert_eq!(
            deliveries[0].response_text,
            "first chunk second chunk \nthird chunk"
        );
    }

    #[test]
    fn parser_delivers_background_task_notification_with_result_text() {
        // #2749 Pattern B: a Background-classified turn (e.g. cron self-prompt)
        // whose response is captured via result.result only used to drop because
        // assistant_text_seen was false. Background turns should still deliver.
        let binding = matched("45");
        let mut parser = SessionRelayParser::default();
        let payload = concat!(
            "{\"type\":\"system\",\"subtype\":\"task_notification\",\"task_id\":\"bg-2\",\"status\":\"completed\",\"summary\":\"background work\"}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"OK | cron self-prompt response\"}\n"
        );
        let deliveries = parser.ingest_frame(&frame(&binding, payload, 1));
        assert_eq!(deliveries.len(), 1);
        assert_eq!(
            deliveries[0].response_text,
            "OK | cron self-prompt response"
        );
        assert_eq!(
            deliveries[0].task_notification_kind,
            Some(TaskNotificationKind::Background)
        );
        assert!(
            deliveries[0].task_notification_context.is_some(),
            "card promotion requires the exact task context beside the response"
        );
    }

    #[test]
    fn parser_drops_footer_only_context_when_provider_output_is_empty() {
        let binding = matched("4055");
        let mut parser = SessionRelayParser::default();
        let payload = concat!(
            "{\"type\":\"system\",\"subtype\":\"task_notification\",\"task_id\":\"bg-empty\",\"tool_use_id\":\"toolu-bg-empty\",\"status\":\"completed\",\"summary\":\"background work\"}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"\"}\n"
        );
        assert!(parser.ingest_frame(&frame(&binding, payload, 1)).is_empty());
        assert!(
            parser.task_notification_context.is_none(),
            "an empty provider result stays footer-only and resets without card promotion"
        );
    }

    #[test]
    fn parser_preserves_monitor_priority_for_origin_tagging() {
        let binding = matched("43");
        let mut parser = SessionRelayParser::default();
        let payload = concat!(
            "{\"type\":\"system\",\"subtype\":\"task_notification\",\"task_id\":\"bg-1\",\"status\":\"completed\",\"summary\":\"background\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_mon_1\",\"name\":\"Monitor\",\"input\":{\"command\":\"gh pr view\"}}]}}\n",
            "{\"type\":\"system\",\"subtype\":\"task_started\",\"task_id\":\"mon-1\",\"tool_use_id\":\"toolu_mon_1\",\"task_type\":\"tool\"}\n",
            "{\"type\":\"system\",\"subtype\":\"task_notification\",\"task_id\":\"mon-1\",\"status\":\"completed\",\"summary\":\"Monitor event\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"monitor result\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}\n"
        );
        let deliveries = parser.ingest_frame(&frame(&binding, payload, 1));
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].response_text, "monitor result");
        assert_eq!(
            deliveries[0].task_notification_kind,
            Some(TaskNotificationKind::MonitorAutoTurn)
        );
    }

    fn external_input_delivery(session_name: &str, channel_id: u64) -> SessionRelayDelivery {
        SessionRelayDelivery {
            provider: ProviderKind::Claude,
            channel_id,
            session_name: session_name.to_string(),
            response_text: "answer".to_string(),
            task_notification_kind: None,
            task_notification_context: None,
            terminal_consumed_end: None,
            frame_turn_user_msg_id: 0,
            frame_turn_started_at: "2026-06-04T00:00:00Z".to_string(),
            frame_turn_start_offset: None,
        }
    }

    fn session_bound_lease(
        channel_id: u64,
        session: &str,
        turn_seq: u64,
    ) -> ExternalInputRelayLease {
        ExternalInputRelayLease {
            channel_id: Some(channel_id),
            turn_id: Some(format!("external:claude:{channel_id}:{session}:{turn_seq}")),
            session_key: Some(format!("host:{session}")),
            relay_owner: ExternalInputRelayOwner::SessionBoundRelay,
            runtime_kind: Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui),
            generation:
                crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        }
    }

    // #3041 P1-4 (§4-④): the CORE regression. Before P1-4 the session-bound
    // external_input lease was cleared only on the four Ok delivery branches; an
    // Err / `?` / HTTP-503 left it set for the full 600s TTL. This drives a real
    // `deliver_response` whose `shared_for_provider` is unavailable (empty
    // HealthRegistry) so the FIRST `?` returns `Err(Transient)` — and asserts the
    // RAII guard's Drop STILL released the lease, so the NEXT terminal delivery
    // is NOT blocked for up to ~10 minutes.
    //
    // SAFETY (await_holding_lock): `shared_test_env_lock()` and the dedupe
    // `TEST_LOCK` are std test-serialization Mutexes (NOT production locks). They
    // are held across `deliver_response().await` only to keep this test's
    // `AGENTDESK_ROOT_DIR` + shared dedupe state isolated from other tests; the
    // awaited future itself never tries to re-acquire either lock, so there is no
    // deadlock — this is the established pattern (see `src/reconcile.rs`).
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn deliver_response_err_path_releases_external_input_lease_via_guard() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        struct EnvReset(Option<std::ffi::OsString>);
        impl Drop for EnvReset {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                    None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
                }
            }
        }
        let _env_reset = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let temp = tempfile::TempDir::new().unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();

        let channel_id = 8_041_u64;
        let session = "AgentDesk-claude-p1-4-err";
        let lease = session_bound_lease(channel_id, session, 1);
        crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            ProviderKind::Claude.as_str(),
            session,
            lease.clone(),
        );
        // Precondition: the input-dedup lease is present (it routes, does not block).
        assert!(
            crate::services::tui_prompt_dedupe::external_input_relay_lease_present(
                ProviderKind::Claude.as_str(),
                session,
                channel_id,
            )
        );

        // Empty HealthRegistry → `shared_for_provider` is None → the first `?`
        // in `deliver_response` returns Err(Transient) before any Discord POST.
        let sink = SessionBoundDiscordRelaySink::new(Arc::new(HealthRegistry::new()));
        let result = sink
            .deliver_response(external_input_delivery(session, channel_id))
            .await;
        assert!(
            matches!(result, Err(RelaySinkError::Transient(_))),
            "precondition: this path must return Err (shared unavailable) to exercise the leak path, got {result:?}"
        );

        // The RAII guard's Drop fired on the Err `?` exit → lease released.
        assert!(
            !crate::services::tui_prompt_dedupe::external_input_relay_lease_present(
                ProviderKind::Claude.as_str(),
                session,
                channel_id,
            ),
            "the external_input lease MUST be released on the Err/`?` path (RAII guard Drop), so the next terminal delivery is not blocked for the 600s TTL"
        );
    }

    // #3041 P1-4: NO-CLOBBER. If a NEWER turn re-takes the same
    // (provider, tmux_session, channel) lease DURING a slow delivery, an old
    // delivery's guard Drop must NOT clear the newer turn's lease — the guard
    // compare-and-clears (`_if_matches`) only its OWN captured lease.
    #[test]
    fn lease_guard_drop_preserves_newer_turn_lease_no_clobber() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();

        let channel_id = 8_042_u64;
        let session = "AgentDesk-claude-p1-4-no-clobber";

        // Turn 1 records its lease; the sink arms a guard capturing turn 1's lease.
        let lease1 = session_bound_lease(channel_id, session, 1);
        crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            ProviderKind::Claude.as_str(),
            session,
            lease1.clone(),
        );
        let guard = SessionBoundExternalInputLeaseGuard::arm_if_present(
            &ProviderKind::Claude,
            channel_id,
            session,
        )
        .expect("guard arms when a SessionBoundRelay lease is present");

        // DURING the slow delivery a NEWER turn (turn 2) re-takes the same key.
        let lease2 = session_bound_lease(channel_id, session, 2);
        let recorded2 = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            ProviderKind::Claude.as_str(),
            session,
            lease2.clone(),
        );
        assert_ne!(
            guard.generation, recorded2.generation,
            "each recorded lease must get a DISTINCT generation"
        );

        // The OLD guard drops (turn 1 finished/errored) — must NOT clobber turn 2.
        drop(guard);
        assert_eq!(
            crate::services::tui_prompt_dedupe::external_input_relay_lease(
                ProviderKind::Claude.as_str(),
                session,
                channel_id,
            ),
            Some(recorded2),
            "a newer turn's lease must survive an old guard's drop (compare-and-clear by generation)"
        );
    }

    // #3041 P1-4 codex: NO-CLOBBER for two VALUE-IDENTICAL `Unassigned` leases.
    // Two legacy `Unassigned` turns for the same (provider, tmux_session, channel)
    // have `turn_id`/`session_key`/`runtime_kind` ALL `None`, so they are
    // indistinguishable by value. The per-record `generation` makes them distinct:
    // an OLD guard's Drop must clear ONLY its own generation and leave the newer
    // (value-identical) lease in place. Without the generation, a slow old
    // delivery's guard would wrongly release the newer turn's lease.
    #[test]
    fn lease_guard_drop_preserves_newer_unassigned_lease_by_generation() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();

        let channel_id = 8_044_u64;
        let session = "AgentDesk-claude-p1-4-unassigned-no-clobber";

        // Turn 1: a legacy/Unassigned lease (all trace fields None) is recorded and
        // the sink arms a guard capturing turn 1's generation.
        crate::services::tui_prompt_dedupe::record_external_input_relay_lease(
            ProviderKind::Claude.as_str(),
            session,
            Some(channel_id),
        );
        let guard = SessionBoundExternalInputLeaseGuard::arm_if_present(
            &ProviderKind::Claude,
            channel_id,
            session,
        )
        .expect("guard arms for an Unassigned input-dedup lease");

        // DURING the slow delivery a NEWER, VALUE-IDENTICAL Unassigned turn re-takes
        // the same key (same provider/session/channel; all trace fields None).
        crate::services::tui_prompt_dedupe::record_external_input_relay_lease(
            ProviderKind::Claude.as_str(),
            session,
            Some(channel_id),
        );
        let newer = crate::services::tui_prompt_dedupe::external_input_relay_lease(
            ProviderKind::Claude.as_str(),
            session,
            channel_id,
        )
        .expect("newer Unassigned lease present");
        assert_eq!(
            newer.relay_owner,
            ExternalInputRelayOwner::Unassigned,
            "the newer lease is the value-identical Unassigned marker"
        );
        assert_ne!(
            guard.generation, newer.generation,
            "two Unassigned leases for the same key must get DISTINCT generations"
        );

        // The OLD guard drops — by generation it does NOT match the newer lease, so
        // the newer (value-identical) lease must SURVIVE.
        drop(guard);
        assert_eq!(
            crate::services::tui_prompt_dedupe::external_input_relay_lease(
                ProviderKind::Claude.as_str(),
                session,
                channel_id,
            ),
            Some(newer),
            "a newer value-identical Unassigned lease must survive an old guard's drop (clear-by-generation, no clobber)"
        );
    }

    // #3041 P1-4: when the guard's OWN lease is still current at drop time, the
    // Drop releases it (the happy-path release that replaces the manual Ok-path
    // clears). Also verifies arm_if_present is INERT for a foreign-owner lease
    // (BridgeAdapter) — that lease belongs to another subsystem and must be left
    // for ITS guard, never cleared by the session-bound sink.
    #[test]
    fn lease_guard_drop_clears_own_lease_and_is_inert_for_foreign_owner() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();

        let channel_id = 8_043_u64;
        let session = "AgentDesk-claude-p1-4-own";

        // Own (SessionBoundRelay) lease → guard arms and clears on drop.
        let own = session_bound_lease(channel_id, session, 1);
        crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            ProviderKind::Claude.as_str(),
            session,
            own.clone(),
        );
        {
            let _guard = SessionBoundExternalInputLeaseGuard::arm_if_present(
                &ProviderKind::Claude,
                channel_id,
                session,
            )
            .expect("guard arms for own SessionBoundRelay lease");
        }
        assert!(
            !crate::services::tui_prompt_dedupe::external_input_relay_lease_present(
                ProviderKind::Claude.as_str(),
                session,
                channel_id,
            ),
            "the guard releases its own lease on drop (replaces the manual Ok-path clears)"
        );

        // Foreign-owner (BridgeAdapter) lease → guard does NOT arm; the foreign
        // lease is left intact for the owning subsystem's own guard.
        let foreign = ExternalInputRelayLease {
            relay_owner: ExternalInputRelayOwner::BridgeAdapter,
            ..session_bound_lease(channel_id, session, 2)
        };
        let recorded_foreign = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            ProviderKind::Claude.as_str(),
            session,
            foreign.clone(),
        );
        assert!(
            SessionBoundExternalInputLeaseGuard::arm_if_present(
                &ProviderKind::Claude,
                channel_id,
                session,
            )
            .is_none(),
            "the session-bound sink must NOT take ownership of a foreign-owner (BridgeAdapter) lease"
        );
        assert_eq!(
            crate::services::tui_prompt_dedupe::external_input_relay_lease(
                ProviderKind::Claude.as_str(),
                session,
                channel_id,
            ),
            Some(recorded_foreign),
            "a foreign-owner lease is preserved (input-dedup / cross-subsystem routing not regressed)"
        );
    }

    /// #3151: the `SinkDeliveryLeaseGuard` is the deterministic seam for the
    /// in-flight sink-delivery marker. Driving the full `deliver_response` needs
    /// Discord HTTP, so these tests exercise the guard's acquire/commit/release
    /// semantics directly on a real per-channel `DeliveryLeaseCell` — the exact
    /// cell the watcher gate reads.
    mod inflight_sink_marker {
        use super::super::SinkDeliveryLeaseGuard;
        use crate::services::discord::turn_finalizer::TurnKey;
        use crate::services::discord::{
            DeliveryLeaseCell, DeliveryLeaseKey, LeaseHolder, LeaseOutcome, LeaseSnapshot,
        };
        use serenity::model::id::ChannelId;
        use std::sync::Arc;

        const START: u64 = 100;
        const END: u64 = 200;

        fn lease_key(ch: ChannelId) -> DeliveryLeaseKey {
            DeliveryLeaseKey::from_turn_key(TurnKey::new(ch, 5, 0))
        }

        /// (a) SLOW SINK IN FLIGHT: acquiring the guard sets the cell to
        /// `Leased{Sink, [start,end)}` — the marker the watcher gate reads as
        /// "a sink POST is in flight" → WaitInFlight (no duplicate).
        #[tokio::test]
        async fn acquire_sets_leased_sink_marker() {
            let ch = ChannelId::new(7301);
            let cell = Arc::new(DeliveryLeaseCell::new(ch));
            let turn = lease_key(ch);
            let guard =
                SinkDeliveryLeaseGuard::acquire(&cell, turn, START, END).expect("acquire wins");
            match cell.read() {
                LeaseSnapshot::Leased {
                    holder, start, end, ..
                } => {
                    assert_eq!(holder, LeaseHolder::Sink);
                    assert_eq!((start, end), (START, END));
                }
                other => panic!("expected Leased{{Sink}}, got {other:?}"),
            }
            drop(guard);
        }

        /// SUCCESS path: commit() flips the marker to `Committed{Sink, Delivered}`;
        /// the guard's Drop then RELEASES it back to `Unleased`. (Production advances
        /// the committed offset BEFORE commit, so the watcher reads committed>=end.)
        #[tokio::test]
        async fn commit_then_drop_releases_to_unleased() {
            let ch = ChannelId::new(7302);
            let cell = Arc::new(DeliveryLeaseCell::new(ch));
            let turn = lease_key(ch);
            {
                let guard =
                    SinkDeliveryLeaseGuard::acquire(&cell, turn, START, END).expect("acquire wins");
                guard.commit(LeaseOutcome::Delivered);
                match cell.read() {
                    LeaseSnapshot::Committed {
                        holder, outcome, ..
                    } => {
                        assert_eq!(holder, LeaseHolder::Sink);
                        assert_eq!(outcome, LeaseOutcome::Delivered);
                    }
                    other => panic!("expected Committed{{Sink}}, got {other:?}"),
                }
            }
            // Guard dropped without an explicit release call — Drop released it.
            assert!(
                matches!(cell.read(), LeaseSnapshot::Unleased),
                "Drop releases the committed marker back to Unleased"
            );
        }

        /// #3159 BUG 1: REFUSED-ADVANCE path. When the identity gate refuses the
        /// offset advance, `advance_after_confirmed_post` commits `NotDelivered`
        /// instead of `Delivered`. The marker is `Committed{Sink, NotDelivered}`,
        /// which the watcher routes through committed-offset reconciliation (committed
        /// stayed < end because the advance never ran) → SendFull. No under-delivery.
        #[tokio::test]
        async fn commit_not_delivered_marks_refused_advance() {
            let ch = ChannelId::new(7306);
            let cell = Arc::new(DeliveryLeaseCell::new(ch));
            let turn = lease_key(ch);
            {
                let guard =
                    SinkDeliveryLeaseGuard::acquire(&cell, turn, START, END).expect("acquire wins");
                guard.commit(LeaseOutcome::NotDelivered);
                match cell.read() {
                    LeaseSnapshot::Committed {
                        holder, outcome, ..
                    } => {
                        assert_eq!(holder, LeaseHolder::Sink);
                        assert_eq!(
                            outcome,
                            LeaseOutcome::NotDelivered,
                            "a refused advance must commit NotDelivered, not Delivered"
                        );
                    }
                    other => panic!("expected Committed{{Sink, NotDelivered}}, got {other:?}"),
                }
            }
            assert!(
                matches!(cell.read(), LeaseSnapshot::Unleased),
                "Drop releases the NotDelivered marker back to Unleased"
            );
        }

        /// FAILURE/Err path: the guard is dropped WITHOUT commit (the POST `?`-errored
        /// or the identity gate blocked the advance) → the cell returns to `Unleased`
        /// and committed is NOT advanced (the watcher then SendFulls — no black-hole).
        #[tokio::test]
        async fn drop_without_commit_releases_without_committing() {
            let ch = ChannelId::new(7303);
            let cell = Arc::new(DeliveryLeaseCell::new(ch));
            let turn = lease_key(ch);
            {
                let _guard =
                    SinkDeliveryLeaseGuard::acquire(&cell, turn, START, END).expect("acquire wins");
                // No commit() — simulate the Err/`?` path.
            }
            assert!(
                matches!(cell.read(), LeaseSnapshot::Unleased),
                "a failure path that never commits leaves the cell Unleased (committed not advanced)"
            );
        }

        /// ACQUIRE FAILS when the watcher/bridge already holds the range: the guard is
        /// `None` → the sink POSTs markerless (no double-commit, no self-black-hole),
        /// and the existing holder's lease is untouched.
        #[test]
        fn acquire_fails_when_another_holder_owns_range() {
            let ch = ChannelId::new(7304);
            let cell = Arc::new(DeliveryLeaseCell::new(ch));
            let turn = lease_key(ch);
            let now = crate::services::discord::lease_now_ms();
            let watcher_holder = LeaseHolder::Watcher { instance_id: 1 };
            // A watcher already holds the cell for this range (B2).
            assert!(cell.try_acquire(
                turn.clone(),
                watcher_holder,
                START,
                END,
                now.saturating_add(10_000),
            ));
            // The sink's acquire loses → None (markerless POST; no duplicate).
            assert!(
                SinkDeliveryLeaseGuard::acquire(&cell, turn, START, END).is_none(),
                "the sink acquire must lose to the watcher's existing lease"
            );
            // The watcher's lease is intact (still Leased by the watcher).
            match cell.read() {
                LeaseSnapshot::Leased { holder, .. } => assert_eq!(holder, watcher_holder),
                other => panic!("expected the watcher's lease intact, got {other:?}"),
            }
        }

        /// A STALE-EXPIRED prior holder is self-healed by `acquire`'s
        /// `reclaim_if_expired` (mirrors the watcher) so the sink's acquire still
        /// wins — otherwise the sink would POST markerless and reintroduce the dup.
        #[tokio::test]
        async fn acquire_self_heals_an_expired_prior_holder() {
            let ch = ChannelId::new(7305);
            let cell = Arc::new(DeliveryLeaseCell::new(ch));
            let turn = lease_key(ch);
            let now = crate::services::discord::lease_now_ms();
            // A dead prior holder whose deadline is already in the past.
            assert!(cell.try_acquire(
                turn.clone(),
                LeaseHolder::Watcher { instance_id: 9 },
                START,
                END,
                now.saturating_sub(1),
            ));
            // The sink's acquire reclaims the expired holder first, then wins.
            let guard = SinkDeliveryLeaseGuard::acquire(&cell, turn, START, END)
                .expect("acquire wins after self-healing the expired holder");
            match cell.read() {
                LeaseSnapshot::Leased { holder, .. } => assert_eq!(holder, LeaseHolder::Sink),
                other => panic!("expected Leased{{Sink}} after reclaim, got {other:?}"),
            }
            drop(guard);
        }
    }

    // #3089 A0 — characterization of the session-bound should-send-new-chunks
    // predicate's EXACT 2000-byte boundary (design §5 A0 item 1). The parent
    // module already proves long-vs-short; this pins the strict-`>` cliff (2000
    // single, 2001 splits) so the controller's single length policy must
    // reproduce this surface's boundary. Pinned inline in this `#[cfg(test)] mod
    // tests` block of the FROZEN (#3036, baseline 1731) file => ZERO prod LoC.
    mod a0_characterization_tests {
        use super::super::session_bound_should_send_new_chunks_for_placeholder as should_send;
        use crate::services::discord::DISCORD_MSG_LIMIT;

        #[test]
        fn a0_session_bound_predicate_boundary_is_strictly_greater_than_2000() {
            assert_eq!(DISCORD_MSG_LIMIT, 2000, "the shared length limit is 2000");
            assert!(
                !should_send(&"a".repeat(DISCORD_MSG_LIMIT)),
                "exactly 2000 bytes is NOT over-limit (strict >)"
            );
            assert!(
                should_send(&"a".repeat(DISCORD_MSG_LIMIT + 1)),
                "2001 bytes is over-limit => new chunks"
            );
            assert!(!should_send("short"));
        }
    }
}
