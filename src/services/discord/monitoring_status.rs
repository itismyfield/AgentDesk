use std::sync::{Arc, OnceLock};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};
use tokio::sync::Mutex;

use super::outbound::{
    DeliveryResult, DiscordOutboundClient, DiscordOutboundMessage, DiscordOutboundPolicy,
    OutboundDeduper, deliver_outbound,
};
use super::{SharedData, health, rate_limit_wait};
use crate::server::routes::dispatches::discord_delivery::{
    DispatchMessagePostError, DispatchMessagePostErrorKind,
};
use crate::server::routes::state::{MonitoringEntry, MonitoringStore, global_monitoring_store};

const RENDER_DEBOUNCE: StdDuration = StdDuration::from_millis(300);

#[cfg_attr(test, allow(dead_code))]
const MONITORING_TTL: chrono::Duration = chrono::Duration::minutes(10);

#[cfg_attr(test, allow(dead_code))]
const SWEEP_INTERVAL: StdDuration = StdDuration::from_secs(60);

#[cfg_attr(test, allow(dead_code))]
static SWEEPER_STARTED: OnceLock<()> = OnceLock::new();

#[derive(Clone)]
struct MonitoringOutboundClient {
    http: Arc<serenity::Http>,
    shared: Option<Arc<SharedData>>,
}

fn monitoring_deduper() -> &'static OutboundDeduper {
    static DEDUPER: OnceLock<OutboundDeduper> = OnceLock::new();
    DEDUPER.get_or_init(OutboundDeduper::new)
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
    let mut message = if rendered_msg_id.is_some() {
        DiscordOutboundMessage::new(channel_id.get().to_string(), content)
    } else {
        DiscordOutboundMessage::new(channel_id.get().to_string(), content).with_correlation(
            format!("monitoring:{}", channel_id.get()),
            format!(
                "monitoring:{}:send:{}",
                channel_id.get(),
                uuid::Uuid::new_v4()
            ),
        )
    };
    if let Some(message_id) = rendered_msg_id {
        message = message.with_edit_message_id(message_id.to_string());
    }

    match deliver_outbound(
        client,
        dedup,
        message,
        DiscordOutboundPolicy::preserve_inline_content(),
    )
    .await
    {
        DeliveryResult::Success { message_id } | DeliveryResult::Fallback { message_id, .. } => {
            parse_delivered_monitoring_message_id(&message_id).map(Some)
        }
        DeliveryResult::Duplicate { message_id } => message_id
            .as_deref()
            .map(parse_delivered_monitoring_message_id)
            .transpose(),
        DeliveryResult::Skipped { .. } => Ok(rendered_msg_id),
        DeliveryResult::PermanentFailure { detail } => Err(detail),
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
    tokio::spawn(async move {
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

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn spawn_expiry_sweeper(
    monitoring: Arc<Mutex<MonitoringStore>>,
    health_registry: Option<Arc<health::HealthRegistry>>,
) {
    if SWEEPER_STARTED.set(()).is_err() {
        return;
    }

    tokio::spawn(async move {
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

#[allow(dead_code)]
pub(in crate::services::discord) async fn render_channel_monitoring(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
) {
    let monitoring = global_monitoring_store();
    if let Err(error) =
        render_channel_monitoring_from_store(http, &monitoring, Some(shared), channel_id).await
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ monitoring status render failed for channel {}: {}",
            channel_id.get(),
            error
        );
    }
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
            if let Err(error) = channel_id
                .delete_message(http, MessageId::new(msg_id))
                .await
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ failed to delete monitoring status msg {} in channel {}: {}",
                    msg_id,
                    channel_id.get(),
                    error
                );
            }
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
            monitoring_deduper(),
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
        monitoring_deduper(),
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
const MONITORING_ACTION_HINT: &str = "   ⤷ /cancel 으로 턴 취소, /restart 로 에이전트 재시작";

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct MockMonitoringOutboundClient {
        posts: StdMutex<Vec<(String, String)>>,
        edits: StdMutex<Vec<(String, String, String)>>,
    }

    impl DiscordOutboundClient for MockMonitoringOutboundClient {
        async fn post_message(
            &self,
            target_channel: &str,
            content: &str,
        ) -> Result<String, DispatchMessagePostError> {
            self.posts
                .lock()
                .unwrap()
                .push((target_channel.to_string(), content.to_string()));
            Ok("9001".to_string())
        }

        async fn edit_message(
            &self,
            target_channel: &str,
            message_id: &str,
            content: &str,
        ) -> Result<String, DispatchMessagePostError> {
            self.edits.lock().unwrap().push((
                target_channel.to_string(),
                message_id.to_string(),
                content.to_string(),
            ));
            Ok(message_id.to_string())
        }
    }

    fn entry(
        key: &str,
        description: &str,
        started_at: &str,
    ) -> Result<MonitoringEntry, chrono::ParseError> {
        let started_at = DateTime::parse_from_rfc3339(started_at)?.with_timezone(&Utc);
        Ok(MonitoringEntry {
            key: key.to_string(),
            description: description.to_string(),
            started_at,
            last_refresh: started_at,
        })
    }

    #[test]
    fn format_empty_monitoring_message_returns_none() {
        assert_eq!(format_monitoring_message(&[]), None);
    }

    #[test]
    fn format_single_monitoring_message() -> Result<(), chrono::ParseError> {
        let entries = vec![entry("one", "터미널 신호 대기", "2026-04-24T01:20:00Z")?];

        assert_eq!(
            format_monitoring_message(&entries),
            Some(
                "👀 모니터링 중: 터미널 신호 대기 (시작 10:20)\n   ⤷ /cancel 으로 턴 취소, /restart 로 에이전트 재시작"
                    .to_string()
            )
        );
        Ok(())
    }

    #[test]
    fn format_multiple_monitoring_messages() -> Result<(), chrono::ParseError> {
        let entries = vec![
            entry("one", "터미널 신호 대기", "2026-04-24T01:20:00Z")?,
            entry("two", "CI 완료 대기", "2026-04-24T02:05:00Z")?,
        ];

        assert_eq!(
            format_monitoring_message(&entries),
            Some(
                "👀 모니터링 중 (2건):\n- 터미널 신호 대기 (10:20)\n- CI 완료 대기 (11:05)\n   ⤷ /cancel 으로 턴 취소, /restart 로 에이전트 재시작"
                    .to_string()
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn deliver_monitoring_status_uses_shared_send_and_edit_contract() {
        let client = MockMonitoringOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let channel_id = ChannelId::new(42);

        let sent = deliver_monitoring_status(&client, &dedup, channel_id, None, "status")
            .await
            .expect("send succeeds");
        let edited = deliver_monitoring_status(&client, &dedup, channel_id, Some(1234), "updated")
            .await
            .expect("edit succeeds");
        let changed_edit =
            deliver_monitoring_status(&client, &dedup, channel_id, Some(1234), "updated again")
                .await
                .expect("changed edit succeeds");
        let reverted_edit =
            deliver_monitoring_status(&client, &dedup, channel_id, Some(1234), "updated")
                .await
                .expect("reverted edit succeeds");

        assert_eq!(sent, Some(9001));
        assert_eq!(edited, Some(1234));
        assert_eq!(changed_edit, Some(1234));
        assert_eq!(reverted_edit, Some(1234));
        assert_eq!(
            client.posts.lock().unwrap().as_slice(),
            &[("42".to_string(), "status".to_string())]
        );
        assert_eq!(
            client.edits.lock().unwrap().as_slice(),
            &[
                ("42".to_string(), "1234".to_string(), "updated".to_string()),
                (
                    "42".to_string(),
                    "1234".to_string(),
                    "updated again".to_string()
                ),
                ("42".to_string(), "1234".to_string(), "updated".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn deliver_monitoring_status_does_not_dedupe_reverted_edit_content() {
        let client = MockMonitoringOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let channel_id = ChannelId::new(42);

        deliver_monitoring_status(&client, &dedup, channel_id, Some(1234), "A")
            .await
            .expect("first edit succeeds");
        deliver_monitoring_status(&client, &dedup, channel_id, Some(1234), "B")
            .await
            .expect("second edit succeeds");
        deliver_monitoring_status(&client, &dedup, channel_id, Some(1234), "A")
            .await
            .expect("reverted edit succeeds");

        assert_eq!(
            client.edits.lock().unwrap().as_slice(),
            &[
                ("42".to_string(), "1234".to_string(), "A".to_string()),
                ("42".to_string(), "1234".to_string(), "B".to_string()),
                ("42".to_string(), "1234".to_string(), "A".to_string())
            ]
        );
    }
}
