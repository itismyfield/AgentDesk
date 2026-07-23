//! #4799: watcher-side discrete markers for suppressed machine triggers.
//!
//! Footer-owned background notifications receive a semantic-event-keyed lifecycle
//! marker. Durable-card-owned subagent notifications deliberately remain silent
//! here so the watcher never adds a second surface beside their task card.

use super::*;
use crate::services::discord::task_notification_delivery;

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
    let footer_only_event_key =
        task_notification_context.and_then(|context| context.footer_only_marker_event_key());
    let marker_kind = task_notification_kind.filter(|kind| {
        matches!(
            kind,
            TaskNotificationKind::MonitorAutoTurn | TaskNotificationKind::Background
        )
    });
    if let Some(kind) = marker_kind {
        let _ = enqueue_suppressed_task_notification(
            shared.pg_pool.as_ref(),
            channel_id,
            tmux_session_name,
            data_start_offset,
            kind,
            footer_only_event_key,
            monitor_event_count,
            &monitor_entry_keys,
        );
    }
}
