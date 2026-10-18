use super::*;
use crate::config::ToolActivityDisplay;

impl PlaceholderLiveEvents {
    pub(in crate::services::discord) fn render_footer_status_panel(
        &self,
        channel_id: ChannelId,
        provider: &ProviderKind,
        started_at_unix: i64,
        full_response: &str,
    ) -> String {
        // Body-bearing footers must reserve exactly the pre-summary body budget.
        let display = if full_response.is_empty() {
            self.tool_activity_display
        } else {
            ToolActivityDisplay::Class
        };
        self.render_status_panel_with_display(channel_id, provider, started_at_unix, display)
    }

    pub(super) fn render_status_panel_with_display(
        &self,
        channel_id: ChannelId,
        provider: &ProviderKind,
        started_at_unix: i64,
        display: ToolActivityDisplay,
    ) -> String {
        let mut snapshot = self
            .status_by_channel
            .get(&channel_id)
            .map(|entry| {
                let mut guard = entry
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // Preserve the existing render-time sweep of silent background agents.
                status_panel::force_abort_stuck_subagent_slots(&mut guard.subagents, panel_now());
                guard.clone()
            })
            .unwrap_or_default();
        let turn_trigger_line = self.request_anchor_line(channel_id, &snapshot);
        snapshot.tool_activity_display = display;
        let time_line = self.panel_time_line(channel_id, started_at_unix);
        render_status_panel(snapshot, provider, time_line, turn_trigger_line)
    }
}
