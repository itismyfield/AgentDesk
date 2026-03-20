use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::io::AsRawFd;

use crate::services;
use crate::config;
use crate::db;
use crate::engine::PolicyEngine;
use crate::server;

use super::VERSION;
pub(crate) const REMOTECC_DCSERVER_LAUNCHD_LABEL: &str = "com.itismyfield.remotecc.dcserver";
const REMOTECC_DCSERVER_LABEL_ENV: &str = "REMOTECC_DCSERVER_LABEL";
const REMOTECC_ROOT_DIR_ENV: &str = "REMOTECC_ROOT_DIR";

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

pub fn is_launchd_job_loaded(label: &str) -> bool {
    let output = match std::process::Command::new("launchctl").arg("list").output() {
        Ok(output) if output.status.success() => output,
        _ => return false,
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.split_whitespace().last() == Some(label))
}

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

pub fn remotecc_runtime_root() -> Option<PathBuf> {
    if let Ok(override_root) = env::var(REMOTECC_ROOT_DIR_ENV) {
        let trimmed = override_root.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }

    dirs::home_dir().map(|h| h.join(".remotecc"))
}

pub fn current_dcserver_launchd_label() -> String {
    env::var(REMOTECC_DCSERVER_LABEL_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| REMOTECC_DCSERVER_LAUNCHD_LABEL.to_string())
}

pub fn current_dcserver_root_marker() -> Option<String> {
    env::var(REMOTECC_ROOT_DIR_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

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

pub fn dcserver_process_matches_instance(command: &str) -> bool {
    if !command.contains("remotecc --dcserver") {
        return false;
    }

    match current_dcserver_root_marker() {
        Some(root) => command.contains(&format!("{REMOTECC_ROOT_DIR_ENV}={root}")),
        None => !command.contains(&format!("{REMOTECC_ROOT_DIR_ENV}=")),
    }
}

pub fn dcserver_instance_pids() -> Vec<u32> {
    let output = match std::process::Command::new("pgrep")
        .args(["-f", "remotecc --dcserver"])
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

pub fn instance_bot_settings_path() -> Option<PathBuf> {
    remotecc_runtime_root().map(|root| {
        // TODO: Remove legacy fallback after 2026-03-26
        let new_path = root.join("config").join("bot_settings.json");
        if new_path.exists() { return new_path; }
        let legacy = root.join("bot_settings.json");
        if legacy.exists() { return legacy; }
        new_path
    })
}

pub fn dcserver_stdout_log_path() -> Option<PathBuf> {
    remotecc_runtime_root().map(|root| {
        // TODO: Remove legacy fallback after 2026-03-26
        let new_path = root.join("logs").join("dcserver.stdout.log");
        if new_path.exists() { return new_path; }
        let legacy = root.join("dcserver.stdout.log");
        if legacy.exists() { return legacy; }
        new_path
    })
}

pub fn current_release_link_path() -> Option<PathBuf> {
    remotecc_runtime_root().map(|root| root.join("releases").join("current"))
}

pub fn previous_release_link_path() -> Option<PathBuf> {
    remotecc_runtime_root().map(|root| root.join("releases").join("previous"))
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

pub fn dcserver_process_running() -> bool {
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

        if recent.contains(" bot error:") || recent.contains("Error: no bot tokens found") {
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

    if !previous_target.join("remotecc").exists() {
        return Err(format!(
            "rollback target missing binary: {}",
            previous_target.join("remotecc").display()
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
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status();
        killed += 1;
    }
    if killed > 0 {
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

pub fn parse_restart_dcserver_report_context(
    args: &[String],
    start_index: usize,
) -> Result<Option<services::discord::restart_report::RestartReportContext>, String> {
    use services::discord::restart_report::{
        restart_report_context_from_env, RestartReportContext,
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
                    .ok_or_else(|| "--report-provider requires one of: claude, codex".to_string())?
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
                    "invalid value for --report-provider: {} (expected claude or codex)",
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
        load_restart_report, restart_report_context_from_env, save_restart_report,
        RestartCompletionReport,
    };
    const READY_TIMEOUT: Duration = Duration::from_secs(30);

    let report_context = report_context_override.or_else(restart_report_context_from_env);
    if report_context.is_none() {
        eprintln!(
            "ℹ no restart follow-up target configured; pass --report-channel-id/--report-provider or set REMOTECC_REPORT_* to send a Discord completion message"
        );
    }
    let write_restart_report = |status: &str, summary: String| {
        let Some(context) = report_context.as_ref() else {
            return;
        };
        let mut report =
            RestartCompletionReport::new(context.provider.clone(), context.channel_id, status, summary);
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

    // Read bot_settings.json to find stored token(s)
    let settings_path = instance_bot_settings_path().expect("Cannot determine runtime root");

    match std::fs::read_to_string(&settings_path) {
        Ok(_) => {}
        Err(_) => {
            eprintln!("Error: {} not found.", settings_path.display());
            eprintln!("Run 'remotecc --dcserver' after configuring bot_settings.json.");
            write_restart_report(
                "failed",
                "bot_settings.json이 없어서 dcserver restart를 시작하지 못했습니다.".to_string(),
            );
            return;
        }
    }

    let configs = load_discord_bot_launch_configs();
    if configs.is_empty() {
        eprintln!("Error: no bot tokens found in bot_settings.json");
        write_restart_report(
            "failed",
            "bot_settings.json에 bot token이 없어서 dcserver restart를 시작하지 못했습니다."
                .to_string(),
        );
        return;
    }

    // Increment generation counter — every restart request gets a unique generation,
    // even for same-version deployments (e.g. code-only changes without version bump).
    let new_generation =
        services::discord::runtime_store::increment_generation();

    // Show version transition if available
    if let Some(root) = remotecc_runtime_root() {
        // TODO: Remove legacy fallback after 2026-03-26
        let new_vf = root.join("runtime").join("dcserver.version");
        let legacy_vf = root.join("dcserver.version");
        let version_file = if new_vf.exists() { new_vf } else if legacy_vf.exists() { legacy_vf } else { new_vf };
        if let Ok(running_version) = std::fs::read_to_string(&version_file) {
            let running = running_version.trim();
            println!(
                "   Running: v{running} → Deploying: v{VERSION} (generation {new_generation})"
            );
        }
    }

    println!("🔄 Restarting Discord bot server...");
    println!("   Configured bots: {}", configs.len());

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
    if let Some(root) = remotecc_runtime_root() {
        let marker = root.join("restart_pending");
        let _ = fs::write(&marker, VERSION);
        println!("   ⏳ Deferred restart requested — waiting for active turns to complete (max {}s)", DEFERRED_TIMEOUT.as_secs());

        let start = Instant::now();
        loop {
            // If marker file is gone, dcserver consumed it and exited
            if !marker.exists() {
                println!("   ✓ dcserver acknowledged restart marker");
                break;
            }
            // Check if dcserver process is still running
            // TODO: Remove legacy fallback after 2026-03-26
            let new_pid = root.join("runtime").join("dcserver.pid");
            let legacy_pid = root.join("dcserver.pid");
            let pid_file = if new_pid.exists() { new_pid } else if legacy_pid.exists() { legacy_pid } else { new_pid };
            if let Ok(pid_str) = fs::read_to_string(&pid_file) {
                if let Ok(pid) = pid_str.trim().parse::<u32>() {
                    // Check if process still exists via /bin/kill -0
                    let status = std::process::Command::new("kill")
                        .args(["-0", &pid.to_string()])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                    if matches!(status, Ok(s) if !s.success()) {
                        println!("   ✓ dcserver process exited gracefully");
                        let _ = fs::remove_file(&marker);
                        break;
                    }
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

    // NOTE: We intentionally do NOT kill AgentDesk-* Claude work sessions here.
    // They will be reconnected by restore_tmux_watchers() after the new dcserver starts.
    // Orphan sessions (channels renamed/deleted) are cleaned up inside the bot event loop.

    // Launch new dcserver inside tmux session "remoteCC"
    // Write a launcher script to avoid token exposure in ps aux
    let launcher_path = remotecc_runtime_root()
        .map(|root| {
            let scripts_dir = root.join("scripts");
            let _ = std::fs::create_dir_all(&scripts_dir);
            scripts_dir.join("_launch_dcserver.sh")
        })
        .expect("Cannot determine runtime root");

    // Use production binary at ~/.remotecc/bin/remotecc (trunk-based: separate from build output)
    let prod_bin = remotecc_runtime_root()
        .map(|root| root.join("bin").join("remotecc"))
        .expect("Cannot determine runtime root");
    let exe = if prod_bin.exists() {
        prod_bin.display().to_string()
    } else {
        // Fallback: project build output or current exe
        let project_exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("release")
            .join("remotecc");
        if project_exe.exists() {
            project_exe.display().to_string()
        } else {
            std::env::current_exe()
                .expect("Cannot determine executable path")
                .display()
                .to_string()
        }
    };

    let root_env = current_dcserver_root_marker()
        .map(|root| {
            format!(
                "export {REMOTECC_ROOT_DIR_ENV}='{}'\n",
                root.replace('\'', "'\\''")
            )
        })
        .unwrap_or_default();
    let label_env = env::var(REMOTECC_DCSERVER_LABEL_ENV)
        .ok()
        .map(|label| label.trim().to_string())
        .filter(|label| !label.is_empty())
        .map(|label| {
            format!(
                "export {REMOTECC_DCSERVER_LABEL_ENV}='{}'\n",
                label.replace('\'', "'\\''")
            )
        })
        .unwrap_or_default();
    let script = format!(
        "#!/bin/bash\nunset CLAUDECODE\n{root_env}{label_env}exec {} --dcserver\n",
        exe
    );
    std::fs::write(&launcher_path, &script).expect("Failed to write launcher script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launcher_path, std::fs::Permissions::from_mode(0o700))
            .expect("Failed to set script permissions");
    }

    let tmux_session = "AgentDesk-dcserver";

    // Kill existing tmux session if it exists
    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", tmux_session])
        .output();
    std::thread::sleep(std::time::Duration::from_millis(500));

    let child = std::process::Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            tmux_session,
            launcher_path.to_str().unwrap(),
        ])
        .spawn();

    match child {
        Ok(_) => {
            // Verify the session exists
            let check = std::process::Command::new("tmux")
                .args(["has-session", "-t", tmux_session])
                .status();
            if check.map(|s| s.success()).unwrap_or(false) {
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
    if let Some(root) = remotecc_runtime_root() {
        for subdir in ["config", "credential", "runtime", "logs", "scripts"] {
            let _ = std::fs::create_dir_all(root.join(subdir));
        }
    }

    // Single-instance guard via flock — prevents race conditions
    #[cfg(unix)]
    let _lock_file = {
        let lock_path = remotecc_runtime_root()
            .map(|r| r.join("runtime/dcserver.lock"))
            .unwrap_or_else(|| PathBuf::from("/tmp/agentdesk-dcserver.lock"));
        let f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .expect("Failed to open lock file");
        let ret = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if ret != 0 {
            eprintln!("  ✗ Another dcserver is already running (lock held on {:?}). Exiting.", lock_path);
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

    // Write PID/version files
    if let Some(root) = remotecc_runtime_root() {
        let runtime_dir = root.join("runtime");
        let _ = std::fs::write(runtime_dir.join("dcserver.pid"), std::process::id().to_string());
        let _ = std::fs::write(runtime_dir.join("dcserver.version"), VERSION);
    }

    // Prevent CLAUDECODE from leaking into child tmux sessions
    // SAFETY: We're single-threaded at this point (before tokio runtime starts).
    unsafe { std::env::remove_var("CLAUDECODE"); }

    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
    let settings_path = instance_bot_settings_path();

    let title = format!("  RemoteCC v{}  |  Discord Bot Server  ", VERSION);
    let width = title.chars().count();
    println!();
    println!("  ┌{}┐", "─".repeat(width));
    println!("  │{}│", title);
    println!("  └{}┘", "─".repeat(width));
    println!();
    println!("  ▸ Status : Connecting...");

    rt.block_on(async {
        println!();

        // ── AgentDesk HTTP server ──────────────────────────────────
        // Load agentdesk.yaml (graceful: use defaults if missing)
        let ad_config = config::load_graceful();

        // Initialize SQLite DB
        match db::init(&ad_config) {
            Ok(ad_db) => {
                // Sync agents from config → DB
                let agent_count = ad_config.agents.len();
                if agent_count > 0 {
                    match db::agents::sync_agents_from_config(&ad_db, &ad_config.agents) {
                        Ok(n) => println!("  ▸ Agents : {n} synced from config"),
                        Err(e) => eprintln!("  ⚠ Agent sync failed: {e}"),
                    }
                }

                // Start axum HTTP server (background task)
                let http_port = ad_config.server.port;
                match PolicyEngine::new(&ad_config, ad_db.clone()) {
                    Ok(engine) => {
                        let http_config = ad_config.clone();
                        tokio::spawn(async move {
                            if let Err(e) = server::run(http_config, ad_db, engine).await {
                                eprintln!("  ⚠ HTTP server error: {e}");
                            }
                        });
                        println!("  ▸ HTTP    : listening on {}:{}", ad_config.server.host, http_port);
                    }
                    Err(e) => {
                        eprintln!("  ⚠ Policy engine init failed: {e} — HTTP server not started");
                    }
                }
            }
            Err(e) => {
                eprintln!("  ⚠ DB init failed: {e} — HTTP server not started");
            }
        }

        // HTTP API port for self-referencing requests (dcserver → own HTTP server)
        let api_port = ad_config.server.port;

        // ── Discord bot ────────────────────────────────────────────
        // Process-global counters shared across all providers for deferred restart barrier
        let global_active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let global_finalizing = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // Health check HTTP server (shared across all providers)
        let health_registry = std::sync::Arc::new(services::discord::health::HealthRegistry::new());
        let health_port = services::discord::health::resolve_port();
        let health_reg_clone = health_registry.clone();
        tokio::spawn(async move {
            services::discord::health::serve(health_reg_clone, health_port).await;
        });

        match token {
            Some(token) => {
                let provider = services::discord::resolve_discord_bot_provider(&token);
                let shutdown_remaining = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(1));
                services::discord::run_bot(
                    &token,
                    provider,
                    global_active,
                    global_finalizing,
                    shutdown_remaining,
                    health_registry,
                    api_port,
                )
                .await;
            }
            None => {
                let configs = services::discord::load_discord_bot_launch_configs();
                if configs.is_empty() {
                    eprintln!(
                        "Error: no bot tokens found in {}",
                        settings_path
                            .as_deref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "bot_settings.json".to_string())
                    );
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

                let shutdown_remaining = std::sync::Arc::new(
                    std::sync::atomic::AtomicUsize::new(configs.len()),
                );
                let mut tasks = Vec::new();
                for config in configs {
                    let ga = global_active.clone();
                    let gf = global_finalizing.clone();
                    let sr = shutdown_remaining.clone();
                    let hr = health_registry.clone();
                    let port = api_port;
                    tasks.push(tokio::spawn(async move {
                        services::discord::run_bot(&config.token, config.provider, ga, gf, sr, hr, port).await;
                    }));
                }

                for task in tasks {
                    let _ = task.await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::parse_restart_dcserver_report_context;
    use crate::services::provider::ProviderKind;

    #[test]
    fn parses_explicit_restart_report_context() {
        let args = vec![
            "remotecc".to_string(),
            "--restart-dcserver".to_string(),
            "--report-channel-id".to_string(),
            "1479671301387059200".to_string(),
            "--report-provider".to_string(),
            "codex".to_string(),
        ];

        let context = parse_restart_dcserver_report_context(&args, 2)
            .unwrap()
            .expect("expected restart report context");
        assert_eq!(context.channel_id, 1479671301387059200);
        assert_eq!(context.provider, ProviderKind::Codex);
        assert_eq!(context.current_msg_id, None);
    }

    #[test]
    fn parses_restart_report_message_id() {
        let args = vec![
            "remotecc".to_string(),
            "--restart-dcserver".to_string(),
            "--report-channel-id".to_string(),
            "1479671301387059200".to_string(),
            "--report-provider".to_string(),
            "codex".to_string(),
            "--report-message-id".to_string(),
            "1480460000000000000".to_string(),
        ];

        let context = parse_restart_dcserver_report_context(&args, 2)
            .unwrap()
            .expect("expected restart report context");
        assert_eq!(context.current_msg_id, Some(1480460000000000000));
    }

    #[test]
    fn rejects_partial_restart_report_context() {
        let args = vec![
            "remotecc".to_string(),
            "--restart-dcserver".to_string(),
            "--report-channel-id".to_string(),
            "1479671301387059200".to_string(),
        ];

        let err = parse_restart_dcserver_report_context(&args, 2).unwrap_err();
        assert!(err.contains("--report-channel-id requires --report-provider"));
    }

    #[test]
    fn rejects_restart_report_message_id_without_context() {
        let args = vec![
            "remotecc".to_string(),
            "--restart-dcserver".to_string(),
            "--report-message-id".to_string(),
            "1480460000000000000".to_string(),
        ];

        let err = parse_restart_dcserver_report_context(&args, 2).unwrap_err();
        assert!(
            err.contains("--report-message-id requires --report-channel-id and --report-provider")
        );
    }

    #[test]
    fn rejects_unknown_restart_report_provider() {
        let args = vec![
            "remotecc".to_string(),
            "--restart-dcserver".to_string(),
            "--report-channel-id".to_string(),
            "1479671301387059200".to_string(),
            "--report-provider".to_string(),
            "openclaw".to_string(),
        ];

        let err = parse_restart_dcserver_report_context(&args, 2).unwrap_err();
        assert!(err.contains("invalid value for --report-provider"));
    }
}
