use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

use super::{DoctorProfile, FixSafety, RunContext, SecurityExposure, Severity};

#[derive(Clone, Debug)]
pub(crate) struct DoctorOptions {
    pub(crate) fix: bool,
    pub(crate) json: bool,
    pub(crate) allow_restart: bool,
    pub(crate) repair_sqlite_cache: bool,
    pub(crate) allow_remote: bool,
    pub(crate) profile: Option<DoctorProfile>,
    pub(crate) run_context: RunContext,
    pub(crate) artifact_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CheckGroup {
    Core,
    ProviderRuntime,
    OptionalConnectors,
}

impl CheckGroup {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            CheckGroup::Core => "core",
            CheckGroup::ProviderRuntime => "provider_runtime",
            CheckGroup::OptionalConnectors => "optional_connectors",
        }
    }

    fn default_subsystem(self) -> &'static str {
        match self {
            CheckGroup::Core => "server",
            CheckGroup::ProviderRuntime => "provider_runtime",
            CheckGroup::OptionalConnectors => "optional_connectors",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

impl CheckStatus {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            CheckStatus::Pass => "pass",
            CheckStatus::Warn => "warn",
            CheckStatus::Fail => "fail",
        }
    }
}

#[derive(Clone)]
pub(super) struct Check {
    pub(super) id: &'static str,
    pub(super) group: CheckGroup,
    pub(super) name: &'static str,
    pub(super) status: CheckStatus,
    pub(super) severity: Severity,
    pub(super) subsystem: &'static str,
    pub(super) detail: String,
    pub(super) guidance: Option<String>,
    pub(super) path: Option<String>,
    pub(super) expected: Option<String>,
    pub(super) actual: Option<String>,
    pub(super) next_steps: Vec<String>,
    pub(super) evidence: Option<Value>,
    pub(super) fix_safety: FixSafety,
    pub(super) security_exposure: SecurityExposure,
}

impl Check {
    pub(super) fn ok(
        id: &'static str,
        group: CheckGroup,
        name: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            id,
            group,
            name,
            status: CheckStatus::Pass,
            severity: Severity::Info,
            subsystem: group.default_subsystem(),
            detail: detail.into(),
            guidance: None,
            path: None,
            expected: None,
            actual: None,
            next_steps: Vec::new(),
            evidence: None,
            fix_safety: FixSafety::ReadOnly,
            security_exposure: SecurityExposure::None,
        }
    }

    pub(super) fn warn(
        id: &'static str,
        group: CheckGroup,
        name: &'static str,
        detail: impl Into<String>,
        guidance: impl Into<String>,
    ) -> Self {
        Self {
            id,
            group,
            name,
            status: CheckStatus::Warn,
            severity: Severity::Warning,
            subsystem: group.default_subsystem(),
            detail: detail.into(),
            guidance: Some(guidance.into()),
            path: None,
            expected: None,
            actual: None,
            next_steps: Vec::new(),
            evidence: None,
            fix_safety: FixSafety::ReadOnly,
            security_exposure: SecurityExposure::None,
        }
    }

    pub(super) fn fail(
        id: &'static str,
        group: CheckGroup,
        name: &'static str,
        detail: impl Into<String>,
        guidance: impl Into<String>,
    ) -> Self {
        Self {
            id,
            group,
            name,
            status: CheckStatus::Fail,
            severity: Severity::Error,
            subsystem: group.default_subsystem(),
            detail: detail.into(),
            guidance: Some(guidance.into()),
            path: None,
            expected: None,
            actual: None,
            next_steps: Vec::new(),
            evidence: None,
            fix_safety: FixSafety::NotFixable,
            security_exposure: SecurityExposure::None,
        }
    }

    fn icon(&self) -> &'static str {
        match self.status {
            CheckStatus::Pass => "✓",
            CheckStatus::Warn => "!",
            CheckStatus::Fail => "✗",
        }
    }

    fn label(&self) -> &'static str {
        match self.status {
            CheckStatus::Pass => "PASS",
            CheckStatus::Warn => "WARN",
            CheckStatus::Fail => "FAIL",
        }
    }

    pub(super) fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub(super) fn with_expected_actual(
        mut self,
        expected: impl Into<String>,
        actual: impl Into<String>,
    ) -> Self {
        self.expected = Some(expected.into());
        self.actual = Some(actual.into());
        self
    }

    pub(super) fn with_next_steps(mut self, next_steps: Vec<String>) -> Self {
        self.next_steps = next_steps;
        self
    }

    pub(super) fn with_severity(mut self, severity: Severity) -> Self {
        self.severity = severity;
        self
    }

    pub(super) fn with_subsystem(mut self, subsystem: &'static str) -> Self {
        self.subsystem = subsystem;
        self
    }

    pub(super) fn with_evidence(mut self, evidence: Value) -> Self {
        self.evidence = Some(evidence);
        self
    }

    pub(super) fn with_fix_safety(mut self, fix_safety: FixSafety) -> Self {
        self.fix_safety = fix_safety;
        self
    }

    pub(super) fn with_security_exposure(mut self, security_exposure: SecurityExposure) -> Self {
        self.security_exposure = security_exposure;
        self
    }
}

pub(super) struct FixAction {
    pub(super) id: &'static str,
    pub(super) name: &'static str,
    pub(super) status: &'static str,
    pub(super) ok: bool,
    pub(super) detail: String,
    pub(super) skipped: bool,
    pub(super) requires_explicit_consent: bool,
    pub(super) fix_safety: FixSafety,
    pub(super) safety_gate: &'static str,
    pub(super) skipped_reason: Option<String>,
    pub(super) evidence: Option<Value>,
}

impl FixAction {
    pub(super) fn ok(id: &'static str, name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            id,
            name,
            status: "applied",
            ok: true,
            detail: detail.into(),
            skipped: false,
            requires_explicit_consent: false,
            fix_safety: FixSafety::SafeLocalRepair,
            safety_gate: "safe_local_repair",
            skipped_reason: None,
            evidence: None,
        }
    }

    pub(super) fn fail(id: &'static str, name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            id,
            name,
            status: "failed",
            ok: false,
            detail: detail.into(),
            skipped: false,
            requires_explicit_consent: false,
            fix_safety: FixSafety::NotFixable,
            safety_gate: "repair_failed",
            skipped_reason: None,
            evidence: None,
        }
    }

    pub(super) fn skipped(
        id: &'static str,
        name: &'static str,
        detail: impl Into<String>,
        fix_safety: FixSafety,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            id,
            name,
            status: "skipped",
            ok: true,
            detail: detail.into(),
            skipped: true,
            requires_explicit_consent: !matches!(fix_safety, FixSafety::ReadOnly),
            fix_safety,
            safety_gate: "explicit_consent_required",
            skipped_reason: Some(reason.into()),
            evidence: None,
        }
    }

    pub(super) fn partial(id: &'static str, name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            id,
            name,
            status: "partial_repair",
            ok: false,
            detail: detail.into(),
            skipped: false,
            requires_explicit_consent: true,
            fix_safety: FixSafety::ExplicitRestartRequired,
            safety_gate: "partial_repair_requires_operator",
            skipped_reason: None,
            evidence: None,
        }
    }

    pub(super) fn with_safety_gate(mut self, safety_gate: &'static str) -> Self {
        self.safety_gate = safety_gate;
        self
    }

    pub(super) fn with_evidence(mut self, evidence: Value) -> Self {
        self.evidence = Some(evidence);
        self
    }
}

#[derive(Serialize)]
pub(crate) struct DoctorSummary {
    pub(super) passed: usize,
    pub(super) warned: usize,
    pub(super) failed: usize,
    pub(super) total: usize,
}

#[derive(Serialize)]
pub(crate) struct DoctorCheckReport {
    pub(super) id: &'static str,
    pub(super) group: &'static str,
    pub(super) name: &'static str,
    pub(super) status: &'static str,
    pub(super) severity: &'static str,
    pub(super) subsystem: &'static str,
    pub(super) ok: bool,
    pub(super) detail: String,
    pub(super) guidance: Option<String>,
    pub(super) path: Option<String>,
    pub(super) expected: Option<String>,
    pub(super) actual: Option<String>,
    pub(super) next_steps: Vec<String>,
    pub(super) evidence: Option<Value>,
    pub(super) fix_safety: &'static str,
    pub(super) security_exposure: &'static str,
}

#[derive(Serialize)]
pub(crate) struct DoctorSectionReport {
    pub(super) status: &'static str,
    pub(super) summary: DoctorSummary,
    pub(super) check_ids: Vec<&'static str>,
}

#[derive(Clone, Serialize)]
pub(crate) struct DoctorFixReport {
    pub(super) id: &'static str,
    pub(super) name: &'static str,
    pub(super) status: &'static str,
    pub(super) ok: bool,
    pub(super) detail: String,
    pub(super) skipped: bool,
    pub(super) requires_explicit_consent: bool,
    pub(super) fix_safety: &'static str,
    pub(super) safety_gate: &'static str,
    pub(super) skipped_reason: Option<String>,
    pub(super) evidence: Option<Value>,
}

#[derive(Serialize)]
pub(crate) struct DoctorReport {
    pub(super) version: &'static str,
    pub(super) ok: bool,
    pub(super) fix_requested: bool,
    pub(super) fix_applied: bool,
    pub(super) run_context: &'static str,
    pub(super) artifact_path: Option<String>,
    pub(super) profile: Option<&'static str>,
    pub(super) summary: DoctorSummary,
    pub(super) sections: BTreeMap<&'static str, DoctorSectionReport>,
    pub(super) checks: Vec<DoctorCheckReport>,
    pub(super) auto_fixes: Vec<DoctorFixReport>,
    pub(super) fixes: Vec<DoctorFixReport>,
}

#[derive(Clone, Debug)]
pub(super) struct HealthSnapshot {
    pub(super) base: String,
    pub(super) body: Option<Value>,
    pub(super) error: Option<String>,
}

pub(super) fn print_group(title: &str, checks: &[Check]) {
    if checks.is_empty() {
        return;
    }
    println!("{title}");
    for check in checks {
        println!(
            "  {} [{}] {}: {}",
            check.icon(),
            check.label(),
            check.name,
            check.detail
        );
        if let Some(guidance) = &check.guidance {
            if !guidance.trim().is_empty() {
                println!("      → {}", guidance);
            }
        }
        if !check.next_steps.is_empty() {
            println!("      → try: {}", check.next_steps.join(" or "));
        }
    }
    println!();
}
