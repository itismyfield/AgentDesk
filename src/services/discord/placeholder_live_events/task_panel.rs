use super::common::{
    EVENT_LINE_MAX_CHARS, TASK_PANEL_LINE_MAX_CHARS, escape_status_panel_markdown,
    first_content_line, sanitized_tool_name, truncate_chars,
};

const DISPATCH_ID_SHORT_LEN: usize = 8;
const TASK_PANEL_TITLE_MAX_CHARS: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TaskPanelSnapshot {
    pub(super) dispatch_id: String,
    pub(super) card_id: Option<String>,
    pub(super) dispatch_type: Option<String>,
    pub(super) owner_instance_id: Option<String>,
    pub(super) card_title: Option<String>,
    pub(super) dispatch_title: Option<String>,
    pub(super) github_issue_number: Option<i64>,
}

#[derive(Debug, Default, Clone)]
pub(in crate::services::discord) struct TaskPanelInfo<'a> {
    pub dispatch_id: &'a str,
    pub card_id: Option<&'a str>,
    pub dispatch_type: Option<&'a str>,
    pub owner_instance_id: Option<&'a str>,
    pub card_title: Option<&'a str>,
    pub dispatch_title: Option<&'a str>,
    pub github_issue_number: Option<i64>,
}

pub(super) fn clean_task_panel_value(raw: &str) -> String {
    first_content_line(raw)
}

pub(super) fn render_task_panel_line(task: &TaskPanelSnapshot) -> String {
    let short_id = short_dispatch_id(&task.dispatch_id);
    let title = task
        .card_title
        .as_deref()
        .or(task.dispatch_title.as_deref())
        .map(|value| truncate_chars(value, TASK_PANEL_TITLE_MAX_CHARS));

    let mut parts: Vec<String> = Vec::new();
    parts.push("Task     ".to_string());

    if let Some(dispatch_type) = task.dispatch_type.as_deref() {
        parts.push(escape_status_panel_markdown(dispatch_type));
    }

    match (task.github_issue_number, title.as_deref()) {
        (Some(issue_number), Some(title)) => {
            parts.push(format!(
                "gh#{issue_number} \"{}\"",
                escape_status_panel_markdown(title)
            ));
            parts.push(format!("dsp #{}", escape_status_panel_markdown(&short_id)));
        }
        (Some(issue_number), None) => {
            parts.push(format!("gh#{issue_number}"));
            parts.push(format!("dsp #{}", escape_status_panel_markdown(&short_id)));
        }
        (None, Some(title)) => {
            parts.push(format!("\"{}\"", escape_status_panel_markdown(title)));
            parts.push(format!("#{}", escape_status_panel_markdown(&short_id)));
        }
        (None, None) => {
            parts.push(format!(
                "dispatch #{}",
                escape_status_panel_markdown(&task.dispatch_id)
            ));
            if let Some(card_id) = task.card_id.as_deref() {
                parts.push(format!("card #{}", escape_status_panel_markdown(card_id)));
            }
        }
    }

    let header = parts.remove(0);
    let body = parts.join(" · ");
    let line = if body.is_empty() {
        header.trim_end().to_string()
    } else {
        format!("{header} {body}")
    };
    truncate_chars(&line, TASK_PANEL_LINE_MAX_CHARS)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TaskToolSlot {
    pub(super) name: String,
    pub(super) task_id: Option<String>,
    pub(super) summary: Option<String>,
    pub(super) status: Option<String>,
}

pub(super) fn clean_task_tool_value(raw: impl AsRef<str>) -> Option<String> {
    let value = first_content_line(raw.as_ref());
    (!value.is_empty()).then_some(value)
}

pub(super) fn render_task_tool_slot(slot: &TaskToolSlot) -> String {
    let label = sanitized_tool_name(&slot.name).unwrap_or_else(|| "Task".to_string());
    let mut detail_parts = Vec::new();
    if let Some(task_id) = slot.task_id.as_deref() {
        detail_parts.push(escape_status_panel_markdown(task_id));
    }
    if let Some(summary) = slot.summary.as_deref() {
        if slot.task_id.as_deref() != Some(summary) {
            detail_parts.push(escape_status_panel_markdown(summary));
        }
    }
    if let Some(status) = slot.status.as_deref() {
        detail_parts.push(escape_status_panel_markdown(status));
    }

    let line = if detail_parts.is_empty() {
        format!("└ {label}")
    } else {
        format!("└ {label} {}", detail_parts.join(" · "))
    };
    truncate_chars(&line, EVENT_LINE_MAX_CHARS)
}

fn short_dispatch_id(dispatch_id: &str) -> String {
    dispatch_id.chars().take(DISPATCH_ID_SHORT_LEN).collect()
}
