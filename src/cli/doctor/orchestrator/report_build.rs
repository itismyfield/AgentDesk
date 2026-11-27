use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::json;

use super::{
    Check, CheckGroup, CheckStatus, DoctorCheckReport, DoctorFixReport, DoctorOptions,
    DoctorProfile, DoctorReport, DoctorSectionReport, DoctorSummary, FixAction, HealthSnapshot,
    RunContext, build_core_checks, build_provider_checks, config, repair_response,
};
use crate::services::operator_connectors as connectors;

#[derive(Debug)]
pub(super) struct PermissionFinding {
    pub(super) label: &'static str,
    pub(super) path: String,
    pub(super) exists: bool,
    pub(super) mode: Option<String>,
    pub(super) owner_is_current: Option<bool>,
    pub(super) risk: Option<String>,
}

pub(super) fn permission_finding(
    label: &'static str,
    path: &Path,
    _sensitive: bool,
) -> PermissionFinding {
    let metadata = fs::metadata(path);
    let Ok(_metadata) = metadata else {
        return PermissionFinding {
            label,
            path: path.display().to_string(),
            exists: false,
            mode: None,
            owner_is_current: None,
            risk: None,
        };
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mode = _metadata.mode() & 0o777;
        let owner_is_current = _metadata.uid() == unsafe { libc::geteuid() };
        let risk = if _sensitive && mode & 0o077 != 0 {
            Some(format!("mode {mode:o} exposes group/other bits"))
        } else if !owner_is_current {
            Some("owner differs from current user".to_string())
        } else {
            None
        };
        PermissionFinding {
            label,
            path: path.display().to_string(),
            exists: true,
            mode: Some(format!("{mode:o}")),
            owner_is_current: Some(owner_is_current),
            risk,
        }
    }

    #[cfg(not(unix))]
    {
        PermissionFinding {
            label,
            path: path.display().to_string(),
            exists: true,
            mode: None,
            owner_is_current: None,
            risk: None,
        }
    }
}

pub(super) fn build_all_checks(cfg: &config::Config, snapshot: &HealthSnapshot) -> Vec<Check> {
    let mut checks = build_core_checks(cfg, snapshot);
    checks.extend(build_provider_checks(cfg, snapshot));
    checks.extend(build_optional_connector_checks());
    checks
}

fn build_optional_connector_checks() -> Vec<Check> {
    connectors::optional_connector_statuses()
        .into_iter()
        .map(optional_connector_check)
        .collect()
}

fn optional_connector_check(status: connectors::OptionalConnectorStatus) -> Check {
    let (id, name) = optional_connector_check_identity(status.id);
    let evidence = json!({
        "connector": status.id,
        "state": status.state.as_str(),
        "source": status.source.as_deref(),
        "reason": status.reason,
        "optional": status.optional,
        "env_var": status.env_var,
        "capabilities": &status.capabilities,
        "setup_actions": &status.setup_actions,
    });
    let check = match status.state {
        connectors::OptionalConnectorState::Ready
        | connectors::OptionalConnectorState::Skipped
        | connectors::OptionalConnectorState::MissingConfig => {
            Check::ok(id, CheckGroup::OptionalConnectors, name, status.detail.clone())
        }
        connectors::OptionalConnectorState::MissingPath
        | connectors::OptionalConnectorState::MissingProvider
        | connectors::OptionalConnectorState::InvalidConfig => Check::warn(
            id,
            CheckGroup::OptionalConnectors,
            name,
            status.detail.clone(),
            format!(
                "{} is not ready (state={}). Core runtime can continue, but connector-backed routines stay disabled until setup is fixed.",
                status.env_var,
                status.state.as_str()
            ),
        )
        .with_expected_actual("optional connector ready or not configured", status.state.as_str())
        .with_next_steps(status.setup_actions.clone()),
    };

    let check = if let Some(source) = &status.source {
        check.with_path(source.clone())
    } else {
        check
    };
    check.with_evidence(evidence)
}

fn optional_connector_check_identity(id: &str) -> (&'static str, &'static str) {
    match id {
        "obsidian_agent_prompts" => ("optional_obsidian_agents", "Obsidian agent prompts"),
        "obsidian_skill_root" => ("optional_obsidian_skill_root", "Obsidian skill root"),
        _ => ("optional_connector", "Optional connector"),
    }
}

fn summarize_checks(checks: &[Check]) -> DoctorSummary {
    let mut passed = 0;
    let mut warned = 0;
    let mut failed = 0;

    for check in checks {
        match check.status {
            CheckStatus::Pass => passed += 1,
            CheckStatus::Warn => warned += 1,
            CheckStatus::Fail => failed += 1,
        }
    }

    DoctorSummary {
        passed,
        warned,
        failed,
        total: checks.len(),
    }
}

fn doctor_summary_status(summary: &DoctorSummary) -> &'static str {
    if summary.failed > 0 {
        "fail"
    } else if summary.warned > 0 {
        "warn"
    } else {
        "pass"
    }
}

fn build_doctor_sections(checks: &[Check]) -> BTreeMap<&'static str, DoctorSectionReport> {
    const SECTION_NAMES: [&str; 4] = ["core", "config", "launchd", "optional_connectors"];

    let mut sections = BTreeMap::new();
    for section in SECTION_NAMES {
        let section_checks = checks
            .iter()
            .filter(|check| doctor_section_includes_check(section, check))
            .cloned()
            .collect::<Vec<_>>();
        let summary = summarize_checks(&section_checks);
        sections.insert(
            section,
            DoctorSectionReport {
                status: doctor_summary_status(&summary),
                summary,
                check_ids: section_checks.iter().map(|check| check.id).collect(),
            },
        );
    }
    sections
}

fn doctor_section_includes_check(section: &str, check: &Check) -> bool {
    match section {
        "core" => matches!(check.group, CheckGroup::Core),
        "config" => {
            check.subsystem.contains("config")
                || check.id.contains("config")
                || check.id.contains("runtime")
        }
        "launchd" => {
            check.subsystem.contains("launchd")
                || check.id.contains("launchd")
                || check.detail.contains("launchd")
        }
        "optional_connectors" => matches!(check.group, CheckGroup::OptionalConnectors),
        _ => false,
    }
}

pub(super) fn build_json_report(
    options: &DoctorOptions,
    checks: &[Check],
    actions: &[FixAction],
) -> DoctorReport {
    let checks = [checks, &repair_response::verification_checks(actions)].concat();
    let summary = summarize_checks(&checks);
    let sections = build_doctor_sections(&checks);
    let checks = checks
        .iter()
        .map(|check| DoctorCheckReport {
            id: check.id,
            group: check.group.as_str(),
            name: check.name,
            status: check.status.as_str(),
            severity: check.severity.as_str(),
            subsystem: check.subsystem,
            ok: matches!(check.status, CheckStatus::Pass),
            detail: check.detail.clone(),
            guidance: check.guidance.clone(),
            path: check.path.clone(),
            expected: check.expected.clone(),
            actual: check.actual.clone(),
            next_steps: check.next_steps.clone(),
            evidence: check.evidence.clone(),
            fix_safety: check.fix_safety.as_str(),
            security_exposure: check.security_exposure.as_str(),
        })
        .collect();
    let fixes = actions
        .iter()
        .map(|action| DoctorFixReport {
            id: action.id,
            name: action.name,
            status: action.status,
            ok: action.ok,
            detail: action.detail.clone(),
            skipped: action.skipped,
            requires_explicit_consent: action.requires_explicit_consent,
            fix_safety: action.fix_safety.as_str(),
            safety_gate: action.safety_gate,
            skipped_reason: action.skipped_reason.clone(),
            evidence: action.evidence.clone(),
        })
        .collect::<Vec<_>>();
    let fix_applied = fixes.iter().any(|action| !action.skipped && action.ok);

    DoctorReport {
        version: env!("CARGO_PKG_VERSION"),
        ok: summary.failed == 0,
        fix_requested: options.fix,
        fix_applied,
        run_context: options.run_context.as_str(),
        artifact_path: options
            .artifact_path
            .as_ref()
            .map(|path| path.display().to_string()),
        profile: options.profile.map(DoctorProfile::as_str),
        summary,
        sections,
        checks,
        auto_fixes: if matches!(options.run_context, RunContext::StartupOnce) {
            fixes.clone()
        } else {
            Vec::new()
        },
        fixes,
    }
}
