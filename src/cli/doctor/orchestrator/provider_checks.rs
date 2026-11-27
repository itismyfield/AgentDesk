use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::provider_credentials::{
    check_claude_cswap_global_conflict, check_credential_permissions,
};
use super::qwen_checks::{
    check_qwen_auth_hints, check_qwen_runtime_artifacts, check_qwen_settings_files,
};
use super::{
    Check, CheckGroup, FixSafety, HealthSnapshot, SecurityExposure, Severity, check_runtime_path,
    config, dcserver_log_hint, health_detail_endpoint, provider_cli_name,
};
use crate::services::provider::{ProviderKind, ProviderRuntimeExt, ProviderRuntimeProbe};

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
        "Check {provider_name} CLI installation/PATH and service runtime PATH. If there are connection issues, check {log_hint} logs and provider authentication status."
    )
}

fn provider_check_id(provider: &ProviderKind) -> &'static str {
    provider.doctor_check_id()
}

pub(super) fn build_provider_checks(cfg: &config::Config, snapshot: &HealthSnapshot) -> Vec<Check> {
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
        check_provider_cli(ProviderKind::OpenCode, opencode_configured, snapshot),
        check_opencode_mcp_config(opencode_configured),
        check_opencode_serve_health_probe(opencode_configured),
        check_provider_cli(ProviderKind::Qwen, qwen_configured, snapshot),
        check_qwen_settings_files(qwen_configured),
        check_qwen_auth_hints(qwen_configured),
        check_qwen_runtime_artifacts(qwen_configured),
        check_provider_bindings(cfg, snapshot),
        check_claude_cswap_global_conflict(),
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
            "configure the memento server in runtime mcp_servers or the top-level mcp in ~/.config/opencode/opencode.json.",
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
            "Check CLI installation, configuration files, and provider/model authentication status to ensure opencode serve starts normally.",
        )
        .with_expected_actual("opencode serve /global/health returns 200", "probe failed")
        .with_next_steps(vec![
            "opencode --version".to_string(),
            "opencode serve --hostname 127.0.0.1 --port 0".to_string(),
            format!("tail -n 200 {}", dcserver_log_hint()),
        ]),
    }
}

pub(super) fn check_health_db_dashboard(snapshot: &HealthSnapshot) -> Check {
    let Some(body) = snapshot.body.as_ref() else {
        return Check::fail(
            "health_db_dashboard",
            CheckGroup::Core,
            "DB/Dashboard Health",
            "health payload unavailable",
            "The dcserver health detail endpoint must be reachable to summarize DB/dashboard status.",
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
            "DB health is false. Check the Postgres source-of-truth status first.",
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
            "Dashboard dist is missing or unreadable. Even if the API is working, check the UI asset deployment status.",
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
            "The health detail payload does not provide a DB/dashboard summary.",
        )
        .with_subsystem("health")
        .with_path(health_detail_endpoint(&snapshot.base))
        .with_expected_actual("db/dashboard fields present", detail)
        .with_evidence(evidence),
    }
}

pub(super) fn check_config_audit(snapshot: &HealthSnapshot) -> Check {
    let Some(body) = snapshot.body.as_ref() else {
        return Check::fail(
            "config_audit",
            CheckGroup::Core,
            "Config Audit",
            "health payload unavailable",
            "Cannot read config audit report.",
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
            "The dcserver startup config audit has not run yet, or there is no persisted report.",
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

    let has_binding_issues = !disconnected.is_empty()
        || !missing_channels.is_empty()
        || !missing_runtime_providers.is_empty()
        || !missing_auth_hints.is_empty()
        || !duplicate_providers.is_empty();
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

    if !has_binding_issues {
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
            "Check agent/provider/channel binding and Discord bot auth hints separately.",
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

fn provider_credential_summary(probe: &ProviderRuntimeProbe) -> String {
    match (probe.credential_present, probe.credential_source.as_deref()) {
        (true, Some(source)) => {
            format!("credentials=present source={source} (auth usability unverified)")
        }
        (true, None) => {
            "credentials=present source=unknown (auth usability unverified)".to_string()
        }
        (false, _) => "credentials=not_observed (auth usability unverified)".to_string(),
    }
}

fn provider_auth_check_command(provider: &ProviderKind, binary_name: &str) -> String {
    crate::services::provider_auth::auth_check_hint(
        provider
            .registry_entry()
            .and_then(|entry| entry.auth.auth_check_argv),
        binary_name,
    )
}

fn provider_runtime_evidence(probe: &ProviderRuntimeProbe) -> Value {
    json!({
        "provider": probe.provider.as_str(),
        "credential_present": probe.credential_present,
        "credential_source": probe.credential_source.clone(),
        "credential_status_basis": "presence_only",
        "auth_usability_verified": false,
        "auth_check_argv": probe
            .provider
            .registry_entry()
            .and_then(|entry| entry.auth.auth_check_argv)
            .map(|argv| argv.join(" ")),
        "binary_source": probe.resolution.source.clone(),
        "resolved_path": probe.resolution.resolved_path.clone(),
        "canonical_path": probe.resolution.canonical_path.clone(),
        "probe_failure_kind": probe.probe_failure_kind.clone(),
        "resolution_failure_kind": probe.resolution.failure_kind.clone(),
        "resolution_attempts": probe.resolution.attempts.clone(),
        "skipped_candidate_failures": probe.skipped_candidate_failures.clone(),
    })
}

fn check_provider_cli(
    provider: ProviderKind,
    configured: bool,
    snapshot: &HealthSnapshot,
) -> Check {
    let id = provider_check_id(&provider);
    let name = provider_cli_name::provider_cli_check_name(&provider);
    let capability_summary = provider_capability_summary(&provider);
    let connected = provider_connected(snapshot, &provider);
    let log_hint = dcserver_log_hint();
    let binary_name = provider.as_str().to_string();
    match provider.probe_runtime() {
        Some(probe) => match (
            probe.resolution.resolved_path.clone(),
            probe.version.clone(),
        ) {
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
                let auth_note = provider_credential_summary(&probe);
                let detail = format!(
                    "{ver} — {path} [{source}; {capability_summary}; {health_note}; {auth_note}]"
                );

                if !configured {
                    Check::ok(
                        id,
                        CheckGroup::ProviderRuntime,
                        name,
                        format!("{detail} — installed but not referenced by current config/health"),
                    )
                    .with_path(path)
                    .with_expected_actual(
                        "provider configured only when used",
                        "binary exists but provider is unused",
                    )
                    .with_evidence(provider_runtime_evidence(&probe))
                    .with_security_exposure(SecurityExposure::CredentialMetadata)
                } else if !probe.credential_present {
                    Check::warn(
                        id,
                        CheckGroup::ProviderRuntime,
                        name,
                        detail,
                        provider_runtime_guidance(&provider),
                    )
                    .with_path(path)
                    .with_expected_actual(
                        "provider credential visible to doctor (auth usability unverified)",
                        "credential not observed by doctor process",
                    )
                    .with_next_steps(vec![
                        provider_auth_check_command(&provider, &binary_name),
                        format!("which {}", binary_name),
                        format!("tail -n 200 {}", log_hint),
                    ])
                    .with_evidence(provider_runtime_evidence(&probe))
                    .with_security_exposure(SecurityExposure::CredentialMetadata)
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
                    .with_evidence(provider_runtime_evidence(&probe))
                    .with_security_exposure(SecurityExposure::CredentialMetadata)
                } else {
                    Check::ok(id, CheckGroup::ProviderRuntime, name, detail)
                        .with_path(path)
                        .with_expected_actual("provider binary usable", "provider binary usable")
                        .with_evidence(provider_runtime_evidence(&probe))
                        .with_security_exposure(SecurityExposure::CredentialMetadata)
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
                    "{path} — version probe failed [{source}; {probe_failure_kind}; {capability_summary}; {}]",
                    provider_credential_summary(&probe)
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
                    .with_evidence(provider_runtime_evidence(&probe))
                    .with_security_exposure(SecurityExposure::CredentialMetadata)
                } else {
                    Check::ok(id, CheckGroup::ProviderRuntime, name, detail)
                        .with_path(path)
                        .with_expected_actual(
                            "provider probe required only when configured",
                            "version probe failed for unused provider",
                        )
                        .with_evidence(provider_runtime_evidence(&probe))
                        .with_security_exposure(SecurityExposure::CredentialMetadata)
                }
            }
            (None, Some(ver)) => Check::ok(
                id,
                CheckGroup::ProviderRuntime,
                name,
                format!(
                    "{ver} — unknown path [{capability_summary}; {}]",
                    provider_credential_summary(&probe)
                ),
            )
            .with_expected_actual("provider path known", "version known but path unknown")
            .with_evidence(provider_runtime_evidence(&probe))
            .with_security_exposure(SecurityExposure::CredentialMetadata),
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
                        format!(
                            "not found in runtime PATH [{failure_kind}; {capability_summary}; {}]",
                            provider_credential_summary(&probe)
                        ),
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
                    .with_evidence(provider_runtime_evidence(&probe))
                    .with_security_exposure(SecurityExposure::CredentialMetadata)
                } else {
                    Check::ok(
                        id,
                        CheckGroup::ProviderRuntime,
                        name,
                        format!(
                            "not configured [{capability_summary}; {}]",
                            provider_credential_summary(&probe)
                        ),
                    )
                    .with_expected_actual(
                        "provider configured if needed",
                        "provider not configured",
                    )
                    .with_evidence(provider_runtime_evidence(&probe))
                    .with_security_exposure(SecurityExposure::CredentialMetadata)
                }
            }
        },
        None => Check::fail(
            id,
            CheckGroup::ProviderRuntime,
            name,
            "unsupported provider",
            "Unsupported provider.",
        )
        .with_expected_actual("supported provider", "unsupported provider"),
    }
}
