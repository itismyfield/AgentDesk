use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::io::AsRawFd;

use crate::config;
use crate::engine::PolicyEngine;
use crate::server;
use crate::services;

use super::VERSION;
pub(crate) const AGENTDESK_DCSERVER_LAUNCHD_LABEL: &str = "com.agentdesk.release";
const AGENTDESK_DCSERVER_LABEL_ENV: &str = "AGENTDESK_DCSERVER_LABEL";
const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

#[cfg(target_os = "macos")]
pub fn current_launchd_domain() -> Option<String> {
    let output = std::process::Command::new("id").arg("-u").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if uid.is_empty() {
        return None;
    }
    Some(format!("gui/{}", uid))
}

#[cfg(not(target_os = "macos"))]
pub fn current_launchd_domain() -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
pub fn is_launchd_job_loaded(label: &str) -> bool {
    let output = match std::process::Command::new("launchctl").arg("list").output() {
        Ok(output) if output.status.success() => output,
        _ => return false,
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.split_whitespace().last() == Some(label))
}

#[cfg(not(target_os = "macos"))]
pub fn is_launchd_job_loaded(_label: &str) -> bool {
    false
}

#[cfg(target_os = "macos")]
pub fn kickstart_launchd_job(label: &str) -> bool {
    let Some(domain) = current_launchd_domain() else {
        return false;
    };
    let target = format!("{}/{}", domain, label);
    std::process::Command::new("launchctl")
        .args(["kickstart", "-k", &target])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
pub fn kickstart_launchd_job(_label: &str) -> bool {
    false
}

// ── systemd helpers (Linux) ─────────────────────────────────────

const SYSTEMD_SERVICE_NAME: &str = "agentdesk-dcserver";

#[cfg(target_os = "linux")]
pub fn is_systemd_service_active() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", SYSTEMD_SERVICE_NAME])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
pub fn is_systemd_service_active() -> bool {
    false
}

#[cfg(target_os = "linux")]
pub fn is_systemd_service_enabled() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-enabled", "--quiet", SYSTEMD_SERVICE_NAME])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
pub fn is_systemd_service_enabled() -> bool {
    false
}

#[cfg(target_os = "linux")]
pub fn restart_systemd_service() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "restart", SYSTEMD_SERVICE_NAME])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
pub fn restart_systemd_service() -> bool {
    false
}

pub fn restart_systemd_dcserver_and_verify(timeout: Duration) -> Result<(), String> {
    let stdout_path =
        dcserver_stdout_log_path().ok_or_else(|| "dcserver stdout log path missing".to_string())?;
    let start_offset = fs::metadata(&stdout_path).map(|m| m.len()).unwrap_or(0);

    if !restart_systemd_service() {
        return Err("systemctl --user restart failed".to_string());
    }

    verify_dcserver_ready_since(start_offset, timeout)
}

// ── Windows service helpers ─────────────────────────────────────

const WINDOWS_SERVICE_NAME: &str = "AgentDeskDcserver";

#[cfg(target_os = "windows")]
pub fn is_windows_service_installed() -> bool {
    std::process::Command::new("sc")
        .args(["query", WINDOWS_SERVICE_NAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "windows"))]
pub fn is_windows_service_installed() -> bool {
    false
}

#[cfg(target_os = "windows")]
pub fn is_windows_service_running() -> bool {
    let output = match std::process::Command::new("sc")
        .args(["query", WINDOWS_SERVICE_NAME])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return false,
    };

    String::from_utf8_lossy(&output.stdout).contains("RUNNING")
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
pub fn is_windows_service_running() -> bool {
    false
}

#[cfg(target_os = "windows")]
pub fn restart_windows_service() -> bool {
    // Try NSSM first, fall back to sc.exe
    let nssm = std::process::Command::new("nssm")
        .args(["restart", WINDOWS_SERVICE_NAME])
        .status();
    if matches!(nssm, Ok(s) if s.success()) {
        return true;
    }
    // Fallback: sc stop + sc start
    let _ = std::process::Command::new("sc")
        .args(["stop", WINDOWS_SERVICE_NAME])
        .status();
    std::thread::sleep(Duration::from_secs(2));
    std::process::Command::new("sc")
        .args(["start", WINDOWS_SERVICE_NAME])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "windows"))]
pub fn restart_windows_service() -> bool {
    false
}

pub fn restart_windows_dcserver_and_verify(timeout: Duration) -> Result<(), String> {
    let stdout_path =
        dcserver_stdout_log_path().ok_or_else(|| "dcserver stdout log path missing".to_string())?;
    let start_offset = fs::metadata(&stdout_path).map(|m| m.len()).unwrap_or(0);

    if !restart_windows_service() {
        return Err("Windows service restart failed".to_string());
    }

    verify_dcserver_ready_since(start_offset, timeout)
}

pub fn agentdesk_runtime_root() -> Option<PathBuf> {
    crate::config::runtime_root()
}

pub fn current_dcserver_launchd_label() -> String {
    env::var(AGENTDESK_DCSERVER_LABEL_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| AGENTDESK_DCSERVER_LAUNCHD_LABEL.to_string())
}

pub fn current_dcserver_root_marker() -> Option<String> {
    env::var(AGENTDESK_ROOT_DIR_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(unix)]
pub fn dcserver_process_command(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["eww", "-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if command.is_empty() {
        return None;
    }
    Some(command)
}

#[cfg(not(unix))]
pub fn dcserver_process_command(_pid: u32) -> Option<String> {
    None
}

pub fn dcserver_process_matches_instance(command: &str) -> bool {
    let is_dcserver = command.contains("agentdesk dcserver");
    if !is_dcserver {
        return false;
    }

    match current_dcserver_root_marker() {
        Some(root) => command.contains(&format!("{AGENTDESK_ROOT_DIR_ENV}={root}")),
        None => !command.contains(&format!("{AGENTDESK_ROOT_DIR_ENV}=")),
    }
}

#[cfg(unix)]
pub fn dcserver_instance_pids() -> Vec<u32> {
    let output = match std::process::Command::new("pgrep")
        .args(["-f", "agentdesk.*dcserver"])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .filter(|pid| {
            dcserver_process_command(*pid)
                .as_deref()
                .map(dcserver_process_matches_instance)
                .unwrap_or(false)
        })
        .collect()
}

#[cfg(not(unix))]
pub fn dcserver_instance_pids() -> Vec<u32> {
    Vec::new()
}

pub fn instance_agentdesk_config_path() -> Option<PathBuf> {
    agentdesk_runtime_root().map(|root| {
        let canonical = crate::runtime_layout::config_file_path(&root);
        let legacy = crate::runtime_layout::legacy_config_file_path(&root);
        if canonical.is_file() || !legacy.is_file() {
            canonical
        } else {
            legacy
        }
    })
}

pub fn dcserver_stdout_log_path() -> Option<PathBuf> {
    agentdesk_runtime_root().map(|root| root.join("logs").join("dcserver.stdout.log"))
}

pub fn current_release_link_path() -> Option<PathBuf> {
    agentdesk_runtime_root().map(|root| root.join("releases").join("current"))
}

pub fn previous_release_link_path() -> Option<PathBuf> {
    agentdesk_runtime_root().map(|root| root.join("releases").join("previous"))
}

pub fn read_release_link_target(path: &Path) -> Option<PathBuf> {
    fs::read_link(path).ok()
}

#[cfg(unix)]
pub fn update_release_link(link_path: &Path, target: &Path) -> Result<(), String> {
    use std::os::unix::fs::symlink;

    match fs::remove_file(link_path) {
        Ok(()) | Err(_) => {} // ignore NotFound or any pre-existing state
    }
    symlink(target, link_path).map_err(|e| format!("create symlink failed: {e}"))
}

#[cfg(not(unix))]
pub fn update_release_link(_link_path: &Path, _target: &Path) -> Result<(), String> {
    Err("symlinks not supported on this platform".to_string())
}

pub fn dcserver_process_running() -> bool {
    #[cfg(target_os = "windows")]
    {
        return is_windows_service_running();
    }

    !dcserver_instance_pids().is_empty()
}

pub fn verify_dcserver_ready_since(start_offset: u64, timeout: Duration) -> Result<(), String> {
    let stdout_path =
        dcserver_stdout_log_path().ok_or_else(|| "dcserver stdout log path missing".to_string())?;
    let deadline = Instant::now() + timeout;

    loop {
        if !dcserver_process_running() {
            if Instant::now() >= deadline {
                return Err("dcserver process is not running".to_string());
            }
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }

        let recent = match fs::read(&stdout_path) {
            Ok(bytes) if (start_offset as usize) < bytes.len() => {
                String::from_utf8_lossy(&bytes[start_offset as usize..]).to_string()
            }
            Ok(_) => String::new(),
            Err(_) => String::new(),
        };

        if recent.contains("Bot connected — Registered commands")
            || recent.contains("✓ Bot connected")
        {
            return Ok(());
        }

        if recent.contains("continuing in onboarding mode") {
            return Ok(());
        }

        if recent.contains(" bot error:") {
            return Err("dcserver emitted startup error".to_string());
        }

        if Instant::now() >= deadline {
            return Err("timed out waiting for dcserver ready log".to_string());
        }

        std::thread::sleep(Duration::from_secs(1));
    }
}

pub fn restart_launchd_dcserver_and_verify(label: &str, timeout: Duration) -> Result<(), String> {
    let stdout_path =
        dcserver_stdout_log_path().ok_or_else(|| "dcserver stdout log path missing".to_string())?;
    let start_offset = fs::metadata(&stdout_path).map(|m| m.len()).unwrap_or(0);

    if !kickstart_launchd_job(label) {
        return Err("launchd kickstart failed".to_string());
    }

    verify_dcserver_ready_since(start_offset, timeout)
}

pub fn rollback_to_previous_release(label: &str, timeout: Duration) -> Result<PathBuf, String> {
    let previous_link =
        previous_release_link_path().ok_or_else(|| "previous release link missing".to_string())?;
    let current_link =
        current_release_link_path().ok_or_else(|| "current release link missing".to_string())?;
    let previous_target = read_release_link_target(&previous_link)
        .ok_or_else(|| format!("no rollback target found at {}", previous_link.display()))?;

    if !previous_target.join("agentdesk").exists() {
        return Err(format!(
            "rollback target missing binary: {}",
            previous_target.join("agentdesk").display()
        ));
    }

    update_release_link(&current_link, &previous_target)?;
    restart_launchd_dcserver_and_verify(label, timeout)?;
    Ok(previous_target)
}

/// Kill all existing dcserver processes (prevents duplicates from different paths).
pub fn kill_existing_dcserver_processes() {
    let my_pid = std::process::id();
    let mut killed = 0;
    for pid in dcserver_instance_pids() {
        if pid == my_pid {
            continue;
        }
        println!("   Killing existing dcserver (PID {})", pid);
        #[cfg(unix)]
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status();
        #[cfg(not(unix))]
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status();
        killed += 1;
    }
    if killed > 0 {
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

#[allow(dead_code)]
pub fn parse_restart_dcserver_report_context(
    args: &[String],
    start_index: usize,
) -> Result<Option<services::discord::restart_report::RestartReportContext>, String> {
    use services::discord::restart_report::{
        RestartReportContext, restart_report_context_from_env,
    };
    use services::provider::ProviderKind;

    let mut report_channel_id: Option<u64> = None;
    let mut report_provider: Option<String> = None;
    let mut report_message_id: Option<u64> = None;
    let mut i = start_index;

    while i < args.len() {
        match args[i].as_str() {
            "--report-channel-id" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "--report-channel-id requires a numeric ID".to_string())?;
                report_channel_id = Some(
                    raw.parse::<u64>()
                        .map_err(|_| format!("invalid value for --report-channel-id: {raw}"))?,
                );
                i += 2;
            }
            "--report-provider" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| {
                        "--report-provider requires one of: claude, codex, gemini, opencode, qwen"
                            .to_string()
                    })?
                    .clone();
                report_provider = Some(raw);
                i += 2;
            }
            "--report-message-id" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "--report-message-id requires a numeric ID".to_string())?;
                report_message_id = Some(
                    raw.parse::<u64>()
                        .map_err(|_| format!("invalid value for --report-message-id: {raw}"))?,
                );
                i += 2;
            }
            other => {
                return Err(format!("unknown option for --restart-dcserver: {other}"));
            }
        }
    }

    match (report_provider, report_channel_id, report_message_id) {
        (None, None, None) => Ok(restart_report_context_from_env()),
        (None, None, Some(_)) => Err(
            "--report-message-id requires --report-channel-id and --report-provider".to_string(),
        ),
        (Some(_), None, _) => Err("--report-provider requires --report-channel-id".to_string()),
        (None, Some(_), _) => Err("--report-channel-id requires --report-provider".to_string()),
        (Some(provider_raw), Some(channel_id), current_msg_id) => {
            let provider = ProviderKind::from_str(&provider_raw).ok_or_else(|| {
                format!(
                    "invalid value for --report-provider: {} (expected claude, codex, gemini, opencode, or qwen)",
                    provider_raw
                )
            })?;
            Ok(Some(RestartReportContext {
                provider,
                channel_id,
                current_msg_id,
            }))
        }
    }
}
pub fn handle_restart_dcserver(
    report_context_override: Option<services::discord::restart_report::RestartReportContext>,
) {
    use services::discord::load_discord_bot_launch_configs;
    use services::discord::restart_report::{
        RestartCompletionReport, load_restart_report, restart_report_context_from_env,
        save_restart_report,
    };
    const READY_TIMEOUT: Duration = Duration::from_secs(30);

    let report_context = report_context_override.or_else(restart_report_context_from_env);
    if report_context.is_none() {
        eprintln!(
            "ℹ no restart follow-up target configured; pass --report-channel-id/--report-provider or set AGENTDESK_REPORT_* to send a Discord completion message"
        );
    }
    let write_restart_report = |status: &str, summary: String| {
        let Some(context) = report_context.as_ref() else {
            return;
        };
        let mut report = RestartCompletionReport::new(
            context.provider.clone(),
            context.channel_id,
            status,
            summary,
        );
        if let Some(existing) = load_restart_report(&context.provider, context.channel_id) {
            report.current_msg_id = existing.current_msg_id;
        }
        if report.current_msg_id.is_none() {
            report.current_msg_id = context.current_msg_id;
        }
        if let Err(e) = save_restart_report(&report) {
            eprintln!("⚠ failed to save restart follow-up report: {e}");
        }
    };

    let settings_display = instance_agentdesk_config_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "agentdesk.yaml".to_string());
    let configs = load_discord_bot_launch_configs();
    let onboarding_mode = configs.is_empty();

    if onboarding_mode {
        eprintln!(
            "  ⚠ No configured Discord bots found in {} — dcserver will start in onboarding mode",
            settings_display
        );
    }

    // Increment generation counter — every restart request gets a unique generation,
    // even for same-version deployments (e.g. code-only changes without version bump).
    let new_generation = services::discord::runtime_store::increment_generation();

    // Show version transition if available
    if let Some(root) = agentdesk_runtime_root() {
        let version_file = root.join("runtime").join("dcserver.version");
        if let Ok(running_version) = std::fs::read_to_string(&version_file) {
            let running = running_version.trim();
            println!(
                "   Running: v{running} → Deploying: v{VERSION} (generation {new_generation})"
            );
        }
    }

    println!("🔄 Restarting Discord bot server...");
    if onboarding_mode {
        println!("   Mode: onboarding (HTTP-only, no Discord bots)");
    } else {
        println!("   Configured bots: {}", configs.len());
    }

    let previous_release = previous_release_link_path()
        .as_deref()
        .and_then(read_release_link_target);

    if let Some(context) = report_context.as_ref() {
        let mut pending_report = RestartCompletionReport::new(
            context.provider.clone(),
            context.channel_id,
            "pending",
            "dcserver restart requested; 새 프로세스가 completion follow-up을 이어받는 중입니다.",
        );
        if let Some(existing) = load_restart_report(&context.provider, context.channel_id) {
            pending_report.current_msg_id = existing.current_msg_id;
            pending_report.user_msg_id = existing.user_msg_id;
        }
        if pending_report.current_msg_id.is_none() {
            pending_report.current_msg_id = context.current_msg_id;
        }
        if let Err(e) = save_restart_report(&pending_report) {
            eprintln!("⚠ failed to save pending restart follow-up report: {e}");
        }
    }

    // Deferred restart: write marker file and wait for dcserver to self-exit
    // after all active turns complete. Falls back to force-kill on timeout.
    const DEFERRED_TIMEOUT: Duration = Duration::from_secs(120);
    if let Some(root) = agentdesk_runtime_root() {
        let marker = root.join("restart_pending");
        if let Err(e) = fs::write(&marker, VERSION) {
            eprintln!(
                "   ⚠ Failed to write restart marker {}: {e} — falling back to force-kill",
                marker.display()
            );
            kill_existing_dcserver_processes();
            return;
        }
        println!(
            "   ⏳ Deferred restart requested — waiting for active turns to complete (max {}s)",
            DEFERRED_TIMEOUT.as_secs()
        );

        let start = Instant::now();
        loop {
            // If marker file is gone, dcserver consumed it and exited
            if !marker.exists() {
                println!("   ✓ dcserver acknowledged restart marker");
                break;
            }
            // Check if dcserver process is still running
            let pid_file = root.join("runtime").join("dcserver.pid");
            if let Ok(pid_str) = fs::read_to_string(&pid_file)
                && let Ok(pid) = pid_str.trim().parse::<u32>()
            {
                // Check if process still exists
                let process_alive = {
                    #[cfg(unix)]
                    {
                        let status = std::process::Command::new("kill")
                            .args(["-0", &pid.to_string()])
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status();
                        matches!(status, Ok(s) if s.success())
                    }
                    #[cfg(not(unix))]
                    {
                        let status = std::process::Command::new("tasklist")
                            .args(["/FI", &format!("PID eq {}", pid), "/NH"])
                            .output();
                        matches!(status, Ok(o) if String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
                    }
                };
                if !process_alive {
                    println!("   ✓ dcserver process exited gracefully");
                    let _ = fs::remove_file(&marker);
                    break;
                }
            }
            if start.elapsed() >= DEFERRED_TIMEOUT {
                eprintln!("   ⚠ Deferred restart timeout — falling back to force kill");
                let _ = fs::remove_file(&marker);
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    // Kill remaining dcserver processes (either timeout fallback or normal cleanup)
    kill_existing_dcserver_processes();

    let launchd_label = current_dcserver_launchd_label();
    if is_launchd_job_loaded(&launchd_label) {
        println!("   launchd service detected: {}", launchd_label);
        match restart_launchd_dcserver_and_verify(&launchd_label, READY_TIMEOUT) {
            Ok(()) => {
                let current_release = current_release_link_path()
                    .as_deref()
                    .and_then(read_release_link_target)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(unknown)".to_string());
                println!(
                    "✅ Discord bot restarted via launchd '{}' and passed ready check",
                    launchd_label
                );
                write_restart_report(
                    "ok",
                    format!(
                        "launchd restart 완료, ready check 통과\n- current release: `{}`",
                        current_release
                    ),
                );
                return;
            }
            Err(e) => {
                eprintln!("⚠ launchd restart verification failed: {e}");
                if let Some(prev) = previous_release.as_ref() {
                    eprintln!("↩ rolling back current release to {}", prev.display());
                    match rollback_to_previous_release(&launchd_label, READY_TIMEOUT) {
                        Ok(restored) => {
                            println!(
                                "✅ Rolled back to {} and dcserver passed ready check",
                                restored.display()
                            );
                            write_restart_report(
                                "rolled_back",
                                format!(
                                    "launchd restart는 실패했지만 rollback 후 복구됨\n- restored release: `{}`\n- reason: `{}`",
                                    restored.display(),
                                    e
                                ),
                            );
                        }
                        Err(rollback_err) => {
                            eprintln!("❌ Rollback failed: {rollback_err}");
                            write_restart_report(
                                "failed",
                                format!(
                                    "launchd restart 실패 후 rollback도 실패\n- restart error: `{}`\n- rollback error: `{}`",
                                    e, rollback_err
                                ),
                            );
                        }
                    }
                } else {
                    eprintln!("⚠ no previous release link available for rollback");
                    write_restart_report(
                        "failed",
                        format!(
                            "launchd restart 실패, rollback target 없음\n- restart error: `{}`",
                            e
                        ),
                    );
                }
                return;
            }
        }
    }

    // systemd restart path (Linux)
    // When a service manager is detected, do NOT fall through to tmux —
    // running a separate tmux process alongside the supervisor would cause conflicts.
    if is_systemd_service_enabled() || is_systemd_service_active() {
        println!("   systemd user service detected: {}", SYSTEMD_SERVICE_NAME);
        match restart_systemd_dcserver_and_verify(READY_TIMEOUT) {
            Ok(()) => {
                println!(
                    "✅ Discord bot restarted via systemd '{}' and passed ready check",
                    SYSTEMD_SERVICE_NAME
                );
                write_restart_report(
                    "ok",
                    format!(
                        "systemd restart 완료, ready check 통과\n- service: `{}`",
                        SYSTEMD_SERVICE_NAME
                    ),
                );
            }
            Err(e) => {
                eprintln!("❌ systemd restart verification failed: {e}");
                eprintln!("   Hint: check logs with 'journalctl --user -u {SYSTEMD_SERVICE_NAME}'");
                write_restart_report(
                    "failed",
                    format!(
                        "systemd restart 실패\n- service: `{}`\n- error: `{}`",
                        SYSTEMD_SERVICE_NAME, e
                    ),
                );
            }
        }
        return;
    }

    // Windows service restart path
    if is_windows_service_installed() {
        println!("   Windows service detected: {}", WINDOWS_SERVICE_NAME);
        match restart_windows_dcserver_and_verify(READY_TIMEOUT) {
            Ok(()) => {
                println!(
                    "✅ Discord bot restarted via Windows service '{}' and passed ready check",
                    WINDOWS_SERVICE_NAME
                );
                write_restart_report(
                    "ok",
                    format!(
                        "Windows service restart 완료, ready check 통과\n- service: `{}`",
                        WINDOWS_SERVICE_NAME
                    ),
                );
            }
            Err(e) => {
                eprintln!("❌ Windows service restart failed: {e}");
                eprintln!(
                    "   Hint: check with 'nssm status {WINDOWS_SERVICE_NAME}' or 'sc query {WINDOWS_SERVICE_NAME}'"
                );
                write_restart_report(
                    "failed",
                    format!(
                        "Windows service restart 실패\n- service: `{}`\n- error: `{}`",
                        WINDOWS_SERVICE_NAME, e
                    ),
                );
            }
        }
        return;
    }

    // NOTE: We intentionally do NOT kill AgentDesk-* work sessions here.
    // They will be reconnected by restore_tmux_watchers() after the new dcserver starts.
    // Orphan sessions (channels renamed/deleted) are cleaned up inside the bot event loop.

    // Launch new dcserver inside tmux session "AgentDesk-dcserver"
    // Write a launcher script to avoid token exposure in ps aux
    let Some(runtime_root) = agentdesk_runtime_root() else {
        eprintln!("Error: Cannot determine runtime root");
        write_restart_report(
            "failed",
            "runtime root를 결정할 수 없어서 tmux fallback restart를 시작하지 못했습니다."
                .to_string(),
        );
        return;
    };
    let scripts_dir = runtime_root.join("scripts");
    let _ = std::fs::create_dir_all(&scripts_dir);
    let launcher_path = scripts_dir.join("_launch_dcserver.sh");

    // Use production binary at ~/.adk/release/bin/agentdesk (trunk-based: separate from build output)
    let prod_bin = runtime_root.join("bin").join("agentdesk");
    let exe = if prod_bin.exists() {
        prod_bin.display().to_string()
    } else {
        // Fallback: project build output or current exe
        let project_exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("release")
            .join("agentdesk");
        if project_exe.exists() {
            project_exe.display().to_string()
        } else {
            match std::env::current_exe() {
                Ok(p) => p.display().to_string(),
                Err(e) => {
                    eprintln!("Error: Cannot determine executable path: {e}");
                    write_restart_report(
                        "failed",
                        format!("실행 바이너리 경로를 결정할 수 없습니다: {e}"),
                    );
                    return;
                }
            }
        }
    };

    let root_env = current_dcserver_root_marker()
        .map(|root| {
            format!(
                "export {AGENTDESK_ROOT_DIR_ENV}='{}'\n",
                root.replace('\'', "'\\''")
            )
        })
        .unwrap_or_default();
    let label_env = env::var(AGENTDESK_DCSERVER_LABEL_ENV)
        .ok()
        .map(|label| label.trim().to_string())
        .filter(|label| !label.is_empty())
        .map(|label| {
            format!(
                "export {AGENTDESK_DCSERVER_LABEL_ENV}='{}'\n",
                label.replace('\'', "'\\''")
            )
        })
        .unwrap_or_default();
    let script = format!(
        "#!/bin/bash\nunset CLAUDECODE\n{root_env}{label_env}exec {} dcserver\n",
        exe
    );
    if let Err(e) = std::fs::write(&launcher_path, &script) {
        eprintln!("Error: Failed to write launcher script: {e}");
        write_restart_report("failed", format!("launcher script 쓰기 실패: {e}"));
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            std::fs::set_permissions(&launcher_path, std::fs::Permissions::from_mode(0o700))
        {
            eprintln!("Error: Failed to set script permissions: {e}");
            write_restart_report("failed", format!("launcher script 권한 설정 실패: {e}"));
            return;
        }
    }

    let tmux_session = "AgentDesk-dcserver";

    // Kill existing tmux session if it exists
    crate::services::platform::tmux::kill_session_with_reason(
        tmux_session,
        "dcserver tmux launcher restart: replace existing AgentDesk-dcserver session",
    );
    std::thread::sleep(std::time::Duration::from_millis(500));

    let launcher_str = launcher_path.to_string_lossy();
    let create_result =
        crate::services::platform::tmux::create_session(tmux_session, None, launcher_str.as_ref());

    match create_result {
        Ok(output) if output.status.success() => {
            // Verify the session exists
            if crate::services::platform::tmux::has_session(tmux_session) {
                // Use current log size as offset to avoid matching stale "Bot connected" lines
                let log_offset = dcserver_stdout_log_path()
                    .and_then(|p| fs::metadata(&p).ok())
                    .map(|m| m.len())
                    .unwrap_or(0);
                match verify_dcserver_ready_since(log_offset, READY_TIMEOUT) {
                    Ok(()) => {
                        println!(
                            "✅ Discord bot started in tmux session '{}' and passed ready check",
                            tmux_session
                        );
                        write_restart_report(
                            "ok",
                            format!(
                                "tmux fallback restart 완료, ready check 통과\n- session: `{}`",
                                tmux_session
                            ),
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "⚠ tmux session '{}' started but ready check failed: {}",
                            tmux_session, e
                        );
                        write_restart_report(
                            "failed",
                            format!(
                                "tmux fallback restart는 됐지만 ready check 실패\n- session: `{}`\n- error: `{}`",
                                tmux_session, e
                            ),
                        );
                    }
                }
            } else {
                eprintln!(
                    "❌ tmux session '{}' failed to start. Check with: tmux a -t {}",
                    tmux_session, tmux_session
                );
                write_restart_report(
                    "failed",
                    format!("tmux fallback restart 실패\n- session: `{}`", tmux_session),
                );
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("❌ tmux new-session failed: {}", stderr.trim());
            write_restart_report(
                "failed",
                format!("tmux fallback restart 실패\n- stderr: `{}`", stderr.trim()),
            );
        }
        Err(e) => {
            eprintln!("❌ Failed to start tmux session: {}", e);
            write_restart_report(
                "failed",
                format!("tmux fallback restart spawn 실패\n- error: `{}`", e),
            );
        }
    }
}

pub fn handle_dcserver(token: Option<String>) {
    // Ensure directory structure exists first (needed for lock file)
    if let Some(root) = agentdesk_runtime_root() {
        for subdir in ["config", "credential", "runtime", "logs", "scripts"] {
            let _ = std::fs::create_dir_all(root.join(subdir));
        }
    }

    if let Err(error) = crate::logging::init_tracing() {
        eprintln!("  ✖ Failed to initialize tracing: {error}");
        std::process::exit(1);
    }

    // Single-instance guard via flock — prevents race conditions
    #[cfg(unix)]
    let _lock_file = {
        let lock_path = agentdesk_runtime_root()
            .map(|r| r.join("runtime/dcserver.lock"))
            .unwrap_or_else(|| PathBuf::from("/tmp/agentdesk-dcserver.lock"));
        let f = match fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Failed to open lock file {:?}: {}", lock_path, e);
                std::process::exit(1);
            }
        };
        let ret = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if ret != 0 {
            eprintln!(
                "  ✗ Another dcserver is already running (lock held on {:?}). Exiting.",
                lock_path
            );
            std::process::exit(1);
        }
        // Write our PID into the lock file
        use std::io::Write;
        let mut ff = &f;
        let _ = ff.write_all(std::process::id().to_string().as_bytes());
        f // keep File open — dropping it releases the lock
    };

    // Also kill any stale processes (e.g. orphaned without lock)
    kill_existing_dcserver_processes();

    let runtime_root = agentdesk_runtime_root();
    let legacy_scan = runtime_root
        .as_ref()
        .map(|root| crate::services::discord_config_audit::scan_legacy_sources(root))
        .unwrap_or_default();

    if let Some(root) = runtime_root.as_ref() {
        match crate::runtime_layout::ensure_runtime_layout(&root) {
            Ok(report) => {
                if report.migrated {
                    if let Some(backup) = report.backup_path {
                        println!(
                            "  ▸ Config migration : v2 complete (backup: {})",
                            backup.display()
                        );
                    } else {
                        println!("  ▸ Config migration : v2 complete");
                    }
                }
            }
            Err(error) => {
                eprintln!("  ✖ Failed to prepare runtime layout: {error}");
                std::process::exit(1);
            }
        }
    }

    // Write PID/version files
    if let Some(root) = agentdesk_runtime_root() {
        let runtime_dir = root.join("runtime");
        let _ = std::fs::write(
            runtime_dir.join("dcserver.pid"),
            std::process::id().to_string(),
        );
        let _ = std::fs::write(runtime_dir.join("dcserver.version"), VERSION);
    }

    // Ensure the persistent session temp-file directory exists so wrappers
    // spawned after this point write into the canonical location rather
    // than falling back into /tmp. See issue #892.
    if let Err(e) = crate::services::tmux_common::ensure_sessions_dir_on_startup() {
        eprintln!("  ⚠ Failed to prepare sessions dir: {e}");
    }

    // Prevent CLAUDECODE from leaking into child tmux sessions
    // SAFETY: We're single-threaded at this point (before tokio runtime starts).
    unsafe {
        std::env::remove_var("CLAUDECODE");
    }

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Failed to create Tokio runtime: {}", e);
            std::process::exit(1);
        }
    };
    let settings_path = instance_agentdesk_config_path();

    let title = format!("  AgentDesk v{}  |  Discord Bot Server  ", VERSION);
    let width = title.chars().count();
    println!();
    println!("  ┌{}┐", "─".repeat(width));
    println!("  │{}│", title);
    println!("  └{}┘", "─".repeat(width));
    println!();
    println!("  ▸ Status : Connecting...");

    rt.block_on(async move {
        println!();

        // ── AgentDesk HTTP server ──────────────────────────────────
        // Load agentdesk.yaml (graceful: use defaults if missing)
        let loaded = if let Some(root) = runtime_root.as_ref() {
            match crate::services::discord_config_audit::load_runtime_config(root) {
                Ok(loaded) => loaded,
                Err(error) => {
                    eprintln!("  ✖ Failed to load config after layout prep: {error}");
                    std::process::exit(1);
                }
            }
        } else {
            let fallback_path = settings_path
                .clone()
                .unwrap_or_else(|| PathBuf::from("config").join("agentdesk.yaml"));
            crate::services::discord_config_audit::LoadedRuntimeConfig {
                config: config::load_graceful(),
                existed: fallback_path.is_file(),
                path: fallback_path,
            }
        };
        let mut ad_config = loaded.config;

        // ── Workspace branch guard (#181) ──────────────────────────
        // Ensure no workspace repo is checked out on a wt/* worktree branch.
        // This can happen when an agent checks out a worktree branch on the main repo,
        // and the worktree directory is later cleaned up by the policy merge cleaner.
        {
            use std::collections::HashSet;
            let mut workspaces = HashSet::new();

            for workspace in crate::services::discord::agentdesk_config::configured_workspaces() {
                workspaces.insert(workspace);
            }

            // Fall back to role_map.json while older installs are still migrating.
            if workspaces.is_empty()
                && let Some(rm_path) = agentdesk_runtime_root()
                    .map(|r| r.join("config").join("role_map.json"))
                    .filter(|p| p.exists())
                && let Ok(content) = std::fs::read_to_string(&rm_path)
                && let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
            {
                for section in ["byChannelId", "byChannelName"] {
                    if let Some(map) = json.get(section).and_then(|v| v.as_object()) {
                        for (_key, entry) in map {
                            if let Some(ws) = entry.get("workspace").and_then(|v| v.as_str()) {
                                let expanded = if ws.starts_with("~/") {
                                    if let Some(home) = dirs::home_dir() {
                                        format!("{}{}", home.display(), &ws[1..])
                                    } else {
                                        ws.to_string()
                                    }
                                } else {
                                    ws.to_string()
                                };
                                workspaces.insert(expanded);
                            }
                        }
                    }
                }
            }

            for ws in &workspaces {
                let ws_path = std::path::Path::new(ws);
                if !ws_path.join(".git").exists() {
                    continue;
                }
                if let Ok(output) = std::process::Command::new("git")
                    .args(["-C", ws, "branch", "--show-current"])
                    .output()
                {
                    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if branch.starts_with("wt/") {
                        eprintln!(
                            "  ⚠ [branch-guard] Workspace {} on worktree branch '{}' — recovering to main",
                            ws, branch
                        );
                        let _ = std::process::Command::new("git")
                            .args(["-C", ws, "stash", "--include-untracked", "-m", "auto-stash before branch-guard recovery"])
                            .output();
                        let _ = std::process::Command::new("git")
                            .args(["-C", ws, "checkout", "main"])
                            .output();
                        let _ = std::process::Command::new("git")
                            .args(["-C", ws, "pull", "--ff-only"])
                            .output();
                        let _ = std::process::Command::new("git")
                            .args(["-C", ws, "worktree", "prune"])
                            .output();
                        eprintln!(
                            "  ✓ [branch-guard] Recovered {} to main (was: {})",
                            ws, branch
                        );
                    }
                }
            }
        }

        // ── Discord bot setup (before HTTP server so registry is available) ──
        // Process-global counters shared across all providers for deferred restart barrier
        let global_active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let global_finalizing = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // Health registry (shared across all providers, passed to axum server)
        let health_registry = std::sync::Arc::new(services::discord::health::HealthRegistry::new());
        health_registry.init_bot_tokens().await;
        let launch_configs = services::discord::load_discord_bot_launch_configs();
        let onboarding_mode =
            should_run_http_only_onboarding(token.as_deref(), launch_configs.len());
        let startup_provider_count = if token.is_some() {
            1
        } else {
            launch_configs.len()
        };
        let startup_reconcile_remaining =
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(startup_provider_count));
        let startup_doctor_started =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        if onboarding_mode
            && ad_config
                .server
                .auth_token
                .as_deref()
                .map(str::is_empty)
                .unwrap_or(true)
        {
            let loopback = config::loopback();
            if ad_config.server.host != loopback {
                eprintln!(
                    "  ⚠ No auth token configured during onboarding; restricting HTTP server to loopback ({loopback})"
                );
                ad_config.server.host = loopback;
            }
        }

        let mut discord_engine: Option<PolicyEngine> = None;
        // Load data-driven pipeline definition (#106) — fail-fast on error
        let pipeline_path = ad_config.policies.dir.join("default-pipeline.yaml");
        if pipeline_path.exists() {
            match crate::pipeline::load(&pipeline_path) {
                Ok(()) => println!("  ▸ Pipeline : loaded {}", pipeline_path.display()),
                Err(e) => {
                    eprintln!("  ✖ Failed to load pipeline definition: {e}");
                    eprintln!("    path: {}", pipeline_path.display());
                    std::process::exit(1);
                }
            }
        }

        let discord_pg_pool = match crate::db::postgres::connect_and_migrate(&ad_config).await {
            Ok(Some(pool)) => {
                if let Some(root) = runtime_root.as_ref() {
                    match crate::services::discord_config_audit::load_runtime_config(root)
                        .and_then(|loaded| {
                            crate::services::discord_config_audit::audit_and_reconcile_config_only(
                                root,
                                loaded.config,
                                loaded.path,
                                loaded.existed,
                                &legacy_scan,
                                false,
                            )
                        })
                    {
                        Ok(outcome) => {
                            ad_config = outcome.config;
                        }
                        Err(error) => {
                            eprintln!(
                                "  ✖ Config audit after PostgreSQL migration failed: {error}"
                            );
                            std::process::exit(1);
                        }
                    }
                }
                let startup_pg_pool =
                    match crate::db::postgres::connect_for_startup(&ad_config).await {
                        Ok(pool) => pool,
                        Err(error) => {
                            eprintln!(
                                "  ⚠ PostgreSQL warmup pool unavailable: {error} — falling back to runtime pool"
                            );
                            None
                        }
                    };
                let startup_pool = startup_pg_pool.as_ref().unwrap_or(&pool);
                if let Err(error) =
                    crate::db::postgres::startup_reseed(startup_pool, &ad_config).await
                {
                    eprintln!("  ✖ PostgreSQL startup reseed failed: {error}");
                    std::process::exit(1);
                }
                drop(startup_pg_pool);
                pool
            }
            Ok(None) => {
                eprintln!("  ✖ PostgreSQL is required for Discord HTTP runtime");
                std::process::exit(1);
            }
            Err(error) => {
                eprintln!("  ✖ PostgreSQL connect/migrate failed: {error}");
                std::process::exit(1);
            }
        };
        crate::services::termination_audit::init_audit_db(Some(discord_pg_pool.clone()));

        // Start axum HTTP server (background task) — now serves all API
        // endpoints including /api/send, /api/senddm, /api/health
        let http_port = ad_config.server.port;
        match PolicyEngine::new_with_pg(&ad_config, Some(discord_pg_pool.clone())) {
            Ok(engine) => {
                discord_engine = Some(engine.clone());
                let http_config = ad_config.clone();
                let registry_for_http = health_registry.clone();
                let http_pg_pool = Some(discord_pg_pool.clone());
                tokio::spawn(async move {
                    if let Err(e) = server::run(
                        http_config,
                        engine,
                        Some(registry_for_http),
                        http_pg_pool,
                    )
                    .await
                    {
                        eprintln!("  ⚠ HTTP server error: {e}");
                    }
                });
                println!(
                    "  ▸ HTTP    : listening on {}:{} (unified API + health)",
                    ad_config.server.host, http_port
                );
            }
            Err(e) => {
                eprintln!("  ⚠ Policy engine init failed: {e} — HTTP server not started");
            }
        }
        // HTTP API port for self-referencing requests (dcserver → own HTTP server)
        let api_port = ad_config.server.port;
        let placeholder_live_events_enabled = ad_config.placeholder.live_events_enabled;

        // Self-watchdog: probes the axum server's /api/health endpoint
        services::discord::health::spawn_watchdog(api_port);

        // Async heartbeat: proves the tokio runtime is scheduling tasks.
        // If this stops printing, the runtime is hung (watchdog will catch it).
        tokio::spawn(async {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
            interval.tick().await; // skip first immediate tick
            loop {
                interval.tick().await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                eprintln!("  [{ts}] 💓 runtime heartbeat");
            }
        });

        // #1203: 30 s disk-space probe. Fans a "💾 디스크 부족" banner out to
        // every channel that already has a monitoring row whenever free space
        // dips below 5 GiB or an ENOSPC fault was recorded in the last 5 min,
        // and removes the row when health returns.
        let disk_probe_path = crate::cli::agentdesk_runtime_root()
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        services::disk_monitor::spawn_disk_monitor_tick(disk_probe_path);

        match token {
            Some(token) => {
                let provider = services::discord::resolve_discord_bot_provider(&token);
                let shutdown_remaining =
                    std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(1));
                services::discord::run_bot(
                    &token,
                    provider,
                    services::discord::RunBotContext {
                        global_active,
                        global_finalizing,
                        shutdown_remaining,
                        startup_reconcile_remaining,
                        startup_doctor_started,
                        health_registry,
                        api_port,
                        pg_pool: Some(discord_pg_pool),
                        engine: discord_engine,
                        placeholder_live_events_enabled,
                    },
                )
                .await;
            }
            None => {
                let configs = launch_configs;
                if onboarding_mode {
                    let settings_display = settings_path
                        .as_deref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "agentdesk.yaml".to_string());
                    eprintln!(
                        "  ⚠ No configured Discord bots found in {} — waiting for HTTP server...",
                        settings_display
                    );
                    // Gate onboarding-ready signal on actual /api/health success.
                    // The HTTP server is spawned async above; probe until it
                    // responds or give up so the watchdog can trigger rollback.
                    let health_url =
                        config::local_api_url(api_port, "/api/health");
                    let mut http_ok = false;
                    for _ in 0..15 {
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                        if let Ok(resp) = reqwest::get(&health_url).await
                            && resp.status().is_success()
                        {
                            http_ok = true;
                            break;
                        }
                    }
                    if http_ok {
                        // Emit to stdout — verify_dcserver_ready_since() reads
                        // dcserver.stdout.log for this substring.
                        println!(
                            "  ⚠ continuing in onboarding mode (HTTP only) — {}",
                            settings_display
                        );
                    } else {
                        eprintln!(
                            "  ✗ HTTP server not reachable at {} — onboarding mode unavailable",
                            health_url
                        );
                    }
                    std::future::pending::<()>().await;
                    return;
                }

                println!(
                    "  ▸ Providers : {}",
                    configs
                        .iter()
                        .map(|cfg| cfg.provider.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );

                let shutdown_remaining =
                    std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(configs.len()));
                let mut tasks = Vec::new();
                for config in configs {
                    let ga = global_active.clone();
                    let gf = global_finalizing.clone();
                    let sr = shutdown_remaining.clone();
                    let startup_remaining = startup_reconcile_remaining.clone();
                    let startup_started = startup_doctor_started.clone();
                    let hr = health_registry.clone();
                    let port = api_port;
                    let pg_pool_clone = Some(discord_pg_pool.clone());
                    let engine_clone = discord_engine.clone();
                    let live_events_enabled = placeholder_live_events_enabled;
                    tasks.push(tokio::spawn(async move {
                        services::discord::run_bot(
                            &config.token,
                            config.provider,
                            services::discord::RunBotContext {
                                global_active: ga,
                                global_finalizing: gf,
                                shutdown_remaining: sr,
                                startup_reconcile_remaining: startup_remaining,
                                startup_doctor_started: startup_started,
                                health_registry: hr,
                                api_port: port,
                                pg_pool: pg_pool_clone,
                                engine: engine_clone,
                                placeholder_live_events_enabled: live_events_enabled,
                            },
                        )
                        .await;
                    }));
                }

                for task in tasks {
                    if let Err(e) = task.await {
                        eprintln!("  ⚠ bot task terminated unexpectedly: {e}");
                    }
                }
            }
        }
    });
}

fn should_run_http_only_onboarding(token: Option<&str>, launch_config_count: usize) -> bool {
    token.is_none() && launch_config_count == 0
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::should_run_http_only_onboarding;

    #[test]
    fn onboarding_mode_when_no_explicit_token_and_no_saved_bots() {
        assert!(should_run_http_only_onboarding(None, 0));
    }

    #[test]
    fn explicit_token_disables_http_only_onboarding() {
        assert!(!should_run_http_only_onboarding(Some("token"), 0));
    }

    #[test]
    fn saved_bot_configs_disable_http_only_onboarding() {
        assert!(!should_run_http_only_onboarding(None, 1));
    }
}
