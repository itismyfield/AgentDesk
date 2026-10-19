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

#[cfg(test)]
pub(in crate::services::discord) use self::tests::sink_fixtures::*;
use super::delivery_lease_cell::source_epoch_observer;
use super::formatting::{self, ReplaceLongMessageOutcome};
use super::health::HealthRegistry;
use super::inflight::{self, InflightTurnState, RelayOwnerKind, TurnSource};
use super::outbound::delivery_record as dr;
use super::outbound::turn_output_controller as toc;
use super::replace_outcome_policy::edit_fail_fallback_disposition;
use super::{DeliveryLeaseCell, DeliveryLeaseHeartbeat, LeaseHolder, task_supervisor, tmux};
use crate::services::cluster::stream_relay::{
    RelaySink, RelaySinkError, RelaySinkOutcome, StreamFrame,
};
use crate::services::cluster::watcher_supervisor::{SupervisorConfig, run_watcher_supervisor_loop};
use crate::services::provider::ProviderKind;
use tracing::Instrument;

mod routing;
pub(in crate::services::discord) use routing::session_bound_discord_relay_can_own_terminal_delivery;
use routing::{
    SessionBoundExternalInputLeaseGuard, SessionBoundTerminalDeliveryRoute,
    SessionBoundTerminalDeliveryRouteDecision, SessionRelayTraceContext, SinkDeliveryLeaseGuard,
    SinkPostHeartbeat, session_bound_should_send_new_chunks_for_placeholder,
    session_bound_terminal_delivery_route_decision_with_lease, session_relay_trace_context,
    sink_delivery_lease_coordinate,
};
#[cfg(test)]
use routing::{
    session_bound_terminal_delivery_route, session_bound_terminal_delivery_route_or_skip,
};

mod delivery_commit;
mod delivery_frontier;
mod delivery_outcome_classify;
mod idle_jsonl;
#[cfg(test)]
pub(in crate::services::discord) use idle_jsonl::idle_range_is_committed;
pub(in crate::services::discord) mod journal;
mod short_controller;
#[path = "session_relay_sink/supervision.rs"]
mod supervision;
pub(crate) use supervision::run_session_bound_discord_relay_supervisor;
#[cfg(test)]
use supervision::{LegacySessionRelayConsumer, run_idle_jsonl_relay_loop};
// #3960: orphaned `SessionBoundRelay` TUI-direct reclaim (producer-liveness TOCTOU).
mod orphan_reclaim;
mod relay_format;
mod task_notification_context;
mod terminal_handoff;
pub(in crate::services::discord) mod turn_parser;
use self::idle_jsonl::IdlePending::{Deferred, RetainedForRetry, SentUnconfirmed};
use self::idle_jsonl::{
    IdleCursor, IdleJsonlSessionInitRearm, IdleJsonlSuppression, IdlePending, IdleRelayRangeAction,
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
use crate::services::tui_o::cutover::{IdentityError, claim_then_send};

static SESSION_BOUND_DISCORD_DELIVERY_ENABLED: AtomicBool = AtomicBool::new(false);
const IDLE_JSONL_RELAY_POLL_INTERVAL: Duration = Duration::from_millis(500);
const IDLE_JSONL_RELAY_RECENT_INFLIGHT_GRACE: Duration = Duration::from_secs(10);
const IDLE_JSONL_RELAY_MAX_BYTES_PER_TICK: u64 = 1_048_576;

pub(in crate::services::discord) fn session_bound_discord_delivery_enabled() -> bool {
    SESSION_BOUND_DISCORD_DELIVERY_ENABLED.load(Ordering::Acquire)
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
    pub(in crate::services::discord) test_gateway: Option<Arc<dyn super::gateway::TurnGateway>>,
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
        idle_jsonl::idle_range_is_committed(
            shared,
            provider,
            channel_id,
            session_name,
            delivery.relay_range,
            delivery.relay_generation_mtime_ns,
        )
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
        // Resolve an unknown destination before formatting; ownership gates only the body below.
        if channel_id == 0 {
            crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel(channel_id, None)
                .map_err(|error| {
                RelaySinkError::Transient(format!("TUI output identity held: {error}"))
            })?;
        }
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
        // O posts this body: consume the range without transport or delivery evidence. Ownership
        // is only read here; each route below claims at its own transport.
        if crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel_tmux(
            channel_id,
            Some(&delivery.session_name),
        )
        .map_err(|error| RelaySinkError::Transient(format!("TUI output identity held: {error}")))?
        {
            self.advance_after_o_delegated_terminal(
                &shared,
                &provider,
                channel_id,
                &delivery.session_name,
                &delivery,
                sink_lease_guard.as_ref(),
            );
            return Ok(SessionRelayDeliveryOutcome::Delivered);
        }
        let body_claim = crate::services::tui_o::cutover::BodyClaim::tmux(
            channel_id,
            Some(&delivery.session_name),
        );

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
                        body_claim: Some(body_claim),
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
                let send = || {
                    journal::send_long_chunks_with_anchor_receipt(
                        gateway,
                        &http,
                        channel,
                        msg_id,
                        &relay_text,
                        &shared,
                    )
                };
                let (message_ids, chunk_anchor_receipt) =
                    claimed_body_send(claim_then_send(Some(body_claim), send).await)??;
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
            let (http_ref, shared_ref, text) = (&http, &shared, relay_text.as_str());
            let (anchor, receipt) = (&mut last_chunk_anchor, &mut edit_anchor_receipt);
            let replace = move || async move {
                if let Some(gateway) = gateway {
                    gateway
                        .replace_message_with_outcome(channel, msg_id, text)
                        .await
                } else {
                    formatting::replace_long_message_raw_with_outcome_returning_receipt(
                        http_ref, channel, msg_id, text, shared_ref, anchor, receipt,
                    )
                    .await
                    .map_err(|error| error.to_string())
                }
            };
            let replace_outcome =
                claimed_body_send(claim_then_send(Some(body_claim), replace).await)?;
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
                body_claim,
            )
            .await
        }
    }
}

/// A claimed body send's own result; O owning the channel or a held identity sent nothing.
fn claimed_body_send<T>(
    sent: Result<crate::services::tui_o::cutover::BodySend<T>, IdentityError>,
) -> Result<T, RelaySinkError> {
    match sent {
        Ok(crate::services::tui_o::cutover::BodySend::Sent(sent)) => Ok(sent),
        Ok(crate::services::tui_o::cutover::BodySend::OwnedByO) => Err(RelaySinkError::Transient(
            "O owns this channel's body".to_string(),
        )),
        Err(error) => Err(RelaySinkError::Transient(format!(
            "TUI output identity held: {error}"
        ))),
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
pub(in crate::services::discord) mod tests;
