use super::*;

pub(super) fn fix_action(action: &DoctorFixReport) -> FixAction {
    FixAction {
        id: action.id,
        name: action.name,
        status: action.status,
        ok: action.ok,
        detail: action.detail.clone(),
        skipped: action.skipped,
        requires_explicit_consent: action.requires_explicit_consent,
        fix_safety: FixSafety::from_wire(action.fix_safety).unwrap_or(FixSafety::NotFixable),
        safety_gate: action.safety_gate,
        skipped_reason: action.skipped_reason.clone(),
        evidence: action.evidence.clone(),
    }
}

pub(super) fn check(check: &DoctorCheckReport) -> Check {
    Check {
        id: check.id,
        group: check_group_from_report(check.group),
        name: check.name,
        status: match check.status {
            "pass" => CheckStatus::Pass,
            "warn" => CheckStatus::Warn,
            _ => CheckStatus::Fail,
        },
        severity: match check.severity {
            "info" => Severity::Info,
            "warning" => Severity::Warning,
            "critical" => Severity::Critical,
            _ => Severity::Error,
        },
        subsystem: check.subsystem,
        detail: check.detail.clone(),
        guidance: check.guidance.clone(),
        path: check.path.clone(),
        expected: check.expected.clone(),
        actual: check.actual.clone(),
        next_steps: check.next_steps.clone(),
        evidence: check.evidence.clone(),
        fix_safety: FixSafety::from_wire(check.fix_safety).unwrap_or(FixSafety::NotFixable),
        security_exposure: match check.security_exposure {
            "local_path" => SecurityExposure::LocalPath,
            "operational_metadata" => SecurityExposure::OperationalMetadata,
            "credential_metadata" => SecurityExposure::CredentialMetadata,
            "public_surface" => SecurityExposure::PublicSurface,
            _ => SecurityExposure::None,
        },
    }
}
