use super::*;

pub(super) fn upsert(watcher_provider: ProviderKind, channel_id: ChannelId) {
    crate::services::discord::inflight::o_seed_observation::record_event(
        &watcher_provider,
        channel_id.get(),
        "monitor_upsert_call",
    );
}

pub(super) fn response_outer(
    watcher_provider: ProviderKind,
    channel_id: ChannelId,
    full_response: String,
) {
    if !full_response.is_empty() {
        crate::services::discord::inflight::o_seed_observation::record_parser_response(
            &watcher_provider,
            channel_id.get(),
            &full_response,
        );
    }
}

pub(super) fn initial(
    watcher_provider: ProviderKind,
    channel_id: ChannelId,
    task_notification_kind: Option<TaskNotificationKind>,
) {
    if matches!(
        task_notification_kind,
        Some(TaskNotificationKind::MonitorAutoTurn)
    ) {
        crate::services::discord::inflight::o_seed_observation::record_event(
            &watcher_provider,
            channel_id.get(),
            "monitor_initial_candidate",
        );
    }
}

pub(super) fn start(watcher_provider: ProviderKind, channel_id: ChannelId) {
    crate::services::discord::inflight::o_seed_observation::record_event(
        &watcher_provider,
        channel_id.get(),
        "monitor_start_call",
    );
}

pub(super) fn response_inner(
    watcher_provider: ProviderKind,
    channel_id: ChannelId,
    full_response: String,
) {
    if !full_response.is_empty() {
        crate::services::discord::inflight::o_seed_observation::record_parser_response(
            &watcher_provider,
            channel_id.get(),
            &full_response,
        );
    }
}

pub(super) fn inner(
    watcher_provider: ProviderKind,
    channel_id: ChannelId,
    task_notification_kind: Option<TaskNotificationKind>,
) {
    if matches!(
        task_notification_kind,
        Some(TaskNotificationKind::MonitorAutoTurn)
    ) {
        crate::services::discord::inflight::o_seed_observation::record_event(
            &watcher_provider,
            channel_id.get(),
            "monitor_inner_candidate",
        );
    }
}

pub(super) fn eof(watcher_provider: ProviderKind, channel_id: ChannelId) {
    crate::services::discord::inflight::o_seed_observation::record_event(
        &watcher_provider,
        channel_id.get(),
        "inner_eof",
    );
}

pub(super) fn fresh_idle(watcher_provider: ProviderKind, channel_id: ChannelId) {
    crate::services::discord::inflight::o_seed_observation::record_event(
        &watcher_provider,
        channel_id.get(),
        "fresh_idle_classified",
    );
}

pub(super) fn post_work_idle(watcher_provider: ProviderKind, channel_id: ChannelId) {
    crate::services::discord::inflight::o_seed_observation::record_event(
        &watcher_provider,
        channel_id.get(),
        "post_work_idle_classified",
    );
}
