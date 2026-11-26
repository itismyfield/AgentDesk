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

mod db_checks;
mod discord_checks;
mod disk_checks;
#[cfg(target_os = "macos")]
mod fd_headroom;
mod fix_apply;
mod runtime_checks;
#[cfg(not(target_os = "macos"))]
mod service_manager;

use db_checks::{check_postgres_connection, check_stale_zero_byte_db_files};
#[cfg(test)]
use db_checks::{checksum_mismatch_evidence, format_checksum_mismatches};
use discord_checks::check_discord_bot;
#[cfg(test)]
use discord_checks::discord_bot_check_from_health;
use disk_checks::{check_disk_usage, check_github_repo_registry};
#[cfg(target_os = "macos")]
use fd_headroom::{
    FD_HEADROOM_WARN_PERCENT, FD_HEADROOM_WARN_REMAINING, check_file_descriptor_headroom,
};
use fix_apply::{apply_service_fix, apply_stale_mailbox_fixes, print_fix_actions};
use runtime_checks::{
    check_degraded_reasons, check_runtime_path, check_runtime_root, check_server_running,
};
#[cfg(not(target_os = "macos"))]
use service_manager::check_service_manager;

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

#[cfg(not(target_os = "macos"))]
#[allow(dead_code, reason = "Retained for cross-platform lint path resolution")]
fn check_file_descriptor_headroom() -> Option<Check> {
    None
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
mod profile_filter_tests;

#[cfg(test)]
mod snapshot_reader_tests;
