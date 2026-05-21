//! Discord [`RelaySink`] for the session-bound `StreamRelay` path.
//!
//! `tmux_watcher` remains the tmux file reader / producer, but when the
//! supervisor has a matched session, this sink performs the terminal Discord
//! write. Inflight state only selects placeholder-edit metadata; a missing
//! inflight is still a valid pane-bound new-message route. The watcher then
//! treats terminal delivery as delegated instead of sending directly.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serenity::model::id::{ChannelId, MessageId};

use super::formatting::{self, ReplaceLongMessageOutcome};
use super::health::HealthRegistry;
use super::inflight::{InflightTurnState, RelayOwnerKind};
use super::tmux::{WatcherToolState, process_watcher_lines};
use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::cluster::stream_relay::{RelaySink, RelaySinkError, StreamFrame};
use crate::services::cluster::watcher_supervisor::{SupervisorConfig, run_watcher_supervisor_loop};
use crate::services::provider::ProviderKind;
use crate::services::session_backend::StreamLineState;

static SESSION_BOUND_DISCORD_DELIVERY_ENABLED: AtomicBool = AtomicBool::new(false);

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
    state.rebind_origin || matches!(state.effective_relay_owner_kind(), RelayOwnerKind::Watcher)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionBoundTerminalDeliveryRoute {
    NewMessage,
    PlaceholderEdit(MessageId),
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
    if !state.rebind_origin && state.current_msg_id != 0 {
        return Some(SessionBoundTerminalDeliveryRoute::PlaceholderEdit(
            MessageId::new(state.current_msg_id),
        ));
    }
    Some(SessionBoundTerminalDeliveryRoute::NewMessage)
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

    async fn deliver_response(&self, delivery: SessionRelayDelivery) -> Result<(), RelaySinkError> {
        let channel_id = delivery.channel_id;
        let provider = delivery.provider;
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

        let inflight = super::inflight::load_inflight_state(&provider, channel_id);
        let route =
            session_bound_terminal_delivery_route(inflight.as_ref(), &delivery.session_name);
        let Some(route) = route else {
            tracing::debug!(
                provider = provider.as_str(),
                channel = channel_id,
                tmux_session = %delivery.session_name,
                "session-bound relay sink skipped bridge-owned or mismatched inflight"
            );
            return Ok(());
        };

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
                        chars = relay_text.chars().count(),
                        "session-bound relay sink delivered terminal response via placeholder edit"
                    );
                    Ok(())
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
                        chars = relay_text.chars().count(),
                        error = %edit_error,
                        "session-bound relay sink delivered terminal response via fallback; preserving original msg_id (#2757)"
                    );
                    Ok(())
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
            tracing::info!(
                provider = provider.as_str(),
                channel = channel_id,
                tmux_session = %delivery.session_name,
                prompt_anchor_message_id = prompt_anchor_reference
                    .map(|(_, message_id)| message_id.get()),
                chars = relay_text.chars().count(),
                "session-bound relay sink delivered terminal response via new message"
            );
            Ok(())
        }
    }
}

#[async_trait]
impl RelaySink for SessionBoundDiscordRelaySink {
    async fn deliver(&self, frame: &StreamFrame) -> Result<(), RelaySinkError> {
        let deliveries = self.ingest_frame(frame);
        for delivery in deliveries {
            self.deliver_response(delivery).await?;
        }
        Ok(())
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
    run_watcher_supervisor_loop(SupervisorConfig::default(), sink, shutdown).await;
    SESSION_BOUND_DISCORD_DELIVERY_ENABLED.store(false, Ordering::Release);
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

            let has_user_visible_response = !self.full_response.trim().is_empty()
                && (self.task_notification_kind.is_none() || self.assistant_text_seen);
            if has_user_visible_response {
                deliveries.push(SessionRelayDelivery {
                    provider: frame.binding.provider.clone(),
                    channel_id,
                    session_name: frame.session_name.clone(),
                    response_text: self.full_response.clone(),
                    task_notification_kind: self.task_notification_kind,
                });
            }
            self.reset_turn();
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
        assert!(session_bound_discord_relay_can_own_terminal_delivery(
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
    fn terminal_delivery_route_preserves_bridge_owned_skip_and_watcher_routes() {
        let tmux = "AgentDesk-claude-relay-test";
        let bridge_owned = inflight_for(tmux, RelayOwnerKind::None, false);
        assert_eq!(
            session_bound_terminal_delivery_route(Some(&bridge_owned), tmux),
            None
        );

        let watcher_owned = inflight_for(tmux, RelayOwnerKind::Watcher, false);
        assert_eq!(
            session_bound_terminal_delivery_route(Some(&watcher_owned), tmux),
            Some(SessionBoundTerminalDeliveryRoute::PlaceholderEdit(
                MessageId::new(9002)
            ))
        );

        let mut watcher_external = inflight_for(tmux, RelayOwnerKind::Watcher, false);
        watcher_external.user_msg_id = 0;
        watcher_external.current_msg_id = 0;
        assert_eq!(
            session_bound_terminal_delivery_route(Some(&watcher_external), tmux),
            Some(SessionBoundTerminalDeliveryRoute::NewMessage)
        );

        let rebind_origin = inflight_for(tmux, RelayOwnerKind::Watcher, true);
        assert_eq!(
            session_bound_terminal_delivery_route(Some(&rebind_origin), tmux),
            Some(SessionBoundTerminalDeliveryRoute::NewMessage)
        );

        assert_eq!(
            session_bound_terminal_delivery_route(Some(&watcher_owned), "AgentDesk-claude-other"),
            None
        );
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
