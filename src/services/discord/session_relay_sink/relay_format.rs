//! Session-bound relay display-text derivation, moved verbatim from the sink
//! delivery path. The RAW pre-format body stays in the parent (#4081 delivery
//! fingerprints record RAW, never this display text).

use super::super::formatting;
use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::provider::ProviderKind;

pub(super) fn session_bound_relay_text(
    shared: &super::super::SharedData,
    provider: &ProviderKind,
    raw_response_text: &str,
    task_notification_kind: Option<&TaskNotificationKind>,
) -> String {
    let formatted = if shared.ui.status_panel_v2_enabled {
        formatting::format_for_discord_with_status_panel(raw_response_text, provider)
    } else {
        formatting::format_for_discord_with_provider(raw_response_text, provider)
    };
    if matches!(
        task_notification_kind,
        Some(TaskNotificationKind::MonitorAutoTurn)
    ) {
        super::super::prepend_monitor_auto_turn_origin(&formatted)
    } else {
        formatted
    }
}
