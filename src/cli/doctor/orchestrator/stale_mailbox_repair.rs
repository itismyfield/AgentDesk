use super::*;

pub(super) fn apply_stale_mailbox_fixes_with_post(
    snapshot: &HealthSnapshot,
    options: &DoctorOptions,
    mut post: impl FnMut(&str, Value) -> Result<Value, String>,
) -> Vec<FixAction> {
    let Some(body) = snapshot.body.as_ref() else {
        return Vec::new();
    };
    mailbox::classify_mailbox_findings(body)
        .into_iter()
        .filter_map(|finding| {
            let request = match repair_decision(body, &finding) {
                Err(issue) => {
                    let mut action = FixAction::skipped(finding.id, "Stale Mailbox Repair",
                        "measurement unavailable; automatic repair withheld", FixSafety::NotFixable, issue.describe())
                        .with_safety_gate("measurement_unavailable")
                        .with_evidence(json!({"finding": finding.evidence, "measurement_issue": issue}));
                    action.requires_explicit_consent = false;
                    return Some(action);
                }
                Ok(None) => {
                    if options.run_context == RunContext::StartupOnce { return None; }
                    return Some(FixAction::skipped(finding.id, "Stale Mailbox Repair",
                        "skipped stale mailbox repair because live work evidence exists",
                        FixSafety::ExplicitRestartRequired, "live tmux/process/dispatch evidence present")
                        .with_evidence(finding.evidence));
                }
                Ok(Some(request)) => request,
            };
            let channel_id = request.channel_id;
            let request = json!(request);
            Some(match post("/api/doctor/stale-mailbox/repair", request)
            {
                Ok(response) => {
                    let status = stale_mailbox_repair_response_status(&response);
                    let evidence = json!({
                        "finding": finding.evidence,
                        "repair": response
                    });
                    match status {
                        "applied" => FixAction::ok(
                            finding.id,
                            "Stale Mailbox Repair",
                            format!("cleared stale mailbox state for channel {channel_id}"),
                        )
                        .with_safety_gate("no_live_work_evidence")
                        .with_evidence(evidence),
                        "partial_repair" => FixAction::partial(
                            finding.id,
                            "Stale Mailbox Repair",
                            format!(
                                "partial stale mailbox repair for channel {channel_id}; operator follow-up required"
                            ),
                        )
                        .with_evidence(evidence),
                        "skipped" => {
                            FixAction::skipped(
                                finding.id,
                                "Stale Mailbox Repair",
                                format!("skipped stale mailbox repair for channel {channel_id}"),
                                stale_mailbox_repair_fix_safety(&response),
                                response
                                    .get("skipped_reason")
                                    .and_then(Value::as_str)
                                    .unwrap_or("repair safety gate skipped the request"),
                            )
                            .with_safety_gate(stale_mailbox_repair_safety_gate(&response))
                            .with_evidence(evidence)
                        }
                        _ => FixAction::fail(
                            finding.id,
                            "Stale Mailbox Repair",
                            format!("stale mailbox repair returned status={status}"),
                        )
                        .with_evidence(evidence),
                    }
                }
                Err(error) => FixAction::fail(
                    finding.id,
                    "Stale Mailbox Repair",
                    format!("protected stale mailbox repair failed: {error}"),
                )
                .with_safety_gate("protected_repair_failed")
                .with_evidence(finding.evidence),
            })
        })
        .collect()
}

fn repair_decision(
    body: &Value,
    finding: &mailbox::MailboxFinding,
) -> health::measurement::Measurement<Option<mailbox::RepairRequest>> {
    health::degraded_reasons(body)?;
    if finding.live_work_present.clone()? {
        return Ok(None);
    }
    finding
        .request
        .clone()
        .map(Some)
        .ok_or_else(|| health::measurement::FieldIssue::new("channel_id", None, "positive u64"))
}
