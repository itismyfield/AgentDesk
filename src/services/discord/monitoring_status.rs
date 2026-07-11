use std::sync::{Arc, OnceLock};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};
use tokio::sync::Mutex;

use super::outbound::delivery::{deliver_outbound, first_raw_message_id};
use super::outbound::message::{OutboundOperation, OutboundTarget};
use super::outbound::{
    DeliveryResult, DiscordOutboundClient, DiscordOutboundMessage, DiscordOutboundPolicy,
    OutboundDeduper, shared_outbound_deduper,
};
use super::{SharedData, health, rate_limit_wait};
use crate::services::dispatches::discord_delivery::{
    DispatchMessagePostError, DispatchMessagePostErrorKind,
};
use crate::services::monitoring_store::{MonitoringEntry, MonitoringStore};

const RENDER_DEBOUNCE: StdDuration = StdDuration::from_millis(300);

const MONITORING_TTL: chrono::Duration = chrono::Duration::minutes(10);

const SWEEP_INTERVAL: StdDuration = StdDuration::from_secs(60);

static SWEEPER_STARTED: OnceLock<()> = OnceLock::new();

#[derive(Clone)]
struct MonitoringOutboundClient {
    http: Arc<serenity::Http>,
    shared: Option<Arc<SharedData>>,
}

impl DiscordOutboundClient for MonitoringOutboundClient {
    async fn post_message(
        &self,
        target_channel: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError> {
        let channel_id = parse_monitoring_channel_id(target_channel)?;
        wait_if_shared(self.shared.as_ref(), channel_id).await;
        channel_id
            .send_message(&self.http, serenity::CreateMessage::new().content(content))
            .await
            .map(|message| message.id.get().to_string())
            .map_err(monitoring_post_error)
    }

    async fn edit_message(
        &self,
        target_channel: &str,
        message_id: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError> {
        let channel_id = parse_monitoring_channel_id(target_channel)?;
        let message_id = message_id
            .parse::<u64>()
            .map(MessageId::new)
            .map_err(|error| {
                DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    format!("invalid monitoring message id {message_id}: {error}"),
                )
            })?;
        wait_if_shared(self.shared.as_ref(), channel_id).await;
        channel_id
            .edit_message(
                &self.http,
                message_id,
                serenity::EditMessage::new().content(content),
            )
            .await
            .map(|message| message.id.get().to_string())
            .map_err(monitoring_post_error)
    }
}

fn parse_monitoring_channel_id(raw: &str) -> Result<ChannelId, DispatchMessagePostError> {
    raw.parse::<u64>().map(ChannelId::new).map_err(|error| {
        DispatchMessagePostError::new(
            DispatchMessagePostErrorKind::Other,
            format!("invalid monitoring channel id {raw}: {error}"),
        )
    })
}

fn monitoring_post_error(error: serenity::Error) -> DispatchMessagePostError {
    let detail = error.to_string();
    let lowered = detail.to_ascii_lowercase();
    let kind = if detail.contains("BASE_TYPE_MAX_LENGTH")
        || lowered.contains("2000 or fewer in length")
        || lowered.contains("length")
    {
        DispatchMessagePostErrorKind::MessageTooLong
    } else {
        DispatchMessagePostErrorKind::Other
    };
    DispatchMessagePostError::new(kind, detail)
}

async fn deliver_monitoring_status<C: DiscordOutboundClient>(
    client: &C,
    dedup: &OutboundDeduper,
    channel_id: ChannelId,
    rendered_msg_id: Option<u64>,
    content: &str,
) -> Result<Option<u64>, String> {
    let mut policy = DiscordOutboundPolicy::preserve_inline_content();
    let message = if let Some(message_id) = rendered_msg_id {
        policy = policy.without_idempotency();
        DiscordOutboundMessage::new(
            format!("monitoring:no-idempotency:{}", channel_id.get()),
            "monitoring:no-idempotency:edit",
            content,
            OutboundTarget::Channel(channel_id),
            policy,
        )
        .with_operation(OutboundOperation::Edit {
            message_id: MessageId::new(message_id),
        })
    } else {
        DiscordOutboundMessage::new(
            format!("monitoring:{}", channel_id.get()),
            format!(
                "monitoring:{}:send:{}",
                channel_id.get(),
                uuid::Uuid::new_v4()
            ),
            content,
            OutboundTarget::Channel(channel_id),
            policy,
        )
    };

    match deliver_outbound(client, dedup, message, None).await {
        DeliveryResult::Sent { messages, .. } | DeliveryResult::Fallback { messages, .. } => {
            first_raw_message_id(&messages)
                .as_deref()
                .ok_or_else(|| "monitoring delivery returned no message id".to_string())
                .and_then(parse_delivered_monitoring_message_id)
                .map(Some)
        }
        DeliveryResult::Duplicate {
            existing_messages, ..
        } => first_raw_message_id(&existing_messages)
            .as_deref()
            .map(parse_delivered_monitoring_message_id)
            .transpose(),
        DeliveryResult::Skip { .. } => Ok(rendered_msg_id),
        DeliveryResult::TransientFailure { reason }
        | DeliveryResult::PermanentFailure { reason }
        | DeliveryResult::ConfirmedMissing { reason } => Err(reason),
    }
}

fn parse_delivered_monitoring_message_id(message_id: &str) -> Result<u64, String> {
    message_id
        .parse::<u64>()
        .map_err(|error| format!("invalid monitoring delivery message id {message_id}: {error}"))
}

pub(crate) fn schedule_render_channel(
    monitoring: Arc<Mutex<MonitoringStore>>,
    health_registry: Option<Arc<health::HealthRegistry>>,
    channel_id: ChannelId,
) {
    super::task_supervisor::spawn_observed("monitoring_status_render_channel", async move {
        let version = {
            let mut store = monitoring.lock().await;
            store.next_render_version(channel_id.get())
        };

        tokio::time::sleep(RENDER_DEBOUNCE).await;

        let should_render = {
            let store = monitoring.lock().await;
            store.is_latest_render_version(channel_id.get(), version)
        };
        if !should_render {
            return;
        }

        if let Err(error) = render_channel_monitoring_from_registry(
            &monitoring,
            health_registry.as_ref(),
            channel_id,
        )
        .await
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ monitoring status render failed for channel {}: {}",
                channel_id.get(),
                error
            );
        }
    });
}

pub(crate) fn spawn_expiry_sweeper(
    monitoring: Arc<Mutex<MonitoringStore>>,
    health_registry: Option<Arc<health::HealthRegistry>>,
) {
    if SWEEPER_STARTED.set(()).is_err() {
        return;
    }

    super::task_supervisor::spawn_observed("monitoring_status_expiry_sweeper", async move {
        let mut interval = tokio::time::interval(SWEEP_INTERVAL);
        loop {
            interval.tick().await;
            let affected_channels = {
                let mut store = monitoring.lock().await;
                store.sweep_expired_affected(MONITORING_TTL)
            };

            for channel_id in affected_channels {
                schedule_render_channel(
                    monitoring.clone(),
                    health_registry.clone(),
                    ChannelId::new(channel_id),
                );
            }
        }
    });
}

async fn render_channel_monitoring_from_registry(
    monitoring: &Arc<Mutex<MonitoringStore>>,
    health_registry: Option<&Arc<health::HealthRegistry>>,
    channel_id: ChannelId,
) -> Result<(), String> {
    let Some(registry) = health_registry else {
        return Ok(());
    };
    let http = resolve_status_http(registry).await?;
    render_channel_monitoring_from_store(&http, monitoring, None, channel_id).await
}

async fn resolve_status_http(
    registry: &Arc<health::HealthRegistry>,
) -> Result<Arc<serenity::Http>, String> {
    if let Ok(http) = health::resolve_bot_http(registry, "notify").await {
        return Ok(http);
    }
    health::resolve_bot_http(registry, "announce")
        .await
        .map_err(|(_, body)| body)
}

async fn render_channel_monitoring_from_store(
    http: &Arc<serenity::Http>,
    monitoring: &Arc<Mutex<MonitoringStore>>,
    shared: Option<&Arc<SharedData>>,
    channel_id: ChannelId,
) -> Result<(), String> {
    let (entries, rendered_msg_id) = {
        let store = monitoring.lock().await;
        (
            store.list(channel_id.get()),
            store.get_rendered_msg(channel_id.get()),
        )
    };

    let Some(content) = format_monitoring_message(&entries) else {
        if let Some(msg_id) = rendered_msg_id {
            wait_if_shared(shared, channel_id).await;
            let result = channel_id
                .delete_message(http, MessageId::new(msg_id))
                .await;
            if let Err(error) = &result {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ failed to delete monitoring status msg {} in channel {}: {}",
                    msg_id,
                    channel_id.get(),
                    error
                );
            }
            // #3607: monitoring status messages are not provider-scoped.
            crate::services::observability::emit_relay_delete_result(
                "",
                channel_id.get(),
                msg_id,
                "monitoring_status_clear",
                "delete_nonterminal",
                &result,
            );
            let mut store = monitoring.lock().await;
            store.set_rendered_msg(channel_id.get(), None);
        }
        return Ok(());
    };

    let outbound_client = MonitoringOutboundClient {
        http: http.clone(),
        shared: shared.cloned(),
    };

    if let Some(msg_id) = rendered_msg_id {
        match deliver_monitoring_status(
            &outbound_client,
            shared_outbound_deduper(),
            channel_id,
            Some(msg_id),
            &content,
        )
        .await
        {
            Ok(_) => return Ok(()),
            Err(error) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ failed to edit monitoring status msg {} in channel {}: {}",
                    msg_id,
                    channel_id.get(),
                    error
                );
                let mut store = monitoring.lock().await;
                store.set_rendered_msg(channel_id.get(), None);
            }
        }
    }

    let message_id = deliver_monitoring_status(
        &outbound_client,
        shared_outbound_deduper(),
        channel_id,
        None,
        &content,
    )
    .await?
    .ok_or_else(|| {
        format!(
            "monitoring status delivery skipped for channel {}",
            channel_id.get()
        )
    })?;
    let mut store = monitoring.lock().await;
    store.set_rendered_msg(channel_id.get(), Some(message_id));
    Ok(())
}

async fn wait_if_shared(shared: Option<&Arc<SharedData>>, channel_id: ChannelId) {
    if let Some(shared) = shared {
        rate_limit_wait(shared, channel_id).await;
    }
}

/// Hint line appended to monitoring banners so users know which slash
/// commands address the underlying turn. Kept as a constant so the
/// single-entry and multi-entry branches stay in sync.
const MONITORING_ACTION_HINT: &str = "   ⤷ /stop 으로 턴 취소, /restart 로 에이전트 재시작";

pub(crate) fn format_monitoring_message(entries: &[MonitoringEntry]) -> Option<String> {
    match entries {
        [] => None,
        [entry] => Some(format!(
            "👀 모니터링 중: {} (시작 {})\n{}",
            entry.description,
            format_kst_hhmm(entry.started_at),
            MONITORING_ACTION_HINT,
        )),
        entries => {
            let mut lines = Vec::with_capacity(entries.len() + 2);
            lines.push(format!("👀 모니터링 중 ({}건):", entries.len()));
            lines.extend(entries.iter().map(|entry| {
                format!(
                    "- {} ({})",
                    entry.description,
                    format_kst_hhmm(entry.started_at)
                )
            }));
            lines.push(MONITORING_ACTION_HINT.to_string());
            Some(lines.join("\n"))
        }
    }
}

fn format_kst_hhmm(value: DateTime<Utc>) -> String {
    value
        .with_timezone(&chrono_tz::Asia::Seoul)
        .format("%H:%M")
        .to_string()
}
