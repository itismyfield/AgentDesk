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
        .filter(|finding| {
            if matches!(options.run_context, RunContext::StartupOnce) {
                !finding.live_work_present
            } else {
                true
            }
        })
        .map(|finding| {
            if finding.live_work_present {
                return FixAction::skipped(
                    finding.id,
                    "Stale Mailbox Repair",
                    "skipped stale mailbox repair because live work evidence exists",
                    FixSafety::ExplicitRestartRequired,
                    "live tmux/process/dispatch evidence present",
                )
                .with_evidence(finding.evidence);
            }
            let Some(channel_id) = finding
                .evidence
                .get("mailbox")
                .and_then(|mailbox| mailbox.get("channel_id"))
                .and_then(Value::as_u64)
            else {
                return FixAction::skipped(
                    finding.id,
                    "Stale Mailbox Repair",
                    "stale mailbox finding has no channel id for local repair",
                    FixSafety::SafeLocalRepair,
                    "channel evidence missing",
                )
                .with_safety_gate("missing_channel_evidence")
                .with_evidence(finding.evidence);
            };
            let expected_has_cancel_token = finding
                .evidence
                .get("mailbox")
                .and_then(|mailbox| mailbox.get("has_cancel_token"))
                .and_then(Value::as_bool);
            let request = json!({
                "channel_id": channel_id,
                "expected_has_cancel_token": expected_has_cancel_token
            });
            match post("/api/doctor/stale-mailbox/repair", request)
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
            }
        })
        .collect()
}
