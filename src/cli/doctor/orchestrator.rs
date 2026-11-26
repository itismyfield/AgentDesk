#[cfg(target_os = "macos")]
mod fd_usage;
#[cfg(target_os = "macos")]
use fd_usage::{fd_usage_near_limit, fd_usage_percent};

mod health_snapshot;
use health_snapshot::fetch_health_snapshot;
mod provider_credentials;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::contract::{DoctorProfile, FixSafety, RunContext, SecurityExposure, Severity};
use super::{health, mailbox};

mod api_exposure;
mod config_dir_checks;
mod configuration_audit;
use crate::cli::dcserver;
use crate::config;
use serde_json::{Value, json};

mod check_model;
mod provider_checks;
mod qwen_checks;
mod report_build;

use super::provider_cli_name;
use check_model::{Check, CheckGroup, CheckStatus, FixAction, HealthSnapshot, print_group};
pub(crate) use check_model::{
    DoctorCheckReport, DoctorFixReport, DoctorOptions, DoctorReport, DoctorSectionReport,
    DoctorSummary,
};
use provider_checks::{build_provider_checks, check_config_audit, check_health_db_dashboard};
use qwen_checks::{qwen_home_dir, qwen_project_dir};
use report_build::{build_all_checks, build_json_report, permission_finding};

fn dcserver_log_hint() -> String {
    dcserver::dcserver_stdout_log_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "~/.adk/release/logs/dcserver.stdout.log".to_string())
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

fn highest_reason_fix_safety(reasons: &[health::ClassifiedReason]) -> FixSafety {
    reasons
        .iter()
        .map(|reason| reason.fix_safety)
        .max_by_key(|safety| safety.restriction_rank())
        .unwrap_or(FixSafety::ReadOnly)
}

fn stale_zero_byte_db_candidates(
    runtime_root: &std::path::Path,
    canonical_db_path: &std::path::Path,
) -> Vec<PathBuf> {
    crate::compat::legacy_db_paths::LEGACY_LOCAL_DB_FILENAMES
        .iter()
        .map(|name| runtime_root.join(name))
        .filter(|candidate| candidate != canonical_db_path)
        .filter(|candidate| {
            fs::metadata(candidate)
                .map(|meta| meta.is_file() && meta.len() == 0)
                .unwrap_or(false)
        })
        .collect()
}

mod herdr_policy;
use herdr_policy::build_core_checks;

fn apply_safe_fixes(cfg: &config::Config, options: &DoctorOptions) -> Vec<FixAction> {
    let mut actions = Vec::new();

    match crate::config::runtime_root() {
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

    match crate::config::runtime_root() {
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

mod dispatch_outbox;
mod managed_herdr_config;
mod repair_response;
mod report_display;
use dispatch_outbox::check_dispatch_outbox;
mod observation_checks;
use observation_checks::check_mailbox_consistency;
#[cfg(test)]
mod observation_tests;
mod stale_mailbox_repair;

fn apply_stale_mailbox_fixes(snapshot: &HealthSnapshot, options: &DoctorOptions) -> Vec<FixAction> {
    stale_mailbox_repair::apply_stale_mailbox_fixes_with_post(
        snapshot,
        options,
        crate::cli::client::post_json_value,
    )
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
        let observation = health::degraded_reasons(body);
        let reasons = observation.as_deref().unwrap_or(&[]);
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
                "All providers are connected; check degraded reasons instead of token/offline guidance.",
            )
            .with_subsystem("provider_runtime")
            .with_severity(highest_reason_severity(&provider_reasons))
            .with_fix_safety(highest_reason_fix_safety(&provider_reasons))
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
                "dcserver may not have registered providers yet. Check startup logs and bot token configuration.",
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
                "overall={overall}, connected={}/{}, offline={}{}",
                connected.len(),
                total,
                if disconnected.is_empty() {
                    "-".to_string()
                } else {
                    disconnected.join(", ")
                },
                observation.as_ref().err().map(|issue| format!("; reasons={}", issue.describe())).unwrap_or_default()
            ),
            "Check Discord tokens, gateway connection status, and dcserver stdout logs for offline providers.",
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
            "The server responds, but the Discord provider health registry is empty. Check if running in standalone mode.",
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
            "Discord provider initialization may have failed even if the server is running. Check dcserver stdout logs.",
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
            "Ensure dcserver is running and /api/health is reachable.",
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
            "Install tmux if you plan to use the Claude/Codex tmux backend.",
        )
        .with_path("tmux")
        .with_expected_actual("tmux available in PATH", "tmux not found")
        .with_next_steps(vec!["which tmux".to_string()]),
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
            "Could not read the login shell PATH. Compare the service environment PATH with your shell PATH.",
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
                let observation = health::degraded_reasons(body);
                let reasons = observation.as_deref().unwrap_or(&[]);
                let reason_detail = if let Err(issue) = observation.as_ref() {
                    format!("{detail}; reasons={}", issue.describe())
                } else if reasons.is_empty() {
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
                    "The health endpoint responded, but the service status is not healthy. Check the subsystem for each degraded reason first.",
                )
                .with_subsystem("health")
                .with_severity(highest_reason_severity(&reasons))
                .with_fix_safety(highest_reason_fix_safety(&reasons))
                .with_security_exposure(SecurityExposure::OperationalMetadata)
                .with_evidence(match &observation {
                    Ok(reasons) => health::reasons_evidence(reasons),
                    Err(issue) => json!({"measurement_issue": issue}),
                })
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
                    "check the auth token or /api/health/detail permissions.",
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
                    "Check if the dcserver/axum server is running and if the firewall/port is accessible.",
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
    match crate::config::runtime_root() {
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
            "you can create the default runtime directory with agentdesk doctor --fix.",
        )
        .with_path(path.display().to_string())
        .with_expected_actual("runtime root exists", "runtime root missing")
        .with_next_steps(vec!["agentdesk doctor --fix".to_string()]),
        None => Check::fail(
            "runtime_root",
            CheckGroup::Core,
            "Runtime Root",
            "unable to determine runtime root",
            "check AGENTDESK_ROOT_DIR or the default ~/.adk/release path.",
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
            "could not classify degraded reasons because the health endpoint is unavailable.",
        )
        .with_subsystem("health")
        .with_security_exposure(SecurityExposure::OperationalMetadata)
        .with_fix_safety(FixSafety::NotFixable);
    };
    let reasons = match health::degraded_reasons(body) {
        Ok(reasons) => reasons,
        Err(issue) => {
            return observation_checks::unmeasured_check("health_degraded_reasons", &issue);
        }
    };
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
            "check health degraded reasons by subsystem.",
        ),
        CheckStatus::Fail => Check::fail(
            "health_degraded_reasons",
            CheckGroup::Core,
            "Health Reasons",
            detail.clone(),
            "resolve error/critical degraded reasons first.",
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
        .with_fix_safety(highest_reason_fix_safety(&reasons))
        .with_expected_actual("no degraded reasons", detail)
        .with_next_steps(next_steps);
    check
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
        let fallback_session = dcserver::current_dcserver_tmux_fallback_session();
        if dcserver::is_tmux_fallback_session_loaded(&fallback_session) {
            return Check::ok(
                "service_manager",
                CheckGroup::Core,
                "Service Manager",
                format!("tmux fallback — {fallback_session} active"),
            )
            .with_expected_actual(
                "launchd job loaded or tmux fallback active",
                format!("{fallback_session} active"),
            );
        }

        Check::warn(
            "service_manager",
            CheckGroup::Core,
            "Service Manager",
            format!("launchd — {label} not loaded"),
            "launchd로 운영 중이면 plist 로드 상태를 확인하세요. 수동 실행 환경이면 무시해도 됩니다.",
        )
        .with_expected_actual(
            "launchd job loaded or tmux fallback active",
            format!("{label} not loaded; {fallback_session} inactive"),
        )
        .with_next_steps(vec![
            format!(
                "launchctl print {}/{label}",
                dcserver::current_launchd_domain().unwrap_or_else(|| "gui/$(id -u)".to_string())
            ),
            format!("tmux has-session -t ={fallback_session}:"),
            "agentdesk doctor --fix".to_string(),
        ])
    }
}

#[cfg(target_os = "macos")]
const FD_HEADROOM_WARN_PERCENT: u64 = 80;
#[cfg(target_os = "macos")]
const FD_HEADROOM_WARN_REMAINING: u64 = 64;

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FdLimitValue {
    Finite(u64),
    Unlimited,
    Unknown,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LaunchdMaxfilesLimit {
    soft: FdLimitValue,
    hard: FdLimitValue,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug, PartialEq, Eq)]
struct FdUsageSample {
    process: &'static str,
    pid: u32,
    open_files: u64,
    soft_limit: FdLimitValue,
    limit_source: &'static str,
}

#[cfg(target_os = "macos")]
fn parse_fd_limit_value(value: &str) -> Option<FdLimitValue> {
    if value.eq_ignore_ascii_case("unlimited") {
        return Some(FdLimitValue::Unlimited);
    }
    value.parse::<u64>().ok().map(FdLimitValue::Finite)
}

#[cfg(target_os = "macos")]
fn parse_launchctl_maxfiles_limit(stdout: &str) -> Option<LaunchdMaxfilesLimit> {
    stdout.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        if parts.next()? != "maxfiles" {
            return None;
        }
        let soft = parse_fd_limit_value(parts.next()?)?;
        let hard = parse_fd_limit_value(parts.next()?)?;
        Some(LaunchdMaxfilesLimit { soft, hard })
    })
}

#[cfg(target_os = "macos")]
fn read_launchctl_maxfiles_limit() -> Option<LaunchdMaxfilesLimit> {
    let output = std::process::Command::new("launchctl")
        .args(["limit", "maxfiles"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_launchctl_maxfiles_limit(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(target_os = "macos")]
fn launchagent_plist_path(label: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{label}.plist")),
    )
}

#[cfg(target_os = "macos")]
fn launchagent_plist_nofile_limit(label: &str) -> Option<u64> {
    let plist_path = launchagent_plist_path(label)?;
    let output = std::process::Command::new("/usr/libexec/PlistBuddy")
        .arg("-c")
        .arg("Print :SoftResourceLimits:NumberOfFiles")
        .arg(&plist_path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .ok()
}

#[cfg(not(target_os = "macos"))]
#[allow(dead_code, reason = "Retained for cross-platform lint path resolution")]
fn tmux_server_pids() -> Vec<u32> {
    Vec::new()
}

#[cfg(target_os = "macos")]
fn tmux_server_pids() -> Vec<u32> {
    let owner_marker = crate::services::tmux_common::current_tmux_owner_marker();
    let sessions = match crate::services::platform::tmux::list_sessions_with_server_pid() {
        Ok(sessions) => sessions,
        Err(_) => return Vec::new(),
    };
    let mut pids = BTreeSet::new();
    for session in sessions {
        if session.server_pid == 0 || !session.session_name.starts_with("AgentDesk-") {
            continue;
        }
        let owner_path = crate::services::tmux_common::tmux_owner_path(&session.session_name);
        let belongs_to_current_runtime = std::fs::read_to_string(owner_path)
            .map(|value| value.trim() == owner_marker)
            .unwrap_or(false);
        if belongs_to_current_runtime {
            pids.insert(session.server_pid);
        }
    }
    pids.into_iter().collect()
}

#[cfg(target_os = "macos")]
fn lsof_fd_column_counts_against_rlimit(fd: &str) -> bool {
    fd.chars()
        .next()
        .map(|first| first.is_ascii_digit())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn open_file_count_for_pid(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("lsof")
        .args(["-nP", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .skip(1)
            .filter(|line| {
                line.split_whitespace()
                    .nth(3)
                    .is_some_and(lsof_fd_column_counts_against_rlimit)
            })
            .count() as u64,
    )
}

#[cfg(target_os = "macos")]
fn fd_limit_display(limit: FdLimitValue) -> String {
    match limit {
        FdLimitValue::Finite(value) => value.to_string(),
        FdLimitValue::Unlimited => "unlimited".to_string(),
        FdLimitValue::Unknown => "unknown".to_string(),
    }
}

#[cfg(target_os = "macos")]
fn fd_limit_evidence(limit: FdLimitValue) -> Value {
    match limit {
        FdLimitValue::Finite(value) => json!(value),
        FdLimitValue::Unlimited => json!("unlimited"),
        FdLimitValue::Unknown => json!("unknown"),
    }
}

#[cfg(target_os = "macos")]
fn sample_usage_percent(sample: &FdUsageSample) -> Option<u64> {
    match sample.soft_limit {
        FdLimitValue::Finite(limit) => Some(fd_usage_percent(sample.open_files, limit)),
        FdLimitValue::Unlimited | FdLimitValue::Unknown => None,
    }
}

#[cfg(target_os = "macos")]
fn sample_is_near_limit(sample: &FdUsageSample) -> bool {
    match sample.soft_limit {
        FdLimitValue::Finite(limit) => fd_usage_near_limit(sample.open_files, limit),
        FdLimitValue::Unlimited | FdLimitValue::Unknown => false,
    }
}

#[cfg(target_os = "macos")]
fn fd_sample_detail(sample: &FdUsageSample) -> String {
    let limit = fd_limit_display(sample.soft_limit);
    match sample_usage_percent(sample) {
        Some(percent) => format!(
            "{} pid {} uses {}/{} open files ({}%, source={})",
            sample.process, sample.pid, sample.open_files, limit, percent, sample.limit_source
        ),
        None => format!(
            "{} pid {} uses {} open files (limit={}, source={})",
            sample.process, sample.pid, sample.open_files, limit, sample.limit_source
        ),
    }
}

#[cfg(target_os = "macos")]
fn file_descriptor_evidence(
    launchd_limit: Option<LaunchdMaxfilesLimit>,
    launchd_job_loaded: bool,
    plist_nofile_limit: Option<u64>,
    samples: &[FdUsageSample],
) -> Value {
    let sample_evidence: Vec<Value> = samples
        .iter()
        .map(|sample| {
            json!({
                "process": sample.process,
                "pid": sample.pid,
                "open_files": sample.open_files,
                "soft_limit": fd_limit_evidence(sample.soft_limit),
                "limit_source": sample.limit_source,
                "usage_percent": sample_usage_percent(sample),
            })
        })
        .collect();

    json!({
        "launchctl_maxfiles_soft": launchd_limit
            .map(|limit| fd_limit_evidence(limit.soft))
            .unwrap_or_else(|| json!("unknown")),
        "launchctl_maxfiles_hard": launchd_limit
            .map(|limit| fd_limit_evidence(limit.hard))
            .unwrap_or_else(|| json!("unknown")),
        "launchd_job_loaded": launchd_job_loaded,
        "plist_soft_number_of_files": plist_nofile_limit,
        "warn_percent": FD_HEADROOM_WARN_PERCENT,
        "warn_remaining": FD_HEADROOM_WARN_REMAINING,
        "samples": sample_evidence,
    })
}

#[cfg(not(target_os = "macos"))]
#[allow(dead_code, reason = "Retained for cross-platform lint path resolution")]
fn check_file_descriptor_headroom() -> Option<Check> {
    None
}

#[cfg(target_os = "macos")]
fn check_file_descriptor_headroom() -> Check {
    let label = dcserver::current_dcserver_launchd_label();
    let launchd_limit = read_launchctl_maxfiles_limit();
    let launchd_job_loaded = dcserver::is_launchd_job_loaded(&label);
    let plist_nofile_limit = launchagent_plist_nofile_limit(&label);
    let launchd_soft_limit = launchd_limit
        .map(|limit| limit.soft)
        .unwrap_or(FdLimitValue::Unknown);
    let dcserver_soft_limit = if launchd_job_loaded {
        plist_nofile_limit
            .map(FdLimitValue::Finite)
            .unwrap_or(launchd_soft_limit)
    } else {
        launchd_soft_limit
    };
    // dcserver itself is the launchd job and inherits the per-job plist limit.
    // tmux server PIDs are scoped to current-runtime AgentDesk sessions, not
    // all user tmux daemons, so they can use the same resolved limit/source.
    let resolved_limit_source = if launchd_job_loaded && plist_nofile_limit.is_some() {
        "launchd_plist"
    } else {
        "launchctl_maxfiles"
    };

    let mut samples = Vec::new();
    for pid in dcserver::dcserver_instance_pids() {
        if let Some(open_files) = open_file_count_for_pid(pid) {
            samples.push(FdUsageSample {
                process: "dcserver",
                pid,
                open_files,
                soft_limit: dcserver_soft_limit,
                limit_source: resolved_limit_source,
            });
        }
    }
    for pid in tmux_server_pids() {
        if let Some(open_files) = open_file_count_for_pid(pid) {
            samples.push(FdUsageSample {
                process: "tmux",
                pid,
                open_files,
                soft_limit: dcserver_soft_limit,
                limit_source: resolved_limit_source,
            });
        }
    }

    let evidence = file_descriptor_evidence(
        launchd_limit,
        launchd_job_loaded,
        plist_nofile_limit,
        &samples,
    );

    if launchd_limit.is_none() {
        return Check::warn(
            "file_descriptor_headroom",
            CheckGroup::Core,
            "File Descriptor Headroom",
            "launchctl maxfiles limit unavailable",
            "macOS launchd 한도를 읽을 수 없어 tmux/dcserver FD 사용량을 한도와 비교하지 못했습니다.",
        )
        .with_expected_actual("launchctl limit maxfiles readable", "unavailable")
        .with_evidence(evidence)
        .with_next_steps(vec!["launchctl limit maxfiles".to_string()]);
    }

    let pressured: Vec<&FdUsageSample> = samples
        .iter()
        .filter(|sample| sample_is_near_limit(sample))
        .collect();
    if !pressured.is_empty() {
        let detail = pressured
            .iter()
            .map(|sample| fd_sample_detail(sample))
            .collect::<Vec<_>>()
            .join("; ");
        return Check::warn(
            "file_descriptor_headroom",
            CheckGroup::Core,
            "File Descriptor Headroom",
            detail,
            "If the file descriptor count is close to the soft limit, mkfifo/spawn may fail with EMFILE (os error 24). Increase SoftResourceLimits:NumberOfFiles in the launchd plist and restart dcserver/tmux.",
        )
        .with_expected_actual(
            format!(
                "all sampled processes below {FD_HEADROOM_WARN_PERCENT}% usage and more than {FD_HEADROOM_WARN_REMAINING} FDs remaining"
            ),
            pressured
                .iter()
                .map(|sample| fd_sample_detail(sample))
                .collect::<Vec<_>>()
                .join("; "),
        )
        .with_evidence(evidence)
        .with_next_steps(vec![
            format!(
                "/usr/libexec/PlistBuddy -c 'Print :SoftResourceLimits:NumberOfFiles' ~/Library/LaunchAgents/{label}.plist"
            ),
            "launchctl limit maxfiles".to_string(),
            "reload launchd service or restart the AgentDesk/tmux sessions after raising the limit".to_string(),
        ]);
    }

    let detail = if samples.is_empty() {
        format!(
            "launchctl maxfiles soft={}, no dcserver/tmux process samples",
            fd_limit_display(launchd_soft_limit)
        )
    } else {
        let max_sample = samples
            .iter()
            .max_by_key(|sample| sample_usage_percent(sample).unwrap_or(0))
            .expect("samples is not empty");
        fd_sample_detail(max_sample)
    };

    Check::ok(
        "file_descriptor_headroom",
        CheckGroup::Core,
        "File Descriptor Headroom",
        detail,
    )
    .with_expected_actual("fd usage below warning threshold", "ok")
    .with_evidence(evidence)
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
            "check the status with `systemctl --user status agentdesk-dcserver` or try restarting with `agentdesk doctor --fix`.",
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
            "check the status with `sc query AgentDeskDcserver` or try restarting with `agentdesk doctor --fix`.",
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
            "check whether the deployment is a Windows service or a manual execution.",
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
            let checksum_mismatches = runtime
                .block_on(crate::db::postgres::applied_migration_checksum_mismatch_details(&pool));
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
                        "checksum_mismatches": checksum_mismatch_evidence(&checksum_mismatches),
                    }))
                    .with_expected_actual("postgres connection and migration metadata readable", "ok")
                }
                (Ok(status), Ok(checksum_mismatches)) => Check::fail(
                    "postgres_connection",
                    CheckGroup::Core,
                    "PostgreSQL",
                    format!(
                        "{summary} — migration drift: missing_from_resolved={:?} unsuccessful={:?} checksum_mismatches={}",
                        status.missing_from_resolved,
                        unsuccessful_migration_versions(&status),
                        format_checksum_mismatches(&checksum_mismatches)
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
                    "checksum_mismatches": checksum_mismatch_evidence(&checksum_mismatches),
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
            "check the DATABASE_URL or database configuration values (host/port/dbname/user/password).",
        )
        .with_expected_actual("postgres connection succeeds", error)
        .with_next_steps(vec![
            "agentdesk doctor --json".to_string(),
            format!("tail -n 200 {}", dcserver_log_hint()),
        ]),
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

fn checksum_mismatch_evidence(
    mismatches: &[crate::db::postgres::MigrationChecksumMismatch],
) -> Vec<Value> {
    mismatches
        .iter()
        .map(|mismatch| {
            json!({
                "version": mismatch.version,
                "applied_checksum": mismatch.applied_checksum,
                "resolved_checksum": mismatch.resolved_checksum,
            })
        })
        .collect()
}

fn format_checksum_mismatches(
    mismatches: &[crate::db::postgres::MigrationChecksumMismatch],
) -> String {
    if mismatches.is_empty() {
        return "[]".to_string();
    }
    let parts = mismatches
        .iter()
        .map(|mismatch| {
            format!(
                "version={} applied_checksum={} resolved_checksum={}",
                mismatch.version, mismatch.applied_checksum, mismatch.resolved_checksum
            )
        })
        .collect::<Vec<_>>();
    format!("[{}]", parts.join(", "))
}

fn postgres_migration_status_is_healthy(
    status: &crate::db::postgres::MigrationStatus,
    checksum_mismatches: &[crate::db::postgres::MigrationChecksumMismatch],
) -> bool {
    status.missing_from_resolved.is_empty()
        && unsuccessful_migration_versions(status).is_empty()
        && checksum_mismatches.is_empty()
}

fn check_stale_zero_byte_db_files(cfg: &config::Config) -> Check {
    let Some(runtime_root) = crate::config::runtime_root() else {
        return Check::warn(
            "stale_db_files",
            CheckGroup::Core,
            "Stale DB Files",
            "runtime root unresolved",
            "verify the canonical DB path and clean up zero-byte stale DB files in the runtime root.",
        )
        .with_expected_actual(
            "runtime root path resolvable",
            "runtime root path unresolved",
        );
    };

    let canonical_db_path = cfg.data.dir.join(&cfg.data.db_name);
    let stale_paths = stale_zero_byte_db_candidates(&runtime_root, &canonical_db_path);
    if crate::db::postgres::database_enabled(cfg) {
        if stale_paths.is_empty() {
            return Check::ok(
                "stale_db_files",
                CheckGroup::Core,
                "Legacy SQLite Stale Files",
                format!(
                    "no zero-byte legacy SQLite files near {}",
                    runtime_root.display()
                ),
            )
            .with_subsystem("sqlite_cache")
            .with_path(runtime_root.display().to_string())
            .with_expected_actual(
                "Postgres source-of-truth active with no stale zero-byte SQLite files",
                "no stale local artifacts found",
            );
        }

        return Check::warn(
            "stale_db_files",
            CheckGroup::Core,
            "Legacy SQLite Stale Files",
            format!(
                "{} zero-byte legacy SQLite file(s) near {}",
                stale_paths.len(),
                runtime_root.display()
            ),
            "Postgres is enabled; these zero-byte SQLite files are stale local artifacts and can be removed with the explicit repair flag.",
        )
        .with_subsystem("sqlite_cache")
        .with_fix_safety(FixSafety::ExplicitDbRepairRequired)
        .with_security_exposure(SecurityExposure::LocalPath)
        .with_path(runtime_root.display().to_string())
        .with_expected_actual(
            "Postgres source-of-truth active with no stale zero-byte SQLite files",
            format!("{} stale local artifact(s)", stale_paths.len()),
        )
        .with_next_steps(vec![
            "agentdesk doctor --fix --repair-sqlite-cache".to_string()
        ]);
    }
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
            "the canonical DB is {}; do not open guessed paths with sqlite3. Clean up stale files with agentdesk doctor --fix if necessary.",
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

const DISK_WARN_BYTES: u64 = 30 * 1024 * 1024 * 1024;
const DISK_FAIL_BYTES: u64 = 80 * 1024 * 1024 * 1024;
const DISK_USAGE_EXCLUDED_ROOT_CHILDREN: &[&str] = &["workspaces", "worktrees"];

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
    match crate::config::runtime_root() {
        Some(path) if !path.exists() => Check::warn(
            "disk_usage",
            CheckGroup::Core,
            "Disk Usage",
            format!("{} — runtime root missing", path.display()),
            "you can create the default runtime directory with agentdesk doctor --fix.",
        )
        .with_path(path.display().to_string())
        .with_expected_actual(
            "runtime root exists for disk usage scan",
            "runtime root missing",
        )
        .with_next_steps(vec!["agentdesk doctor --fix".to_string()]),
        Some(path) => match recursive_dir_size(&path) {
            Ok(raw_total) => {
                let mut excluded_total = 0u64;
                let mut children = fs::read_dir(&path)
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| {
                        let entry = entry.ok()?;
                        let child_path = entry.path();
                        let name = child_path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("unknown")
                            .to_string();
                        let size = recursive_dir_size(&child_path).ok()?;
                        Some((name, size))
                    })
                    .collect::<Vec<_>>();
                for (name, size) in &children {
                    if DISK_USAGE_EXCLUDED_ROOT_CHILDREN.contains(&name.as_str()) {
                        excluded_total = excluded_total.saturating_add(*size);
                    }
                }
                let total = raw_total.saturating_sub(excluded_total);
                let mb = total as f64 / 1_048_576.0;
                children.sort_by(|left, right| right.1.cmp(&left.1));
                let top_children = children
                    .into_iter()
                    .take(5)
                    .map(|(name, size)| {
                        json!({
                            "name": name,
                            "mb": (size as f64 / 1_048_576.0),
                            "excluded_from_threshold": DISK_USAGE_EXCLUDED_ROOT_CHILDREN.contains(&name.as_str())
                        })
                    })
                    .collect::<Vec<_>>();
                let evidence = json!({
                    "runtime_root": path.display().to_string(),
                    "total_bytes": total,
                    "raw_total_bytes": raw_total,
                    "excluded_root_children": DISK_USAGE_EXCLUDED_ROOT_CHILDREN,
                    "excluded_bytes": excluded_total,
                    "warn_threshold_bytes": DISK_WARN_BYTES,
                    "fail_threshold_bytes": DISK_FAIL_BYTES,
                    "top_children": top_children
                });
                let detail = if excluded_total > 0 {
                    format!(
                        "{:.1} MB operational usage in {} ({:.1} MB excluded: workspaces/worktrees)",
                        mb,
                        path.display(),
                        excluded_total as f64 / 1_048_576.0
                    )
                } else {
                    format!("{:.1} MB recursively in {}", mb, path.display())
                };
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
            "check AGENTDESK_ROOT_DIR or the default ~/.adk/release path.",
        )
        .with_expected_actual(
            "runtime root path resolvable",
            "runtime root path unresolved",
        ),
    }
}

fn report_config() -> config::Config {
    config::runtime_config()
}

pub(crate) fn run_doctor_report(options: DoctorOptions) -> Result<DoctorReport, String> {
    config::test_instance::environment::refuse_service_management()?;
    let cfg = report_config();
    let mut actions = Vec::new();
    if options.fix {
        actions = managed_herdr_config::fixed(&cfg, apply_safe_fixes(&cfg, &options));
        let pre_fix_snapshot = fetch_health_snapshot(&options);
        actions.extend(apply_service_fix(&pre_fix_snapshot, &options));
        actions.extend(apply_stale_mailbox_fixes(&pre_fix_snapshot, &options));
    }

    let snapshot = fetch_health_snapshot(&options);
    let mut checks = managed_herdr_config::checked(&cfg, build_all_checks(&cfg, &snapshot));
    if let Some(profile) = options.profile {
        checks.retain(|check| doctor_profile_includes_check(profile, check));
    }
    Ok(build_json_report(&options, &checks, &actions))
}

fn doctor_profile_includes_check(profile: DoctorProfile, check: &Check) -> bool {
    match profile {
        DoctorProfile::Quick => matches!(
            check.subsystem,
            "server" | "health" | "provider_runtime" | "dispatch_outbox" | "optional_connectors"
        ),
        DoctorProfile::Deep => true,
        DoctorProfile::Security => {
            matches!(
                check.subsystem,
                "security" | "config_audit" | "health" | "provider_runtime"
            ) || !matches!(check.security_exposure, SecurityExposure::None)
        }
    }
}

fn check_group_from_report(group: &str) -> CheckGroup {
    match group {
        "provider_runtime" => CheckGroup::ProviderRuntime,
        "optional_connectors" => CheckGroup::OptionalConnectors,
        _ => CheckGroup::Core,
    }
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
                .map(report_display::fix_action)
                .collect::<Vec<_>>();
            print_fix_actions(&actions);
        }
        let checks = report
            .checks
            .iter()
            .map(report_display::check)
            .collect::<Vec<_>>();
        for (group, label) in [
            (CheckGroup::Core, "Core"),
            (CheckGroup::ProviderRuntime, "Provider Runtime"),
            (CheckGroup::OptionalConnectors, "Optional Connectors"),
        ] {
            let group_checks: Vec<Check> = checks
                .iter()
                .filter(|check| check.group == group)
                .cloned()
                .collect();
            print_group(label, &group_checks);
        }

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

#[cfg(test)]
mod profile_filter_tests {
    use super::{
        Check, CheckGroup, DoctorOptions, HealthSnapshot, build_json_report,
        check_group_from_report, check_mailbox_consistency, checksum_mismatch_evidence,
        doctor_profile_includes_check, format_checksum_mismatches,
    };
    use crate::cli::doctor::contract::{DoctorProfile, RunContext};
    use serde_json::json;

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_fd_probes_are_empty() {
        assert!(super::tmux_server_pids().is_empty());
        assert!(super::check_file_descriptor_headroom().is_none());
    }

    #[test]
    fn mailbox_consistency_findings_use_provider_runtime_group() {
        let snapshot = HealthSnapshot {
            base: "http://localhost:8787".to_string(),
            body: Some(json!({
                "global_active": 0,
                "mailboxes": [{
                    "channel_id": 42,
                    "has_cancel_token": true,
                    "queue_depth": 0,
                    "watcher_attached": false,
                    "inflight_state_present": false,
                    "tmux_present": false,
                    "process_present": false,
                    "active_dispatch_present": false
                }]
            })),
            error: None,
        };

        let checks = check_mailbox_consistency(&snapshot);

        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].id, "mailbox_busy_without_active_turn");
        assert_eq!(checks[0].fix_safety, super::FixSafety::SafeLocalRepair);
        assert_eq!(checks[0].group, CheckGroup::ProviderRuntime);
        assert_eq!(checks[0].subsystem, "provider_runtime");
    }

    #[test]
    fn quick_profile_keeps_dispatch_outbox_checks() {
        let dispatch_outbox = Check::ok(
            "dispatch_outbox",
            CheckGroup::Core,
            "Dispatch Outbox",
            "dispatch outbox is healthy",
        )
        .with_subsystem("dispatch_outbox");
        let config_audit = Check::ok(
            "config_paths",
            CheckGroup::Core,
            "Config Paths",
            "config paths are healthy",
        )
        .with_subsystem("config_audit");

        assert!(doctor_profile_includes_check(
            DoctorProfile::Quick,
            &dispatch_outbox
        ));
        assert!(!doctor_profile_includes_check(
            DoctorProfile::Quick,
            &config_audit
        ));
    }

    #[test]
    fn quick_profile_keeps_portable_optional_state_checks() {
        let optional_connector = Check::ok(
            "optional_obsidian_agents",
            CheckGroup::OptionalConnectors,
            "Obsidian agent prompts",
            "state=missing_config reason=missing_config",
        );

        assert!(doctor_profile_includes_check(
            DoctorProfile::Quick,
            &optional_connector
        ));
    }

    #[test]
    fn json_report_exposes_portable_readiness_sections() {
        let checks = vec![
            Check::ok("runtime_root", CheckGroup::Core, "Runtime root", "ok")
                .with_subsystem("config"),
            Check::ok(
                "launchd_service",
                CheckGroup::Core,
                "Launchd service",
                "launchd not loaded in manual mode",
            )
            .with_subsystem("launchd"),
            Check::ok(
                "optional_obsidian_agents",
                CheckGroup::OptionalConnectors,
                "Obsidian agent prompts",
                "state=missing_config reason=missing_config",
            ),
        ];
        let options = DoctorOptions {
            fix: false,
            json: true,
            allow_restart: false,
            repair_sqlite_cache: false,
            allow_remote: false,
            profile: None,
            run_context: RunContext::ManualCli,
            artifact_path: None,
        };
        let report = build_json_report(&options, &checks, &[]);

        for section in ["core", "config", "launchd", "optional_connectors"] {
            assert!(
                report.sections.contains_key(section),
                "missing doctor section {section}"
            );
        }
        assert_eq!(report.sections["optional_connectors"].summary.total, 1);
    }

    #[test]
    fn report_group_round_trip_keeps_portable_groups() {
        assert_eq!(
            check_group_from_report("optional_connectors"),
            CheckGroup::OptionalConnectors
        );
    }

    #[test]
    fn postgres_checksum_mismatch_detail_includes_applied_and_resolved_hashes() {
        let mismatches = vec![crate::db::postgres::MigrationChecksumMismatch {
            version: 1,
            applied_checksum: "oldchecksum".to_string(),
            resolved_checksum: "newchecksum".to_string(),
        }];

        let formatted = format_checksum_mismatches(&mismatches);
        assert!(formatted.contains("version=1"));
        assert!(formatted.contains("applied_checksum=oldchecksum"));
        assert!(formatted.contains("resolved_checksum=newchecksum"));

        let evidence = checksum_mismatch_evidence(&mismatches);
        assert_eq!(evidence[0]["version"], 1);
        assert_eq!(evidence[0]["applied_checksum"], "oldchecksum");
        assert_eq!(evidence[0]["resolved_checksum"], "newchecksum");
    }
}

#[cfg(test)]
mod snapshot_reader_tests;
