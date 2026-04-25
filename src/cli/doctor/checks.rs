use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use serde_json::Value;

use super::super::{client, dcserver};
use super::types::{Check, CheckGroup, HealthSnapshot};
use crate::config;
use crate::services::provider::ProviderKind;

pub(super) fn fetch_health_snapshot() -> HealthSnapshot {
    let base = client::api_base();
    match client::get_json("/api/health") {
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

pub(super) fn health_providers(snapshot: &HealthSnapshot) -> Option<&Vec<Value>> {
    snapshot.body.as_ref()?.get("providers")?.as_array()
}

pub(super) fn provider_connected(
    snapshot: &HealthSnapshot,
    provider: &ProviderKind,
) -> Option<bool> {
    let provider_name = provider.as_str();
    health_providers(snapshot)?
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(provider_name))
        .and_then(|entry| entry.get("connected").and_then(Value::as_bool))
}

pub(super) fn configured_provider_names(
    cfg: &config::Config,
    snapshot: &HealthSnapshot,
) -> BTreeSet<String> {
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

pub(super) fn provider_runtime_guidance(provider: &ProviderKind) -> String {
    let provider_name = provider.as_str();
    let log_hint = dcserver_log_hint();
    format!(
        "{provider_name} CLI 설치/PATH와 서비스 런타임 PATH를 확인하고, 연결 문제가 있으면 {log_hint} 로그와 provider 인증 상태를 점검하세요."
    )
}

pub(super) fn provider_unused_guidance(provider: &ProviderKind) -> String {
    format!(
        "{} CLI는 설치되어 있지만 현재 config/health 기준 활성 provider가 아닙니다. 의도한 구성이면 무시해도 됩니다.",
        provider.as_str()
    )
}

pub(super) fn dcserver_log_hint() -> String {
    dcserver::dcserver_stdout_log_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "~/.adk/release/logs/dcserver.stdout.log".to_string())
}

pub(super) fn qwen_home_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("QWEN_HOME") {
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }

    #[cfg(test)]
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

pub(super) fn qwen_project_dir() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

pub(super) fn qwen_system_defaults_path() -> Option<PathBuf> {
    std::env::var_os("QWEN_CODE_SYSTEM_DEFAULTS_PATH")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

pub(super) fn qwen_system_settings_path() -> Option<PathBuf> {
    std::env::var_os("QWEN_CODE_SYSTEM_SETTINGS_PATH")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

pub(super) fn qwen_user_settings_path() -> Option<PathBuf> {
    qwen_home_dir().map(|home| home.join(".qwen").join("settings.json"))
}

pub(super) fn qwen_project_settings_path() -> Option<PathBuf> {
    qwen_project_dir().map(|dir| dir.join(".qwen").join("settings.json"))
}

pub(super) fn format_artifact_scope(scope: &str, items: &[&str]) -> String {
    if items.is_empty() {
        format!("{scope}: -")
    } else {
        format!("{scope}: {}", items.join(", "))
    }
}

pub(super) fn check_qwen_settings_files(configured: bool) -> Check {
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

pub(super) fn check_qwen_auth_hints(configured: bool) -> Check {
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

pub(super) fn check_qwen_runtime_artifacts(configured: bool) -> Check {
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

pub(super) fn health_endpoint(base: &str) -> String {
    format!("{}/api/health", base.trim_end_matches('/'))
}

pub(super) fn stale_zero_byte_db_candidates(
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

pub(super) fn provider_check_id(provider: &ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Claude => "provider_claude",
        ProviderKind::Codex => "provider_codex",
        ProviderKind::Gemini => "provider_gemini",
        ProviderKind::Qwen => "provider_qwen",
        ProviderKind::Unsupported(_) => "provider_unsupported",
    }
}

pub(super) fn build_core_checks(cfg: &config::Config, snapshot: &HealthSnapshot) -> Vec<Check> {
    vec![
        check_server_running(snapshot),
        check_discord_bot(snapshot),
        check_runtime_root(),
        check_data_dir(cfg),
        check_tmux(),
        check_service_manager(),
        check_postgres_connection(cfg),
        check_db_integrity(cfg),
        check_stale_zero_byte_db_files(cfg),
        check_github_repo_registry(cfg),
        check_disk_usage(),
    ]
}

pub(super) fn build_provider_checks(cfg: &config::Config, snapshot: &HealthSnapshot) -> Vec<Check> {
    let configured = configured_provider_names(cfg, snapshot);
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
        check_provider_cli(ProviderKind::Qwen, qwen_configured, snapshot),
        check_qwen_settings_files(qwen_configured),
        check_qwen_auth_hints(qwen_configured),
        check_qwen_runtime_artifacts(qwen_configured),
    ]
}

pub(super) fn build_all_checks(cfg: &config::Config, snapshot: &HealthSnapshot) -> Vec<Check> {
    let mut checks = build_core_checks(cfg, snapshot);
    checks.extend(build_provider_checks(cfg, snapshot));
    checks
}

pub(super) fn discord_bot_check_from_health(base: &str, body: &Value) -> Check {
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

pub(super) fn check_discord_bot(snapshot: &HealthSnapshot) -> Check {
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

pub(super) fn check_tmux() -> Check {
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

pub(super) fn provider_capability_summary(provider: &ProviderKind) -> String {
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

pub(super) fn check_provider_cli(
    provider: ProviderKind,
    configured: bool,
    snapshot: &HealthSnapshot,
) -> Check {
    let id = provider_check_id(&provider);
    let name = match provider {
        ProviderKind::Claude => "claude CLI",
        ProviderKind::Codex => "codex CLI",
        ProviderKind::Gemini => "gemini CLI",
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

pub(super) fn check_runtime_path() -> Check {
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

pub(super) fn check_server_running(snapshot: &HealthSnapshot) -> Check {
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

pub(super) fn check_runtime_root() -> Check {
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

pub(super) fn check_data_dir(cfg: &config::Config) -> Check {
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
pub(super) fn check_service_manager() -> Check {
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
pub(super) fn check_service_manager() -> Check {
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
pub(super) fn check_service_manager() -> Check {
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
pub(super) fn check_service_manager() -> Check {
    Check::ok(
        "service_manager",
        CheckGroup::Core,
        "Service Manager",
        "N/A",
    )
}

pub(super) fn check_db_integrity(cfg: &config::Config) -> Check {
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
    match libsql_rusqlite::Connection::open(&db_path) {
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

pub(super) fn check_postgres_connection(cfg: &config::Config) -> Check {
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
            drop(pool);
            Check::ok(
                "postgres_connection",
                CheckGroup::Core,
                "PostgreSQL",
                format!("{summary} — ok"),
            )
            .with_expected_actual("postgres connection succeeds", "ok")
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

pub(super) fn check_stale_zero_byte_db_files(cfg: &config::Config) -> Check {
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

pub(super) fn check_github_repo_registry(cfg: &config::Config) -> Check {
    let db_path = cfg.data.dir.join(&cfg.data.db_name);
    if !db_path.exists() {
        return Check::warn(
            "github_repo_registry",
            CheckGroup::Core,
            "GitHub Repo Registry",
            format!("{} — skipped (DB missing)", db_path.display()),
            "DB 파일이 없어서 config.github.repos와 github_repos 비교를 건너뒀습니다.",
        )
        .with_path(db_path.display().to_string())
        .with_expected_actual("github_repos matches config.github.repos", "db unavailable");
    }

    let (configured, invalid_config) = normalized_config_repo_ids(cfg);
    let db_repos = match open_registered_github_repo_ids(&db_path) {
        Ok(repos) => repos,
        Err(error) => {
            return Check::warn(
                "github_repo_registry",
                CheckGroup::Core,
                "GitHub Repo Registry",
                format!("{} — skipped ({error})", db_path.display()),
                "DB repo registry를 읽지 못했습니다. DB 상태와 권한을 확인하세요.",
            )
            .with_path(db_path.display().to_string())
            .with_expected_actual(
                "github_repos matches config.github.repos",
                format!("registry read failed: {error}"),
            );
        }
    };

    let missing_in_db: Vec<String> = configured.difference(&db_repos).cloned().collect();
    let extra_in_db: Vec<String> = db_repos.difference(&configured).cloned().collect();

    if missing_in_db.is_empty() && extra_in_db.is_empty() && invalid_config.is_empty() {
        return Check::ok(
            "github_repo_registry",
            CheckGroup::Core,
            "GitHub Repo Registry",
            format!("config={} db={}", configured.len(), db_repos.len()),
        )
        .with_path(db_path.display().to_string())
        .with_expected_actual(
            "github_repos matches config.github.repos",
            format!("config={} db={}", configured.len(), db_repos.len()),
        );
    }

    let mut detail_parts = vec![format!("config={} db={}", configured.len(), db_repos.len())];
    if !missing_in_db.is_empty() {
        detail_parts.push(format!("missing_in_db={}", missing_in_db.join(",")));
    }
    if !extra_in_db.is_empty() {
        detail_parts.push(format!("extra_in_db={}", extra_in_db.join(",")));
    }
    if !invalid_config.is_empty() {
        detail_parts.push(format!("invalid_config={}", invalid_config.join(",")));
    }
    let detail = detail_parts.join(" ");

    Check::warn(
        "github_repo_registry",
        CheckGroup::Core,
        "GitHub Repo Registry",
        detail.clone(),
        "서버 시작 시 config.github.repos를 github_repos에 seed해야 합니다. 누락 repo는 서버 재기동으로 복구되고, extra row는 stale registry인지 점검하세요.",
    )
    .with_path(db_path.display().to_string())
    .with_expected_actual("github_repos matches config.github.repos", detail)
}

pub(super) fn normalized_config_repo_ids(cfg: &config::Config) -> (BTreeSet<String>, Vec<String>) {
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

pub(super) fn open_registered_github_repo_ids(
    db_path: &std::path::Path,
) -> Result<BTreeSet<String>, String> {
    let conn =
        libsql_rusqlite::Connection::open(db_path).map_err(|e| format!("cannot open: {e}"))?;
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

pub(super) fn check_disk_usage() -> Check {
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
