//! Footer-only background completion marker rendering.

use super::{TaskCardEvent, TaskNotificationContext};

const FOOTER_ONLY_MARKER_PREFIX: &str = "⚙️ Background complete";
const FOOTER_ONLY_MARKER_DETAIL_LIMIT: usize = 600;

impl TaskNotificationContext {
    pub(in crate::services::discord) fn footer_only_marker_content(&self) -> String {
        footer_only_background_marker_content(&self.to_event(0, "", "").payload.render(1))
    }
}

impl TaskCardEvent {
    pub(in crate::services::discord) fn rendered_footer_only_content(&self) -> (String, String) {
        let rendered_card = self.payload.render(1);
        let marker = footer_only_background_marker_content(&rendered_card);
        (rendered_card, marker)
    }
}

/// Project the already-rendered footer card into a bounded lifecycle marker.
fn footer_only_background_marker_content(rendered_card: &str) -> String {
    // Any harness control anchor invalidates the whole detail. Per-line redaction
    // could expose the value between a private opening and closing tag.
    if crate::services::provider_output_guard::contains_provider_control_anchor(rendered_card) {
        return FOOTER_ONLY_MARKER_PREFIX.to_string();
    }
    // Card metadata (`-#`) includes internal task identity; the lifecycle notice
    // needs only the human-facing summary and preview.
    let detail = rendered_card
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("-#"))
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
