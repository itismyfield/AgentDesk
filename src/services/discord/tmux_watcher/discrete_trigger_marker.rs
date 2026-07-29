//! #4799: watcher-side discrete markers for suppressed machine triggers.
//!
//! Footer-owned background notifications receive a semantic-event-keyed lifecycle
//! marker. Durable-card-owned subagent notifications deliberately remain silent
//! here so the watcher never adds a second surface beside their task card.

use super::*;
use crate::services::discord::task_notification_delivery;
use crate::services::message_outbox::enqueue_lifecycle_notification_best_effort;

struct SuppressedTaskNotificationMarker<'a> {
    channel_id: ChannelId,
    tmux_session_name: &'a str,
    data_start_offset: u64,
    kind: TaskNotificationKind,
    task_notification_event_key: Option<&'a str>,
    background_context: Option<&'a task_notification_delivery::TaskNotificationContext>,
    event_count: usize,
    monitor_entry_keys: &'a [String],
}

impl SuppressedTaskNotificationMarker<'_> {
    fn render(&self) -> Option<(String, &'static str, String)> {
        Some(match self.kind {
            TaskNotificationKind::MonitorAutoTurn => {
                let session_key = self
                    .task_notification_event_key
                    .and_then(|event_key| {
                        task_notification_delivery::task_notification_marker_session_key(
                            self.channel_id,
                            event_key,
                            TaskNotificationKind::MonitorAutoTurn,
                        )
                    })
                    .unwrap_or_else(|| {
                        format!(
                            "monitor_auto_turn:ch:{}:off:{}",
                            self.channel_id.get(),
                            self.data_start_offset
                        )
                    });
                let label = crate::services::provider::parse_provider_and_channel_from_tmux_name(
                    self.tmux_session_name,
                )
                .map(|(_, channel_name)| channel_name)
                .filter(|channel_name| !channel_name.trim().is_empty())
                .unwrap_or_else(|| self.tmux_session_name.to_string());
                let summary = if self.monitor_entry_keys.is_empty() {
                    format!(
                        "🔔 Monitor {}회 처리 · (등록된 모니터 없음)",
                        self.event_count
                    )
                } else {
                    format!(
                        "🔔 Monitor {}회 처리 · 다음 모니터: {{{}}}",
                        self.event_count,
                        self.monitor_entry_keys.join(", ")
                    )
                };
                (
                    session_key,
                    MONITOR_AUTO_TURN_REASON_CODE,
                    format!("{summary} · 대상: {label}"),
                )
            }
            TaskNotificationKind::Background => (
                task_notification_delivery::task_notification_marker_session_key(
                    self.channel_id,
                    self.task_notification_event_key?,
                    TaskNotificationKind::Background,
                )?,
                "lifecycle.background_task_complete",
                self.background_context?.footer_only_marker_content(),
            ),
            // Subagent completions are durable-card owned, so a suppressed watcher
            // terminal must not add a duplicate discrete marker.
            TaskNotificationKind::Subagent => return None,
        })
    }
}

fn enqueue_suppressed_task_notification(
    pg_pool: Option<&sqlx::PgPool>,
    marker: SuppressedTaskNotificationMarker<'_>,
) -> bool {
    let target = format!("channel:{}", marker.channel_id.get());
    let Some((session_key, reason_code, content)) = marker.render() else {
        return false;
    };
    enqueue_lifecycle_notification_best_effort(
        pg_pool,
        target.as_str(),
        Some(session_key.as_str()),
        reason_code,
        content.as_str(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn background_context(summary: &str) -> task_notification_delivery::TaskNotificationContext {
        task_notification_delivery::TaskNotificationContext::from_stream_json(
            &serde_json::json!({
                "type": "system",
                "subtype": "task_notification",
                "task_id": "background-4912",
                "status": "completed",
                "summary": summary,
                "task_notification_kind": "background",
            }),
            &crate::services::session_backend::StreamLineState::new(),
        )
        .expect("background context")
    }

    #[test]
    fn background_marker_includes_summary_and_preserves_event_identity() {
        let channel_id = ChannelId::new(4_912);
        let marker = SuppressedTaskNotificationMarker {
            channel_id,
            tmux_session_name: "AgentDesk-claude-4912",
            data_start_offset: 44,
            kind: TaskNotificationKind::Background,
            task_notification_event_key: Some("event-identity"),
            background_context: Some(&background_context(
                "Background command \"short task\" completed (exit code 0)",
            )),
            event_count: 1,
            monitor_entry_keys: &[],
        }
        .render()
        .expect("footer-owned background completion gets a marker");
        assert_eq!(marker.0, "footer_background:ch:4912:event-identity");
        assert_eq!(marker.1, "lifecycle.background_task_complete");
        assert_eq!(
            marker.2,
            "⚙️ Background complete\n✅ Task completed\n**Background command \"short task\" completed (exit code 0)**"
        );
    }

    #[test]
    fn background_marker_matches_shared_projection_and_bound() {
        let summary = "x".repeat(1_000);
        let context = background_context(&summary);
        let marker = SuppressedTaskNotificationMarker {
            channel_id: ChannelId::new(4_912),
            tmux_session_name: "AgentDesk-claude-4912",
            data_start_offset: 44,
            kind: TaskNotificationKind::Background,
            task_notification_event_key: Some("event-identity"),
            background_context: Some(&context),
            event_count: 1,
            monitor_entry_keys: &[],
        }
        .render()
        .expect("background marker");
        assert_eq!(marker.2, context.footer_only_marker_content());
        assert!(marker.2.chars().count() <= 603);
    }

    #[test]
    fn subagent_marker_remains_suppressed() {
        assert!(
            SuppressedTaskNotificationMarker {
                channel_id: ChannelId::new(4_912),
                tmux_session_name: "AgentDesk-claude-4912",
                data_start_offset: 44,
                kind: TaskNotificationKind::Subagent,
                task_notification_event_key: Some("subagent-event"),
                background_context: Some(&background_context("Agent completed")),
                event_count: 1,
                monitor_entry_keys: &[],
            }
            .render()
            .is_none()
        );
    }
}

pub(super) async fn enqueue_suppressed_machine_trigger_marker(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    tmux_session_name: &str,
    data_start_offset: u64,
    task_notification_kind: Option<TaskNotificationKind>,
    task_notification_context: Option<&task_notification_delivery::TaskNotificationContext>,
    monitor_event_count: usize,
) {
    let monitor_entry_keys: Vec<String> = if matches!(
        task_notification_kind,
        Some(TaskNotificationKind::MonitorAutoTurn)
    ) {
        let store_arc = crate::services::monitoring_store::global_monitoring_store();
        let store = store_arc.lock().await;
        store
            .list(channel_id.get())
            .into_iter()
            .map(|entry| entry.key)
            .collect()
    } else {
        Vec::new()
    };
    let task_notification_event_key = task_notification_context.map(|context| context.event_key());
    let background_context = task_notification_context
        .filter(|context| matches!(context.routing_kind(), TaskNotificationKind::Background));
    let marker_kind = task_notification_kind.filter(|kind| {
        matches!(
            kind,
            TaskNotificationKind::MonitorAutoTurn | TaskNotificationKind::Background
        )
    });
    if let Some(kind) = marker_kind {
        let _ = enqueue_suppressed_task_notification(
            shared.pg_pool.as_ref(),
            SuppressedTaskNotificationMarker {
                channel_id,
                tmux_session_name,
                data_start_offset,
                kind,
                task_notification_event_key,
                background_context,
                event_count: monitor_event_count,
                monitor_entry_keys: &monitor_entry_keys,
            },
        );
    }
}
