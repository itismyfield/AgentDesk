use super::*;

pub(super) fn stale_mailbox_repair_response_status(response: &Value) -> &str {
    response
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            if response.get("ok").and_then(Value::as_bool) == Some(true) {
                "applied"
            } else if response.get("skipped").and_then(Value::as_bool) == Some(true)
                || response.get("safety_gate").is_some()
            {
                "skipped"
            } else {
                "partial_repair"
            }
        })
}

pub(super) fn stale_mailbox_repair_safety_gate(response: &Value) -> &'static str {
    match response
        .get("safety_gate")
        .and_then(Value::as_str)
        .unwrap_or("repair_skipped")
    {
        "mailbox_not_found" => "mailbox_not_found",
        "expected_evidence_mismatch" => "expected_evidence_mismatch",
        "queue_not_empty" => "queue_not_empty",
        "active_dispatch_present" => "active_dispatch_present",
        "tmux_present" => "tmux_present",
        _ => "repair_skipped",
    }
}

pub(super) fn stale_mailbox_repair_fix_safety(response: &Value) -> FixSafety {
    match response.get("fix_safety").and_then(Value::as_str) {
        Some("explicit_restart_required") => FixSafety::ExplicitRestartRequired,
        Some("explicit_db_repair_required") => FixSafety::ExplicitDbRepairRequired,
        Some("not_fixable") => FixSafety::NotFixable,
        Some("read_only") => FixSafety::ReadOnly,
        _ => FixSafety::SafeLocalRepair,
    }
}
