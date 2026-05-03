use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::contract::{DoctorProfile, FixSafety, RunContext, SecurityExposure, Severity};
use super::{health, mailbox};
use crate::cli::dcserver;
use crate::config;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use crate::db::{open_write_connection, schema};
use crate::services::provider::ProviderKind;
use serde::Serialize;
use serde_json::{Value, json};

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
enum CheckGroup {
    Core,
    ProviderRuntime,
}

impl CheckGroup {
    fn as_str(self) -> &'static str {
        match self {
            CheckGroup::Core => "core",
            CheckGroup::ProviderRuntime => "provider_runtime",
        }
    }

    fn default_subsystem(self) -> &'static str {
        match self {
            CheckGroup::Core => "server",
            CheckGroup::ProviderRuntime => "provider_runtime",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

impl CheckStatus {
    fn as_str(self) -> &'static str {
        match self {
            CheckStatus::Pass => "pass",
            CheckStatus::Warn => "warn",
            CheckStatus::Fail => "fail",
        }
    }
}

#[derive(Clone)]
struct Check {
    id: &'static str,
    group: CheckGroup,
    name: &'static str,
    status: CheckStatus,
    severity: Severity,
    subsystem: &'static str,
    detail: String,
    guidance: Option<String>,
    path: Option<String>,
    expected: Option<String>,
    actual: Option<String>,
    next_steps: Vec<String>,
    evidence: Option<Value>,
    fix_safety: FixSafety,
    security_exposure: SecurityExposure,
}

impl Check {
    fn ok(
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

    fn warn(
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

    fn fail(
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

    fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    fn with_expected_actual(
        mut self,
        expected: impl Into<String>,
        actual: impl Into<String>,
    ) -> Self {
        self.expected = Some(expected.into());
        self.actual = Some(actual.into());
        self
    }

    fn with_next_steps(mut self, next_steps: Vec<String>) -> Self {
        self.next_steps = next_steps;
        self
    }

    fn with_severity(mut self, severity: Severity) -> Self {
        self.severity = severity;
        self
    }

    fn with_subsystem(mut self, subsystem: &'static str) -> Self {
        self.subsystem = subsystem;
        self
    }

    fn with_evidence(mut self, evidence: Value) -> Self {
        self.evidence = Some(evidence);
        self
    }

    fn with_fix_safety(mut self, fix_safety: FixSafety) -> Self {
        self.fix_safety = fix_safety;
        self
    }

    fn with_security_exposure(mut self, security_exposure: SecurityExposure) -> Self {
        self.security_exposure = security_exposure;
        self
    }
}

struct FixAction {
    id: &'static str,
    name: &'static str,
    status: &'static str,
    ok: bool,
    detail: String,
    skipped: bool,
    requires_explicit_consent: bool,
    fix_safety: FixSafety,
    safety_gate: &'static str,
    skipped_reason: Option<String>,
    evidence: Option<Value>,
}

impl FixAction {
    fn ok(id: &'static str, name: &'static str, detail: impl Into<String>) -> Self {
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

    fn fail(id: &'static str, name: &'static str, detail: impl Into<String>) -> Self {
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

    fn skipped(
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

    fn partial(id: &'static str, name: &'static str, detail: impl Into<String>) -> Self {
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

    fn with_safety_gate(mut self, safety_gate: &'static str) -> Self {
        self.safety_gate = safety_gate;
        self
    }

    fn with_evidence(mut self, evidence: Value) -> Self {
        self.evidence = Some(evidence);
        self
    }
}

#[derive(Serialize)]
pub(crate) struct DoctorSummary {
    passed: usize,
    warned: usize,
    failed: usize,
    total: usize,
}

#[derive(Serialize)]
pub(crate) struct DoctorCheckReport {
    id: &'static str,
    group: &'static str,
    name: &'static str,
    status: &'static str,
    severity: &'static str,
    subsystem: &'static str,
    ok: bool,
    detail: String,
    guidance: Option<String>,
    path: Option<String>,
    expected: Option<String>,
    actual: Option<String>,
    next_steps: Vec<String>,
    evidence: Option<Value>,
    fix_safety: &'static str,
    security_exposure: &'static str,
}

#[derive(Clone, Serialize)]
pub(crate) struct DoctorFixReport {
    id: &'static str,
    name: &'static str,
    status: &'static str,
    ok: bool,
    detail: String,
    skipped: bool,
    requires_explicit_consent: bool,
    fix_safety: &'static str,
    safety_gate: &'static str,
    skipped_reason: Option<String>,
    evidence: Option<Value>,
}

#[derive(Serialize)]
pub(crate) struct DoctorReport {
    version: &'static str,
    ok: bool,
    fix_requested: bool,
    fix_applied: bool,
    run_context: &'static str,
    artifact_path: Option<String>,
    profile: Option<&'static str>,
    summary: DoctorSummary,
    checks: Vec<DoctorCheckReport>,
    auto_fixes: Vec<DoctorFixReport>,
    fixes: Vec<DoctorFixReport>,
}

#[derive(Clone, Debug)]
struct HealthSnapshot {
    base: String,
    body: Option<Value>,
    error: Option<String>,
}

fn fetch_health_snapshot(options: &DoctorOptions) -> HealthSnapshot {
    let base = crate::cli::client::api_base();
    let cfg = config::load_graceful();
    if cfg
        .server
        .auth_token
        .as_deref()
        .map(str::trim)
        .is_some_and(|token| !token.is_empty())
        && !options.allow_remote
        && !health::is_loopback_base_url(&base)
    {
        return HealthSnapshot {
            base,
            body: None,
            error: Some(
                "non-loopback AGENTDESK_API_URL with configured auth token requires --allow-remote"
                    .to_string(),
            ),
        };
    }

    match crate::cli::client::get_json("/api/health/detail").or_else(|detail_error| {
        if detail_error.contains("(404)") {
            crate::cli::client::get_json("/api/health")
        } else {
            Err(detail_error)
        }
    }) {
        Ok(body) => HealthSnapshot {
            base,
            body: Some(body),
            error: None,
        },
        Err(e) => HealthSnapshot {
            base,
            body: None,
            error: Some(e.to_string()),
        },
    }
}

fn health_providers(snapshot: &HealthSnapshot) -> Option<&Vec<Value>> {
    snapshot.body.as_ref()?.get("providers")?.as_array()
}

fn provider_connected(snapshot: &HealthSnapshot, provider: &ProviderKind) -> Option<bool> {
    let provider_name = provider.as_str();
    health_providers(snapshot)?
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(provider_name))
        .and_then(|entry| entry.get("connected").and_then(Value::as_bool))
}

fn configured_provider_names(cfg: &config::Config, snapshot: &HealthSnapshot) -> BTreeSet<String> {
    let mut configured = BTreeSet::new();

    for agent in &cfg.agents {
        if let Some(provider) = ProviderKind::from_str(&agent.provider) {
            configured.insert(provider.as_str().to_string());
        }
    }

    if let Some(providers) = health_providers(snapshot) {
        for entry in providers {
            if let Some(name) = entry.get("name").and_then(Value::as_str) {
                configured.insert(name.to_string());
            }
        }
    }

    configured
}

// Guidance strings are written in Korean; this project targets Korean-primary operators.
fn provider_runtime_guidance(provider: &ProviderKind) -> String {
    let provider_name = provider.as_str();
    let log_hint = dcserver_log_hint();
    format!(
        "{provider_name} CLI 설치/PATH와 서비스 런타임 PATH를 확인하고, 연결 문제가 있으면 {log_hint} 로그와 provider 인증 상태를 점검하세요."
    )
}

fn provider_unused_guidance(provider: &ProviderKind) -> String {
    format!(
        "{} CLI는 설치되어 있지만 현재 config/health 기준 활성 provider가 아닙니다. 의도한 구성이면 무시해도 됩니다.",
        provider.as_str()
    )
}

fn dcserver_log_hint() -> String {
    dcserver::dcserver_stdout_log_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "~/.adk/release/logs/dcserver.stdout.log".to_string())
}

fn qwen_home_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("QWEN_HOME") {
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    if let Some(path) = std::env::var_os("AGENTDESK_TEST_HOME") {
        let path = PathBuf::from(path);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }

    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .or_else(dirs::home_dir)
}

fn qwen_project_dir() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

fn qwen_system_defaults_path() -> Option<PathBuf> {
    std::env::var_os("QWEN_CODE_SYSTEM_DEFAULTS_PATH")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn qwen_system_settings_path() -> Option<PathBuf> {
    std::env::var_os("QWEN_CODE_SYSTEM_SETTINGS_PATH")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn qwen_user_settings_path() -> Option<PathBuf> {
    qwen_home_dir().map(|home| home.join(".qwen").join("settings.json"))
}

fn qwen_project_settings_path() -> Option<PathBuf> {
    qwen_project_dir().map(|dir| dir.join(".qwen").join("settings.json"))
}

fn format_artifact_scope(scope: &str, items: &[&str]) -> String {
    if items.is_empty() {
        format!("{scope}: -")
    } else {
        format!("{scope}: {}", items.join(", "))
    }
}

fn check_qwen_settings_files(configured: bool) -> Check {
    let candidates = [
        ("system defaults", qwen_system_defaults_path()),
        ("user settings", qwen_user_settings_path()),
        ("project settings", qwen_project_settings_path()),
        ("system settings", qwen_system_settings_path()),
    ];

    let found: Vec<String> = candidates
        .iter()
        .filter_map(|(label, path)| {
            path.as_ref()
                .filter(|path| path.is_file())
                .map(|path| format!("{label}={}", path.display()))
        })
        .collect();

    if !found.is_empty() {
        return Check::ok(
            "provider_qwen_settings",
            CheckGroup::ProviderRuntime,
            "qwen settings files",
            format!("found {}", found.join(" | ")),
        )
        .with_path(found[0].clone())
        .with_expected_actual(
            "Qwen settings layers discoverable",
            format!("{} settings layer(s) found", found.len()),
        );
    }

    let guidance = "Qwen은 settings 없이도 동작할 수 있지만, 모델 picker와 운영 surface를 안정적으로 쓰려면 ~/.qwen/settings.json 또는 <workspace>/.qwen/settings.json 구성을 권장합니다.";
    if configured {
        Check::warn(
            "provider_qwen_settings",
            CheckGroup::ProviderRuntime,
            "qwen settings files",
            "no Qwen settings files detected",
            guidance,
        )
        .with_expected_actual(
            "at least one Qwen settings layer",
            "no settings.json detected",
        )
        .with_next_steps(vec![
            "ls -la ~/.qwen".to_string(),
            "ls -la ./.qwen".to_string(),
        ])
    } else {
        Check::ok(
            "provider_qwen_settings",
            CheckGroup::ProviderRuntime,
            "qwen settings files",
            "no Qwen settings files detected (provider not configured)",
        )
        .with_expected_actual(
            "settings present if qwen is actively used",
            "qwen not configured",
        )
    }
}

fn check_qwen_auth_hints(configured: bool) -> Check {
    let home = qwen_home_dir();
    let project = qwen_project_dir();
    let oauth_cache = home
        .as_ref()
        .map(|path| path.join(".qwen").join("oauth_creds.json"));
    let project_qwen_env = project.as_ref().map(|path| path.join(".qwen").join(".env"));
    let project_env = project.as_ref().map(|path| path.join(".env"));

    let mut hints = Vec::new();
    if oauth_cache.as_ref().is_some_and(|path| path.is_file()) {
        hints.push("cached OAuth");
    }
    if project_qwen_env.as_ref().is_some_and(|path| path.is_file()) {
        hints.push("project .qwen/.env");
    }
    if project_env.as_ref().is_some_and(|path| path.is_file()) {
        hints.push("project .env");
    }

    let detail = if hints.is_empty() {
        "interactive: OAuth or API key | headless: cached auth or API key only".to_string()
    } else {
        format!(
            "interactive: OAuth or API key | headless: cached auth or API key only | found: {}",
            hints.join(", ")
        )
    };

    if !hints.is_empty() {
        return Check::ok(
            "provider_qwen_auth",
            CheckGroup::ProviderRuntime,
            "qwen auth hints",
            detail,
        )
        .with_expected_actual("cached auth or API-key hint visible", hints.join(", "))
        .with_next_steps(vec![
            "qwen auth status".to_string(),
            "Open a Qwen CLI session and run /stats".to_string(),
        ]);
    }

    let guidance = "API key 경로는 project .qwen/.env 우선, 그다음 .env를 확인하세요. Qwen CLI는 env-file을 merge하지 않습니다. 사용량/제한은 숫자를 doctor에 고정하지 말고 Qwen CLI 세션의 /stats 또는 공식 문서를 확인하세요.";
    if configured {
        Check::warn(
            "provider_qwen_auth",
            CheckGroup::ProviderRuntime,
            "qwen auth hints",
            detail,
            guidance,
        )
        .with_expected_actual(
            "cached auth or API-key hint visible",
            "no oauth cache or env hints found",
        )
        .with_next_steps(vec![
            "qwen auth status".to_string(),
            "ls -la ./.qwen".to_string(),
        ])
    } else {
        Check::ok(
            "provider_qwen_auth",
            CheckGroup::ProviderRuntime,
            "qwen auth hints",
            format!("{detail} (provider not configured)"),
        )
        .with_expected_actual(
            "auth hint required only if qwen is used",
            "qwen not configured",
        )
    }
}

fn check_qwen_runtime_artifacts(configured: bool) -> Check {
    let home = qwen_home_dir();
    let project = qwen_project_dir();

    let home_artifacts = [
        (
            "extensions",
            home.as_ref()
                .map(|path| path.join(".qwen").join("extensions")),
            false,
        ),
        (
            "commands",
            home.as_ref()
                .map(|path| path.join(".qwen").join("commands")),
            false,
        ),
        (
            "agents",
            home.as_ref().map(|path| path.join(".qwen").join("agents")),
            false,
        ),
        (
            "skills",
            home.as_ref().map(|path| path.join(".qwen").join("skills")),
            false,
        ),
        (
            "output-language.md",
            home.as_ref()
                .map(|path| path.join(".qwen").join("output-language.md")),
            true,
        ),
    ];
    let project_artifacts = [
        (
            "commands",
            project
                .as_ref()
                .map(|path| path.join(".qwen").join("commands")),
            false,
        ),
        (
            "agents",
            project
                .as_ref()
                .map(|path| path.join(".qwen").join("agents")),
            false,
        ),
        (
            "skills",
            project
                .as_ref()
                .map(|path| path.join(".qwen").join("skills")),
            false,
        ),
        (
            "PROJECT_SUMMARY.md",
            project
                .as_ref()
                .map(|path| path.join(".qwen").join("PROJECT_SUMMARY.md")),
            true,
        ),
        (
            "settings.json",
            project
                .as_ref()
                .map(|path| path.join(".qwen").join("settings.json")),
            true,
        ),
        (
            ".qwen/.env",
            project.as_ref().map(|path| path.join(".qwen").join(".env")),
            true,
        ),
        (".env", project.as_ref().map(|path| path.join(".env")), true),
    ];

    let found_home: Vec<&str> = home_artifacts
        .iter()
        .filter_map(|(label, path, is_file)| {
            path.as_ref().and_then(|path| {
                let exists = if *is_file {
                    path.is_file()
                } else {
                    path.is_dir()
                };
                exists.then_some(*label)
            })
        })
        .collect();
    let found_project: Vec<&str> = project_artifacts
        .iter()
        .filter_map(|(label, path, is_file)| {
            path.as_ref().and_then(|path| {
                let exists = if *is_file {
                    path.is_file()
                } else {
                    path.is_dir()
                };
                exists.then_some(*label)
            })
        })
        .collect();

    let detail = format!(
        "{} | {}",
        format_artifact_scope("home", &found_home),
        format_artifact_scope("project", &found_project)
    );

    if !found_home.is_empty() || !found_project.is_empty() {
        return Check::ok(
            "provider_qwen_runtime",
            CheckGroup::ProviderRuntime,
            "qwen runtime artifacts",
            detail,
        )
        .with_expected_actual(
            "Qwen runtime artifacts visible when configured",
            format!("home={} project={}", found_home.len(), found_project.len()),
        );
    }

    let guidance = "Qwen은 ~/.qwen/extensions, ~/.qwen/skills, <workspace>/.qwen/PROJECT_SUMMARY.md, <workspace>/.qwen/.env 같은 로컬 자산을 그대로 사용합니다. headless 환경에서는 project .qwen/.env 우선 여부를 함께 확인하세요.";
    if configured {
        Check::warn(
            "provider_qwen_runtime",
            CheckGroup::ProviderRuntime,
            "qwen runtime artifacts",
            detail,
            guidance,
        )
        .with_expected_actual(
            "at least one Qwen runtime artifact when heavily customized",
            "no Qwen runtime artifacts detected",
        )
        .with_next_steps(vec![
            "ls -la ~/.qwen".to_string(),
            "ls -la ./.qwen".to_string(),
        ])
    } else {
        Check::ok(
            "provider_qwen_runtime",
            CheckGroup::ProviderRuntime,
            "qwen runtime artifacts",
            format!("{detail} (provider not configured)"),
        )
        .with_expected_actual(
            "runtime artifacts required only if qwen is used",
            "qwen not configured",
        )
    }
}

fn health_endpoint(base: &str) -> String {
    format!("{}/api/health", base.trim_end_matches('/'))
}

fn health_detail_endpoint(base: &str) -> String {
    format!("{}/api/health/detail", base.trim_end_matches('/'))
}

fn highest_reason_severity(reasons: &[health::ClassifiedReason]) -> Severity {
    let mut result = Severity::Info;
    for reason in reasons {
        result = match (result, reason.severity) {
            (Severity::Critical, _) | (_, Severity::Critical) => Severity::Critical,
            (Severity::Error, _) | (_, Severity::Error) => Severity::Error,
            (Severity::Warning, _) | (_, Severity::Warning) => Severity::Warning,
            _ => Severity::Info,
        };
    }
    result
}

fn stale_zero_byte_db_candidates(
    runtime_root: &std::path::Path,
    canonical_db_path: &std::path::Path,
) -> Vec<PathBuf> {
    [
        runtime_root.join("agentdesk.db"),
        runtime_root.join("data.db"),
    ]
    .into_iter()
    .filter(|candidate| candidate != canonical_db_path)
    .filter(|candidate| {
        fs::metadata(candidate)
            .map(|meta| meta.is_file() && meta.len() == 0)
            .unwrap_or(false)
    })
    .collect()
}

fn provider_check_id(provider: &ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Claude => "provider_claude",
        ProviderKind::Codex => "provider_codex",
        ProviderKind::Gemini => "provider_gemini",
        ProviderKind::OpenCode => "provider_opencode",
        ProviderKind::Qwen => "provider_qwen",
        ProviderKind::Unsupported(_) => "provider_unsupported",
    }
}

fn build_core_checks(cfg: &config::Config, snapshot: &HealthSnapshot) -> Vec<Check> {
    let mut checks = vec![
        check_server_running(snapshot),
        check_discord_bot(snapshot),
        check_degraded_reasons(snapshot),
        check_health_db_dashboard(snapshot),
        check_dispatch_outbox(snapshot),
        check_config_audit(snapshot),
        check_runtime_root(),
        check_data_dir(cfg),
        check_tmux(),
        check_service_manager(),
        check_postgres_connection(cfg),
        check_db_integrity(cfg),
        check_stale_zero_byte_db_files(cfg),
        check_github_repo_registry(cfg),
        check_disk_usage(),
    ];
    checks.extend(check_mailbox_consistency(snapshot));
    checks
}

fn build_provider_checks(cfg: &config::Config, snapshot: &HealthSnapshot) -> Vec<Check> {
    let configured = configured_provider_names(cfg, snapshot);
    let opencode_configured = configured.contains("opencode");
    let qwen_configured = configured.contains("qwen");
    vec![
        check_runtime_path(),
        check_provider_cli(
            ProviderKind::Claude,
            configured.contains("claude"),
            snapshot,
        ),
        check_provider_cli(ProviderKind::Codex, configured.contains("codex"), snapshot),
        check_provider_cli(
            ProviderKind::Gemini,
            configured.contains("gemini"),
            snapshot,
        ),
        check_provider_cli(ProviderKind::OpenCode, opencode_configured, snapshot),
        check_opencode_mcp_config(opencode_configured),
        check_opencode_serve_health_probe(opencode_configured),
        check_provider_cli(ProviderKind::Qwen, qwen_configured, snapshot),
        check_qwen_settings_files(qwen_configured),
        check_qwen_auth_hints(qwen_configured),
        check_qwen_runtime_artifacts(qwen_configured),
        check_provider_bindings(cfg, snapshot),
        check_credential_permissions(cfg),
    ]
}

fn check_opencode_mcp_config(configured: bool) -> Check {
    let available = crate::services::mcp_config::provider_has_memento_mcp(&ProviderKind::OpenCode);
    if available {
        Check::ok(
            "provider_opencode_mcp",
            CheckGroup::ProviderRuntime,
            "OpenCode MCP config",
            "memento MCP is visible through runtime config or ~/.config/opencode/opencode.json",
        )
        .with_expected_actual("memento MCP configured", "memento MCP configured")
    } else if configured {
        Check::warn(
            "provider_opencode_mcp",
            CheckGroup::ProviderRuntime,
            "OpenCode MCP config",
            "memento MCP not visible for OpenCode",
            "runtime mcp_servers 또는 ~/.config/opencode/opencode.json top-level mcp에 memento 서버를 설정하세요.",
        )
        .with_expected_actual("memento MCP configured", "memento MCP missing")
        .with_next_steps(vec![
            "agentdesk doctor --json".to_string(),
            "jq '.mcp' ~/.config/opencode/opencode.json".to_string(),
        ])
    } else {
        Check::ok(
            "provider_opencode_mcp",
            CheckGroup::ProviderRuntime,
            "OpenCode MCP config",
            "OpenCode is not configured",
        )
        .with_expected_actual("OpenCode configured if needed", "OpenCode not configured")
    }
}

fn check_opencode_serve_health_probe(configured: bool) -> Check {
    if !configured {
        return Check::ok(
            "provider_opencode_serve",
            CheckGroup::ProviderRuntime,
            "OpenCode serve health",
            "OpenCode is not configured",
        )
        .with_expected_actual(
            "OpenCode serve probe required when configured",
            "not configured",
        );
    }

    let working_dir = std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    match crate::services::opencode::probe_serve_health(&working_dir) {
        Ok(detail) => Check::ok(
            "provider_opencode_serve",
            CheckGroup::ProviderRuntime,
            "OpenCode serve health",
            detail,
        )
        .with_expected_actual("opencode serve /global/health returns 200", "healthy"),
        Err(error) => Check::fail(
            "provider_opencode_serve",
            CheckGroup::ProviderRuntime,
            "OpenCode serve health",
            error,
            "opencode serve가 정상 기동되는지 CLI 설치, 설정 파일, provider/model 인증 상태를 확인하세요.",
        )
        .with_expected_actual("opencode serve /global/health returns 200", "probe failed")
        .with_next_steps(vec![
            "opencode --version".to_string(),
            "opencode serve --hostname 127.0.0.1 --port 0".to_string(),
            format!("tail -n 200 {}", dcserver_log_hint()),
        ]),
    }
}

fn check_health_db_dashboard(snapshot: &HealthSnapshot) -> Check {
    let Some(body) = snapshot.body.as_ref() else {
        return Check::fail(
            "health_db_dashboard",
            CheckGroup::Core,
            "DB/Dashboard Health",
            "health payload unavailable",
            "dcserver health detail endpoint에 접근할 수 있어야 DB/dashboard 상태를 요약할 수 있습니다.",
        )
        .with_subsystem("health")
        .with_fix_safety(FixSafety::NotFixable)
        .with_security_exposure(SecurityExposure::OperationalMetadata);
    };

    let db_ok = body.get("db").and_then(Value::as_bool);
    let dashboard_ok = body.get("dashboard").and_then(Value::as_bool);
    let detail = format!(
        "db={} dashboard={}",
        db_ok.map_or("unknown".to_string(), |ok| ok.to_string()),
        dashboard_ok.map_or("unknown".to_string(), |ok| ok.to_string())
    );
    let evidence = json!({
        "db": db_ok,
        "dashboard": dashboard_ok,
        "server_up": body.get("server_up").and_then(Value::as_bool)
    });

    match (db_ok, dashboard_ok) {
        (Some(true), Some(true)) => Check::ok(
            "health_db_dashboard",
            CheckGroup::Core,
            "DB/Dashboard Health",
            detail.clone(),
        )
        .with_subsystem("health")
        .with_path(health_detail_endpoint(&snapshot.base))
        .with_expected_actual("db=true dashboard=true", detail)
        .with_evidence(evidence),
        (Some(false), _) => Check::fail(
            "health_db_dashboard",
            CheckGroup::Core,
            "DB/Dashboard Health",
            detail.clone(),
            "DB health가 false입니다. Postgres/SQLite source-of-truth 상태를 먼저 확인하세요.",
        )
        .with_subsystem("health")
        .with_severity(Severity::Error)
        .with_path(health_detail_endpoint(&snapshot.base))
        .with_expected_actual("db=true", detail)
        .with_evidence(evidence)
        .with_security_exposure(SecurityExposure::OperationalMetadata),
        (_, Some(false)) => Check::warn(
            "health_db_dashboard",
            CheckGroup::Core,
            "DB/Dashboard Health",
            detail.clone(),
            "dashboard dist가 없거나 unreadable입니다. API는 동작하더라도 UI asset 배포 상태를 확인하세요.",
        )
        .with_subsystem("health")
        .with_path(health_detail_endpoint(&snapshot.base))
        .with_expected_actual("dashboard=true", detail)
        .with_evidence(evidence)
        .with_security_exposure(SecurityExposure::OperationalMetadata),
        _ => Check::warn(
            "health_db_dashboard",
            CheckGroup::Core,
            "DB/Dashboard Health",
            detail.clone(),
            "health detail payload가 DB/dashboard summary를 제공하지 않습니다.",
        )
        .with_subsystem("health")
        .with_path(health_detail_endpoint(&snapshot.base))
        .with_expected_actual("db/dashboard fields present", detail)
        .with_evidence(evidence),
    }
}

fn check_dispatch_outbox(snapshot: &HealthSnapshot) -> Check {
    let Some(body) = snapshot.body.as_ref() else {
        return Check::fail(
            "dispatch_outbox",
            CheckGroup::Core,
            "Dispatch Outbox",
            "health payload unavailable",
            "dispatch outbox health를 읽을 수 없습니다.",
        )
        .with_subsystem("health")
        .with_fix_safety(FixSafety::NotFixable);
    };
    let stats = body.get("dispatch_outbox");
    let pending = stats
        .and_then(|v| v.get("pending"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let retrying = stats
        .and_then(|v| v.get("retrying"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let permanent_failures = stats
        .and_then(|v| v.get("permanent_failures"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let oldest_pending_age = stats
        .and_then(|v| v.get("oldest_pending_age"))
        .and_then(Value::as_i64)
        .or_else(|| body.get("outbox_age").and_then(Value::as_i64))
        .unwrap_or(0);
    let detail = format!(
        "pending={pending} retrying={retrying} permanent_failures={permanent_failures} oldest_pending_age={oldest_pending_age}s"
    );
    let evidence = json!({
        "dispatch_outbox": {
            "pending": pending,
            "retrying": retrying,
            "permanent_failures": permanent_failures,
            "oldest_pending_age": oldest_pending_age
        }
    });

    if permanent_failures > 0 {
        Check::fail(
            "dispatch_outbox",
            CheckGroup::Core,
            "Dispatch Outbox",
            detail.clone(),
            "permanent dispatch outbox failure가 있습니다. delivery/follow-up 경로를 확인하세요.",
        )
        .with_subsystem("health")
        .with_severity(Severity::Error)
        .with_path(health_detail_endpoint(&snapshot.base))
        .with_expected_actual("no permanent outbox failures", detail)
        .with_evidence(evidence)
        .with_security_exposure(SecurityExposure::OperationalMetadata)
    } else if oldest_pending_age >= 60 || pending > 0 || retrying > 0 {
        Check::warn(
            "dispatch_outbox",
            CheckGroup::Core,
            "Dispatch Outbox",
            detail.clone(),
            "pending/retrying outbox가 남아 있습니다. oldest age가 증가하면 delivery worker를 확인하세요.",
        )
        .with_subsystem("health")
        .with_path(health_detail_endpoint(&snapshot.base))
        .with_expected_actual("empty or fresh outbox", detail)
        .with_evidence(evidence)
        .with_security_exposure(SecurityExposure::OperationalMetadata)
    } else {
        Check::ok(
            "dispatch_outbox",
            CheckGroup::Core,
            "Dispatch Outbox",
            detail.clone(),
        )
        .with_subsystem("health")
        .with_path(health_detail_endpoint(&snapshot.base))
        .with_expected_actual("outbox healthy", detail)
        .with_evidence(evidence)
    }
}

fn check_config_audit(snapshot: &HealthSnapshot) -> Check {
    let Some(body) = snapshot.body.as_ref() else {
        return Check::fail(
            "config_audit",
            CheckGroup::Core,
            "Config Audit",
            "health payload unavailable",
            "config audit report를 읽을 수 없습니다.",
        )
        .with_subsystem("config_audit")
        .with_fix_safety(FixSafety::NotFixable);
    };
    let Some(report) = body.get("config_audit") else {
        return Check::warn(
            "config_audit",
            CheckGroup::Core,
            "Config Audit",
            "no persisted config audit report in health detail",
            "dcserver startup config audit가 아직 실행되지 않았거나 persisted report가 없습니다.",
        )
        .with_subsystem("config_audit")
        .with_path(health_detail_endpoint(&snapshot.base))
        .with_expected_actual("config_audit report present", "missing");
    };
    let status = report
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let warnings_count = report
        .get("warnings_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let actions_count = report
        .get("actions")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let db = report.get("db").cloned().unwrap_or_else(|| json!({}));
    let evidence = json!({
        "status": status,
        "warnings_count": warnings_count,
        "actions_count": actions_count,
        "db": {
            "missing_agents": db.get("missing_agents").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
            "extra_agents": db.get("extra_agents").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
            "mismatched_agents": db.get("mismatched_agents").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
            "synced_agents": db.get("synced_agents").cloned().unwrap_or(Value::Null)
        }
    });
    let detail = format!("status={status} warnings={warnings_count} actions={actions_count}");
    if status == "ok" && warnings_count == 0 {
        Check::ok(
            "config_audit",
            CheckGroup::Core,
            "Config Audit",
            detail.clone(),
        )
        .with_subsystem("config_audit")
        .with_path(health_detail_endpoint(&snapshot.base))
        .with_expected_actual("config audit ok", detail)
        .with_evidence(evidence)
        .with_security_exposure(SecurityExposure::OperationalMetadata)
    } else {
        Check::warn(
            "config_audit",
            CheckGroup::Core,
            "Config Audit",
            detail.clone(),
            "agentdesk.yaml/legacy role map/bot settings drift summary를 확인하세요. public health에는 raw source path를 노출하지 않습니다.",
        )
        .with_subsystem("config_audit")
        .with_path(health_detail_endpoint(&snapshot.base))
        .with_expected_actual("config audit ok", detail)
        .with_evidence(evidence)
        .with_security_exposure(SecurityExposure::OperationalMetadata)
        .with_next_steps(vec![format!("curl -s {}", health_detail_endpoint(&snapshot.base))])
    }
}

fn check_provider_bindings(cfg: &config::Config, snapshot: &HealthSnapshot) -> Check {
    let mut health_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut disconnected = Vec::new();
    if let Some(providers) = health_providers(snapshot) {
        for entry in providers {
            if let Some(name) = entry.get("name").and_then(Value::as_str) {
                *health_counts.entry(name.to_string()).or_default() += 1;
                if entry.get("connected").and_then(Value::as_bool) == Some(false) {
                    disconnected.push(name.to_string());
                }
            }
        }
    }

    let duplicate_providers: Vec<String> = health_counts
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(name, count)| format!("{name}x{count}"))
        .collect();

    let mut bot_bound_agents = BTreeSet::new();
    let mut bot_bound_providers = BTreeSet::new();
    for bot in cfg.discord.bots.values() {
        if let Some(agent) = bot
            .agent
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            bot_bound_agents.insert(agent.to_string());
        }
        if let Some(provider) = bot
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            bot_bound_providers.insert(provider.to_string());
        }
    }

    let mut bindings = Vec::new();
    let mut missing_channels = Vec::new();
    let mut missing_runtime_providers = Vec::new();
    let mut missing_auth_hints = Vec::new();
    for agent in &cfg.agents {
        let mut agent_has_channel = false;
        for (slot_provider, channel) in agent.channels.iter() {
            let Some(channel) = channel else {
                continue;
            };
            let Some(target) = channel.target() else {
                continue;
            };
            agent_has_channel = true;
            let provider = channel
                .provider()
                .unwrap_or_else(|| slot_provider.to_string());
            let target_kind = if channel.channel_id().is_some() {
                "id"
            } else if channel.channel_name().is_some() {
                "name"
            } else {
                "alias"
            };
            if !health_counts.contains_key(&provider) {
                missing_runtime_providers.push(format!("{}:{provider}", agent.id));
            }
            let has_auth_hint =
                bot_bound_agents.contains(&agent.id) || bot_bound_providers.contains(&provider);
            if !has_auth_hint {
                missing_auth_hints.push(format!("{}:{provider}", agent.id));
            }
            bindings.push(json!({
                "agent_id": agent.id,
                "agent_provider": agent.provider,
                "channel_provider": provider,
                "target_kind": target_kind,
                "target": target,
                "has_bot_binding": has_auth_hint
            }));
        }
        if !agent_has_channel {
            missing_channels.push(format!("{}:{}", agent.id, agent.provider));
        }
    }

    missing_runtime_providers.sort();
    missing_runtime_providers.dedup();
    missing_auth_hints.sort();
    missing_auth_hints.dedup();
    disconnected.sort();
    disconnected.dedup();

    let has_duplicate_providers = !duplicate_providers.is_empty();
    let has_binding_issues = !disconnected.is_empty()
        || !missing_channels.is_empty()
        || !missing_runtime_providers.is_empty()
        || !missing_auth_hints.is_empty();
    let detail = format!(
        "bindings={} duplicate_providers={} disconnected={} missing_channels={} missing_runtime_providers={} missing_auth_hints={}",
        bindings.len(),
        duplicate_providers.len(),
        disconnected.len(),
        missing_channels.len(),
        missing_runtime_providers.len(),
        missing_auth_hints.len()
    );
    let evidence = json!({
        "bindings": bindings,
        "duplicate_providers": duplicate_providers,
        "disconnected_providers": disconnected,
        "missing_channels": missing_channels,
        "missing_runtime_providers": missing_runtime_providers,
        "missing_auth_hints": missing_auth_hints,
    });

    if has_duplicate_providers {
        Check::fail(
            "provider_bindings",
            CheckGroup::ProviderRuntime,
            "Provider Bindings",
            detail.clone(),
            "health registry에 duplicate provider entry가 있습니다. registration deduplication과 runtime bootstrap 로그를 확인하세요.",
        )
        .with_subsystem("provider_binding")
        .with_severity(Severity::Error)
        .with_security_exposure(SecurityExposure::OperationalMetadata)
        .with_evidence(evidence)
        .with_expected_actual("unique provider health entries", detail)
    } else if !has_binding_issues {
        Check::ok(
            "provider_bindings",
            CheckGroup::ProviderRuntime,
            "Provider Bindings",
            detail.clone(),
        )
        .with_subsystem("provider_binding")
        .with_expected_actual("provider/agent/channel bindings consistent", detail)
        .with_evidence(evidence)
        .with_security_exposure(SecurityExposure::OperationalMetadata)
    } else {
        Check::warn(
            "provider_bindings",
            CheckGroup::ProviderRuntime,
            "Provider Bindings",
            detail.clone(),
            "agent/provider/channel binding과 Discord bot auth hint를 분리해 확인하세요.",
        )
        .with_subsystem("provider_binding")
        .with_expected_actual("provider/agent/channel bindings consistent", detail)
        .with_evidence(evidence)
        .with_security_exposure(SecurityExposure::OperationalMetadata)
        .with_next_steps(vec![
            "agentdesk doctor --json".to_string(),
            format!("tail -n 200 {}", dcserver_log_hint()),
        ])
    }
}

#[derive(Debug)]
struct PermissionFinding {
    label: &'static str,
    path: String,
    exists: bool,
    mode: Option<String>,
    owner_is_current: Option<bool>,
    risk: Option<String>,
}

fn permission_finding(label: &'static str, path: &Path, sensitive: bool) -> PermissionFinding {
    let metadata = fs::metadata(path);
    let Ok(metadata) = metadata else {
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
        let mode = metadata.mode() & 0o777;
        let owner_is_current = metadata.uid() == unsafe { libc::geteuid() };
        let risk = if sensitive && mode & 0o077 != 0 {
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

fn check_credential_permissions(cfg: &config::Config) -> Check {
    let mut candidates: Vec<(&'static str, PathBuf, bool)> = Vec::new();
    if let Some(root) = config::runtime_root() {
        candidates.push((
            "agentdesk_yaml",
            crate::runtime_layout::config_file_path(&root),
            cfg.server
                .auth_token
                .as_deref()
                .is_some_and(|token| !token.trim().is_empty()),
        ));
        candidates.push((
            "discord_credential_dir",
            crate::runtime_layout::credential_dir(&root),
            true,
        ));
        let mut bot_names = cfg.discord.bots.keys().cloned().collect::<Vec<_>>();
        bot_names.sort();
        for bot_name in bot_names {
            let label = match bot_name.as_str() {
                "command" => "discord_command_token",
                "announce" => "discord_announce_token",
                "notify" => "discord_notify_token",
                _ => "discord_bot_token",
            };
            candidates.push((
                label,
                crate::runtime_layout::credential_token_path(&root, &bot_name),
                true,
            ));
        }
    }
    if let Some(home) = qwen_home_dir() {
        candidates.push((
            "qwen_oauth_cache",
            home.join(".qwen").join("oauth_creds.json"),
            true,
        ));
    }
    if let Some(project) = qwen_project_dir() {
        candidates.push(("qwen_project_env", project.join(".qwen").join(".env"), true));
        candidates.push(("project_env", project.join(".env"), true));
    }

    let findings = candidates
        .iter()
        .map(|(label, path, sensitive)| permission_finding(label, path, *sensitive))
        .collect::<Vec<_>>();
    let risks = findings
        .iter()
        .filter_map(|finding| {
            finding
                .risk
                .as_ref()
                .map(|risk| format!("{}: {risk}", finding.label))
        })
        .collect::<Vec<_>>();
    let existing = findings.iter().filter(|finding| finding.exists).count();
    let evidence = json!({
        "checked": findings.iter().map(|finding| json!({
            "label": finding.label,
            "path": finding.path.clone(),
            "exists": finding.exists,
            "mode": finding.mode.clone(),
            "owner_is_current": finding.owner_is_current,
            "risk": finding.risk.clone(),
        })).collect::<Vec<_>>(),
        "risk_count": risks.len(),
    });
    let detail = format!(
        "checked={} existing={} risks={}",
        findings.len(),
        existing,
        risks.len()
    );
    if risks.is_empty() {
        Check::ok(
            "credential_permissions",
            CheckGroup::ProviderRuntime,
            "Credential Permissions",
            detail.clone(),
        )
        .with_subsystem("security")
        .with_expected_actual("no credential permission risks", detail)
        .with_evidence(evidence)
        .with_security_exposure(SecurityExposure::CredentialMetadata)
    } else {
        Check::warn(
            "credential_permissions",
            CheckGroup::ProviderRuntime,
            "Credential Permissions",
            format!("{detail}; {}", risks.join("; ")),
            "credential/config 파일 내용은 읽거나 출력하지 않고 권한/owner metadata만 점검했습니다.",
        )
        .with_subsystem("security")
        .with_expected_actual("credential files owned by current user with private permissions", detail)
        .with_evidence(evidence)
        .with_security_exposure(SecurityExposure::CredentialMetadata)
        .with_next_steps(vec![
            "chmod 700 ~/.adk/release/credential".to_string(),
            "chmod 600 <credential-file>".to_string(),
        ])
    }
}

fn build_all_checks(cfg: &config::Config, snapshot: &HealthSnapshot) -> Vec<Check> {
    let mut checks = build_core_checks(cfg, snapshot);
    checks.extend(build_provider_checks(cfg, snapshot));
    checks
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

fn build_json_report(
    options: &DoctorOptions,
    checks: &[Check],
    actions: &[FixAction],
) -> DoctorReport {
    let summary = summarize_checks(checks);
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
        checks,
        auto_fixes: if matches!(options.run_context, RunContext::StartupOnce) {
            fixes.clone()
        } else {
            Vec::new()
        },
        fixes,
    }
}

fn print_group(title: &str, checks: &[Check]) {
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
    }
    println!();
}

fn apply_safe_fixes(cfg: &config::Config, options: &DoctorOptions) -> Vec<FixAction> {
    let mut actions = Vec::new();

    match dcserver::agentdesk_runtime_root() {
        Some(root) => {
            let dirs = [
                root.clone(),
                root.join("config"),
                root.join("logs"),
                root.join("releases"),
                crate::runtime_layout::credential_dir(&root),
            ];
            let mut failed = None;
            for dir in dirs {
                if let Err(e) = fs::create_dir_all(&dir) {
                    failed = Some(format!("{}: {}", dir.display(), e));
                    break;
                }
            }
            match failed {
                Some(detail) => {
                    actions.push(FixAction::fail("runtime_layout", "Runtime Layout", detail))
                }
                None => actions.push(FixAction::ok(
                    "runtime_layout",
                    "Runtime Layout",
                    format!("ensured runtime directories under {}", root.display()),
                )),
            }
        }
        None => actions.push(FixAction::fail(
            "runtime_layout",
            "Runtime Layout",
            "unable to determine runtime root",
        )),
    }

    match fs::create_dir_all(&cfg.data.dir) {
        Ok(()) => actions.push(FixAction::ok(
            "data_directory",
            "Data Directory",
            format!("ensured {}", cfg.data.dir.display()),
        )),
        Err(e) => actions.push(FixAction::fail(
            "data_directory",
            "Data Directory",
            format!("{}: {}", cfg.data.dir.display(), e),
        )),
    }

    let db_path = cfg.data.dir.join(&cfg.data.db_name);
    if !options.repair_sqlite_cache {
        actions.push(FixAction::skipped(
            "db_schema",
            "DB Schema",
            format!("skipped SQLite schema repair at {}", db_path.display()),
            FixSafety::ExplicitDbRepairRequired,
            "rerun with --fix --repair-sqlite-cache to allow SQLite schema mutation",
        ));
    } else {
        actions.push(
            FixAction::skipped(
                "db_schema",
                "DB Schema",
                format!(
                    "legacy SQLite schema repair retired at {}; Postgres is source-of-truth",
                    db_path.display()
                ),
                FixSafety::ExplicitDbRepairRequired,
                "restore the retired cutover tooling from history only for an approved emergency re-cutover",
            )
            .with_safety_gate("explicit_db_repair_allowed"),
        );
    }

    if !options.repair_sqlite_cache {
        actions.push(FixAction::skipped(
            "stale_db_files",
            "Stale DB Files",
            "skipped stale SQLite cache cleanup",
            FixSafety::ExplicitDbRepairRequired,
            "rerun with --fix --repair-sqlite-cache to remove stale SQLite files",
        ));
        return actions;
    }

    match dcserver::agentdesk_runtime_root() {
        Some(root) => {
            let stale_paths = stale_zero_byte_db_candidates(&root, &db_path);
            if stale_paths.is_empty() {
                actions.push(
                    FixAction::ok(
                        "stale_db_files",
                        "Stale DB Files",
                        "no stale zero-byte DB files found".to_string(),
                    )
                    .with_safety_gate("explicit_db_repair_allowed"),
                );
            } else {
                let mut removed = Vec::new();
                let mut failed = None;
                for path in stale_paths {
                    match fs::remove_file(&path) {
                        Ok(()) => removed.push(path.display().to_string()),
                        Err(error) => {
                            failed = Some(format!("{}: {}", path.display(), error));
                            break;
                        }
                    }
                }
                match failed {
                    Some(detail) => {
                        actions.push(FixAction::fail("stale_db_files", "Stale DB Files", detail))
                    }
                    None => actions.push(
                        FixAction::ok(
                            "stale_db_files",
                            "Stale DB Files",
                            format!("removed {}", removed.join(", ")),
                        )
                        .with_safety_gate("explicit_db_repair_allowed"),
                    ),
                }
            }
        }
        None => actions.push(FixAction::fail(
            "stale_db_files",
            "Stale DB Files",
            "unable to determine runtime root",
        )),
    }

    actions
}

fn snapshot_is_healthy(snapshot: &HealthSnapshot) -> bool {
    let Some(body) = snapshot.body.as_ref() else {
        return false;
    };

    if let Some(status) = body.get("status").and_then(Value::as_str) {
        return status == "healthy";
    }

    let ok = body.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let db = body.get("db").and_then(Value::as_bool).unwrap_or(false);
    ok && db
}

fn apply_service_fix(snapshot: &HealthSnapshot, options: &DoctorOptions) -> Vec<FixAction> {
    const READY_TIMEOUT: Duration = Duration::from_secs(30);

    if snapshot_is_healthy(snapshot) {
        return Vec::new();
    }

    if !options.allow_restart {
        return vec![FixAction::skipped(
            "service_restart",
            "Service Restart",
            "skipped dcserver service restart",
            FixSafety::ExplicitRestartRequired,
            "rerun with --fix --allow-restart to permit service restart",
        )];
    }

    #[cfg(target_os = "macos")]
    {
        let label = dcserver::current_dcserver_launchd_label();
        if dcserver::is_launchd_job_loaded(&label) {
            return vec![
                match dcserver::restart_launchd_dcserver_and_verify(&label, READY_TIMEOUT) {
                    Ok(()) => FixAction::ok(
                        "service_restart",
                        "Service Restart",
                        format!("launchd kickstart succeeded for {label}"),
                    ),
                    Err(e) => FixAction::fail(
                        "service_restart",
                        "Service Restart",
                        format!("launchd kickstart failed for {label}: {e}"),
                    ),
                },
            ];
        }
        return Vec::new();
    }

    #[cfg(target_os = "linux")]
    {
        if dcserver::is_systemd_service_enabled() || dcserver::is_systemd_service_active() {
            return vec![
                match dcserver::restart_systemd_dcserver_and_verify(READY_TIMEOUT) {
                    Ok(()) => FixAction::ok(
                        "service_restart",
                        "Service Restart",
                        "systemd --user restart succeeded for agentdesk-dcserver",
                    ),
                    Err(e) => FixAction::fail(
                        "service_restart",
                        "Service Restart",
                        format!("systemd --user restart failed: {e}"),
                    ),
                },
            ];
        }
        return Vec::new();
    }

    #[cfg(target_os = "windows")]
    {
        if dcserver::is_windows_service_installed() {
            return vec![
                match dcserver::restart_windows_dcserver_and_verify(READY_TIMEOUT) {
                    Ok(()) => FixAction::ok(
                        "service_restart",
                        "Service Restart",
                        "Windows service restart succeeded for AgentDeskDcserver",
                    ),
                    Err(e) => FixAction::fail(
                        "service_restart",
                        "Service Restart",
                        format!("Windows service restart failed: {e}"),
                    ),
                },
            ];
        }
        return Vec::new();
    }

    #[allow(unreachable_code)]
    Vec::new()
}

fn stale_mailbox_repair_response_status(response: &Value) -> &str {
    response
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            if response.get("ok").and_then(Value::as_bool) == Some(true) {
                "applied"
            } else if response.get("skipped").and_then(Value::as_bool) == Some(true)
                || response.get("safety_gate").is_some()
            {
                "skipped"
            } else {
                "partial_repair"
            }
        })
}

fn stale_mailbox_repair_safety_gate(response: &Value) -> &'static str {
    match response
        .get("safety_gate")
        .and_then(Value::as_str)
        .unwrap_or("repair_skipped")
    {
        "mailbox_not_found" => "mailbox_not_found",
        "expected_evidence_mismatch" => "expected_evidence_mismatch",
        "queue_not_empty" => "queue_not_empty",
        "active_dispatch_present" => "active_dispatch_present",
        "tmux_present" => "tmux_present",
        _ => "repair_skipped",
    }
}

fn stale_mailbox_repair_fix_safety(response: &Value) -> FixSafety {
    match response.get("fix_safety").and_then(Value::as_str) {
        Some("explicit_restart_required") => FixSafety::ExplicitRestartRequired,
        Some("explicit_db_repair_required") => FixSafety::ExplicitDbRepairRequired,
        Some("not_fixable") => FixSafety::NotFixable,
        Some("read_only") => FixSafety::ReadOnly,
        _ => FixSafety::SafeLocalRepair,
    }
}

fn apply_stale_mailbox_fixes(snapshot: &HealthSnapshot, options: &DoctorOptions) -> Vec<FixAction> {
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
                    "live tmux/process/dispatch/queue evidence present",
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
            match crate::cli::client::post_json_value("/api/doctor/stale-mailbox/repair", request)
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

fn print_fix_actions(actions: &[FixAction]) {
    if actions.is_empty() {
        return;
    }

    println!("Applying safe fixes");
    for action in actions {
        let label = if action.skipped {
            "SKIPPED"
        } else if action.ok {
            "APPLIED"
        } else {
            "FAILED"
        };
        let icon = if action.skipped {
            "!"
        } else if action.ok {
            "✓"
        } else {
            "✗"
        };
        println!("  {icon} [{label}] {}: {}", action.name, action.detail);
        if let Some(reason) = &action.skipped_reason {
            println!("      → {}", reason);
        }
    }
    println!();
}

fn discord_bot_check_from_health(base: &str, body: &Value) -> Check {
    if let Some(providers) = body.get("providers").and_then(Value::as_array) {
        let total = providers.len();
        let connected: Vec<String> = providers
            .iter()
            .filter(|provider| provider.get("connected").and_then(Value::as_bool) == Some(true))
            .filter_map(|provider| provider.get("name").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        let disconnected: Vec<String> = providers
            .iter()
            .filter(|provider| provider.get("connected").and_then(Value::as_bool) != Some(true))
            .filter_map(|provider| provider.get("name").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        let overall = body
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if total > 0 && connected.len() == total && overall == "healthy" {
            return Check::ok(
                "discord_bot",
                CheckGroup::Core,
                "Discord Bot",
                format!(
                    "{}/{} connected — {}",
                    connected.len(),
                    total,
                    connected.join(", ")
                ),
            )
            .with_path(health_endpoint(base))
            .with_expected_actual(
                "all registered providers connected",
                format!("{}/{} connected", connected.len(), total),
            );
        }
        let reasons = health::degraded_reasons(body);
        let provider_reasons: Vec<_> = reasons
            .iter()
            .filter(|reason| reason.subsystem == "provider_runtime")
            .cloned()
            .collect();
        if total > 0 && connected.len() == total && !provider_reasons.is_empty() {
            let detail = provider_reasons
                .iter()
                .map(|reason| reason.summary.clone())
                .collect::<Vec<_>>()
                .join("; ");
            return Check::warn(
                "discord_bot",
                CheckGroup::Core,
                "Discord Bot",
                format!("overall={overall}, connected={total}/{total}; {detail}"),
                "모든 provider가 connected 상태이므로 token/offline 안내 대신 degraded reason을 확인하세요.",
            )
            .with_subsystem("provider_runtime")
            .with_severity(highest_reason_severity(&provider_reasons))
            .with_fix_safety(FixSafety::ReadOnly)
            .with_security_exposure(SecurityExposure::OperationalMetadata)
            .with_evidence(health::reasons_evidence(&provider_reasons))
            .with_path(health_detail_endpoint(base))
            .with_expected_actual("all providers connected and no provider degraded reasons", detail)
            .with_next_steps(
                provider_reasons
                    .iter()
                    .map(|reason| reason.next_step.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
            );
        }
        if total == 0 {
            return Check::warn(
                "discord_bot",
                CheckGroup::Core,
                "Discord Bot",
                format!("no providers registered in unified health payload — {base}"),
                "dcserver가 아직 provider를 등록하지 못했을 수 있습니다. startup 로그와 bot token 구성을 확인하세요.",
            )
            .with_path(health_endpoint(base))
            .with_expected_actual("provider registry populated", "providers=0")
            .with_next_steps(vec![
                format!("tail -n 200 {}", dcserver_log_hint()),
                "agentdesk doctor --fix".to_string(),
            ]);
        }
        return Check::warn(
            "discord_bot",
            CheckGroup::Core,
            "Discord Bot",
            format!(
                "overall={overall}, connected={}/{}, offline={}",
                connected.len(),
                total,
                if disconnected.is_empty() {
                    "-".to_string()
                } else {
                    disconnected.join(", ")
                }
            ),
            "오프라인 provider의 Discord token, gateway 연결 상태, dcserver stdout 로그를 확인하세요.",
        )
        .with_subsystem("provider_runtime")
        .with_security_exposure(SecurityExposure::OperationalMetadata)
        .with_path(health_endpoint(base))
        .with_expected_actual(
            "all registered providers connected",
            format!(
                "overall={overall}, connected={}/{}, offline={}",
                connected.len(),
                total,
                if disconnected.is_empty() {
                    "-".to_string()
                } else {
                    disconnected.join(", ")
                }
            ),
        )
        .with_next_steps(vec![
            format!("tail -n 200 {}", dcserver_log_hint()),
            "agentdesk doctor --fix".to_string(),
        ]);
    }

    let ok = body.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let db = body.get("db").and_then(Value::as_bool).unwrap_or(false);
    if ok && db {
        Check::warn(
            "discord_bot",
            CheckGroup::Core,
            "Discord Bot",
            format!("standalone health only — provider status unavailable at {base}"),
            "현재 서버는 응답하지만 Discord provider health registry는 비어 있습니다. standalone 실행 중인지 확인하세요.",
        )
        .with_path(health_endpoint(base))
        .with_expected_actual("unified provider registry available", "standalone health payload only")
        .with_next_steps(vec![
            format!("curl -s {}", health_endpoint(base)),
            format!("tail -n 200 {}", dcserver_log_hint()),
        ])
    } else {
        Check::fail(
            "discord_bot",
            CheckGroup::Core,
            "Discord Bot",
            format!("server unhealthy or provider data missing: ok={ok} db={db}"),
            "서버가 떠 있더라도 Discord provider 초기화가 실패했을 수 있습니다. dcserver stdout 로그를 확인하세요.",
        )
        .with_path(health_endpoint(base))
        .with_expected_actual("healthy server with provider registry", format!("ok={ok} db={db}"))
        .with_next_steps(vec![
            "agentdesk doctor --fix".to_string(),
            format!("tail -n 200 {}", dcserver_log_hint()),
        ])
    }
}

fn check_discord_bot(snapshot: &HealthSnapshot) -> Check {
    match snapshot.body.as_ref() {
        Some(body) => discord_bot_check_from_health(&snapshot.base, body),
        None => Check::fail(
            "discord_bot",
            CheckGroup::Core,
            "Discord Bot",
            format!(
                "unreachable ({})",
                snapshot
                    .error
                    .clone()
                    .unwrap_or_else(|| "unknown error".to_string())
            ),
            "dcserver가 실행 중인지, /api/health가 접근 가능한지 확인하세요.",
        )
        .with_path(health_endpoint(&snapshot.base))
        .with_expected_actual("reachable health endpoint", "health endpoint unreachable")
        .with_next_steps(vec![
            "agentdesk doctor --fix".to_string(),
            format!("curl -s {}", health_endpoint(&snapshot.base)),
        ]),
    }
}

fn check_tmux() -> Check {
    match crate::services::platform::tmux::version() {
        Ok(ver) => Check::ok("tmux", CheckGroup::Core, "tmux", ver)
            .with_path("tmux")
            .with_expected_actual("tmux available in PATH", "tmux available"),
        Err(_) => Check::warn(
            "tmux",
            CheckGroup::Core,
            "tmux",
            "not found in PATH",
            "Claude/Codex tmux backend를 쓸 계획이면 tmux를 설치하세요.",
        )
        .with_path("tmux")
        .with_expected_actual("tmux available in PATH", "tmux not found")
        .with_next_steps(vec!["which tmux".to_string()]),
    }
}

fn provider_capability_summary(provider: &ProviderKind) -> String {
    provider
        .capabilities()
        .map(|caps| {
            let mut parts = Vec::new();
            if caps.supports_structured_output {
                parts.push("structured-output");
            }
            if caps.supports_resume {
                parts.push("resume");
            }
            if caps.supports_tool_stream {
                parts.push("tool-stream");
            }
            parts.join(", ")
        })
        .unwrap_or_else(|| "unsupported".to_string())
}

fn check_provider_cli(
    provider: ProviderKind,
    configured: bool,
    snapshot: &HealthSnapshot,
) -> Check {
    let id = provider_check_id(&provider);
    let name = match provider {
        ProviderKind::Claude => "claude CLI",
        ProviderKind::Codex => "codex CLI",
        ProviderKind::Gemini => "gemini CLI",
        ProviderKind::OpenCode => "opencode CLI",
        ProviderKind::Qwen => "qwen CLI",
        ProviderKind::Unsupported(_) => "provider CLI",
    };
    let capability_summary = provider_capability_summary(&provider);
    let connected = provider_connected(snapshot, &provider);
    let log_hint = dcserver_log_hint();
    let binary_name = provider.as_str().to_string();
    match provider.probe_runtime() {
        Some(probe) => match (probe.resolution.resolved_path.clone(), probe.version) {
            (Some(path), Some(ver)) => {
                let health_note = match connected {
                    Some(true) => "health=connected".to_string(),
                    Some(false) => "health=disconnected".to_string(),
                    None => "health=unknown".to_string(),
                };
                let source = probe
                    .resolution
                    .source
                    .as_deref()
                    .unwrap_or("unknown_source");
                let detail =
                    format!("{ver} — {path} [{source}; {capability_summary}; {health_note}]");

                if !configured {
                    Check::warn(
                        id,
                        CheckGroup::ProviderRuntime,
                        name,
                        format!("{detail} — installed but not referenced by current config/health"),
                        provider_unused_guidance(&provider),
                    )
                    .with_path(path)
                    .with_expected_actual(
                        "provider referenced by config or health registry",
                        "binary exists but provider not referenced",
                    )
                    .with_next_steps(vec![
                        "agentdesk doctor --json".to_string(),
                        format!("tail -n 200 {}", log_hint),
                    ])
                } else if connected == Some(false) {
                    Check::warn(
                        id,
                        CheckGroup::ProviderRuntime,
                        name,
                        detail,
                        provider_runtime_guidance(&provider),
                    )
                    .with_path(path)
                    .with_expected_actual("provider connected", "provider disconnected")
                    .with_next_steps(vec![
                        format!("which {}", binary_name),
                        format!("tail -n 200 {}", log_hint),
                    ])
                } else {
                    Check::ok(id, CheckGroup::ProviderRuntime, name, detail)
                        .with_path(path)
                        .with_expected_actual("provider binary usable", "provider binary usable")
                }
            }
            (Some(path), None) => {
                let source = probe
                    .resolution
                    .source
                    .as_deref()
                    .unwrap_or("unknown_source");
                let probe_failure_kind = probe
                    .probe_failure_kind
                    .clone()
                    .unwrap_or_else(|| "version_probe_failed".to_string());
                let detail = format!(
                    "{path} — version probe failed [{source}; {probe_failure_kind}; {capability_summary}]"
                );
                if configured {
                    Check::fail(
                        id,
                        CheckGroup::ProviderRuntime,
                        name,
                        detail,
                        provider_runtime_guidance(&provider),
                    )
                    .with_path(path)
                    .with_expected_actual("provider version probe succeeds", "version probe failed")
                    .with_next_steps(vec![
                        format!("which {}", binary_name),
                        format!("tail -n 200 {}", log_hint),
                    ])
                } else {
                    Check::warn(
                        id,
                        CheckGroup::ProviderRuntime,
                        name,
                        detail,
                        provider_unused_guidance(&provider),
                    )
                    .with_path(path)
                    .with_expected_actual(
                        "provider version probe succeeds",
                        "version probe failed for unused provider",
                    )
                }
            }
            (None, Some(ver)) => Check::ok(
                id,
                CheckGroup::ProviderRuntime,
                name,
                format!("{ver} — unknown path [{capability_summary}]"),
            )
            .with_expected_actual("provider path known", "version known but path unknown"),
            (None, None) => {
                let failure_kind = probe
                    .resolution
                    .failure_kind
                    .clone()
                    .unwrap_or_else(|| "not_found".to_string());
                if configured {
                    Check::fail(
                        id,
                        CheckGroup::ProviderRuntime,
                        name,
                        format!("not found in runtime PATH [{failure_kind}; {capability_summary}]"),
                        provider_runtime_guidance(&provider),
                    )
                    .with_expected_actual(
                        "provider binary exists in runtime PATH",
                        "provider binary not found in runtime PATH",
                    )
                    .with_next_steps(vec![
                        "echo $PATH".to_string(),
                        format!("which {}", binary_name),
                        format!("tail -n 200 {}", log_hint),
                    ])
                } else {
                    Check::ok(
                        id,
                        CheckGroup::ProviderRuntime,
                        name,
                        format!("not configured [{capability_summary}]"),
                    )
                    .with_expected_actual(
                        "provider configured if needed",
                        "provider not configured",
                    )
                }
            }
        },
        None => Check::fail(
            id,
            CheckGroup::ProviderRuntime,
            name,
            "unsupported provider",
            "지원되지 않는 provider입니다.",
        )
        .with_expected_actual("supported provider", "unsupported provider"),
    }
}

fn check_runtime_path() -> Check {
    let current = std::env::var("PATH").unwrap_or_default();
    match crate::services::platform::merged_runtime_path() {
        Some(merged) if merged == current => Check::ok(
            "runtime_path",
            CheckGroup::ProviderRuntime,
            "Runtime PATH",
            "current PATH already matches provider runtime PATH",
        )
        .with_expected_actual("runtime PATH resolved", "runtime PATH matches current PATH"),
        Some(merged) => {
            let entry_count = std::env::split_paths(&merged).count();
            Check::ok(
                "runtime_path",
                CheckGroup::ProviderRuntime,
                "Runtime PATH",
                format!(
                    "provider subprocesses will use merged login-shell PATH ({entry_count} entries)"
                ),
            )
            .with_expected_actual(
                "runtime PATH resolved",
                format!("merged runtime PATH with {entry_count} entries"),
            )
        }
        None => Check::fail(
            "runtime_path",
            CheckGroup::ProviderRuntime,
            "Runtime PATH",
            "unable to resolve provider runtime PATH",
            "login shell PATH를 읽지 못했습니다. 서비스 환경 PATH와 shell PATH를 비교하세요.",
        )
        .with_expected_actual("runtime PATH resolved", "runtime PATH resolution failed")
        .with_next_steps(vec!["echo $PATH".to_string()]),
    }
}

fn check_server_running(snapshot: &HealthSnapshot) -> Check {
    match snapshot.body.as_ref() {
        Some(body) => {
            let ver = body
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let status = body
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    let ok = body.get("ok").and_then(Value::as_bool).unwrap_or(false);
                    let db = body.get("db").and_then(Value::as_bool).unwrap_or(false);
                    Some(if ok && db { "healthy" } else { "degraded" }.to_string())
                })
                .unwrap_or_else(|| "unknown".to_string());
            let detail = format!("{status} v{ver} on {}", snapshot.base);
            if status == "healthy" {
                Check::ok("server", CheckGroup::Core, "Server", detail)
                    .with_path(health_endpoint(&snapshot.base))
                    .with_expected_actual(
                        "reachable healthy health endpoint",
                        format!("status={status}"),
                    )
            } else {
                let reasons = health::degraded_reasons(body);
                let reason_detail = if reasons.is_empty() {
                    detail.clone()
                } else {
                    format!(
                        "{}; reasons={}",
                        detail,
                        reasons
                            .iter()
                            .map(|reason| reason.raw.clone())
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                };
                Check::fail(
                    "server",
                    CheckGroup::Core,
                    "Server",
                    reason_detail.clone(),
                    "health endpoint는 응답했지만 서비스 상태가 healthy가 아닙니다. degraded reason별 subsystem을 먼저 확인하세요.",
                )
                .with_subsystem("health")
                .with_severity(highest_reason_severity(&reasons))
                .with_fix_safety(FixSafety::ReadOnly)
                .with_security_exposure(SecurityExposure::OperationalMetadata)
                .with_evidence(health::reasons_evidence(&reasons))
                .with_path(health_endpoint(&snapshot.base))
                .with_expected_actual(
                    "reachable healthy health endpoint",
                    reason_detail,
                )
                .with_next_steps(vec![
                    format!("curl -s {}", health_detail_endpoint(&snapshot.base)),
                    format!("tail -n 200 {}", dcserver_log_hint()),
                ])
            }
        }
        None => {
            let error = snapshot
                .error
                .clone()
                .unwrap_or_else(|| "unknown error".to_string());
            let (status, severity, guidance) = if error.contains("(401)") || error.contains("(403)")
            {
                (
                    "unauthorized",
                    Severity::Error,
                    "auth token 또는 /api/health/detail 권한을 확인하세요.",
                )
            } else if error.contains("--allow-remote") {
                (
                    "blocked_remote_token",
                    Severity::Critical,
                    "non-loopback URL에 token을 보내려면 명시적으로 --allow-remote를 사용하세요.",
                )
            } else {
                (
                    "unreachable",
                    Severity::Error,
                    "dcserver/axum 서버가 떠 있는지와 방화벽/포트 접근 가능 여부를 확인하세요.",
                )
            };
            Check::fail(
                "server",
                CheckGroup::Core,
                "Server",
                format!("{status} — {} ({error})", snapshot.base),
                guidance,
            )
            .with_subsystem("server")
            .with_severity(severity)
            .with_fix_safety(FixSafety::ExplicitRestartRequired)
            .with_security_exposure(SecurityExposure::OperationalMetadata)
            .with_path(health_detail_endpoint(&snapshot.base))
            .with_expected_actual("reachable health endpoint", error)
            .with_next_steps(vec![
                "agentdesk doctor --fix --allow-restart".to_string(),
                format!("curl -s {}", health_endpoint(&snapshot.base)),
            ])
        }
    }
}

fn check_runtime_root() -> Check {
    match dcserver::agentdesk_runtime_root() {
        Some(path) if path.exists() && path.is_dir() => Check::ok(
            "runtime_root",
            CheckGroup::Core,
            "Runtime Root",
            format!("{}", path.display()),
        )
        .with_path(path.display().to_string())
        .with_expected_actual("runtime root exists", "runtime root exists"),
        Some(path) => Check::fail(
            "runtime_root",
            CheckGroup::Core,
            "Runtime Root",
            format!("{} — missing", path.display()),
            "agentdesk doctor --fix 로 기본 runtime 디렉터리를 생성할 수 있습니다.",
        )
        .with_path(path.display().to_string())
        .with_expected_actual("runtime root exists", "runtime root missing")
        .with_next_steps(vec!["agentdesk doctor --fix".to_string()]),
        None => Check::fail(
            "runtime_root",
            CheckGroup::Core,
            "Runtime Root",
            "unable to determine runtime root",
            "AGENTDESK_ROOT_DIR 또는 기본 ~/.adk/release 경로를 확인하세요.",
        )
        .with_expected_actual(
            "runtime root path resolvable",
            "runtime root path unresolved",
        ),
    }
}

fn check_degraded_reasons(snapshot: &HealthSnapshot) -> Check {
    let Some(body) = snapshot.body.as_ref() else {
        return Check::fail(
            "health_degraded_reasons",
            CheckGroup::Core,
            "Health Reasons",
            snapshot
                .error
                .clone()
                .unwrap_or_else(|| "health endpoint unavailable".to_string()),
            "health endpoint에 접근할 수 없어 degraded reason을 분류하지 못했습니다.",
        )
        .with_subsystem("health")
        .with_security_exposure(SecurityExposure::OperationalMetadata)
        .with_fix_safety(FixSafety::NotFixable);
    };
    let reasons = health::degraded_reasons(body);
    if reasons.is_empty() {
        return Check::ok(
            "health_degraded_reasons",
            CheckGroup::Core,
            "Health Reasons",
            "no degraded reasons",
        )
        .with_subsystem("health")
        .with_path(health_detail_endpoint(&snapshot.base))
        .with_expected_actual("degraded_reasons classified", "none");
    }

    let detail = reasons
        .iter()
        .map(|reason| reason.summary.clone())
        .collect::<Vec<_>>()
        .join("; ");
    let next_steps = reasons
        .iter()
        .map(|reason| reason.next_step.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let status = if reasons
        .iter()
        .any(|reason| matches!(reason.severity, Severity::Error | Severity::Critical))
    {
        CheckStatus::Fail
    } else {
        CheckStatus::Warn
    };
    let mut check = match status {
        CheckStatus::Pass => unreachable!(),
        CheckStatus::Warn => Check::warn(
            "health_degraded_reasons",
            CheckGroup::Core,
            "Health Reasons",
            detail.clone(),
            "health degraded reason을 subsystem별로 확인하세요.",
        ),
        CheckStatus::Fail => Check::fail(
            "health_degraded_reasons",
            CheckGroup::Core,
            "Health Reasons",
            detail.clone(),
            "error/critical degraded reason을 먼저 해결하세요.",
        ),
    };
    check = check
        .with_subsystem(
            reasons
                .first()
                .map(|reason| reason.subsystem)
                .unwrap_or("health"),
        )
        .with_severity(highest_reason_severity(&reasons))
        .with_evidence(health::reasons_evidence(&reasons))
        .with_security_exposure(SecurityExposure::OperationalMetadata)
        .with_fix_safety(FixSafety::ReadOnly)
        .with_expected_actual("no degraded reasons", detail)
        .with_next_steps(next_steps);
    check
}

fn check_mailbox_consistency(snapshot: &HealthSnapshot) -> Vec<Check> {
    let Some(body) = snapshot.body.as_ref() else {
        return Vec::new();
    };
    mailbox::classify_mailbox_findings(body)
        .into_iter()
        .map(|finding| {
            let fix_safety = if finding.live_work_present {
                FixSafety::ExplicitRestartRequired
            } else {
                FixSafety::SafeLocalRepair
            };
            Check::fail(
                finding.id,
                CheckGroup::Core,
                "Turn Mailbox Consistency",
                finding.detail,
                if finding.live_work_present {
                    "live work evidence가 있으므로 자동 정리를 건너뛰고 operator 확인이 필요합니다."
                } else {
                    "live work evidence가 없으면 protected stale-mailbox repair를 적용할 수 있습니다."
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

fn check_data_dir(cfg: &config::Config) -> Check {
    if cfg.data.dir.exists() && cfg.data.dir.is_dir() {
        Check::ok(
            "data_directory",
            CheckGroup::Core,
            "Data Directory",
            format!("{}", cfg.data.dir.display()),
        )
        .with_path(cfg.data.dir.display().to_string())
        .with_expected_actual("data directory exists", "data directory exists")
    } else {
        Check::fail(
            "data_directory",
            CheckGroup::Core,
            "Data Directory",
            format!("{} — missing", cfg.data.dir.display()),
            "agentdesk doctor --fix 로 data 디렉터리와 DB를 생성할 수 있습니다.",
        )
        .with_path(cfg.data.dir.display().to_string())
        .with_expected_actual("data directory exists", "data directory missing")
        .with_next_steps(vec!["agentdesk doctor --fix".to_string()])
    }
}

#[cfg(target_os = "macos")]
fn check_service_manager() -> Check {
    let label = dcserver::current_dcserver_launchd_label();
    if dcserver::is_launchd_job_loaded(&label) {
        Check::ok(
            "service_manager",
            CheckGroup::Core,
            "Service Manager",
            format!("launchd — {label} loaded"),
        )
        .with_expected_actual("launchd job loaded", format!("{label} loaded"))
    } else {
        Check::warn(
            "service_manager",
            CheckGroup::Core,
            "Service Manager",
            format!("launchd — {label} not loaded"),
            "launchd로 운영 중이면 plist 로드 상태를 확인하세요. 수동 실행 환경이면 무시해도 됩니다.",
        )
        .with_expected_actual("launchd job loaded", format!("{label} not loaded"))
        .with_next_steps(vec![
            format!("launchctl print gui/$(id -u)/{label}"),
            "agentdesk doctor --fix".to_string(),
        ])
    }
}

#[cfg(target_os = "linux")]
fn check_service_manager() -> Check {
    let active = dcserver::is_systemd_service_active();
    let enabled = dcserver::is_systemd_service_enabled();
    if active {
        Check::ok(
            "service_manager",
            CheckGroup::Core,
            "Service Manager",
            "systemd --user — agentdesk-dcserver active",
        )
        .with_expected_actual("systemd user service active", "systemd user service active")
    } else if enabled {
        Check::warn(
            "service_manager",
            CheckGroup::Core,
            "Service Manager",
            "systemd --user — agentdesk-dcserver enabled but inactive",
            "`systemctl --user status agentdesk-dcserver` 로 상태를 확인하거나 `agentdesk doctor --fix`로 restart를 시도하세요.",
        )
        .with_expected_actual("systemd user service active", "systemd user service enabled but inactive")
        .with_next_steps(vec![
            "systemctl --user status agentdesk-dcserver".to_string(),
            "agentdesk doctor --fix".to_string(),
        ])
    } else {
        Check::warn(
            "service_manager",
            CheckGroup::Core,
            "Service Manager",
            "systemd --user — agentdesk-dcserver not enabled",
            "서비스로 운영할 계획이면 systemd user service 등록 여부를 확인하세요.",
        )
        .with_expected_actual(
            "systemd user service enabled",
            "systemd user service not enabled",
        )
        .with_next_steps(vec![
            "systemctl --user status agentdesk-dcserver".to_string(),
        ])
    }
}

#[cfg(target_os = "windows")]
fn check_service_manager() -> Check {
    let installed = dcserver::is_windows_service_installed();
    let running = dcserver::is_windows_service_running();
    if running {
        Check::ok(
            "service_manager",
            CheckGroup::Core,
            "Service Manager",
            "Windows service — AgentDeskDcserver running",
        )
        .with_expected_actual("Windows service running", "Windows service running")
    } else if installed {
        Check::warn(
            "service_manager",
            CheckGroup::Core,
            "Service Manager",
            "Windows service — AgentDeskDcserver installed but not running",
            "`sc query AgentDeskDcserver` 로 상태를 확인하거나 `agentdesk doctor --fix`로 restart를 시도하세요.",
        )
        .with_expected_actual("Windows service running", "Windows service installed but not running")
        .with_next_steps(vec![
            "sc query AgentDeskDcserver".to_string(),
            "agentdesk doctor --fix".to_string(),
        ])
    } else {
        Check::warn(
            "service_manager",
            CheckGroup::Core,
            "Service Manager",
            "Windows service — AgentDeskDcserver not installed",
            "Windows service 또는 수동 실행 방식 중 어떤 배포인지 확인하세요.",
        )
        .with_expected_actual("Windows service installed", "Windows service not installed")
        .with_next_steps(vec!["sc query AgentDeskDcserver".to_string()])
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn check_service_manager() -> Check {
    Check::ok(
        "service_manager",
        CheckGroup::Core,
        "Service Manager",
        "N/A",
    )
}

fn check_db_integrity(cfg: &config::Config) -> Check {
    let db_path = cfg.data.dir.join(&cfg.data.db_name);
    if crate::db::postgres::database_enabled(cfg) {
        return Check::ok(
            "db_integrity",
            CheckGroup::Core,
            "Legacy SQLite DB",
            format!("demoted while Postgres is enabled: {}", db_path.display()),
        )
        .with_subsystem("sqlite_cache")
        .with_fix_safety(FixSafety::ReadOnly)
        .with_security_exposure(SecurityExposure::LocalPath)
        .with_path(db_path.display().to_string())
        .with_expected_actual(
            "Postgres source-of-truth active; SQLite local artifact is non-authoritative",
            "legacy SQLite integrity check demoted",
        );
    }
    return Check::warn(
        "db_integrity",
        CheckGroup::Core,
        "Legacy SQLite DB",
        format!("retired from normal builds: {}", db_path.display()),
        "Postgres is the AgentDesk source-of-truth; legacy SQLite integrity checks are no longer compiled.",
    )
    .with_subsystem("sqlite_cache")
    .with_fix_safety(FixSafety::ReadOnly)
    .with_security_exposure(SecurityExposure::LocalPath)
    .with_path(db_path.display().to_string())
    .with_expected_actual("Postgres source-of-truth active", "SQLite check retired");
}

fn check_postgres_connection(cfg: &config::Config) -> Check {
    let summary = crate::db::postgres::database_summary(cfg);
    if !crate::db::postgres::database_enabled(cfg) {
        return Check::ok(
            "postgres_connection",
            CheckGroup::Core,
            "PostgreSQL",
            "disabled",
        )
        .with_expected_actual("postgres bootstrap configured", "disabled");
    }

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return Check::fail(
                "postgres_connection",
                CheckGroup::Core,
                "PostgreSQL",
                format!("{summary} — runtime init failed"),
                "postgres 연결 검증용 async runtime 생성에 실패했습니다.",
            )
            .with_expected_actual(
                "postgres check runtime initializes",
                format!("runtime build failed: {error}"),
            );
        }
    };

    match runtime.block_on(crate::db::postgres::connect(cfg)) {
        Ok(Some(pool)) => {
            let migration_status = runtime.block_on(crate::db::postgres::migration_status(&pool));
            let checksum_mismatches =
                runtime.block_on(crate::db::postgres::applied_migration_checksum_mismatches(&pool));
            drop(pool);
            match (migration_status, checksum_mismatches) {
                (Ok(status), Ok(checksum_mismatches))
                    if postgres_migration_status_is_healthy(&status, &checksum_mismatches) =>
                {
                    let pending = status.pending_versions.len();
                    Check::ok(
                        "postgres_connection",
                        CheckGroup::Core,
                        "PostgreSQL",
                        format!(
                            "{summary} — ok; applied={} resolved={} pending={pending}",
                            status.applied.len(),
                            status.resolved_versions.len(),
                        ),
                    )
                    .with_subsystem("postgres")
                    .with_evidence(json!({
                        "applied_count": status.applied.len(),
                        "resolved_count": status.resolved_versions.len(),
                        "pending_versions": status.pending_versions,
                        "checksum_mismatches": checksum_mismatches,
                    }))
                    .with_expected_actual("postgres connection and migration metadata readable", "ok")
                }
                (Ok(status), Ok(checksum_mismatches)) => Check::fail(
                    "postgres_connection",
                    CheckGroup::Core,
                    "PostgreSQL",
                    format!(
                        "{summary} — migration drift: missing_from_resolved={:?} unsuccessful={:?} checksum_mismatches={:?}",
                        status.missing_from_resolved,
                        unsuccessful_migration_versions(&status),
                        checksum_mismatches
                    ),
                    "Postgres _sqlx_migrations contains drift or unsuccessful migration records.",
                )
                .with_subsystem("postgres")
                .with_fix_safety(FixSafety::NotFixable)
                .with_security_exposure(SecurityExposure::OperationalMetadata)
                .with_evidence(json!({
                    "applied_count": status.applied.len(),
                    "resolved_count": status.resolved_versions.len(),
                    "missing_from_resolved": status.missing_from_resolved,
                    "unsuccessful_versions": unsuccessful_migration_versions(&status),
                    "pending_versions": status.pending_versions,
                    "checksum_mismatches": checksum_mismatches,
                }))
                .with_expected_actual(
                    "applied migrations all exist in resolved migrations, succeeded, and checksum-matched",
                    "migration drift, checksum mismatch, or unsuccessful migration",
                ),
                (Err(error), _) => Check::fail(
                    "postgres_connection",
                    CheckGroup::Core,
                    "PostgreSQL",
                    format!("{summary} — migration metadata failed"),
                    "Postgres connection succeeded but read-only migration metadata could not be queried.",
                )
                .with_subsystem("postgres")
                .with_fix_safety(FixSafety::NotFixable)
                .with_security_exposure(SecurityExposure::OperationalMetadata)
                .with_expected_actual("read-only _sqlx_migrations query succeeds", error),
                (_, Err(error)) => Check::fail(
                    "postgres_connection",
                    CheckGroup::Core,
                    "PostgreSQL",
                    format!("{summary} — migration checksum verification failed"),
                    "Postgres connection succeeded but read-only migration checksum metadata could not be queried.",
                )
                .with_subsystem("postgres")
                .with_fix_safety(FixSafety::NotFixable)
                .with_security_exposure(SecurityExposure::OperationalMetadata)
                .with_expected_actual("read-only _sqlx_migrations checksum query succeeds", error),
            }
        }
        Ok(None) => Check::ok(
            "postgres_connection",
            CheckGroup::Core,
            "PostgreSQL",
            "disabled",
        )
        .with_expected_actual("postgres bootstrap configured", "disabled"),
        Err(error) => Check::fail(
            "postgres_connection",
            CheckGroup::Core,
            "PostgreSQL",
            format!("{summary} — failed"),
            "DATABASE_URL 또는 database 설정값(host/port/dbname/user/password)을 확인하세요.",
        )
        .with_expected_actual("postgres connection succeeds", error)
        .with_next_steps(vec!["agentdesk doctor --json".to_string()]),
    }
}

fn unsuccessful_migration_versions(status: &crate::db::postgres::MigrationStatus) -> Vec<i64> {
    status
        .applied
        .iter()
        .filter(|migration| !migration.success)
        .map(|migration| migration.version)
        .collect()
}

fn postgres_migration_status_is_healthy(
    status: &crate::db::postgres::MigrationStatus,
    checksum_mismatches: &[i64],
) -> bool {
    status.missing_from_resolved.is_empty()
        && unsuccessful_migration_versions(status).is_empty()
        && checksum_mismatches.is_empty()
}

fn check_stale_zero_byte_db_files(cfg: &config::Config) -> Check {
    let Some(runtime_root) = dcserver::agentdesk_runtime_root() else {
        return Check::warn(
            "stale_db_files",
            CheckGroup::Core,
            "Stale DB Files",
            "runtime root unresolved",
            "실제 DB 경로를 먼저 확인한 뒤 root 경로의 0바이트 stale DB 파일을 정리하세요.",
        )
        .with_expected_actual(
            "runtime root path resolvable",
            "runtime root path unresolved",
        );
    };

    let canonical_db_path = cfg.data.dir.join(&cfg.data.db_name);
    if crate::db::postgres::database_enabled(cfg) {
        return Check::warn(
            "stale_db_files",
            CheckGroup::Core,
            "Legacy SQLite Stale Files",
            format!("legacy local artifact scan near {}", runtime_root.display()),
            "Postgres is enabled; zero-byte SQLite files are stale local artifacts, not authoritative DB state.",
        )
        .with_subsystem("sqlite_cache")
        .with_fix_safety(FixSafety::ExplicitDbRepairRequired)
        .with_security_exposure(SecurityExposure::LocalPath)
        .with_path(runtime_root.display().to_string())
        .with_expected_actual(
            "Postgres source-of-truth active; stale SQLite files are local artifacts only",
            "legacy SQLite stale-file scan demoted",
        )
        .with_next_steps(vec![
            "agentdesk doctor --fix --repair-sqlite-cache".to_string()
        ]);
    }
    let stale_paths = stale_zero_byte_db_candidates(&runtime_root, &canonical_db_path);
    if stale_paths.is_empty() {
        return Check::ok(
            "stale_db_files",
            CheckGroup::Core,
            "Stale DB Files",
            format!(
                "none near {} (canonical DB: {})",
                runtime_root.display(),
                canonical_db_path.display()
            ),
        )
        .with_path(runtime_root.display().to_string())
        .with_expected_actual("no zero-byte stale DB files", "no zero-byte stale DB files");
    }

    let listed = stale_paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Check::warn(
        "stale_db_files",
        CheckGroup::Core,
        "Stale DB Files",
        format!("zero-byte stale DB file(s): {listed}"),
        format!(
            "실제 DB는 {} 입니다. 추측 경로로 sqlite3를 열지 말고, 필요하면 agentdesk doctor --fix 로 stale 파일을 정리하세요.",
            canonical_db_path.display()
        ),
    )
    .with_path(runtime_root.display().to_string())
    .with_expected_actual("no zero-byte stale DB files", listed)
    .with_next_steps(vec!["agentdesk doctor --fix".to_string()])
}

fn check_github_repo_registry(cfg: &config::Config) -> Check {
    let db_path = cfg.data.dir.join(&cfg.data.db_name);
    if crate::db::postgres::database_enabled(cfg) {
        return Check::ok(
            "github_repo_registry",
            CheckGroup::Core,
            "Legacy SQLite GitHub Repo Registry",
            "demoted while Postgres is enabled",
        )
        .with_subsystem("sqlite_cache")
        .with_fix_safety(FixSafety::ReadOnly)
        .with_security_exposure(SecurityExposure::OperationalMetadata)
        .with_path(db_path.display().to_string())
        .with_expected_actual(
            "Postgres source-of-truth active; SQLite github_repos is non-authoritative",
            "legacy SQLite registry check demoted",
        );
    }
    return Check::warn(
        "github_repo_registry",
        CheckGroup::Core,
        "Legacy SQLite GitHub Repo Registry",
        "retired from normal builds",
        "Postgres is the AgentDesk source-of-truth; legacy SQLite github_repos comparison is no longer compiled.",
    )
    .with_subsystem("sqlite_cache")
    .with_fix_safety(FixSafety::ReadOnly)
    .with_security_exposure(SecurityExposure::OperationalMetadata)
    .with_path(db_path.display().to_string())
    .with_expected_actual("Postgres source-of-truth active", "SQLite registry check retired");
}

fn normalized_config_repo_ids(cfg: &config::Config) -> (BTreeSet<String>, Vec<String>) {
    let mut valid = BTreeSet::new();
    let mut invalid = BTreeSet::new();

    for raw_repo_id in &cfg.github.repos {
        let repo_id = raw_repo_id.trim();
        if repo_id.is_empty() {
            continue;
        }
        if repo_id.contains('/') {
            valid.insert(repo_id.to_string());
        } else {
            invalid.insert(repo_id.to_string());
        }
    }

    (valid, invalid.into_iter().collect())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn open_registered_github_repo_ids(db_path: &std::path::Path) -> Result<BTreeSet<String>, String> {
    let conn = open_write_connection(db_path).map_err(|e| format!("cannot open: {e}"))?;
    let mut stmt = conn
        .prepare("SELECT id FROM github_repos ORDER BY id")
        .map_err(|e| format!("prepare: {e}"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| format!("query: {e}"))?;

    let mut repos = BTreeSet::new();
    for row in rows {
        repos.insert(row.map_err(|e| format!("row: {e}"))?);
    }
    Ok(repos)
}

const DISK_WARN_BYTES: u64 = 30 * 1024 * 1024 * 1024;
const DISK_FAIL_BYTES: u64 = 80 * 1024 * 1024 * 1024;

fn recursive_dir_size(path: &Path) -> std::io::Result<u64> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(0);
    }

    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        total = total.saturating_add(recursive_dir_size(&entry.path()).unwrap_or(0));
    }
    Ok(total)
}

fn check_disk_usage() -> Check {
    match dcserver::agentdesk_runtime_root() {
        Some(path) if !path.exists() => Check::warn(
            "disk_usage",
            CheckGroup::Core,
            "Disk Usage",
            format!("{} — runtime root missing", path.display()),
            "agentdesk doctor --fix 로 기본 runtime 디렉터리를 생성할 수 있습니다.",
        )
        .with_path(path.display().to_string())
        .with_expected_actual(
            "runtime root exists for disk usage scan",
            "runtime root missing",
        )
        .with_next_steps(vec!["agentdesk doctor --fix".to_string()]),
        Some(path) => match recursive_dir_size(&path) {
            Ok(total) => {
                let mb = total as f64 / 1_048_576.0;
                let mut children = fs::read_dir(&path)
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| {
                        let entry = entry.ok()?;
                        let child_path = entry.path();
                        let size = recursive_dir_size(&child_path).ok()?;
                        Some((
                            child_path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("unknown")
                                .to_string(),
                            size,
                        ))
                    })
                    .collect::<Vec<_>>();
                children.sort_by(|left, right| right.1.cmp(&left.1));
                let top_children = children
                    .into_iter()
                    .take(5)
                    .map(|(name, size)| {
                        json!({
                            "name": name,
                            "mb": (size as f64 / 1_048_576.0)
                        })
                    })
                    .collect::<Vec<_>>();
                let evidence = json!({
                    "runtime_root": path.display().to_string(),
                    "total_bytes": total,
                    "warn_threshold_bytes": DISK_WARN_BYTES,
                    "fail_threshold_bytes": DISK_FAIL_BYTES,
                    "top_children": top_children
                });
                let detail = format!("{:.1} MB recursively in {}", mb, path.display());
                if total >= DISK_FAIL_BYTES {
                    Check::fail(
                        "disk_usage",
                        CheckGroup::Core,
                        "Disk Usage",
                        detail.clone(),
                        "runtime root disk usage exceeded failure threshold; inspect large child directories before deleting anything.",
                    )
                    .with_path(path.display().to_string())
                    .with_expected_actual(
                        format!("< {:.1} MB", DISK_FAIL_BYTES as f64 / 1_048_576.0),
                        format!("{:.1} MB", mb),
                    )
                    .with_evidence(evidence)
                    .with_next_steps(vec![format!("du -sh {}/*", path.display())])
                } else if total >= DISK_WARN_BYTES {
                    Check::warn(
                        "disk_usage",
                        CheckGroup::Core,
                        "Disk Usage",
                        detail.clone(),
                        "runtime root disk usage is above warning threshold; review generated logs/artifacts.",
                    )
                    .with_path(path.display().to_string())
                    .with_expected_actual(
                        format!("< {:.1} MB", DISK_WARN_BYTES as f64 / 1_048_576.0),
                        format!("{:.1} MB", mb),
                    )
                    .with_evidence(evidence)
                    .with_next_steps(vec![format!("du -sh {}/*", path.display())])
                } else {
                    Check::ok("disk_usage", CheckGroup::Core, "Disk Usage", detail)
                        .with_path(path.display().to_string())
                        .with_expected_actual("disk usage below threshold", format!("{:.1} MB", mb))
                        .with_evidence(evidence)
                }
            }
            Err(e) => Check::warn(
                "disk_usage",
                CheckGroup::Core,
                "Disk Usage",
                format!("{} — unreadable ({e})", path.display()),
                "runtime root 권한을 확인하세요.",
            )
            .with_path(path.display().to_string())
            .with_expected_actual(
                "runtime root readable",
                format!("runtime root unreadable: {e}"),
            )
            .with_next_steps(vec![format!("ls -la {}", path.display())]),
        },
        None => Check::fail(
            "disk_usage",
            CheckGroup::Core,
            "Disk Usage",
            "cannot determine runtime root",
            "AGENTDESK_ROOT_DIR 또는 기본 ~/.adk/release 경로를 확인하세요.",
        )
        .with_expected_actual(
            "runtime root path resolvable",
            "runtime root path unresolved",
        ),
    }
}

pub(crate) fn run_doctor_report(options: DoctorOptions) -> Result<DoctorReport, String> {
    let cfg = config::load_graceful();
    let mut actions = Vec::new();
    if options.fix {
        actions = apply_safe_fixes(&cfg, &options);
        let pre_fix_snapshot = fetch_health_snapshot(&options);
        actions.extend(apply_service_fix(&pre_fix_snapshot, &options));
        actions.extend(apply_stale_mailbox_fixes(&pre_fix_snapshot, &options));
    }

    let snapshot = fetch_health_snapshot(&options);
    let mut checks = build_all_checks(&cfg, &snapshot);
    if let Some(profile) = options.profile {
        checks.retain(|check| match profile {
            DoctorProfile::Quick => {
                matches!(check.subsystem, "server" | "health" | "provider_runtime")
            }
            DoctorProfile::Deep => true,
            DoctorProfile::Security => {
                matches!(
                    check.subsystem,
                    "security" | "config_audit" | "health" | "provider_runtime"
                ) || !matches!(check.security_exposure, SecurityExposure::None)
            }
        });
    }
    Ok(build_json_report(&options, &checks, &actions))
}

pub fn cmd_doctor(options: DoctorOptions) -> Result<(), String> {
    let json_output = options.json;
    if options.fix {
        if !json_output {
            println!("AgentDesk Doctor v{}\n", env!("CARGO_PKG_VERSION"));
        }
    } else if !json_output {
        println!("AgentDesk Doctor v{}\n", env!("CARGO_PKG_VERSION"));
    }

    let report = run_doctor_report(options)?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|e| format!("failed to serialize doctor report: {e}"))?
        );
    } else {
        if report.fix_requested {
            let actions = report
                .fixes
                .iter()
                .map(|action| FixAction {
                    id: action.id,
                    name: action.name,
                    status: action.status,
                    ok: action.ok,
                    detail: action.detail.clone(),
                    skipped: action.skipped,
                    requires_explicit_consent: action.requires_explicit_consent,
                    fix_safety: match action.fix_safety {
                        "read_only" => FixSafety::ReadOnly,
                        "safe_local_repair" => FixSafety::SafeLocalRepair,
                        "explicit_restart_required" => FixSafety::ExplicitRestartRequired,
                        "explicit_db_repair_required" => FixSafety::ExplicitDbRepairRequired,
                        _ => FixSafety::NotFixable,
                    },
                    safety_gate: action.safety_gate,
                    skipped_reason: action.skipped_reason.clone(),
                    evidence: action.evidence.clone(),
                })
                .collect::<Vec<_>>();
            print_fix_actions(&actions);
        }
        let checks = report
            .checks
            .iter()
            .map(|check| Check {
                id: check.id,
                group: if check.group == "provider_runtime" {
                    CheckGroup::ProviderRuntime
                } else {
                    CheckGroup::Core
                },
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
                fix_safety: match check.fix_safety {
                    "read_only" => FixSafety::ReadOnly,
                    "safe_local_repair" => FixSafety::SafeLocalRepair,
                    "explicit_restart_required" => FixSafety::ExplicitRestartRequired,
                    "explicit_db_repair_required" => FixSafety::ExplicitDbRepairRequired,
                    _ => FixSafety::NotFixable,
                },
                security_exposure: match check.security_exposure {
                    "local_path" => SecurityExposure::LocalPath,
                    "operational_metadata" => SecurityExposure::OperationalMetadata,
                    "credential_metadata" => SecurityExposure::CredentialMetadata,
                    "public_surface" => SecurityExposure::PublicSurface,
                    _ => SecurityExposure::None,
                },
            })
            .collect::<Vec<_>>();
        let core_checks: Vec<Check> = checks
            .iter()
            .filter(|check| matches!(check.group, CheckGroup::Core))
            .cloned()
            .collect();
        let provider_checks: Vec<Check> = checks
            .iter()
            .filter(|check| matches!(check.group, CheckGroup::ProviderRuntime))
            .cloned()
            .collect();

        print_group("Core", &core_checks);
        print_group("Provider Runtime", &provider_checks);

        println!(
            "  {} passed, {} warned, {} failed out of {} checks",
            report.summary.passed,
            report.summary.warned,
            report.summary.failed,
            report.summary.total
        );
    }

    if report.summary.failed > 0 {
        Err(format!(
            "{} diagnostic check(s) failed",
            report.summary.failed
        ))
    } else {
        Ok(())
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::{
        Check, CheckGroup, CheckStatus, DoctorOptions, FixAction, HealthSnapshot,
        apply_service_fix, apply_stale_mailbox_fixes, build_json_report, check_config_audit,
        check_credential_permissions, check_degraded_reasons, check_github_repo_registry,
        check_mailbox_consistency, check_postgres_connection, check_provider_bindings,
        check_provider_cli, check_qwen_auth_hints, check_qwen_runtime_artifacts,
        check_qwen_settings_files, check_server_running, configured_provider_names,
        discord_bot_check_from_health, postgres_migration_status_is_healthy,
        provider_capability_summary, stale_mailbox_repair_fix_safety,
        stale_mailbox_repair_response_status, stale_mailbox_repair_safety_gate,
        unsuccessful_migration_versions,
    };
    use crate::cli::doctor::contract::{FixSafety, RunContext, Severity};
    use crate::config::{AgentChannel, AgentChannels, AgentDef, ServerConfig};
    use crate::db::{open_write_connection, schema};
    use crate::services::provider::ProviderKind;
    use serde_json::json;
    use std::path::Path;

    fn with_temp_qwen_doctor_env<F>(f: F)
    where
        F: FnOnce(&tempfile::TempDir, &tempfile::TempDir),
    {
        let _guard = crate::services::discord::runtime_store::lock_test_env();
        let temp_home = tempfile::tempdir().unwrap();
        let temp_project = tempfile::tempdir().unwrap();
        let prev_qwen_home = std::env::var_os("QWEN_HOME");
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        let prev_test_home = std::env::var_os("AGENTDESK_TEST_HOME");
        let prev_cwd = std::env::current_dir().unwrap();

        unsafe {
            std::env::set_var("HOME", temp_home.path());
            std::env::set_var("USERPROFILE", temp_home.path());
            std::env::set_var("AGENTDESK_TEST_HOME", temp_home.path());
        }
        std::env::set_current_dir(temp_project.path()).unwrap();

        f(&temp_home, &temp_project);

        std::env::set_current_dir(prev_cwd).unwrap();
        match prev_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match prev_qwen_home {
            Some(value) => unsafe { std::env::set_var("QWEN_HOME", value) },
            None => unsafe { std::env::remove_var("QWEN_HOME") },
        }
        match prev_userprofile {
            Some(value) => unsafe { std::env::set_var("USERPROFILE", value) },
            None => unsafe { std::env::remove_var("USERPROFILE") },
        }
        match prev_test_home {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_TEST_HOME", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_TEST_HOME") },
        }
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, contents).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    fn write_github_repo_registry(db_path: &Path, repos: &[&str]) {
        let conn = open_write_connection(db_path).unwrap();
        schema::migrate(&conn).unwrap();
        for repo in repos {
            conn.execute(
                "INSERT INTO github_repos (id, display_name, sync_enabled) VALUES (?1, ?1, 1)",
                [repo],
            )
            .unwrap();
        }
    }

    fn test_base_url() -> String {
        format!(
            "http://{}:{}",
            ServerConfig::loopback(),
            ServerConfig::default().port,
        )
    }

    #[test]
    fn unified_health_requires_connected_providers() {
        let check = discord_bot_check_from_health(
            &test_base_url(),
            &json!({
                "status": "healthy",
                "providers": [
                    {"name": "claude", "connected": true},
                    {"name": "codex", "connected": true}
                ]
            }),
        );
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("2/2 connected"));
    }

    #[test]
    fn standalone_health_becomes_warning() {
        let check = discord_bot_check_from_health(
            &test_base_url(),
            &json!({
                "ok": true,
                "db": true,
                "version": "0.1.0"
            }),
        );
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("standalone health only"));
    }

    #[test]
    fn degraded_health_fails_server_check() {
        let snapshot = HealthSnapshot {
            base: test_base_url(),
            body: Some(json!({
                "status": "degraded",
                "version": "0.1.0"
            })),
            error: None,
        };

        let check = check_server_running(&snapshot);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("degraded"));
    }

    #[test]
    fn provider_capability_summary_mentions_structured_contract() {
        let summary = provider_capability_summary(&ProviderKind::Codex);
        assert!(summary.contains("structured-output"));
        assert!(summary.contains("resume"));
        assert!(summary.contains("tool-stream"));
    }

    #[test]
    fn configured_provider_names_merges_config_and_health() {
        let mut cfg = crate::config::Config::default();
        cfg.agents.push(crate::config::AgentDef {
            id: "agent-1".to_string(),
            name: "Agent 1".to_string(),
            name_ko: None,
            provider: "codex".to_string(),
            channels: crate::config::AgentChannels::default(),
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
        });

        let snapshot = HealthSnapshot {
            base: test_base_url(),
            body: Some(json!({
                "status": "healthy",
                "providers": [
                    {"name": "claude", "connected": true}
                ]
            })),
            error: None,
        };

        let configured = configured_provider_names(&cfg, &snapshot);
        assert!(configured.contains("claude"));
        assert!(configured.contains("codex"));
    }

    #[test]
    fn degraded_reason_check_classifies_provider_disconnect() {
        let snapshot = HealthSnapshot {
            base: test_base_url(),
            body: Some(json!({
                "status": "degraded",
                "degraded_reasons": ["provider:codex:disconnected"]
            })),
            error: None,
        };

        let check = check_degraded_reasons(&snapshot);
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(check.severity, Severity::Error);
        assert_eq!(check.subsystem, "provider_runtime");
        assert_eq!(check.fix_safety, FixSafety::ReadOnly);
        assert!(check.detail.contains("provider codex is disconnected"));
        assert!(
            check
                .next_steps
                .iter()
                .any(|step| step.contains("codex Discord token"))
        );
    }

    #[test]
    fn connected_providers_with_degraded_reason_do_not_use_offline_token_guidance() {
        let check = discord_bot_check_from_health(
            &test_base_url(),
            &json!({
                "status": "degraded",
                "providers": [
                    {"name": "codex", "connected": true},
                    {"name": "claude", "connected": true}
                ],
                "degraded_reasons": ["provider:codex:pending_queue_depth:2"]
            }),
        );

        assert_eq!(check.status, CheckStatus::Warn);
        assert_eq!(check.subsystem, "provider_runtime");
        assert_eq!(check.fix_safety, FixSafety::ReadOnly);
        assert!(
            check
                .path
                .as_deref()
                .is_some_and(|path| path.ends_with("/api/health/detail"))
        );
        assert!(check.detail.contains("pending queue depth 2"));
        assert!(
            !check
                .guidance
                .as_deref()
                .unwrap_or_default()
                .contains("오프라인 provider의 Discord token")
        );
    }

    fn default_doctor_options() -> DoctorOptions {
        DoctorOptions {
            fix: true,
            json: true,
            allow_restart: false,
            repair_sqlite_cache: false,
            allow_remote: false,
            profile: None,
            run_context: RunContext::ManualCli,
            artifact_path: None,
        }
    }

    #[test]
    fn service_fix_requires_explicit_restart_consent() {
        let snapshot = HealthSnapshot {
            base: test_base_url(),
            body: Some(json!({
                "status": "unhealthy",
                "db": false
            })),
            error: None,
        };
        let options = default_doctor_options();

        let actions = apply_service_fix(&snapshot, &options);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].id, "service_restart");
        assert!(actions[0].skipped);
        assert!(actions[0].requires_explicit_consent);
        assert_eq!(actions[0].fix_safety, FixSafety::ExplicitRestartRequired);
    }

    #[test]
    fn startup_stale_mailbox_fix_skips_when_live_work_evidence_exists() {
        let snapshot = HealthSnapshot {
            base: test_base_url(),
            body: Some(json!({
                "mailboxes": [{
                    "channel_id": 123,
                    "has_cancel_token": false,
                    "queue_depth": 0,
                    "watcher_attached": false,
                    "inflight_state_present": true,
                    "tmux_present": true,
                    "process_present": false,
                    "active_dispatch_present": false,
                    "agent_turn_status": "idle"
                }]
            })),
            error: None,
        };
        let mut startup_options = default_doctor_options();
        startup_options.run_context = RunContext::StartupOnce;

        assert!(apply_stale_mailbox_fixes(&snapshot, &startup_options).is_empty());

        let manual_actions = apply_stale_mailbox_fixes(&snapshot, &default_doctor_options());
        assert_eq!(manual_actions.len(), 1);
        assert!(manual_actions[0].skipped);
        assert_eq!(
            manual_actions[0].fix_safety,
            FixSafety::ExplicitRestartRequired
        );
    }

    #[cfg(unix)]
    #[test]
    fn provider_runtime_check_uses_resolver_exec_path_under_minimal_path() {
        let _guard = crate::services::discord::runtime_store::lock_test_env();
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("provider-helper");
        let provider = temp.path().join("codex");
        let original_path = std::env::var_os("PATH");

        write_executable(&helper, "#!/bin/sh\nprintf 'codex-test 1.2.3\\n'\n");
        write_executable(
            &provider,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  provider-helper\nelse\n  exit 64\nfi\n",
        );

        unsafe {
            std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            std::env::set_var("AGENTDESK_CODEX_PATH", &provider);
        }

        let snapshot = HealthSnapshot {
            base: test_base_url(),
            body: None,
            error: None,
        };
        let check = check_provider_cli(ProviderKind::Codex, true, &snapshot);

        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("codex-test 1.2.3"));
        assert!(check.detail.contains("env_override"));
        assert_eq!(
            check.path.as_deref(),
            Some(provider.to_string_lossy().as_ref())
        );

        unsafe {
            std::env::remove_var("AGENTDESK_CODEX_PATH");
            match original_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn provider_runtime_check_reports_permission_denied() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = crate::services::discord::runtime_store::lock_test_env();
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("codex");
        let original_path = std::env::var_os("PATH");

        std::fs::write(&provider, "#!/bin/sh\nprintf 'codex-test 1.2.3\\n'\n").unwrap();
        let mut perms = std::fs::metadata(&provider).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&provider, perms).unwrap();

        unsafe {
            std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            std::env::set_var("AGENTDESK_CODEX_PATH", &provider);
        }

        let snapshot = HealthSnapshot {
            base: test_base_url(),
            body: None,
            error: None,
        };
        let check = check_provider_cli(ProviderKind::Codex, true, &snapshot);

        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("permission_denied"));
        assert!(check.detail.contains("not found in runtime PATH"));
        assert_eq!(check.path.as_deref(), None);

        unsafe {
            std::env::remove_var("AGENTDESK_CODEX_PATH");
            match original_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    #[test]
    fn qwen_doctor_detects_home_extensions_not_project_extensions() {
        with_temp_qwen_doctor_env(|temp_home, temp_project| {
            let home_qwen = temp_home.path().join(".qwen");
            let project_qwen = temp_project.path().join(".qwen");
            std::fs::create_dir_all(home_qwen.join("extensions")).unwrap();
            std::fs::create_dir_all(&project_qwen).unwrap();

            let check = check_qwen_runtime_artifacts(true);
            assert_eq!(check.status, CheckStatus::Pass);
            assert!(check.detail.contains("home: extensions"));
            assert!(check.detail.contains("project: -"));
        });
    }

    #[test]
    fn qwen_doctor_surfaces_project_summary_and_output_language() {
        with_temp_qwen_doctor_env(|temp_home, temp_project| {
            let home_qwen = temp_home.path().join(".qwen");
            let project_qwen = temp_project.path().join(".qwen");
            std::fs::create_dir_all(&home_qwen).unwrap();
            std::fs::create_dir_all(&project_qwen).unwrap();
            std::fs::write(home_qwen.join("output-language.md"), "Korean").unwrap();
            std::fs::write(project_qwen.join("PROJECT_SUMMARY.md"), "Summary").unwrap();

            let check = check_qwen_runtime_artifacts(true);
            assert_eq!(check.status, CheckStatus::Pass);
            assert!(check.detail.contains("output-language.md"));
            assert!(check.detail.contains("PROJECT_SUMMARY.md"));
        });
    }

    #[test]
    fn qwen_doctor_auth_hint_prefers_project_dot_qwen_env_before_dot_env() {
        with_temp_qwen_doctor_env(|_temp_home, temp_project| {
            let project_qwen = temp_project.path().join(".qwen");
            std::fs::create_dir_all(&project_qwen).unwrap();
            std::fs::write(project_qwen.join(".env"), "DASHSCOPE_API_KEY=one").unwrap();
            std::fs::write(temp_project.path().join(".env"), "OPENAI_API_KEY=two").unwrap();

            let check = check_qwen_auth_hints(true);
            assert_eq!(check.status, CheckStatus::Pass);
            assert!(check.detail.contains("project .qwen/.env"));
            assert!(check.detail.contains("project .env"));
        });
    }

    #[test]
    fn qwen_doctor_prefers_qwen_home_over_home() {
        with_temp_qwen_doctor_env(|temp_home, _temp_project| {
            let qwen_home = tempfile::tempdir().unwrap();
            let qwen_settings = qwen_home.path().join(".qwen").join("settings.json");
            std::fs::create_dir_all(qwen_settings.parent().unwrap()).unwrap();
            std::fs::write(&qwen_settings, "{}").unwrap();

            unsafe {
                std::env::set_var("QWEN_HOME", qwen_home.path());
                std::env::set_var("HOME", temp_home.path());
            }

            let check = check_qwen_settings_files(true);
            assert_eq!(check.status, CheckStatus::Pass);
            assert_eq!(
                check.path.as_deref(),
                Some(format!("user settings={}", qwen_settings.display()).as_str())
            );
        });
    }

    #[test]
    fn github_repo_registry_check_passes_when_config_matches_db() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("agentdesk.db");
        write_github_repo_registry(&db_path, &["owner/repo-a", "owner/repo-b"]);

        let mut cfg = crate::config::Config::default();
        cfg.data.dir = temp.path().to_path_buf();
        cfg.data.db_name = "agentdesk.db".to_string();
        cfg.github.repos = vec![
            " owner/repo-b ".to_string(),
            "owner/repo-a".to_string(),
            "owner/repo-a".to_string(),
        ];

        let check = check_github_repo_registry(&cfg);
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("config=2 db=2"));
    }

    #[test]
    fn github_repo_registry_check_reports_missing_extra_and_invalid_entries() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("agentdesk.db");
        write_github_repo_registry(&db_path, &["owner/repo-a", "owner/repo-stale"]);

        let mut cfg = crate::config::Config::default();
        cfg.data.dir = temp.path().to_path_buf();
        cfg.data.db_name = "agentdesk.db".to_string();
        cfg.github.repos = vec![
            "owner/repo-a".to_string(),
            "owner/repo-missing".to_string(),
            "noslash".to_string(),
        ];

        let check = check_github_repo_registry(&cfg);
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("missing_in_db=owner/repo-missing"));
        assert!(check.detail.contains("extra_in_db=owner/repo-stale"));
        assert!(check.detail.contains("invalid_config=noslash"));
    }

    #[test]
    fn postgres_connection_check_is_ok_when_disabled() {
        let config = crate::config::Config::default();
        let check = check_postgres_connection(&config);
        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(check.detail, "disabled");
    }

    #[test]
    fn config_audit_check_summarizes_without_raw_source_paths() {
        let snapshot = HealthSnapshot {
            base: test_base_url(),
            body: Some(json!({
                "config_audit": {
                    "status": "warn",
                    "warnings_count": 1,
                    "actions": [{"kind": "sync", "source_path": "/Users/kunkun/secret/path"}],
                    "db": {
                        "missing_agents": ["agent-a"],
                        "extra_agents": [],
                        "mismatched_agents": [],
                        "synced_agents": true,
                        "source_path": "/Users/kunkun/secret/db"
                    }
                }
            })),
            error: None,
        };

        let check = check_config_audit(&snapshot);
        assert_eq!(check.status, CheckStatus::Warn);
        assert_eq!(check.subsystem, "config_audit");
        let evidence = serde_json::to_string(&check.evidence).unwrap();
        assert!(evidence.contains("\"missing_agents\":1"));
        assert!(!evidence.contains("/Users/kunkun/secret"));
    }

    #[test]
    fn provider_binding_check_reports_duplicate_entries_as_error() {
        let cfg = crate::config::Config::default();
        let snapshot = HealthSnapshot {
            base: test_base_url(),
            body: Some(json!({
                "providers": [
                    {"name": "codex", "connected": true},
                    {"name": "codex", "connected": true}
                ]
            })),
            error: None,
        };

        let check = check_provider_bindings(&cfg, &snapshot);
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(check.subsystem, "provider_binding");
        assert!(
            check
                .evidence
                .as_ref()
                .and_then(|evidence| evidence.get("duplicate_providers"))
                .and_then(serde_json::Value::as_array)
                .is_some_and(|items| items.iter().any(|item| item == "codexx2"))
        );
    }

    #[test]
    fn provider_binding_check_reports_missing_channel_runtime_and_auth_hint() {
        let mut cfg = crate::config::Config::default();
        cfg.agents.push(AgentDef {
            id: "agent-1".to_string(),
            name: "Agent 1".to_string(),
            name_ko: None,
            provider: "codex".to_string(),
            channels: AgentChannels {
                codex: Some(AgentChannel::from("12345")),
                ..AgentChannels::default()
            },
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
        });
        cfg.agents.push(AgentDef {
            id: "agent-2".to_string(),
            name: "Agent 2".to_string(),
            name_ko: None,
            provider: "qwen".to_string(),
            channels: AgentChannels::default(),
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
        });
        let snapshot = HealthSnapshot {
            base: test_base_url(),
            body: Some(json!({
                "providers": [{"name": "claude", "connected": false}]
            })),
            error: None,
        };

        let check = check_provider_bindings(&cfg, &snapshot);
        assert_eq!(check.status, CheckStatus::Warn);
        let evidence = check.evidence.as_ref().unwrap();
        assert_eq!(evidence["disconnected_providers"], json!(["claude"]));
        assert_eq!(
            evidence["missing_runtime_providers"],
            json!(["agent-1:codex"])
        );
        assert_eq!(evidence["missing_auth_hints"], json!(["agent-1:codex"]));
        assert_eq!(evidence["missing_channels"], json!(["agent-2:qwen"]));
    }

    #[test]
    fn mailbox_consistency_detects_session_record_and_global_active_mismatch() {
        let snapshot = HealthSnapshot {
            base: test_base_url(),
            body: Some(json!({
                "global_active": 1,
                "mailboxes": [{
                    "channel_id": 123,
                    "has_cancel_token": false,
                    "queue_depth": 0,
                    "watcher_attached": false,
                    "inflight_state_present": false,
                    "tmux_present": false,
                    "process_present": false,
                    "active_dispatch_present": false,
                    "session_record_present": true,
                    "session_status": "working",
                    "agent_turn_status": "idle"
                }]
            })),
            error: None,
        };

        let checks = check_mailbox_consistency(&snapshot);
        assert!(checks.iter().any(|check| {
            check.id == "tmux_missing_with_session_record"
                && check.fix_safety == FixSafety::SafeLocalRepair
        }));
        assert!(checks.iter().any(|check| {
            check.id == "global_active_without_active_turn"
                && check.fix_safety == FixSafety::ExplicitRestartRequired
        }));
    }

    #[cfg(unix)]
    #[test]
    fn credential_permission_check_reports_world_readable_sensitive_file_without_secret_value() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = crate::config::shared_test_env_lock().lock().unwrap();
        let previous_root = crate::config::current_test_runtime_root_override();
        let temp = tempfile::tempdir().unwrap();
        crate::config::set_test_runtime_root_override(Some(temp.path().to_path_buf()));

        let config_path = crate::runtime_layout::config_file_path(temp.path());
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "auth_token: super-secret-token").unwrap();
        let mut perms = std::fs::metadata(&config_path).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&config_path, perms).unwrap();

        let mut cfg = crate::config::Config::default();
        cfg.server.auth_token = Some("super-secret-token".to_string());
        let check = check_credential_permissions(&cfg);

        crate::config::set_test_runtime_root_override(previous_root);

        assert_eq!(check.status, CheckStatus::Warn);
        assert_eq!(
            check.security_exposure,
            crate::cli::doctor::contract::SecurityExposure::CredentialMetadata
        );
        let evidence = serde_json::to_string(&check.evidence).unwrap();
        assert!(evidence.contains("agentdesk_yaml"));
        assert!(evidence.contains("group/other bits"));
        assert!(!evidence.contains("super-secret-token"));
    }

    #[test]
    fn json_report_serializes_partial_repair_status() {
        let options = DoctorOptions {
            fix: true,
            json: true,
            allow_restart: false,
            repair_sqlite_cache: false,
            allow_remote: false,
            profile: None,
            run_context: RunContext::ManualCli,
            artifact_path: None,
        };
        let report = build_json_report(
            &options,
            &[],
            &[FixAction::partial(
                "stale_watcher_inflight_without_active_turn",
                "Stale Mailbox Repair",
                "inflight cleanup still needs operator verification",
            )],
        );
        let value = serde_json::to_value(report).expect("serialize doctor report");

        assert_eq!(value["fix_applied"], false);
        assert_eq!(value["fixes"][0]["status"], "partial_repair");
        assert_eq!(value["fixes"][0]["ok"], false);
        assert_eq!(value["fixes"][0]["fix_safety"], "explicit_restart_required");
        assert_eq!(
            value["fixes"][0]["safety_gate"],
            "partial_repair_requires_operator"
        );
    }

    #[test]
    fn stale_mailbox_repair_response_mapping_preserves_skipped_safety_gate() {
        let skipped = json!({
            "ok": false,
            "applied": false,
            "skipped": true,
            "safety_gate": "queue_not_empty",
            "fix_safety": "explicit_restart_required",
            "skipped_reason": "live queue evidence exists"
        });
        assert_eq!(stale_mailbox_repair_response_status(&skipped), "skipped");
        assert_eq!(
            stale_mailbox_repair_safety_gate(&skipped),
            "queue_not_empty"
        );
        assert_eq!(
            stale_mailbox_repair_fix_safety(&skipped),
            FixSafety::ExplicitRestartRequired
        );

        let partial = json!({
            "ok": false,
            "applied": false
        });
        assert_eq!(
            stale_mailbox_repair_response_status(&partial),
            "partial_repair"
        );
    }

    #[test]
    fn postgres_migration_status_is_unhealthy_when_any_record_failed() {
        let status = crate::db::postgres::MigrationStatus {
            applied: vec![crate::db::postgres::AppliedMigrationInfo {
                version: 202604250001,
                description: "failed_migration".to_string(),
                success: false,
            }],
            resolved_versions: vec![202604250001],
            missing_from_resolved: Vec::new(),
            pending_versions: vec![202604250001],
        };

        assert!(!postgres_migration_status_is_healthy(&status, &[]));
        assert_eq!(unsuccessful_migration_versions(&status), vec![202604250001]);
    }

    #[test]
    fn postgres_migration_status_is_unhealthy_when_checksum_mismatch_exists() {
        let status = crate::db::postgres::MigrationStatus {
            applied: vec![crate::db::postgres::AppliedMigrationInfo {
                version: 202604250001,
                description: "ok_migration".to_string(),
                success: true,
            }],
            resolved_versions: vec![202604250001],
            missing_from_resolved: Vec::new(),
            pending_versions: Vec::new(),
        };

        assert!(!postgres_migration_status_is_healthy(
            &status,
            &[202604250001]
        ));
    }

    #[test]
    fn json_report_uses_stable_machine_friendly_fields() {
        let checks = vec![
            Check::warn(
                "service_manager",
                CheckGroup::Core,
                "Service Manager",
                "systemd inactive",
                "restart service",
            )
            .with_path("systemctl --user")
            .with_expected_actual("service active", "service inactive")
            .with_next_steps(vec![
                "systemctl --user status agentdesk-dcserver".to_string(),
                "agentdesk doctor --fix".to_string(),
            ]),
        ];
        let fixes = vec![FixAction::ok(
            "service_restart",
            "Service Restart",
            "systemd restart succeeded",
        )];

        let options = DoctorOptions {
            fix: true,
            json: true,
            allow_restart: false,
            repair_sqlite_cache: false,
            allow_remote: false,
            profile: None,
            run_context: RunContext::ManualCli,
            artifact_path: None,
        };
        let report = build_json_report(&options, &checks, &fixes);
        let value = serde_json::to_value(report).expect("serialize doctor report");

        assert_eq!(value["fix_requested"], true);
        assert_eq!(value["summary"]["warned"], 1);
        assert_eq!(value["checks"][0]["id"], "service_manager");
        assert_eq!(value["checks"][0]["group"], "core");
        assert_eq!(value["checks"][0]["status"], "warn");
        assert_eq!(value["checks"][0]["severity"], "warning");
        assert_eq!(value["checks"][0]["fix_safety"], "read_only");
        assert_eq!(value["checks"][0]["path"], "systemctl --user");
        assert_eq!(value["checks"][0]["expected"], "service active");
        assert_eq!(value["checks"][0]["actual"], "service inactive");
        assert_eq!(
            value["checks"][0]["next_steps"][0],
            "systemctl --user status agentdesk-dcserver"
        );
        assert_eq!(value["fixes"][0]["id"], "service_restart");
        assert_eq!(value["fixes"][0]["status"], "applied");
        assert_eq!(value["fixes"][0]["fix_safety"], "safe_local_repair");
    }
}
