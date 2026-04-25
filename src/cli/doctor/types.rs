use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CheckGroup {
    Core,
    ProviderRuntime,
}

impl CheckGroup {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            CheckGroup::Core => "core",
            CheckGroup::ProviderRuntime => "provider_runtime",
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
    pub(super) detail: String,
    pub(super) guidance: Option<String>,
    pub(super) path: Option<String>,
    pub(super) expected: Option<String>,
    pub(super) actual: Option<String>,
    pub(super) next_steps: Vec<String>,
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
            detail: detail.into(),
            guidance: None,
            path: None,
            expected: None,
            actual: None,
            next_steps: Vec::new(),
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
            detail: detail.into(),
            guidance: Some(guidance.into()),
            path: None,
            expected: None,
            actual: None,
            next_steps: Vec::new(),
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
            detail: detail.into(),
            guidance: Some(guidance.into()),
            path: None,
            expected: None,
            actual: None,
            next_steps: Vec::new(),
        }
    }

    pub(super) fn icon(&self) -> &'static str {
        match self.status {
            CheckStatus::Pass => "✓",
            CheckStatus::Warn => "!",
            CheckStatus::Fail => "✗",
        }
    }

    pub(super) fn label(&self) -> &'static str {
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
}

pub(super) struct FixAction {
    pub(super) id: &'static str,
    pub(super) name: &'static str,
    pub(super) ok: bool,
    pub(super) detail: String,
}

impl FixAction {
    pub(super) fn ok(id: &'static str, name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            id,
            name,
            ok: true,
            detail: detail.into(),
        }
    }

    pub(super) fn fail(id: &'static str, name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            id,
            name,
            ok: false,
            detail: detail.into(),
        }
    }
}

#[derive(Serialize)]
pub(super) struct DoctorSummary {
    pub(super) passed: usize,
    pub(super) warned: usize,
    pub(super) failed: usize,
    pub(super) total: usize,
}

#[derive(Serialize)]
pub(super) struct DoctorCheckReport {
    pub(super) id: &'static str,
    pub(super) group: &'static str,
    pub(super) name: &'static str,
    pub(super) status: &'static str,
    pub(super) ok: bool,
    pub(super) detail: String,
    pub(super) guidance: Option<String>,
    pub(super) path: Option<String>,
    pub(super) expected: Option<String>,
    pub(super) actual: Option<String>,
    pub(super) next_steps: Vec<String>,
}

#[derive(Serialize)]
pub(super) struct DoctorFixReport {
    pub(super) id: &'static str,
    pub(super) name: &'static str,
    pub(super) status: &'static str,
    pub(super) ok: bool,
    pub(super) detail: String,
}

#[derive(Serialize)]
pub(super) struct DoctorReport {
    pub(super) version: &'static str,
    pub(super) ok: bool,
    pub(super) fix_requested: bool,
    pub(super) summary: DoctorSummary,
    pub(super) checks: Vec<DoctorCheckReport>,
    pub(super) fixes: Vec<DoctorFixReport>,
}

#[derive(Clone, Debug)]
pub(super) struct HealthSnapshot {
    pub(super) base: String,
    pub(super) body: Option<Value>,
    pub(super) error: Option<String>,
}
