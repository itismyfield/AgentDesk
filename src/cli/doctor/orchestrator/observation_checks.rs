use super::super::health::measurement::FieldIssue;
use super::*;

pub(super) fn unmeasured_check(id: &'static str, issue: &FieldIssue) -> Check {
    Check::fail(id, CheckGroup::Core, "Health Observation", issue.describe(),
        "Measurement unavailable; check doctor/dcserver versions and health detail, then diagnose again.")
        .with_subsystem("health")
        .with_fix_safety(FixSafety::NotFixable)
        .with_security_exposure(SecurityExposure::OperationalMetadata)
        .with_expected_actual("measured health fields", issue.describe())
        .with_evidence(json!({"measurement_issue": issue}))
}

pub(super) fn check_mailbox_consistency(snapshot: &HealthSnapshot) -> Vec<Check> {
    let Some(body) = snapshot.body.as_ref() else {
        return Vec::new();
    };
    mailbox::classify_mailbox_findings(body)
        .into_iter()
        .map(|finding| {
            if let Err(issue) = &finding.live_work_present {
                let mut check = unmeasured_check(finding.id, issue).with_evidence(finding.evidence.clone());
                check.group = CheckGroup::ProviderRuntime;
                check.subsystem = "provider_runtime";
                if finding.id == "mailbox_observation_unavailable" {
                    check.status = CheckStatus::Warn;
                    check.severity = Severity::Warning;
                }
                return check;
            }
            let fix_safety = if finding.live_work_present == Ok(true) {
                FixSafety::ExplicitRestartRequired
            } else {
                FixSafety::SafeLocalRepair
            };
            Check::fail(
                finding.id,
                CheckGroup::ProviderRuntime,
                "Turn Mailbox Consistency",
                finding.detail,
                if finding.live_work_present == Ok(true) {
                    "operator verification is required because live work evidence exists, skipping auto-cleanup."
                } else {
                    "protected stale-mailbox repair can be applied since no live work evidence is present."
                },
            )
            .with_subsystem("provider_runtime")
            .with_severity(Severity::Error)
            .with_fix_safety(fix_safety)
            .with_security_exposure(SecurityExposure::OperationalMetadata)
            .with_evidence(finding.evidence)
            .with_next_steps(vec![
                "agentdesk doctor --fix".to_string(),
                "POST /api/doctor/stale-mailbox/repair".to_string(),
            ])
        })
        .collect()
}
