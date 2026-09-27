use super::*;
use health::measurement::{FieldIssue, Measurement, read};

fn measured_label<T>(
    response: &Value,
    key: &str,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Measurement<T> {
    let value = read(response, key, "known string", Value::as_str)?;
    parse(value).ok_or_else(|| {
        let mut issue = FieldIssue::new(key, response.get(key), "known string");
        issue.code = "unknown_value";
        issue
    })
}

fn fix_safety(response: &Value) -> Measurement<FixSafety> {
    measured_label(response, "fix_safety", FixSafety::from_wire)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RepairStatus {
    Applied,
    Partial,
    Skipped,
}

fn response_status(response: &Value) -> Measurement<RepairStatus> {
    measured_label(response, "status", |status| match status {
        "applied" => Some(RepairStatus::Applied),
        "partial_repair" => Some(RepairStatus::Partial),
        "skipped" => Some(RepairStatus::Skipped),
        _ => None,
    })
}

fn safety_gate(response: &Value) -> &'static str {
    match response.get("safety_gate").and_then(Value::as_str) {
        Some("mailbox_not_found") => "mailbox_not_found",
        Some("expected_evidence_mismatch") => "expected_evidence_mismatch",
        Some("queue_not_empty") => "queue_not_empty",
        Some("active_dispatch_present") => "active_dispatch_present",
        Some("tmux_present") => "tmux_present",
        Some("no_live_work_evidence") => "no_live_work_evidence",
        Some("tmux_ready_for_input_no_unsent_output") => "tmux_ready_for_input_no_unsent_output",
        _ => "repair_gate_unavailable",
    }
}

pub(super) fn classify(
    finding: &mailbox::MailboxFinding,
    channel_id: u64,
    response: Value,
) -> FixAction {
    let measured = fix_safety(&response)
        .and_then(|safety| response_status(&response).map(|status| (safety, status)));
    let (safety, status) = match measured {
        Ok(measured) => measured,
        Err(issue) => {
            let gate = if issue.field == "fix_safety" {
                "safety_classification_unavailable"
            } else {
                "repair_status_unavailable"
            };
            let status = response
                .get("status")
                .map(Value::to_string)
                .unwrap_or_else(|| "unmeasured".into());
            let label = if issue.field == "fix_safety" {
                "안전 등급을"
            } else {
                "상태를"
            };
            let detail = format!(
                "서버 수리 응답의 {label} 해석할 수 없습니다({}). 서버 응답 상태={}; 실제 변경 여부를 확인하세요. 자동 재시도하지 않습니다.",
                issue.describe(),
                response
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or(&status)
            );
            let mut action =
                FixAction::fail(finding.id, "Stale Mailbox Repair", detail).with_safety_gate(gate);
            action.skipped = response.get("status").and_then(Value::as_str) == Some("skipped");
            if action.skipped {
                action.status = "skipped";
            }
            action.skipped_reason = Some(gate.into());
            return action.with_evidence(
                json!({"finding":finding.evidence, "repair":response, "measurement_issue":issue}),
            );
        }
    };
    let mut action = match status {
        RepairStatus::Applied => FixAction::ok(
            finding.id,
            "Stale Mailbox Repair",
            format!(
                "cleared stale mailbox state for channel {channel_id}; fix_safety={}; safety_gate={}",
                safety.as_str(),
                safety_gate(&response)
            ),
        ),
        RepairStatus::Partial => FixAction::partial(
            finding.id,
            "Stale Mailbox Repair",
            format!(
                "partial stale mailbox repair for channel {channel_id}; operator follow-up required; response fix_safety={}; safety_gate={}",
                safety.as_str(),
                safety_gate(&response)
            ),
        ),
        RepairStatus::Skipped => FixAction::skipped(
            finding.id,
            "Stale Mailbox Repair",
            format!(
                "skipped stale mailbox repair for channel {channel_id}; fix_safety={}; safety_gate={}",
                safety.as_str(),
                safety_gate(&response)
            ),
            safety,
            response
                .get("skipped_reason")
                .and_then(Value::as_str)
                .unwrap_or("repair safety gate skipped the request"),
        ),
    };
    if status != RepairStatus::Partial {
        action.fix_safety = safety;
        action.safety_gate = safety_gate(&response);
    }
    action.with_evidence(json!({"finding":finding.evidence, "repair":response}))
}

pub(super) fn verification_checks(actions: &[FixAction]) -> Vec<Check> {
    actions.iter().filter(|action| matches!(action.safety_gate,
        "safety_classification_unavailable" | "repair_status_unavailable"))
        .map(|action| {
            let mut check = Check::fail(action.safety_gate, CheckGroup::ProviderRuntime,
                "Stale Mailbox Repair Response", &action.detail,
                "doctor/dcserver 버전과 응답 형식을 확인하고 실제 변경 여부를 확인한 뒤 다시 진단하세요.")
                .with_subsystem("provider_runtime");
            check.evidence = action.evidence.clone();
            check
        }).collect()
}
