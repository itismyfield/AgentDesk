use serde_json::Value;

use crate::services::agent_protocol::StatusEvent;

use super::common::{EVENT_LINE_MAX_CHARS, normalize_summary, normalize_tool_key, truncate_chars};

pub(super) fn slots_enabled_by_footer_flag() -> bool {
    // #3089: background Bash task slots are deliberately footer-mode gated so the
    // legacy separate status-panel / completion-card render path is unchanged only
    // when AGENTDESK_SINGLE_MESSAGE_PANEL=0/false (opt-out). As of #3560 the gate is
    // default-ON, so unset/other values render in footer mode. `status_panel_v2_enabled`
    // is already checked by the callers before status events are pushed/rendered.
    super::super::single_message_panel::enabled()
}

pub(super) fn start_event_from_bash_tool_use(
    name: &str,
    value: &Value,
    _args_summary: Option<String>,
    tool_use_id: Option<&str>,
    footer_mode_enabled: bool,
) -> Option<StatusEvent> {
    if !footer_mode_enabled || !is_background_bash_tool(name) || !run_in_background(value) {
        return None;
    }
    Some(StatusEvent::BackgroundTaskStart {
        name: name.to_string(),
        summary: task_summary(value).unwrap_or_else(|| "Bash".to_string()),
        tool_use_id: clean_tool_use_id(tool_use_id)?,
    })
}

pub(super) fn events_from_background_notification(
    status: &str,
    summary: &str,
    tool_use_id: Option<&str>,
) -> Vec<StatusEvent> {
    if notification_is_terminal(status) {
        if let Some(tool_use_id) = clean_tool_use_id(tool_use_id) {
            return vec![StatusEvent::BackgroundTaskEnd {
                tool_use_id,
                success: !notification_is_error(status),
            }];
        }
    }
    (!summary.is_empty())
        .then_some(StatusEvent::Heartbeat)
        .into_iter()
        .collect()
}

pub(super) fn tool_use_id_from_notification(value: &Value) -> Option<String> {
    ["tool_use_id", "tool-use-id", "toolUseId"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

pub(super) fn notification_is_terminal(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "completed"
            | "done"
            | "finished"
            | "success"
            | "failed"
            | "error"
            | "aborted"
            | "killed"
            | "stopped"
            | "cancelled"
            | "canceled"
    )
}

pub(super) fn notification_is_error(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "failed" | "error" | "aborted" | "killed" | "stopped" | "cancelled" | "canceled"
    )
}

fn clean_tool_use_id(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn is_background_bash_tool(name: &str) -> bool {
    normalize_tool_key(name) == "bash"
}

fn run_in_background(value: &Value) -> bool {
    value
        .get("run_in_background")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn task_summary(value: &Value) -> Option<String> {
    ["description", "desc"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
        .map(normalize_summary)
        .filter(|value| !value.is_empty())
        .map(|summary| truncate_chars(&summary, EVENT_LINE_MAX_CHARS))
}
