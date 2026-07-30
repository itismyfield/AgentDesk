//! Footer-only background completion marker rendering.

use super::{TaskCardEvent, TaskNotificationContext};

const FOOTER_ONLY_MARKER_PREFIX: &str = "⚙️ Background complete";
const FOOTER_ONLY_MARKER_DETAIL_LIMIT: usize = 600;

impl TaskCardEvent {
    pub(in crate::services::discord) fn footer_only_marker_content(&self) -> String {
        footer_only_background_marker_content(&self.payload.render(1))
    }
}

/// Project the already-rendered footer card into a bounded lifecycle marker.
pub(super) fn footer_only_background_marker_content(rendered_card: &str) -> String {
    let detail = rendered_card
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with("-#"))
        .filter(|line| {
            ![
                "<task-notification>",
                "<task-id>",
                "<tool-use-id>",
                "<output-file>",
            ]
            .iter()
            .any(|anchor| line.contains(anchor))
        })
        .collect::<Vec<_>>()
        .join("\n");
    if detail.is_empty() {
        return FOOTER_ONLY_MARKER_PREFIX.to_string();
    }
    let content = format!("{FOOTER_ONLY_MARKER_PREFIX}\n{detail}");
    super::super::tui_task_card::clamp_discord_message_content(
        &super::super::tui_task_card::truncate_chars_ascii(
            &content,
            FOOTER_ONLY_MARKER_DETAIL_LIMIT,
        ),
    )
}
