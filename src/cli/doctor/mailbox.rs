use serde_json::{Value, json};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MailboxFinding {
    pub(crate) id: &'static str,
    pub(crate) detail: String,
    pub(crate) evidence: Value,
    pub(crate) live_work_present: bool,
}

fn value_bool(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn value_usize(value: &Value, key: &str) -> usize {
    value.get(key).and_then(Value::as_u64).unwrap_or(0) as usize
}

pub(crate) fn classify_mailbox_snapshot(snapshot: &Value) -> Option<MailboxFinding> {
    let channel_id = snapshot.get("channel_id").and_then(Value::as_u64);
    let has_cancel_token = value_bool(snapshot, "has_cancel_token");
    let queue_depth = value_usize(snapshot, "queue_depth");
    let watcher_attached = value_bool(snapshot, "watcher_attached");
    let inflight_state_present = value_bool(snapshot, "inflight_state_present");
    let tmux_present = value_bool(snapshot, "tmux_present");
    let process_present = value_bool(snapshot, "process_present");
    let session_active_dispatch_present = snapshot
        .get("session_active_dispatch_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.trim().is_empty());
    let active_dispatch_present =
        value_bool(snapshot, "active_dispatch_present") || session_active_dispatch_present;
    let session_record_present = value_bool(snapshot, "session_record_present");
    let session_status = snapshot
        .get("session_status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let agent_turn_status = snapshot
        .get("agent_turn_status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let live_work_present =
        queue_depth > 0 || tmux_present || process_present || active_dispatch_present;

    if has_cancel_token && !live_work_present {
        return Some(MailboxFinding {
            id: "mailbox_busy_without_active_turn",
            detail: format!(
                "channel {} has mailbox cancel token without live queue/tmux/process/dispatch evidence",
                channel_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            ),
            evidence: json!({
                "mailbox": snapshot,
                "turn_state_sources": {
                    "agent_turn_status": agent_turn_status,
                    "queue_depth": queue_depth,
                    "tmux_present": tmux_present,
                    "process_present": process_present,
                    "watcher_attached": watcher_attached,
                    "inflight_state_present": inflight_state_present,
                    "active_dispatch_present": active_dispatch_present
                },
                "session": {
                    "record_present": session_record_present,
                    "status": session_status,
                    "active_dispatch_present": session_active_dispatch_present
                }
            }),
            live_work_present,
        });
    }

    if agent_turn_status == "idle"
        && queue_depth == 0
        && !watcher_attached
        && inflight_state_present
    {
        return Some(MailboxFinding {
            id: "stale_watcher_inflight_without_active_turn",
            detail: format!(
                "channel {} has stale inflight watcher state while agent turn status is idle",
                channel_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            ),
            evidence: json!({
                "mailbox": snapshot,
                "turn_state_sources": {
                    "agent_turn_status": agent_turn_status,
                    "queue_depth": queue_depth,
                    "tmux_present": tmux_present,
                    "process_present": process_present,
                    "watcher_attached": watcher_attached,
                    "inflight_state_present": inflight_state_present,
                    "active_dispatch_present": active_dispatch_present
                },
                "session": {
                    "record_present": session_record_present,
                    "status": session_status,
                    "active_dispatch_present": session_active_dispatch_present
                }
            }),
            live_work_present,
        });
    }

    if agent_turn_status == "idle"
        && queue_depth == 0
        && session_record_present
        && session_status == "working"
        && !tmux_present
        && !active_dispatch_present
    {
        return Some(MailboxFinding {
            id: "tmux_missing_with_session_record",
            detail: format!(
                "channel {} has a working session record but no live tmux/process evidence",
                channel_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            ),
            evidence: json!({
                "mailbox": snapshot,
                "turn_state_sources": {
                    "agent_turn_status": agent_turn_status,
                    "queue_depth": queue_depth,
                    "tmux_present": tmux_present,
                    "process_present": process_present,
                    "watcher_attached": watcher_attached,
                    "inflight_state_present": inflight_state_present,
                    "active_dispatch_present": active_dispatch_present,
                    "session_status": session_status,
                    "session_record_present": session_record_present
                }
            }),
            live_work_present,
        });
    }

    if agent_turn_status == "idle" && tmux_present && !watcher_attached && inflight_state_present {
        return Some(MailboxFinding {
            id: "completed_output_not_relayed",
            detail: format!(
                "channel {} has a tmux session and stale inflight state but no active watcher",
                channel_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            ),
            evidence: json!({
                "mailbox": snapshot,
                "turn_state_sources": {
                    "agent_turn_status": agent_turn_status,
                    "queue_depth": queue_depth,
                    "tmux_present": tmux_present,
                    "process_present": process_present,
                    "watcher_attached": watcher_attached,
                    "inflight_state_present": inflight_state_present,
                    "active_dispatch_present": active_dispatch_present,
                    "session_status": session_status,
                    "session_record_present": session_record_present
                },
                "delivery_completed": false,
                "rebind_spawned": snapshot
                    .get("rebind_spawned")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            }),
            live_work_present,
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_cancel_token_is_detected_independent_of_agent_turn_status() {
        let finding = classify_mailbox_snapshot(&json!({
            "channel_id": 123,
            "has_cancel_token": true,
            "queue_depth": 0,
            "watcher_attached": false,
            "inflight_state_present": false,
            "tmux_present": false,
            "process_present": false,
            "active_dispatch_present": false,
            "agent_turn_status": "active"
        }))
        .expect("stale cancel token should be detected even when derived turn status is active");

        assert_eq!(finding.id, "mailbox_busy_without_active_turn");
        assert!(!finding.live_work_present);
    }

    #[test]
    fn cancel_token_with_live_dispatch_is_not_classified_as_stale() {
        let finding = classify_mailbox_snapshot(&json!({
            "channel_id": 123,
            "has_cancel_token": true,
            "queue_depth": 0,
            "watcher_attached": false,
            "inflight_state_present": true,
            "tmux_present": false,
            "process_present": false,
            "active_dispatch_present": true,
            "agent_turn_status": "active"
        }));

        assert!(finding.is_none());
    }
}

pub(crate) fn classify_mailbox_findings(body: &Value) -> Vec<MailboxFinding> {
    let mut findings = body
        .get("mailboxes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(classify_mailbox_snapshot)
        .collect::<Vec<_>>();

    let global_active = value_usize(body, "global_active");
    let actual_active_turns = body
        .get("mailboxes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|snapshot| {
            value_bool(snapshot, "has_cancel_token")
                || snapshot.get("agent_turn_status").and_then(Value::as_str) == Some("active")
        })
        .count();
    if global_active > actual_active_turns {
        findings.push(MailboxFinding {
            id: "global_active_without_active_turn",
            detail: format!(
                "global_active={} exceeds actual active mailbox turns={}",
                global_active, actual_active_turns
            ),
            evidence: json!({
                "turn_state_sources": {
                    "global_active": global_active,
                    "actual_active_turns": actual_active_turns
                }
            }),
            live_work_present: true,
        });
    }

    findings
}
