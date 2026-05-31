//! Discord [`RelaySink`] for the session-bound `StreamRelay` path.
//!
//! `tmux_watcher` remains the tmux file reader / producer, but when the
//! supervisor has a matched session, this sink performs the terminal Discord
//! write. Inflight state only selects placeholder-edit metadata; a missing
//! inflight is still a valid pane-bound new-message route. The watcher then
//! treats terminal delivery as delegated instead of sending directly.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serenity::model::id::{ChannelId, MessageId};

use super::formatting::{self, ReplaceLongMessageOutcome};
use super::health::HealthRegistry;
use super::inflight::{InflightTurnState, RelayOwnerKind, TurnSource};
use super::tmux::{WatcherToolState, process_watcher_lines};
use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::cluster::stream_relay::{
    RelaySink, RelaySinkError, RelaySinkOutcome, StreamFrame,
};
use crate::services::cluster::watcher_supervisor::{SupervisorConfig, run_watcher_supervisor_loop};
use crate::services::provider::ProviderKind;
use crate::services::session_backend::StreamLineState;
use tracing::Instrument;

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
    // A normal Discord-origin inflight already has the tmux watcher as the
    // terminal delivery owner. The session-bound StreamRelay sink is still
    // attached to the same JSONL, so letting it deliver while an inflight is
    // present creates a second terminal post. Treat only rebind/adopted rows
    // as no real foreground turn; scheduled wakeups and idle background output
    // reach this path with no inflight at all.
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

fn session_bound_terminal_delivery_route_decision(
    inflight: Option<&InflightTurnState>,
    tmux_session_name: &str,
    provider: &ProviderKind,
    channel_id: u64,
) -> SessionBoundTerminalDeliveryRouteDecision {
    if session_bound_external_lease_blocks_delivery(provider, channel_id, tmux_session_name) {
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
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session_name: &str,
) -> bool {
    let Some(lease) = crate::services::tui_prompt_dedupe::external_input_relay_lease(
        provider.as_str(),
        tmux_session_name,
        channel_id,
    ) else {
        return false;
    };
    !matches!(
        lease.relay_owner,
        crate::services::tui_prompt_dedupe::ExternalInputRelayOwner::Unassigned
            | crate::services::tui_prompt_dedupe::ExternalInputRelayOwner::SessionBoundRelay
    )
}

fn session_bound_should_send_new_chunks_for_placeholder(response_text: &str) -> bool {
    response_text.len() > super::DISCORD_MSG_LIMIT
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

    async fn deliver_response(
        &self,
        delivery: SessionRelayDelivery,
    ) -> Result<SessionRelayDeliveryOutcome, RelaySinkError> {
        let channel_id = delivery.channel_id;
        let provider = delivery.provider;
        let inflight = super::inflight::load_inflight_state(&provider, channel_id);
        let trace = session_relay_trace_context(
            &provider,
            channel_id,
            &delivery.session_name,
            inflight.as_ref(),
        );
        let route = match session_bound_terminal_delivery_route_decision(
            inflight.as_ref(),
            &delivery.session_name,
            &provider,
            channel_id,
        ) {
            SessionBoundTerminalDeliveryRouteDecision::Route(route) => route,
            SessionBoundTerminalDeliveryRouteDecision::Skipped => {
                tracing::debug!(
                    provider = provider.as_str(),
                    channel = channel_id,
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
                return Ok(SessionRelayDeliveryOutcome::Skipped);
            }
        };
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

        let formatted = if shared.status_panel_v2_enabled {
            formatting::format_for_discord_with_status_panel(&delivery.response_text, &provider)
        } else {
            formatting::format_for_discord_with_provider(&delivery.response_text, &provider)
        };
        let relay_text = if matches!(
            delivery.task_notification_kind,
            Some(TaskNotificationKind::MonitorAutoTurn)
        ) {
            super::prepend_monitor_auto_turn_origin(&formatted)
        } else {
            formatted
        };
        let channel = ChannelId::new(channel_id);
        if let SessionBoundTerminalDeliveryRoute::PlaceholderEdit(msg_id) = route {
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
                    channel = channel_id,
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
                crate::services::tui_prompt_dedupe::clear_external_input_relay_lease(
                    provider.as_str(),
                    &delivery.session_name,
                    channel_id,
                );
                return Ok(SessionRelayDeliveryOutcome::Committed);
            }
            match formatting::replace_long_message_raw_with_outcome(
                &http,
                channel,
                msg_id,
                &relay_text,
                &shared,
            )
            .await
            {
                Ok(ReplaceLongMessageOutcome::EditedOriginal) => {
                    self.delivered_total.fetch_add(1, Ordering::AcqRel);
                    tracing::info!(
                        provider = provider.as_str(),
                        channel = channel_id,
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
                    crate::services::tui_prompt_dedupe::clear_external_input_relay_lease(
                        provider.as_str(),
                        &delivery.session_name,
                        channel_id,
                    );
                    Ok(SessionRelayDeliveryOutcome::Committed)
                }
                Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { edit_error }) => {
                    // #2757: do not delete msg_id here. The 3e158e588 path
                    // deleted the placeholder assuming it was stale, but
                    // msg_id is the bridge's current_msg_id which may already
                    // contain streamed response content. A transient edit
                    // failure (rate limit, network) then leads to the actual
                    // response being removed. Leave the original message in
                    // place; the fallback copy is the redundant one.
                    self.delivered_total.fetch_add(1, Ordering::AcqRel);
                    tracing::warn!(
                        provider = provider.as_str(),
                        channel = channel_id,
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
                    crate::services::tui_prompt_dedupe::clear_external_input_relay_lease(
                        provider.as_str(),
                        &delivery.session_name,
                        channel_id,
                    );
                    Ok(SessionRelayDeliveryOutcome::Committed)
                }
                Ok(ReplaceLongMessageOutcome::PartialContinuationFailure { error, .. }) => {
                    Err(RelaySinkError::Transient(error.to_string()))
                }
                Err(error) => Err(RelaySinkError::Transient(error.to_string())),
            }
        } else {
            let prompt_anchor = ssh_direct_prompt_anchor_for_response(
                &provider,
                &delivery.session_name,
                channel_id,
            );
            let prompt_anchor_reference = prompt_anchor_reference(prompt_anchor);
            formatting::send_long_message_raw_with_reference(
                &http,
                channel,
                &relay_text,
                &shared,
                prompt_anchor_reference,
            )
            .await
            .map_err(|error| RelaySinkError::Transient(error.to_string()))?;
            if let Some(prompt_anchor) = prompt_anchor {
                clear_ssh_direct_prompt_anchor(&provider, &delivery.session_name, prompt_anchor);
            }
            self.delivered_total.fetch_add(1, Ordering::AcqRel);
            crate::services::tui_prompt_dedupe::clear_external_input_relay_lease(
                provider.as_str(),
                &delivery.session_name,
                channel_id,
            );
            tracing::info!(
                provider = provider.as_str(),
                channel = channel_id,
                tmux_session = %delivery.session_name,
                turn_id = trace.turn_id().unwrap_or(""),
                dispatch_id = trace.dispatch_id().unwrap_or(""),
                session_key = trace.session_key().unwrap_or(""),
                relay_owner = trace.relay_owner(),
                runtime_kind = trace.runtime_kind(),
                prompt_anchor_message_id = prompt_anchor_reference
                    .map(|(_, message_id)| message_id.get()),
                chars = relay_text.chars().count(),
                "session-bound relay sink delivered terminal response via new message"
            );
            crate::services::observability::emit_relay_delivery(
                provider.as_str(),
                channel_id,
                trace.dispatch_id(),
                trace.session_key(),
                trace.turn_id(),
                prompt_anchor_reference.map(|(_, message_id)| message_id.get()),
                "session_relay_sink",
                "post",
                None,
                None,
                true,
                Some("new message"),
            );
            Ok(SessionRelayDeliveryOutcome::Committed)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionRelayDeliveryOutcome {
    Committed,
    Skipped,
}

#[async_trait]
impl RelaySink for SessionBoundDiscordRelaySink {
    async fn deliver(&self, frame: &StreamFrame) -> Result<RelaySinkOutcome, RelaySinkError> {
        let deliveries = self.ingest_frame(frame);
        let mut terminal_committed = false;
        let mut terminal_skipped = false;
        for delivery in deliveries {
            let session_name = delivery.session_name.clone();
            match self.deliver_response(delivery).await {
                Ok(SessionRelayDeliveryOutcome::Committed) => {
                    terminal_committed = true;
                    self.finish_terminal_candidate(&session_name);
                }
                Ok(SessionRelayDeliveryOutcome::Skipped) => {
                    terminal_skipped = true;
                    self.finish_terminal_candidate(&session_name);
                }
                Err(error) => {
                    self.finish_terminal_candidate(&session_name);
                    return Err(error);
                }
            }
        }
        if terminal_committed {
            Ok(RelaySinkOutcome::TerminalCommitted)
        } else if terminal_skipped {
            Ok(RelaySinkOutcome::TerminalSkipped)
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
    let sink: Arc<dyn RelaySink> = Arc::new(SessionBoundDiscordRelaySink::new(health_registry));
    let idle_shutdown = shutdown.clone();
    super::task_supervisor::spawn_observed(
        "session_bound_idle_jsonl_relay",
        async move {
            run_idle_jsonl_relay_loop(idle_shutdown).await;
        }
        .instrument(tracing::info_span!("session_bound_idle_jsonl_relay")),
    );
    run_watcher_supervisor_loop(SupervisorConfig::default(), sink, shutdown).await;
    SESSION_BOUND_DISCORD_DELIVERY_ENABLED.store(false, Ordering::Release);
}

async fn run_idle_jsonl_relay_loop(shutdown: Arc<AtomicBool>) {
    let registry = crate::services::cluster::session_registry::global_session_registry();
    let producers =
        crate::services::cluster::relay_producer_registry::global_relay_producer_registry();
    let mut offsets: HashMap<String, u64> = HashMap::new();
    let mut first_seen_at: HashMap<String, Instant> = HashMap::new();
    let mut last_inflight_seen_at: HashMap<String, Instant> = HashMap::new();

    while !shutdown.load(Ordering::Acquire) {
        let mut seen_sessions = HashSet::new();
        for entry in registry.list_matched() {
            let matched = entry.matched;
            let session_name = matched.expected_session_name.clone();
            seen_sessions.insert(session_name.clone());
            let first_seen = *first_seen_at
                .entry(session_name.clone())
                .or_insert_with(Instant::now);
            let Ok(channel_id) = matched.channel_id.parse::<u64>() else {
                continue;
            };
            let Ok(metadata) = std::fs::metadata(&matched.expected_rollout_path) else {
                continue;
            };
            let len = metadata.len();
            let offset = offsets.entry(session_name.clone()).or_insert(len);
            if len < *offset {
                *offset = 0;
            }

            if super::inflight::load_inflight_state(&matched.provider, channel_id).is_some() {
                last_inflight_seen_at.insert(session_name.clone(), Instant::now());
                *offset = len;
                continue;
            }
            if last_inflight_seen_at
                .get(&session_name)
                .is_some_and(|seen_at| seen_at.elapsed() < IDLE_JSONL_RELAY_RECENT_INFLIGHT_GRACE)
            {
                *offset = len;
                continue;
            }
            if len <= *offset {
                continue;
            }

            let start = *offset;
            let end = len.min(start.saturating_add(IDLE_JSONL_RELAY_MAX_BYTES_PER_TICK));
            let Ok(payload) = read_jsonl_range(&matched.expected_rollout_path, start, end) else {
                continue;
            };
            if payload.is_empty() {
                *offset = end;
                continue;
            }
            if first_seen.elapsed() < IDLE_JSONL_RELAY_RECENT_INFLIGHT_GRACE {
                *offset = end;
                tracing::debug!(
                    provider = matched.provider.as_str(),
                    channel = channel_id,
                    tmux_session = %session_name,
                    bytes = payload.len(),
                    "idle JSONL relay skipped new-session grace payload"
                );
                continue;
            }
            if idle_jsonl_payload_contains_user_event(&payload) {
                *offset = end;
                tracing::debug!(
                    provider = matched.provider.as_str(),
                    channel = channel_id,
                    tmux_session = %session_name,
                    bytes = payload.len(),
                    "idle JSONL relay skipped active-turn payload with user/tool-result event"
                );
                continue;
            }
            if idle_jsonl_payload_contains_schedule_wakeup_setup(&payload) {
                *offset = end;
                tracing::debug!(
                    provider = matched.provider.as_str(),
                    channel = channel_id,
                    tmux_session = %session_name,
                    bytes = payload.len(),
                    "idle JSONL relay skipped ScheduleWakeup setup payload"
                );
                continue;
            }
            if !idle_jsonl_payload_contains_init_event(&payload) {
                *offset = end;
                tracing::debug!(
                    provider = matched.provider.as_str(),
                    channel = channel_id,
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
            if producer.try_send_frame(String::from_utf8_lossy(&payload).into_owned()) {
                *offset = end;
                tracing::debug!(
                    provider = matched.provider.as_str(),
                    channel = channel_id,
                    tmux_session = %session_name,
                    bytes = payload.len(),
                    "idle JSONL relay forwarded background session output"
                );
            }
        }

        offsets.retain(|session, _| seen_sessions.contains(session));
        first_seen_at.retain(|session, _| seen_sessions.contains(session));
        last_inflight_seen_at.retain(|session, _| seen_sessions.contains(session));
        tokio::time::sleep(IDLE_JSONL_RELAY_POLL_INTERVAL).await;
    }
}

fn read_jsonl_range(path: &str, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut payload = Vec::new();
    file.take(end.saturating_sub(start))
        .read_to_end(&mut payload)?;
    Ok(payload)
}

fn idle_jsonl_payload_contains_user_event(payload: &[u8]) -> bool {
    for line in String::from_utf8_lossy(payload).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) == Some("user") {
            return true;
        }
    }
    false
}

fn idle_jsonl_payload_contains_schedule_wakeup_setup(payload: &[u8]) -> bool {
    for line in String::from_utf8_lossy(payload).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if jsonl_event_contains_schedule_wakeup_setup_reference(&value) {
            return true;
        }
    }
    false
}

fn jsonl_event_contains_schedule_wakeup_setup_reference(value: &serde_json::Value) -> bool {
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("assistant") => assistant_event_contains_schedule_wakeup_reference(value),
        Some("result") => value
            .get("result")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| text.contains("ScheduleWakeup")),
        _ => false,
    }
}

fn assistant_event_contains_schedule_wakeup_reference(value: &serde_json::Value) -> bool {
    let Some(content) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };
    content.iter().any(|item| {
        let item_type = item.get("type").and_then(serde_json::Value::as_str);
        match item_type {
            Some("tool_use") => {
                item.get("name").and_then(serde_json::Value::as_str) == Some("ScheduleWakeup")
            }
            Some("text") => item
                .get("text")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| text.contains("ScheduleWakeup")),
            _ => false,
        }
    })
}

fn idle_jsonl_payload_contains_init_event(payload: &[u8]) -> bool {
    for line in String::from_utf8_lossy(payload).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) == Some("system")
            && value.get("subtype").and_then(serde_json::Value::as_str) == Some("init")
        {
            return true;
        }
    }
    false
}

struct SessionRelayParser {
    buffer: String,
    stream_state: StreamLineState,
    full_response: String,
    tool_state: WatcherToolState,
    task_notification_kind: Option<TaskNotificationKind>,
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
            self.assistant_text_seen |= outcome.assistant_text_seen;
            if !outcome.found_result {
                break;
            }

            // #2749: Background task notifications (e.g. CronCreate self-prompts)
            // must still deliver their final response. assistant_text_seen may be
            // false when the parser fell back to result.result text only, but the
            // user still expects to see the answer. Subagent / MonitorAutoTurn keep
            // requiring assistant text to avoid noisy intermediate notifications.
            let task_kind_allows_delivery = match self.task_notification_kind {
                None => true,
                Some(TaskNotificationKind::Background) => true,
                Some(_) => self.assistant_text_seen,
            };
            let has_user_visible_response =
                !self.full_response.trim().is_empty() && task_kind_allows_delivery;
            if has_user_visible_response {
                deliveries.push(SessionRelayDelivery {
                    provider: frame.binding.provider.clone(),
                    channel_id,
                    session_name: frame.session_name.clone(),
                    response_text: self.full_response.clone(),
                    task_notification_kind: self.task_notification_kind,
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
}

fn ssh_direct_prompt_anchor_for_response(
    provider: &ProviderKind,
    tmux_session_name: &str,
    channel_id: u64,
) -> Option<crate::services::tui_prompt_dedupe::TuiPromptAnchor> {
    crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
        provider.as_str(),
        tmux_session_name,
        channel_id,
    )
}

fn clear_ssh_direct_prompt_anchor(
    provider: &ProviderKind,
    tmux_session_name: &str,
    anchor: crate::services::tui_prompt_dedupe::TuiPromptAnchor,
) {
    crate::services::tui_prompt_dedupe::clear_prompt_anchor_for_response(
        provider.as_str(),
        tmux_session_name,
        anchor,
    );
}

fn prompt_anchor_reference(
    anchor: Option<crate::services::tui_prompt_dedupe::TuiPromptAnchor>,
) -> Option<(ChannelId, MessageId)> {
    anchor.map(|anchor| {
        (
            ChannelId::new(anchor.channel_id),
            MessageId::new(anchor.message_id),
        )
    })
}

fn merge_task_notification_kind(
    current: Option<TaskNotificationKind>,
    new_kind: TaskNotificationKind,
) -> Option<TaskNotificationKind> {
    let priority = |kind: TaskNotificationKind| match kind {
        TaskNotificationKind::Subagent => 0,
        TaskNotificationKind::Background => 1,
        TaskNotificationKind::MonitorAutoTurn => 2,
    };

    match current {
        Some(existing) if priority(existing) >= priority(new_kind) => Some(existing),
        _ => Some(new_kind),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::cluster::session_matcher::{MatchedChannel, expected_rollout_path_for};
    use crate::services::discord::inflight::{RelayOwnerKind, TurnSource};

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

    fn frame(binding: &MatchedChannel, payload: &str, sequence: u64) -> StreamFrame {
        StreamFrame {
            session_name: binding.expected_session_name.clone(),
            binding: binding.clone(),
            payload: payload.to_string(),
            sequence,
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
        let tmux = "AgentDesk-codex-bridge-owned-direct";
        crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            ProviderKind::Codex.as_str(),
            tmux,
            crate::services::tui_prompt_dedupe::ExternalInputRelayLease {
                channel_id: Some(4243),
                turn_id: Some("external:codex:4243:trace:1".to_string()),
                session_key: Some("host:AgentDesk-codex-bridge-owned-direct".to_string()),
                relay_owner:
                    crate::services::tui_prompt_dedupe::ExternalInputRelayOwner::BridgeAdapter,
                runtime_kind: Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui),
            },
        );

        assert_eq!(
            session_bound_terminal_delivery_route_decision(None, tmux, &ProviderKind::Codex, 4243,),
            SessionBoundTerminalDeliveryRouteDecision::Skipped
        );
        assert!(
            crate::services::tui_prompt_dedupe::clear_external_input_relay_lease(
                ProviderKind::Codex.as_str(),
                tmux,
                4243,
            )
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

    #[test]
    fn terminal_delivery_route_skip_maps_to_terminal_skipped_outcome() {
        let tmux = "AgentDesk-claude-relay-test";
        let bridge_owned = inflight_for(tmux, RelayOwnerKind::None, false);
        assert_eq!(
            session_bound_terminal_delivery_route_decision(
                Some(&bridge_owned),
                tmux,
                &ProviderKind::Claude,
                42,
            ),
            SessionBoundTerminalDeliveryRouteDecision::Skipped
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
}
