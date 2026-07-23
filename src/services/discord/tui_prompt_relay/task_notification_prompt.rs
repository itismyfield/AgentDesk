//! Prompt-side task-card observation and bot selection (#4055).
//!
//! The giant relay root delegates task-specific durable state here. The
//! terminal sink remains the promotion authority whenever a footer-only event
//! later produces user-visible assistant text.

use super::*;

pub(super) struct TaskPromptObservation {
    start_anchored: bool,
    event: Option<super::super::task_notification_delivery::TaskCardEvent>,
}

pub(super) struct TaskPromptGate {
    pub(super) notify_http: Arc<serenity::Http>,
    pub(super) card_anchor: Option<MessageId>,
}

/// Capture and bridge task semantics before any Discord bot lookup. A bot
/// outage therefore cannot erase the durable event/context that the sink can
/// promote on response delivery.
pub(super) fn observe(
    shared: &Arc<SharedData>,
    prompt: &ObservedTuiPrompt,
    channel_id: ChannelId,
    injected_class: InjectedPromptClass,
) -> TaskPromptObservation {
    let start_anchored = matches!(injected_class, InjectedPromptClass::TaskNotificationEvent)
        && injected_prompt_policy::is_start_anchored_task_notification(&prompt.prompt);
    if start_anchored {
        shared
            .ui
            .placeholder_live_events
            .bridge_task_notification_xml(channel_id, &prompt.prompt);
    }
    let event = if matches!(injected_class, InjectedPromptClass::TaskNotificationEvent) {
        Some(
            super::super::task_notification_delivery::TaskCardEvent::from_task_prompt_with_source_event_id(
                channel_id.get(),
                &prompt.provider,
                &prompt.tmux_session_name,
                &prompt.prompt,
                prompt.source_event_id.as_deref(),
            ),
        )
    } else if injected_class.is_subagent_notification_event() {
        Some(
            super::super::task_notification_delivery::TaskCardEvent::from_subagent_prompt_with_source_event_id(
                channel_id.get(),
                &prompt.provider,
                &prompt.tmux_session_name,
                &prompt.prompt,
                prompt.source_event_id.as_deref(),
            ),
        )
    } else {
        None
    };
    TaskPromptObservation {
        start_anchored,
        event,
    }
}

pub(super) async fn resolve_gate(
    shared: &Arc<SharedData>,
    prompt: &ObservedTuiPrompt,
    channel_id: ChannelId,
    injected_class: InjectedPromptClass,
    lease: &ExternalInputRelayLease,
    observation: TaskPromptObservation,
) -> Option<TaskPromptGate> {
    let Some(event) = observation.event.as_ref() else {
        return legacy_notify_gate(shared, prompt, channel_id, lease).await;
    };

    let footer_only = observation.start_anchored
        && event.supports_footer_deferral()
        && matches!(
            shared
                .ui
                .placeholder_live_events
                .task_notification_display_policy_for_mode(
                    channel_id,
                    &prompt.prompt,
                    super::super::single_message_panel::footer_mode_enabled(
                        super::super::single_message_panel::enabled(),
                        shared.ui.status_panel_v2_enabled,
                    ),
                ),
            super::super::placeholder_live_events::TaskCompletionDisplayPolicy::FooterOnly
        );
    if footer_only {
        if let Err(error) = super::super::task_notification_delivery::record_footer_only(
            shared.pg_pool.as_ref(),
            event,
        )
        .await
        {
            tracing::warn!(
                provider = %prompt.provider,
                channel_id = channel_id.get(),
                error = %error,
                "failed to persist footer-owned task notification"
            );
            return None;
        }
        enqueue_footer_only_background_marker(shared, channel_id, event);
        clear_observed_external_turn_lease_if_current(prompt, channel_id, lease);
        return None;
    }

    let registry = shared.health_registry();
    let notify_role = super::super::bot_role::UtilityBotRole::Notify;
    let notify_http = if let Some(registry) = registry.as_ref() {
        match super::super::health::resolve_utility_bot_http(registry.as_ref(), notify_role).await {
            Ok(http) => Some(http),
            Err((status, body)) => {
                tracing::warn!(
                    provider = %prompt.provider,
                    channel_id = channel_id.get(),
                    status = %status,
                    body = %body,
                    "notify bot unavailable while resolving TUI task card"
                );
                None
            }
        }
    } else {
        None
    };
    let provider_http = shared.serenity_http_or_token_fallback();
    let clients = super::super::task_notification_delivery::CardDeliveryClients::new(
        notify_http
            .map(|http| {
                super::super::task_notification_delivery::CardBot::new(notify_role.alias(), http)
            })
            .into_iter()
            .chain(provider_http.map(|http| {
                super::super::task_notification_delivery::CardBot::new(
                    super::super::task_notification_delivery::provider_bot_key(&prompt.provider),
                    http,
                )
            })),
    );
    let transport =
        super::super::task_notification_delivery::DiscordTaskCardTransport::new(shared.clone());
    let outcome = match super::super::task_notification_delivery::ensure_card_with_shared(
        shared.as_ref(),
        &clients,
        &transport,
        event,
        super::super::task_notification_delivery::EnsureIntent::Observation,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(
                provider = %prompt.provider,
                channel_id = channel_id.get(),
                error = %error,
                "task-notification card authority could not confirm a card"
            );
            return None;
        }
    };
    shared
        .ui
        .placeholder_live_events
        .claim_terminal_slot_for_card(channel_id, event.kind(), event.tool_use_id());
    if injected_class.is_subagent_notification_event()
        || outcome.disposition != super::super::task_notification_delivery::CardDisposition::Created
    {
        clear_observed_external_turn_lease_if_current(prompt, channel_id, lease);
        return None;
    }
    let Some(http) = clients.by_key(&outcome.bot_key).map(|bot| bot.http.clone()) else {
        tracing::warn!(
            provider = %prompt.provider,
            channel_id = channel_id.get(),
            task_card_bot = %outcome.bot_key,
            "confirmed task card's pinned bot disappeared from the local client set"
        );
        return None;
    };
    Some(TaskPromptGate {
        notify_http: http,
        card_anchor: Some(MessageId::new(outcome.message_id)),
    })
}

/// Prompt observation remains a producer when the watcher cannot reach the
/// terminal frame. It shares the watcher key, so observing the same semantic
/// event through both paths still produces one outbox lifecycle notice.
fn enqueue_footer_only_background_marker(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    event: &super::super::task_notification_delivery::TaskCardEvent,
) {
    let target = format!("channel:{}", channel_id.get());
    let session_key =
        super::super::tmux::footer_background_marker_session_key(channel_id, event.event_key());
    let _ = crate::services::message_outbox::enqueue_lifecycle_notification_best_effort(
        shared.pg_pool.as_ref(),
        target.as_str(),
        Some(session_key.as_str()),
        "lifecycle.background_task_complete",
        "⚙️ Background complete",
    );
}

async fn legacy_notify_gate(
    shared: &Arc<SharedData>,
    prompt: &ObservedTuiPrompt,
    channel_id: ChannelId,
    lease: &ExternalInputRelayLease,
) -> Option<TaskPromptGate> {
    let Some(registry) = shared.health_registry() else {
        tracing::warn!(
            provider = %prompt.provider,
            channel_id = channel_id.get(),
            turn_id = lease.turn_id.as_deref().unwrap_or(""),
            session_key = lease.session_key.as_deref().unwrap_or(""),
            relay_owner = lease.relay_owner.as_str(),
            runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
            "skipping SSH-direct TUI prompt notify; health registry unavailable"
        );
        return None;
    };
    let notify_http = match super::super::health::resolve_utility_bot_http(
        registry.as_ref(),
        super::super::bot_role::UtilityBotRole::Notify,
    )
    .await
    {
        Ok(http) => http,
        Err((status, body)) => {
            tracing::warn!(
                provider = %prompt.provider,
                channel_id = channel_id.get(),
                turn_id = lease.turn_id.as_deref().unwrap_or(""),
                session_key = lease.session_key.as_deref().unwrap_or(""),
                relay_owner = lease.relay_owner.as_str(),
                runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                status = %status,
                body = %body,
                "skipping SSH-direct TUI prompt notify; notify bot unavailable"
            );
            return None;
        }
    };
    Some(TaskPromptGate {
        notify_http,
        card_anchor: None,
    })
}
