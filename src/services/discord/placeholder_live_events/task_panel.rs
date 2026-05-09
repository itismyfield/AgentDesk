use super::common::{
    TASK_PANEL_LINE_MAX_CHARS, escape_status_panel_markdown, first_content_line, truncate_chars,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TaskPanelSnapshot {
    pub(super) dispatch_id: String,
    pub(super) card_id: Option<String>,
    pub(super) dispatch_type: Option<String>,
    pub(super) owner_instance_id: Option<String>,
}

pub(super) fn clean_task_panel_value(raw: &str) -> String {
    first_content_line(raw)
}

pub(super) fn render_task_panel_line(task: &TaskPanelSnapshot) -> String {
    let mut parts = vec![format!(
        "Task      dispatch #{}",
        escape_status_panel_markdown(&task.dispatch_id)
    )];
    if let Some(card_id) = task.card_id.as_deref() {
        parts.push(format!("card #{}", escape_status_panel_markdown(card_id)));
    }
    if let Some(dispatch_type) = task.dispatch_type.as_deref() {
        parts.push(escape_status_panel_markdown(dispatch_type));
    }
    truncate_chars(&parts.join(" · "), TASK_PANEL_LINE_MAX_CHARS)
}
