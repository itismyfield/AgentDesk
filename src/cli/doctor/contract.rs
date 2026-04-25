use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Severity {
    Info,
    Warning,
    Error,
    Critical,
}

impl Severity {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Critical => "critical",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FixSafety {
    ReadOnly,
    SafeLocalRepair,
    ExplicitRestartRequired,
    ExplicitDbRepairRequired,
    NotFixable,
}

impl FixSafety {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::SafeLocalRepair => "safe_local_repair",
            Self::ExplicitRestartRequired => "explicit_restart_required",
            Self::ExplicitDbRepairRequired => "explicit_db_repair_required",
            Self::NotFixable => "not_fixable",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SecurityExposure {
    None,
    LocalPath,
    OperationalMetadata,
    CredentialMetadata,
    PublicSurface,
}

impl SecurityExposure {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::LocalPath => "local_path",
            Self::OperationalMetadata => "operational_metadata",
            Self::CredentialMetadata => "credential_metadata",
            Self::PublicSurface => "public_surface",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunContext {
    ManualCli,
    StartupOnce,
    RestartFollowup,
}

impl RunContext {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ManualCli => "manual_cli",
            Self::StartupOnce => "startup_once",
            Self::RestartFollowup => "restart_followup",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DoctorProfile {
    Quick,
    Deep,
    Security,
}

impl DoctorProfile {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Deep => "deep",
            Self::Security => "security",
        }
    }
}
