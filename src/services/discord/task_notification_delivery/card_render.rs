//! Completion-metadata rendering for terminal task cards.

use super::*;

impl TaskCardPayload {
    pub(super) fn render(&self, update_count: u64) -> String {
        match self {
            Self::Task(note) => {
                super::super::tui_task_card::format_task_notification_card(note, update_count)
            }
            Self::Subagent(card) if update_count > 1 => {
                format!("{card}\n\n-# {update_count} updates")
            }
            Self::Subagent(card) => card.clone(),
        }
    }

    pub(super) fn render_with_completion_metadata(
        &self,
        update_count: u64,
        metadata: &super::super::completion_footer_metadata::CompletionFooterMetadata,
    ) -> String {
        let rendered = self.render(update_count);
        if !matches!(self, Self::Task(_)) {
            return rendered;
        }
        let lines = metadata.subtext_lines();
        if lines.is_empty() {
            return rendered;
        }
        let mut rendered = rendered.trim_end().to_string();
        rendered.push_str("\n\n");
        rendered.push_str(&lines.join("\n"));
        super::super::tui_task_card::clamp_discord_message_content(&rendered)
    }
}
