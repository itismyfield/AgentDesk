//! #4799: watcher-side discrete markers for suppressed machine triggers.
//!
//! Footer-owned background notifications receive a semantic-event-keyed lifecycle
//! marker. Durable-card-owned subagent notifications deliberately remain silent
//! here so the watcher never adds a second surface beside their task card.

use super::*;
use crate::services::discord::task_notification_delivery;
// Reuse the canonical reason code rather than re-declaring the literal: it lives
// next to its `.deferred` sibling in tmux_kill_policy, and two copies drift.
use crate::services::discord::tmux::tmux_kill_policy::MONITOR_AUTO_TURN_REASON_CODE;
use crate::services::message_outbox::enqueue_lifecycle_notification_best_effort;

struct SuppressedTaskNotificationMarker<'a> {
    channel_id: ChannelId,
    tmux_session_name: &'a str,
    data_start_offset: u64,
    kind: TaskNotificationKind,
    background_event: Option<&'a task_notification_delivery::TaskCardEvent>,
    event_count: usize,
    monitor_entry_keys: &'a [String],
}

impl SuppressedTaskNotificationMarker<'_> {
    fn render(&self) -> Option<(String, &'static str, String)> {
        Some(match self.kind {
            TaskNotificationKind::MonitorAutoTurn => {
                let session_key = format!(
                    "monitor_auto_turn:ch:{}:off:{}",
                    self.channel_id.get(),
                    self.data_start_offset
                );
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
            TaskNotificationKind::Background => {
                let event = self.background_event?;
                (
                    task_notification_delivery::footer_background_marker_session_key(
                        self.channel_id,
                        event.event_key(),
                    ),
                    "lifecycle.background_task_complete",
                    event.footer_only_marker_content(),
                )
            }
            TaskNotificationKind::Subagent => return None,
        })
    }
}

async fn enqueue_suppressed_task_notification(
    pg_pool: Option<&sqlx::PgPool>,
    marker: SuppressedTaskNotificationMarker<'_>,
) -> bool {
    let target = format!("channel:{}", marker.channel_id.get());
    let Some((session_key, reason_code, content)) = marker.render() else {
        return false;
    };
    if let Some(event) = marker
        .background_event
        .filter(|_| matches!(marker.kind, TaskNotificationKind::Background))
    {
        return match event.enqueue_footer_only_marker(pg_pool).await {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(channel_id = marker.channel_id.get(), %error, "background completion outbox handoff failed");
                false
            }
        };
    }
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

    fn background_event(
        status: &str,
        summary: Option<&str>,
    ) -> task_notification_delivery::TaskCardEvent {
        task_notification_delivery::TaskCardEvent::from_task_prompt(
            4912,
            "claude",
            "AgentDesk-claude-4912",
            &format!(
                "<task-notification><task-id>event-identity</task-id><status>{status}</status><summary>{}</summary></task-notification>",
                summary.unwrap_or(""),
            ),
        )
    }

    #[test]
    fn background_marker_includes_summary_and_preserves_event_identity() {
        let channel_id = ChannelId::new(4_912);
        let event = background_event(
            "completed",
            Some("Background command \"short task\" completed (exit code 0)"),
        );
        let marker = SuppressedTaskNotificationMarker {
            channel_id,
            tmux_session_name: "AgentDesk-claude-4912",
            data_start_offset: 44,
            kind: TaskNotificationKind::Background,
            background_event: Some(&event),
            event_count: 1,
            monitor_entry_keys: &[],
        }
        .render()
        .expect("footer-owned background completion gets a marker");
        assert_eq!(
            marker.0,
            format!("footer_background:ch:4912:{}", event.event_key())
        );
        assert_eq!(marker.1, "lifecycle.background_task_complete");
        assert_eq!(
            marker.2,
            "-# ✅ Background command \"short task\" completed (exit code 0)"
        );
    }

    #[test]
    fn background_marker_omits_missing_summary() {
        let event = background_event("failed", None);
        let marker = SuppressedTaskNotificationMarker {
            channel_id: ChannelId::new(4_912),
            tmux_session_name: "AgentDesk-claude-4912",
            data_start_offset: 44,
            kind: TaskNotificationKind::Background,
            background_event: Some(&event),
            event_count: 1,
            monitor_entry_keys: &[],
        }
        .render()
        .expect("background marker");
        assert_eq!(marker.2, "-# ❌ Task failed");
    }

    #[tokio::test]
    async fn background_marker_provider_outbox_survives_restart_replay_pg() {
        use sqlx::Row;
        let Some(db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "background_marker_provider_replay",
            "background marker outbox contract",
        )
        .await
        else {
            return;
        };
        let pool = db.connect_and_migrate().await;
        let event = background_event("completed", Some("Background command done"));
        let marker = || SuppressedTaskNotificationMarker {
            channel_id: ChannelId::new(4912),
            tmux_session_name: "AgentDesk-claude-4912",
            data_start_offset: 44,
            kind: TaskNotificationKind::Background,
            background_event: Some(&event),
            event_count: 1,
            monitor_entry_keys: &[],
        };
        let key = marker().render().unwrap().0;
        // A pending old-format row is upgraded on observation, preserving its identity.
        crate::services::message_outbox::enqueue_lifecycle_notification_pg(
            &pool,
            "channel:4912",
            Some(&key),
            "lifecycle.background_task_complete",
            "old marker",
        )
        .await
        .unwrap();
        let (prompt_id, watcher_enqueued) = tokio::join!(
            event.enqueue_footer_only_marker(Some(&pool)),
            enqueue_suppressed_task_notification(Some(&pool), marker()),
        );
        assert!(watcher_enqueued);
        let id = prompt_id.unwrap();
        let row =
            sqlx::query("SELECT bot, content, dedupe_expires_at FROM message_outbox WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.get::<String, _>("bot"), "claude");
        assert_eq!(
            row.get::<String, _>("content"),
            "-# ✅ Background command done"
        );
        assert!(
            row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("dedupe_expires_at")
                .is_none()
        );
        sqlx::query("UPDATE message_outbox SET status = 'sent', created_at = NOW() - INTERVAL '2 days' WHERE id = $1")
            .bind(id).execute(&pool).await.unwrap();
        assert_eq!(
            event.enqueue_footer_only_marker(Some(&pool)).await.unwrap(),
            id
        );
        assert!(enqueue_suppressed_task_notification(Some(&pool), marker()).await);
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox WHERE session_key = $1")
                .bind(&key)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            count, 1,
            "both producers and aged replay own one durable row"
        );
        assert!(event.enqueue_footer_only_marker(None).await.is_err());
        pool.close().await;
        db.drop().await;
    }

    #[test]
    fn subagent_marker_remains_suppressed() {
        assert!(
            SuppressedTaskNotificationMarker {
                channel_id: ChannelId::new(4_912),
                tmux_session_name: "AgentDesk-claude-4912",
                data_start_offset: 44,
                kind: TaskNotificationKind::Subagent,
                background_event: None,
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
    let background_event = task_notification_context
        .filter(|context| context.footer_only_marker_event_key().is_some())
        .map(|context| {
            context.to_event(
                channel_id.get(),
                shared.provider.as_str(),
                tmux_session_name,
            )
        });
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
                background_event: background_event.as_ref(),
                event_count: monitor_event_count,
                monitor_entry_keys: &monitor_entry_keys,
            },
        )
        .await;
    }
}
