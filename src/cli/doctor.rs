//! `agentdesk doctor` — environment diagnostics.

use std::collections::BTreeSet;
use std::fs;
use std::process::Command;
use std::time::Duration;

use super::dcserver;
use crate::config;
use crate::db::schema;
use crate::services::provider::ProviderKind;
use serde::Serialize;
use serde_json::Value;

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
    detail: String,
    guidance: Option<String>,
    path: Option<String>,
    expected: Option<String>,
    actual: Option<String>,
    next_steps: Vec<String>,
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
            detail: detail.into(),
            guidance: None,
            path: None,
            expected: None,
            actual: None,
            next_steps: Vec::new(),
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
            detail: detail.into(),
            guidance: Some(guidance.into()),
            path: None,
            expected: None,
            actual: None,
            next_steps: Vec::new(),
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
            detail: detail.into(),
            guidance: Some(guidance.into()),
            path: None,
            expected: None,
            actual: None,
            next_steps: Vec::new(),
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
}

struct FixAction {
    id: &'static str,
    name: &'static str,
    ok: bool,
    detail: String,
}

impl FixAction {
    fn ok(id: &'static str, name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            id,
            name,
            ok: true,
            detail: detail.into(),
        }
    }

    fn fail(id: &'static str, name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            id,
            name,
            ok: false,
            detail: detail.into(),
        }
    }
}

#[derive(Serialize)]
struct DoctorSummary {
    passed: usize,
    warned: usize,
    failed: usize,
    total: usize,
}

#[derive(Serialize)]
struct DoctorCheckReport {
    id: &'static str,
    group: &'static str,
    name: &'static str,
    status: &'static str,
    ok: bool,
    detail: String,
    guidance: Option<String>,
    path: Option<String>,
    expected: Option<String>,
    actual: Option<String>,
    next_steps: Vec<String>,
}

#[derive(Serialize)]
struct DoctorFixReport {
    id: &'static str,
    name: &'static str,
    status: &'static str,
    ok: bool,
    detail: String,
}

#[derive(Serialize)]
struct DoctorReport {
    version: &'static str,
    ok: bool,
    fix_requested: bool,
    summary: DoctorSummary,
    checks: Vec<DoctorCheckReport>,
    fixes: Vec<DoctorFixReport>,
}

#[derive(Clone, Debug)]
struct HealthSnapshot {
    base: String,
    body: Option<Value>,
    error: Option<String>,
}

fn fetch_health_snapshot() -> HealthSnapshot {
    let base = super::client::api_base();
    match super::client::get_json("/api/health") {
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

fn health_endpoint(base: &str) -> String {
    format!("{}/api/health", base.trim_end_matches('/'))
}

fn provider_check_id(provider: &ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Claude => "provider_claude",
        ProviderKind::Codex => "provider_codex",
        ProviderKind::Gemini => "provider_gemini",
        ProviderKind::Unsupported(_) => "provider_unsupported",
    }
}

fn build_core_checks(cfg: &config::Config, snapshot: &HealthSnapshot) -> Vec<Check> {
    vec![
        check_server_running(snapshot),
        check_discord_bot(snapshot),
        check_runtime_root(),
        check_data_dir(cfg),
        check_tmux(),
        check_service_manager(),
        check_db_integrity(cfg),
        check_disk_usage(),
    ]
}

fn build_provider_checks(cfg: &config::Config, snapshot: &HealthSnapshot) -> Vec<Check> {
    let configured = configured_provider_names(cfg, snapshot);
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
    ]
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

fn build_json_report(fix_requested: bool, checks: &[Check], actions: &[FixAction]) -> DoctorReport {
    let summary = summarize_checks(checks);
    let checks = checks
        .iter()
        .map(|check| DoctorCheckReport {
            id: check.id,
            group: check.group.as_str(),
            name: check.name,
            status: check.status.as_str(),
            ok: matches!(check.status, CheckStatus::Pass),
            detail: check.detail.clone(),
            guidance: check.guidance.clone(),
            path: check.path.clone(),
            expected: check.expected.clone(),
            actual: check.actual.clone(),
            next_steps: check.next_steps.clone(),
        })
        .collect();
    let fixes = actions
        .iter()
        .map(|action| DoctorFixReport {
            id: action.id,
            name: action.name,
            status: if action.ok { "applied" } else { "failed" },
            ok: action.ok,
            detail: action.detail.clone(),
        })
        .collect();

    DoctorReport {
        version: env!("CARGO_PKG_VERSION"),
        ok: summary.failed == 0,
        fix_requested,
        summary,
        checks,
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

fn apply_safe_fixes(cfg: &config::Config) -> Vec<FixAction> {
    let mut actions = Vec::new();

    match dcserver::agentdesk_runtime_root() {
        Some(root) => {
            let dirs = [
                root.clone(),
                root.join("config"),
                root.join("logs"),
                root.join("releases"),
                root.join("credential"),
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
    match rusqlite::Connection::open(&db_path) {
        Ok(conn) => match schema::migrate(&conn) {
            Ok(()) => actions.push(FixAction::ok(
                "db_schema",
                "DB Schema",
                format!("ensured schema at {}", db_path.display()),
            )),
            Err(e) => actions.push(FixAction::fail(
                "db_schema",
                "DB Schema",
                format!("migration failed for {}: {}", db_path.display(), e),
            )),
        },
        Err(e) => actions.push(FixAction::fail(
            "db_schema",
            "DB Schema",
            format!("cannot open {}: {}", db_path.display(), e),
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

fn apply_service_fix(snapshot: &HealthSnapshot) -> Vec<FixAction> {
    const READY_TIMEOUT: Duration = Duration::from_secs(30);

    if snapshot_is_healthy(snapshot) {
        return Vec::new();
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

fn print_fix_actions(actions: &[FixAction]) {
    if actions.is_empty() {
        return;
    }

    println!("Applying safe fixes");
    for action in actions {
        let label = if action.ok { "APPLIED" } else { "FAILED" };
        let icon = if action.ok { "✓" } else { "✗" };
        println!("  {icon} [{label}] {}: {}", action.name, action.detail);
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
        ProviderKind::Unsupported(_) => "provider CLI",
    };
    let capability_summary = provider_capability_summary(&provider);
    let connected = provider_connected(snapshot, &provider);
    let log_hint = dcserver_log_hint();
    let binary_name = provider.as_str().to_string();
    match provider.probe_runtime() {
        Some(probe) => match (probe.binary_path, probe.version) {
            (Some(path), Some(ver)) => {
                let health_note = match connected {
                    Some(true) => "health=connected".to_string(),
                    Some(false) => "health=disconnected".to_string(),
                    None => "health=unknown".to_string(),
                };
                let detail = format!("{ver} — {path} [{capability_summary}; {health_note}]");

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
                let detail = format!("{path} — version probe failed [{capability_summary}]");
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
                if configured {
                    Check::fail(
                        id,
                        CheckGroup::ProviderRuntime,
                        name,
                        format!("not found in runtime PATH [{capability_summary}]"),
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
                Check::fail(
                    "server",
                    CheckGroup::Core,
                    "Server",
                    detail,
                    "health endpoint는 응답했지만 서비스 상태가 healthy가 아닙니다. dcserver 상태와 provider 초기화를 확인하세요.",
                )
                .with_path(health_endpoint(&snapshot.base))
                .with_expected_actual(
                    "reachable healthy health endpoint",
                    format!("status={status}"),
                )
                .with_next_steps(vec![
                    "agentdesk doctor --fix".to_string(),
                    format!("curl -s {}", health_endpoint(&snapshot.base)),
                    format!("tail -n 200 {}", dcserver_log_hint()),
                ])
            }
        }
        None => Check::fail(
            "server",
            CheckGroup::Core,
            "Server",
            format!(
                "not reachable — {} ({})",
                snapshot.base,
                snapshot
                    .error
                    .clone()
                    .unwrap_or_else(|| "unknown error".to_string())
            ),
            "dcserver/axum 서버가 떠 있는지와 방화벽/포트 접근 가능 여부를 확인하세요.",
        )
        .with_path(health_endpoint(&snapshot.base))
        .with_expected_actual("reachable health endpoint", "health endpoint unreachable")
        .with_next_steps(vec![
            "agentdesk doctor --fix".to_string(),
            format!("curl -s {}", health_endpoint(&snapshot.base)),
        ]),
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
    if !db_path.exists() {
        return Check::fail(
            "db_integrity",
            CheckGroup::Core,
            "DB File",
            format!("{} — not found", db_path.display()),
            "agentdesk doctor --fix 로 DB 파일과 기본 스키마를 생성할 수 있습니다.",
        )
        .with_path(db_path.display().to_string())
        .with_expected_actual("database file exists", "database file missing")
        .with_next_steps(vec!["agentdesk doctor --fix".to_string()]);
    }
    match rusqlite::Connection::open(&db_path) {
        Ok(conn) => {
            match conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0)) {
                Ok(result) if result == "ok" => {
                    let size = std::fs::metadata(&db_path)
                        .map(|m| format!("{:.1} MB", m.len() as f64 / 1_048_576.0))
                        .unwrap_or_default();
                    Check::ok(
                        "db_integrity",
                        CheckGroup::Core,
                        "DB Integrity",
                        format!("ok — {size}"),
                    )
                    .with_path(db_path.display().to_string())
                    .with_expected_actual("PRAGMA integrity_check = ok", "ok")
                }
                Ok(result) => Check::fail(
                    "db_integrity",
                    CheckGroup::Core,
                    "DB Integrity",
                    format!("issues: {result}"),
                    "DB 손상이 의심됩니다. 백업 후 schema migrate/복구 절차를 검토하세요.",
                )
                .with_path(db_path.display().to_string())
                .with_expected_actual("PRAGMA integrity_check = ok", result)
                .with_next_steps(vec!["agentdesk doctor --fix".to_string()]),
                Err(e) => Check::fail(
                    "db_integrity",
                    CheckGroup::Core,
                    "DB Integrity",
                    format!("check failed: {e}"),
                    "DB 연결 또는 권한 문제를 확인하세요.",
                )
                .with_path(db_path.display().to_string())
                .with_expected_actual(
                    "database integrity check runs",
                    format!("integrity check error: {e}"),
                ),
            }
        }
        Err(e) => Check::fail(
            "db_integrity",
            CheckGroup::Core,
            "DB File",
            format!("cannot open: {e}"),
            "DB 파일 권한과 data 디렉터리 상태를 확인하세요.",
        )
        .with_path(db_path.display().to_string())
        .with_expected_actual(
            "database opens successfully",
            format!("database open failed: {e}"),
        )
        .with_next_steps(vec!["agentdesk doctor --fix".to_string()]),
    }
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
        Some(path) => match std::fs::read_dir(&path) {
            Ok(entries) => {
                let mut total: u64 = 0;
                for entry in entries.flatten() {
                    if let Ok(meta) = entry.metadata() {
                        total += meta.len();
                    }
                }
                let mb = total as f64 / 1_048_576.0;
                Check::ok(
                    "disk_usage",
                    CheckGroup::Core,
                    "Disk Usage",
                    format!("{:.1} MB in {}", mb, path.display()),
                )
                .with_path(path.display().to_string())
                .with_expected_actual("disk usage readable", format!("{:.1} MB", mb))
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

pub fn cmd_doctor(fix: bool, json: bool) -> Result<(), String> {
    let cfg = config::load_graceful();
    let mut actions = Vec::new();
    if fix {
        actions = apply_safe_fixes(&cfg);
        let pre_fix_snapshot = fetch_health_snapshot();
        actions.extend(apply_service_fix(&pre_fix_snapshot));
        if !json {
            println!("AgentDesk Doctor v{}\n", env!("CARGO_PKG_VERSION"));
            print_fix_actions(&actions);
        }
    } else if !json {
        println!("AgentDesk Doctor v{}\n", env!("CARGO_PKG_VERSION"));
    }

    let snapshot = fetch_health_snapshot();
    let checks = build_all_checks(&cfg, &snapshot);
    let summary = summarize_checks(&checks);

    if json {
        let report = build_json_report(fix, &checks, &actions);
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|e| format!("failed to serialize doctor report: {e}"))?
        );
    } else {
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
            summary.passed, summary.warned, summary.failed, summary.total
        );
    }

    if summary.failed > 0 {
        Err(format!("{} diagnostic check(s) failed", summary.failed))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Check, CheckGroup, CheckStatus, FixAction, HealthSnapshot, build_json_report,
        check_server_running, configured_provider_names, discord_bot_check_from_health,
        provider_capability_summary,
    };
    use crate::config::ServerConfig;
    use crate::services::provider::ProviderKind;
    use serde_json::json;

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
            channels: std::collections::HashMap::new(),
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

        let report = build_json_report(true, &checks, &fixes);
        let value = serde_json::to_value(report).expect("serialize doctor report");

        assert_eq!(value["fix_requested"], true);
        assert_eq!(value["summary"]["warned"], 1);
        assert_eq!(value["checks"][0]["id"], "service_manager");
        assert_eq!(value["checks"][0]["group"], "core");
        assert_eq!(value["checks"][0]["status"], "warn");
        assert_eq!(value["checks"][0]["path"], "systemctl --user");
        assert_eq!(value["checks"][0]["expected"], "service active");
        assert_eq!(value["checks"][0]["actual"], "service inactive");
        assert_eq!(
            value["checks"][0]["next_steps"][0],
            "systemctl --user status agentdesk-dcserver"
        );
        assert_eq!(value["fixes"][0]["id"], "service_restart");
        assert_eq!(value["fixes"][0]["status"], "applied");
    }
}
