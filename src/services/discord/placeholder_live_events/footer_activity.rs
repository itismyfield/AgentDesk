use super::*;
use crate::config::ToolActivityDisplay;

impl PlaceholderLiveEvents {
    pub(in crate::services::discord) fn render_footer_status_panel(
        &self,
        channel_id: ChannelId,
        provider: &ProviderKind,
        started_at_unix: i64,
        content: (&str, &str),
    ) -> String {
        let (full_response, indicator) = content;
        let snapshot = self.render_snapshot(channel_id);
        let anchor = self.request_anchor_line(channel_id, &snapshot);
        let time = self.panel_time_line(channel_id, started_at_unix);
        let render = |display| {
            let mut candidate = snapshot.clone();
            candidate.tool_activity_display = display;
            let panel = render_status_panel(candidate, provider, time.clone(), anchor.clone());
            super::super::single_message_panel::compose_footer_status_block(indicator, &panel)
        };
        let summary = render(self.tool_activity_display);
        if full_response.is_empty() || self.tool_activity_display == ToolActivityDisplay::Class {
            return summary;
        }
        let class = render(ToolActivityDisplay::Class);
        // The builder has no current cursor; reserve every possible banner and
        // admit only empty split vectors, which also fit every current suffix.
        let banner_units = common::SESSION_PANEL_LINE_MAX_CHARS * 2
            + format!("\n(최근 대화 {}개를 읽어들였습니다)\n\n", usize::MAX)
                .encode_utf16()
                .count();
        let probe = format!("{}{full_response}", "x".repeat(banner_units));
        let plan = |footer: &str| super::super::formatting::plan_streaming_rollover(&probe, footer);
        let plans = (plan(&summary), plan(&class));
        if plans.0.is_none() && plans.1.is_none() {
            summary
        } else {
            class
        }
    }

    pub(super) fn render_status_panel_with_display(
        &self,
        channel_id: ChannelId,
        provider: &ProviderKind,
        started_at_unix: i64,
        display: ToolActivityDisplay,
    ) -> String {
        let mut snapshot = self.render_snapshot(channel_id);
        // #4093 + #4367 후속: the #3404 live-panel terminal-slot compaction (and
        // its count-change INFO log) is retired — terminal Tasks/Subagents are now
        // hidden from the live panel outright, so nothing is ever compacted out.
        let turn_trigger_line = self.request_anchor_line(channel_id, &snapshot);
        snapshot.tool_activity_display = display;
        let time_line = self.panel_time_line(channel_id, started_at_unix);
        render_status_panel(snapshot, provider, time_line, turn_trigger_line)
    }
    fn render_snapshot(&self, channel_id: ChannelId) -> StatusPanelState {
        self.status_by_channel
            .get(&channel_id)
            .map(|entry| {
                let mut guard = entry
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // #4396 point 2: the #4177 stuck-subagent TTL sweep used to run
                // only at turn-boundary resets, which a long single turn never
                // crosses — sweep on the periodic panel render tick too, so a
                // stuck background subagent slot is bounded by its silence TTL
                // instead of by turn length. Slot activity refreshes the TTL
                // clock (see `SubagentSlot::started_at`), so a live noisy slot
                // is never force-aborted here.
                status_panel::force_abort_stuck_subagent_slots(&mut guard.subagents, panel_now());
                guard.clone()
            })
            .unwrap_or_default()
    }
}
