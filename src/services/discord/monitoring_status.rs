use std::sync::{Arc, OnceLock};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, CreateMessage, EditMessage, MessageId};
use tokio::sync::Mutex;

use super::{SharedData, health, rate_limit_wait};
use crate::server::routes::state::{MonitoringEntry, MonitoringStore, global_monitoring_store};

const RENDER_DEBOUNCE: StdDuration = StdDuration::from_millis(300);
const MONITORING_TTL: chrono::Duration = chrono::Duration::minutes(10);
const SWEEP_INTERVAL: StdDuration = StdDuration::from_secs(60);

static SWEEPER_STARTED: OnceLock<()> = OnceLock::new();

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

    if let Some(msg_id) = rendered_msg_id {
        wait_if_shared(shared, channel_id).await;
        match channel_id
            .edit_message(
                http,
                MessageId::new(msg_id),
                EditMessage::new().content(&content),
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

    wait_if_shared(shared, channel_id).await;
    let message = channel_id
        .send_message(http, CreateMessage::new().content(content))
        .await
        .map_err(|error| {
            format!(
                "failed to send monitoring status message in channel {}: {}",
                channel_id.get(),
                error
            )
        })?;
    let mut store = monitoring.lock().await;
    store.set_rendered_msg(channel_id.get(), Some(message.id.get()));
    Ok(())
}

async fn wait_if_shared(shared: Option<&Arc<SharedData>>, channel_id: ChannelId) {
    if let Some(shared) = shared {
        rate_limit_wait(shared, channel_id).await;
    }
}

pub(crate) fn format_monitoring_message(entries: &[MonitoringEntry]) -> Option<String> {
    match entries {
        [] => None,
        [entry] => Some(format!(
            "👁️ 모니터링 중: {} (시작 {})",
            entry.description,
            format_kst_hhmm(entry.started_at)
        )),
        entries => {
            let mut lines = Vec::with_capacity(entries.len() + 1);
            lines.push(format!("👁️ 모니터링 중 ({}건):", entries.len()));
            lines.extend(entries.iter().map(|entry| {
                format!(
                    "- {} ({})",
                    entry.description,
                    format_kst_hhmm(entry.started_at)
                )
            }));
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
            Some("👁️ 모니터링 중: 터미널 신호 대기 (시작 10:20)".to_string())
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
                "👁️ 모니터링 중 (2건):\n- 터미널 신호 대기 (10:20)\n- CI 완료 대기 (11:05)"
                    .to_string()
            )
        );
        Ok(())
    }
}
