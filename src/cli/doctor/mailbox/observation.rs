use super::*;
use crate::cli::doctor::health::measurement::{boolean, count, read};

pub(super) fn unavailable(snapshot: &Value, issue: FieldIssue) -> MailboxFinding {
    MailboxFinding {
        id: "mailbox_measurement_unavailable",
        detail: issue.describe(),
        evidence: json!({"mailbox": snapshot, "measurement_issue": issue}),
        live_work_present: Err(issue),
        request: None,
    }
}

fn measured_snapshot(snapshot: &Value) -> Measurement<Option<MailboxFinding>> {
    // A failed or cut session lookup leaves the session fields unmeasured, not absent.
    if let Some(error) = snapshot
        .get("session_lookup_error")
        .filter(|v| !v.is_null())
    {
        let mut issue = FieldIssue::new("session_lookup_error", Some(error), "null");
        // A lookup the request deadline never let start is told apart from one that failed.
        if error.get("cause").and_then(Value::as_str) == Some("not_started") {
            issue.code = "not_started";
        }
        return Err(issue);
    }
    let cancel = boolean(snapshot, "has_cancel_token")?;
    let queue = count(snapshot, "queue_depth")?;
    let watcher = boolean(snapshot, "watcher_attached")?;
    let inflight = boolean(snapshot, "inflight_state_present")?;
    let tmux = boolean(snapshot, "tmux_present")?;
    let process = boolean(snapshot, "process_present")?;
    let dispatch = boolean(snapshot, "active_dispatch_present")?;
    let session_dispatch = match snapshot.get("session_active_dispatch_id") {
        None | Some(Value::Null) => false,
        Some(Value::String(id)) => !id.trim().is_empty(),
        value => {
            return Err(FieldIssue::new(
                "session_active_dispatch_id",
                value,
                "string or null",
            ));
        }
    };
    let active = dispatch || session_dispatch;
    // Queued input can outlive its worker, so queue depth is not evidence of live work.
    let live = tmux || process || active;
    let (id, detail) = if cancel && !live {
        (
            "mailbox_busy_without_active_turn",
            "mailbox cancel token without live tmux/process/dispatch evidence",
        )
    } else if queue == 0 && !watcher && inflight {
        (
            "stale_watcher_inflight_without_active_turn",
            "inflight watcher state with no watcher attached and an empty queue",
        )
    } else if queue == 0
        && !tmux
        && !active
        && boolean(snapshot, "session_record_present")?
        && matches!(
            read(snapshot, "session_status", "string", Value::as_str)?,
            "turn_active" | "working"
        )
    {
        (
            "tmux_missing_with_session_record",
            "a working session record but no live tmux/process evidence",
        )
    } else if tmux && !watcher && inflight {
        (
            "completed_output_not_relayed",
            "a tmux session and stale inflight state but no active watcher",
        )
    } else {
        return Ok(None);
    };
    let channel_id = read(snapshot, "channel_id", "positive u64", |value| {
        value.as_u64().filter(|id| *id > 0)
    })?;
    let mut evidence = json!({
        "mailbox": snapshot, "frontier_provenance": frontier_provenance_evidence(snapshot),
        "turn_state_sources": {
            "agent_turn_status": snapshot["agent_turn_status"], "queue_depth": queue,
            "tmux_present": tmux, "process_present": process, "watcher_attached": watcher,
            "inflight_state_present": inflight, "active_dispatch_present": active,
            "session_status": snapshot["session_status"], "session_record_present": snapshot["session_record_present"]
        },
        "session": {"record_present": snapshot["session_record_present"], "status": snapshot["session_status"], "active_dispatch_present": session_dispatch}
    });
    if id == "completed_output_not_relayed" {
        evidence["delivery_completed"] = json!(false);
        evidence["rebind_spawned"] = snapshot["rebind_spawned"].clone();
    }
    Ok(Some(MailboxFinding {
        id,
        detail: format!("channel {channel_id} has {detail}"),
        evidence,
        live_work_present: Ok(live),
        request: Some(RepairRequest {
            channel_id,
            expected_has_cancel_token: cancel,
        }),
    }))
}

pub(crate) fn classify_mailbox_snapshot(snapshot: &Value) -> Option<MailboxFinding> {
    match measured_snapshot(snapshot) {
        Ok(finding) => finding,
        Err(issue) => {
            let not_started = issue.code == "not_started";
            let mut finding = unavailable(snapshot, issue);
            if not_started {
                finding.id = "mailbox_session_lookup_not_started";
            }
            Some(finding)
        }
    }
}

fn observation_unavailable(issue: FieldIssue, evidence: Value) -> MailboxFinding {
    let mut finding = unavailable(&Value::Null, issue);
    finding.evidence = evidence;
    finding.id = "mailbox_observation_unavailable";
    finding
}

/// A snapshot read that did not answer leaves its channel out of `mailboxes`, which then
/// undercounts `global_active`; a failed later read keeps its row and does not count here.
fn unread_rows(body: &Value) -> Option<FieldIssue> {
    let field = "mailbox_observation_failures";
    let unread = match body.get(field) {
        None => false,
        Some(Value::Array(failures)) => failures
            .iter()
            .any(|failure| failure.get("read").and_then(Value::as_str) == Some("snapshot")),
        Some(value) => return Some(FieldIssue::new(field, Some(value), "array")),
    };
    let incomplete = body
        .get("providers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|provider| provider.get("runtime_state_complete") == Some(&Value::Bool(false)));
    (unread || incomplete).then(|| {
        let mut issue = FieldIssue::new("mailboxes", body.get("mailboxes"), "every actor read");
        issue.code = "incomplete";
        issue
    })
}

pub(crate) fn classify_mailbox_findings(body: &Value) -> Vec<MailboxFinding> {
    let Some(value) = body.get("mailboxes") else {
        return Vec::new();
    };
    let Some(mailboxes) = value.as_array() else {
        let issue = FieldIssue::new("mailboxes", Some(value), "array");
        let evidence = json!({"measurement_issue": issue});
        return vec![observation_unavailable(issue, evidence)];
    };
    let mut findings: Vec<_> = mailboxes
        .iter()
        .filter_map(classify_mailbox_snapshot)
        .collect();
    if let Some(issue) = unread_rows(body) {
        let evidence = json!({"measurement_issue": issue,
            "mailbox_observation_failures": body.get("mailbox_observation_failures")});
        findings.push(observation_unavailable(issue, evidence));
        return findings;
    }
    let aggregate = (|| -> Measurement<(usize, usize)> {
        let global = count(body, "global_active")?;
        let mut actual = 0;
        for (index, mailbox) in mailboxes.iter().enumerate() {
            let cancel = boolean(mailbox, "has_cancel_token").map_err(|mut issue| {
                issue.field = format!("mailboxes[{index}].{}", issue.field);
                issue
            })?;
            actual += usize::from(
                cancel
                    || mailbox.get("agent_turn_status").and_then(Value::as_str) == Some("active"),
            );
        }
        Ok((global, actual))
    })();
    match aggregate {
        Err(issue) => {
            let mut finding = unavailable(&Value::Null, issue.clone());
            finding.evidence = json!({"measurement_issue": issue});
            findings.push(finding);
        }
        Ok((global, actual)) if global > actual => findings.push(MailboxFinding {
            id: "global_active_without_active_turn",
            detail: format!("global_active={global} exceeds actual active mailbox turns={actual}"),
            evidence: json!({"turn_state_sources": {"global_active": global, "actual_active_turns": actual}}),
            live_work_present: Ok(true), request: None,
        }),
        _ => {}
    }
    findings
}
