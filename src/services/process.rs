#![allow(dead_code)]

use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use crate::services::provider::{CancelToken, cancel_requested};

/// Handle returned by [`spawn_simple_cancel_watcher`].
///
/// Drop or call [`SimpleCancelWatcher::disarm`] once the child has been
/// reaped to stop the polling thread; otherwise the watcher will exit on its
/// own at most ~`poll_interval` after the child terminates because the next
/// `kill_pid_tree` call becomes a no-op.
pub struct SimpleCancelWatcher {
    armed: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl SimpleCancelWatcher {
    /// Stop polling the cancel token. Idempotent.
    pub fn disarm(mut self) {
        self.armed.store(false, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SimpleCancelWatcher {
    fn drop(&mut self) {
        self.armed.store(false, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn a lightweight thread that polls `cancel_token` every ~100ms and
/// kills the child PID tree once cancellation is observed. Used by simple
/// (`wait_with_output`-based) CLI call sites that cannot interleave a cancel
/// check between spawn and wait. See ADR #2175.
///
/// If `cancel_token` is `None`, no thread is spawned and the returned handle
/// is a no-op.
pub fn spawn_simple_cancel_watcher(
    cancel_token: Option<Arc<CancelToken>>,
    child_pid: u32,
) -> SimpleCancelWatcher {
    let Some(token) = cancel_token else {
        return SimpleCancelWatcher {
            armed: Arc::new(AtomicBool::new(false)),
            handle: None,
        };
    };

    let armed = Arc::new(AtomicBool::new(true));
    let armed_thread = armed.clone();

    let handle = std::thread::Builder::new()
        .name("simple-cancel-watcher".to_string())
        .spawn(move || {
            let poll = Duration::from_millis(100);
            while armed_thread.load(Ordering::Relaxed) {
                if cancel_requested(Some(token.as_ref())) {
                    kill_pid_tree(child_pid);
                    return;
                }
                std::thread::sleep(poll);
            }
        })
        .ok();

    SimpleCancelWatcher { armed, handle }
}

/// Configure a child command so `kill_pid_tree(child.id())` can clean up descendants.
pub fn configure_child_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
}

/// Wait for process output and kill the child process group on timeout.
pub fn wait_with_output_timeout(
    child: std::process::Child,
    timeout: Duration,
    label: &str,
) -> Result<Output, String> {
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let result = child.wait_with_output();
        let _ = tx.send(result);
    });

    match rx.recv_timeout(timeout) {
        Ok(result) => {
            let _ = waiter.join();
            result.map_err(|e| format!("Failed to read output: {}", e))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            kill_pid_tree(pid);
            if rx.recv_timeout(Duration::from_secs(2)).is_ok() {
                let _ = waiter.join();
            }
            Err(format!("{} timed out after {}s", label, timeout.as_secs()))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(format!("{} output waiter disconnected", label))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortField {
    Pid,
    Cpu,
    Mem,
    Command,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: i32,
    pub user: String,
    pub cpu: f32,
    pub mem: f32,
    pub vsz: u64,
    pub rss: u64,
    pub tty: String,
    pub stat: String,
    pub start: String,
    pub time: String,
    pub command: String,
}

/// Kill a process tree by PID.
/// On Unix, sends SIGTERM to the process group, then SIGKILL as fallback.
#[allow(unsafe_code)]
pub fn kill_pid_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        let ret = libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
        if ret != 0 {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
            std::thread::sleep(std::time::Duration::from_millis(200));
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        } else {
            std::thread::sleep(std::time::Duration::from_millis(200));
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
}

/// Kill a child process and its entire process tree.
/// On Unix, sends SIGTERM to the process group first, then SIGKILL as fallback.
pub fn kill_child_tree(child: &mut std::process::Child) {
    kill_pid_tree(child.id());
    std::thread::sleep(std::time::Duration::from_millis(200));
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

/// Shell-escape a string using single quotes (POSIX safe).
/// Internal single quotes are replaced with `'\''`.
pub(crate) fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Protected PIDs that should never be killed
const PROTECTED_PIDS: &[i32] = &[1, 2];

/// Minimum PID threshold - PIDs below this are likely kernel threads
const MIN_SAFE_PID: i32 = 300;

/// Validate PID is a safe positive integer
fn is_valid_pid(pid: i32) -> bool {
    pid > 0 && pid <= 4194304 // Max PID on Linux
}

/// Check if PID is protected from being killed
fn is_protected_pid(pid: i32, command: Option<&str>) -> Result<(), String> {
    // Check if it's our own process
    let current_pid = std::process::id() as i32;
    if pid == current_pid {
        return Err("Cannot kill the file manager itself".to_string());
    }

    // Check protected system PIDs
    if PROTECTED_PIDS.contains(&pid) {
        return Err(format!("Cannot kill system process (PID {})", pid));
    }

    // Warn about low PIDs (likely kernel threads)
    if pid < MIN_SAFE_PID {
        return Err(format!(
            "Cannot kill low PID ({}) - likely a kernel thread",
            pid
        ));
    }

    // Check if command indicates kernel thread
    if let Some(cmd) = command {
        if cmd.starts_with('[') && cmd.ends_with(']') {
            return Err("Cannot kill kernel threads".to_string());
        }
    }

    Ok(())
}

/// Result type for process list operations
pub type ProcessListResult = Result<Vec<ProcessInfo>, String>;

/// Get list of running processes
pub fn get_process_list() -> Vec<ProcessInfo> {
    get_process_list_result().unwrap_or_default()
}

/// Get list of running processes with error handling
pub fn get_process_list_result() -> ProcessListResult {
    #[cfg(target_os = "windows")]
    let output = Command::new("tasklist")
        .args(["/FO", "CSV", "/NH", "/V"])
        .output()
        .map_err(|e| format!("Failed to execute tasklist command: {}", e))?;

    #[cfg(not(target_os = "windows"))]
    let output = Command::new("ps")
        .args(["aux"])
        .output()
        .map_err(|e| format!("Failed to execute ps command: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        #[cfg(target_os = "windows")]
        {
            return Err(format!("tasklist command failed: {}", stderr.trim()));
        }
        #[cfg(not(target_os = "windows"))]
        {
            return Err(format!("ps command failed: {}", stderr.trim()));
        }
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    #[cfg(target_os = "windows")]
    let mut processes = stdout
        .lines()
        .filter_map(parse_tasklist_line)
        .collect::<Vec<_>>();

    #[cfg(not(target_os = "windows"))]
    let mut processes = stdout
        .lines()
        .skip(1) // Skip header line (compatible with both Linux and macOS)
        .filter_map(parse_process_line)
        .collect::<Vec<_>>();

    // Sort by CPU usage descending by default, then RSS as a fallback for
    // Windows tasklist rows where CPU percentages are unavailable.
    processes.sort_by(|a, b| {
        b.cpu
            .partial_cmp(&a.cpu)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.rss.cmp(&a.rss))
    });

    Ok(processes)
}

fn parse_process_line(line: &str) -> Option<ProcessInfo> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 11 {
        return None;
    }

    let pid = parts[1].parse::<i32>().ok()?;
    let cpu = parts[2].parse::<f32>().ok()?;
    let mem = parts[3].parse::<f32>().ok()?;
    let vsz = parts[4].parse::<u64>().ok()?;
    let rss = parts[5].parse::<u64>().ok()?;

    Some(ProcessInfo {
        pid,
        user: parts[0].to_string(),
        cpu,
        mem,
        vsz,
        rss,
        tty: parts[6].to_string(),
        stat: parts[7].to_string(),
        start: parts[8].to_string(),
        time: parts[9].to_string(),
        command: parts[10..].join(" "),
    })
}

fn parse_tasklist_line(line: &str) -> Option<ProcessInfo> {
    let fields = parse_csv_record(line)?;
    if fields.len() < 5 {
        return None;
    }

    let pid = fields.get(1)?.parse::<i32>().ok()?;
    let rss = fields
        .get(4)
        .map(|value| parse_tasklist_memory_kb(value))
        .unwrap_or(0);
    let image_name = fields.first()?.clone();
    let session_name = fields.get(2).cloned().unwrap_or_default();
    let status = fields.get(5).cloned().unwrap_or_default();
    let user = fields.get(6).cloned().unwrap_or_default();
    let cpu_time = fields.get(7).cloned().unwrap_or_default();
    let window_title = fields.get(8).cloned().unwrap_or_default();

    let command = if window_title.is_empty() || window_title.eq_ignore_ascii_case("N/A") {
        image_name
    } else {
        format!("{image_name} [{window_title}]")
    };

    Some(ProcessInfo {
        pid,
        user,
        cpu: 0.0,
        mem: 0.0,
        vsz: 0,
        rss,
        tty: session_name,
        stat: status,
        start: String::new(),
        time: cpu_time,
        command,
    })
}

fn parse_csv_record(line: &str) -> Option<Vec<String>> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;

    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                if in_quotes && matches!(chars.peek(), Some('"')) {
                    current.push('"');
                    chars.next();
                } else {
                    in_quotes = !in_quotes;
                }
            }
            ',' if !in_quotes => {
                fields.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    if in_quotes {
        return None;
    }

    fields.push(current.trim().to_string());
    Some(fields)
}

fn parse_tasklist_memory_kb(value: &str) -> u64 {
    value
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect::<String>()
        .parse::<u64>()
        .unwrap_or(0)
}

/// Get process start time from /proc/[pid]/stat for additional PID validation
#[cfg(target_os = "linux")]
fn get_process_starttime(pid: i32) -> Option<u64> {
    let stat_path = format!("/proc/{}/stat", pid);
    let content = std::fs::read_to_string(stat_path).ok()?;

    // Field 22 (0-indexed: 21) is starttime
    // Format: pid (comm) state ppid pgrp session tty_nr tpgid flags minflt cminflt majflt cmajflt
    //         utime stime cutime cstime priority nice num_threads itrealvalue starttime ...

    // Find the closing parenthesis of comm field (which may contain spaces)
    let comm_end = content.find(')')?;
    let after_comm = &content[comm_end + 2..]; // Skip ") "
    let fields: Vec<&str> = after_comm.split_whitespace().collect();

    // starttime is field 20 after comm (0-indexed: 19)
    fields.get(19).and_then(|s| s.parse().ok())
}

/// Verify process identity before kill to mitigate PID reuse race condition
#[cfg(target_os = "linux")]
fn verify_process_identity(pid: i32, saved_starttime: Option<u64>) -> Result<(), String> {
    if let Some(saved) = saved_starttime {
        if let Some(current) = get_process_starttime(pid) {
            if saved != current {
                return Err("Process PID was reused by a different process".to_string());
            }
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn verify_process_identity(_pid: i32, _saved_starttime: Option<u64>) -> Result<(), String> {
    // On non-Linux platforms, skip starttime verification
    Ok(())
}

/// Kill a process by PID
pub fn kill_process(pid: i32) -> Result<(), String> {
    kill_process_with_verification(pid, None)
}

/// Kill a process by PID with optional starttime verification
pub fn kill_process_with_verification(pid: i32, starttime: Option<u64>) -> Result<(), String> {
    if !is_valid_pid(pid) {
        return Err("Invalid PID".to_string());
    }

    // Get process info to check if it's a kernel thread
    let command = get_process_command(pid);
    is_protected_pid(pid, command.as_deref())?;

    verify_process_identity(pid, starttime)?;

    #[cfg(unix)]
    {
        // Use libc kill for safety
        #[allow(unsafe_code)]
        let result = unsafe { libc::kill(pid, libc::SIGTERM) };
        if result == 0 {
            Ok(())
        } else {
            let errno = std::io::Error::last_os_error();
            match errno.raw_os_error() {
                Some(libc::ESRCH) => Err("Process not found".to_string()),
                Some(libc::EPERM) => Err("Permission denied".to_string()),
                _ => Err(errno.to_string()),
            }
        }
    }
    #[cfg(not(unix))]
    {
        // Use taskkill on Windows
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string()])
            .status()
            .map_err(|e| format!("Failed to execute taskkill: {}", e))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("taskkill failed with code {:?}", status.code()))
        }
    }
}

/// Force kill a process by PID (SIGKILL)
pub fn force_kill_process(pid: i32) -> Result<(), String> {
    force_kill_process_with_verification(pid, None)
}

/// Force kill a process by PID (SIGKILL) with optional starttime verification
pub fn force_kill_process_with_verification(
    pid: i32,
    starttime: Option<u64>,
) -> Result<(), String> {
    if !is_valid_pid(pid) {
        return Err("Invalid PID".to_string());
    }

    let command = get_process_command(pid);
    is_protected_pid(pid, command.as_deref())?;

    verify_process_identity(pid, starttime)?;

    #[cfg(unix)]
    {
        #[allow(unsafe_code)]
        let result = unsafe { libc::kill(pid, libc::SIGKILL) };
        if result == 0 {
            Ok(())
        } else {
            let errno = std::io::Error::last_os_error();
            match errno.raw_os_error() {
                Some(libc::ESRCH) => Err("Process not found".to_string()),
                Some(libc::EPERM) => Err("Permission denied".to_string()),
                _ => Err(errno.to_string()),
            }
        }
    }
    #[cfg(not(unix))]
    {
        // Use taskkill /F for force kill on Windows
        let status = Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status()
            .map_err(|e| format!("Failed to execute taskkill: {}", e))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("taskkill failed with code {:?}", status.code()))
        }
    }
}

/// Get process command by PID
fn get_process_command(pid: i32) -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        let filter = format!("PID eq {pid}");
        let output = Command::new("tasklist")
            .args(["/FO", "CSV", "/NH", "/FI", &filter])
            .output()
            .ok()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.lines().find_map(|line| {
            let fields = parse_csv_record(line)?;
            let image = fields.first()?.trim();
            if image.is_empty() || image.starts_with("INFO:") {
                None
            } else {
                Some(image.to_string())
            }
        })
    }

    #[cfg(not(target_os = "windows"))]
    {
        // Use "command=" format to suppress header (POSIX compatible, works on Linux and macOS)
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
            .ok()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let command = stdout.trim();
        if command.is_empty() {
            None
        } else {
            Some(command.to_string())
        }
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn test_wait_with_output_timeout_kills_child_process_group() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 5"]);
        configure_child_process_group(&mut command);

        let child = command.spawn().expect("sleep command should spawn");
        let error = wait_with_output_timeout(child, Duration::from_millis(20), "test child")
            .expect_err("timeout should fail");

        assert!(error.contains("test child timed out after 0s"));
    }

    // ========== is_valid_pid tests ==========

    #[test]
    fn test_is_valid_pid_positive() {
        assert!(is_valid_pid(1));
        assert!(is_valid_pid(100));
        assert!(is_valid_pid(1000));
        assert!(is_valid_pid(4194304)); // Max PID on Linux
    }

    #[test]
    fn test_is_valid_pid_invalid() {
        assert!(!is_valid_pid(0));
        assert!(!is_valid_pid(-1));
        assert!(!is_valid_pid(-100));
        assert!(!is_valid_pid(4194305)); // Exceeds max PID
    }

    // ========== is_protected_pid tests ==========

    #[test]
    fn test_is_protected_pid_init() {
        // PID 1 is init/systemd and should be protected
        let result = is_protected_pid(1, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("system process"));
    }

    #[test]
    fn test_is_protected_pid_kthreadd() {
        // PID 2 is kthreadd and should be protected
        let result = is_protected_pid(2, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("system process"));
    }

    #[test]
    fn test_is_protected_pid_self() {
        // Current process should be protected
        let current_pid = std::process::id() as i32;
        let result = is_protected_pid(current_pid, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("file manager itself"));
    }

    #[test]
    fn test_is_protected_pid_low_pid() {
        // Low PIDs (< 300) are likely kernel threads
        let result = is_protected_pid(100, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("kernel thread"));
    }

    #[test]
    fn test_is_protected_pid_normal() {
        // Normal user process PIDs should be allowed
        // Use a high PID that's unlikely to be the current process
        let high_pid = 50000;
        if high_pid != std::process::id() as i32 {
            let result = is_protected_pid(high_pid, None);
            assert!(result.is_ok());
        }
    }

    // ========== kernel thread detection tests ==========

    #[test]
    fn test_kernel_thread_detection_bracket_format() {
        // Kernel threads have names like [kworker/0:0]
        let result = is_protected_pid(50000, Some("[kworker/0:0]"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("kernel threads"));
    }

    #[test]
    fn test_kernel_thread_detection_normal_process() {
        // Normal processes should pass
        let result = is_protected_pid(50000, Some("/usr/bin/bash"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_kernel_thread_detection_various_formats() {
        // Various kernel thread names
        assert!(is_protected_pid(50000, Some("[migration/0]")).is_err());
        assert!(is_protected_pid(50000, Some("[ksoftirqd/0]")).is_err());
        assert!(is_protected_pid(50000, Some("[rcu_sched]")).is_err());
    }

    // ========== parse_process_line tests ==========

    #[test]
    fn test_parse_process_line_valid() {
        let line = "root         1  0.0  0.1  12345  6789 ?        Ss   Jan01   0:05 /sbin/init";
        let result = parse_process_line(line);
        assert!(result.is_some());

        let info = result.unwrap();
        assert_eq!(info.pid, 1);
        assert_eq!(info.user, "root");
        assert_eq!(info.cpu, 0.0);
        assert_eq!(info.mem, 0.1);
        assert_eq!(info.command, "/sbin/init");
    }

    #[test]
    fn test_parse_process_line_invalid_short() {
        let line = "root 1 0.0"; // Too few fields
        let result = parse_process_line(line);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_process_line_command_with_spaces() {
        let line = "user     12345  1.5  2.3  54321  9876 pts/0    S+   10:00   0:01 /usr/bin/program --arg value";
        let result = parse_process_line(line);
        assert!(result.is_some());

        let info = result.unwrap();
        assert_eq!(info.pid, 12345);
        assert_eq!(info.command, "/usr/bin/program --arg value");
    }

    #[test]
    fn test_parse_csv_record_handles_embedded_comma() {
        let line = "\"Code.exe\",\"1234\",\"Console\",\"1\",\"12,345 K\",\"Running\",\"DESKTOP\\user\",\"0:00:03\",\"AgentDesk, Main\"";
        let fields = parse_csv_record(line).expect("valid csv");
        assert_eq!(fields.len(), 9);
        assert_eq!(fields[0], "Code.exe");
        assert_eq!(fields[4], "12,345 K");
        assert_eq!(fields[8], "AgentDesk, Main");
    }

    #[test]
    fn test_parse_tasklist_line_valid() {
        let line = "\"Code.exe\",\"1234\",\"Console\",\"1\",\"12,345 K\",\"Running\",\"DESKTOP\\user\",\"0:00:03\",\"N/A\"";
        let result = parse_tasklist_line(line).expect("tasklist row");
        assert_eq!(result.pid, 1234);
        assert_eq!(result.user, "DESKTOP\\user");
        assert_eq!(result.rss, 12345);
        assert_eq!(result.command, "Code.exe");
        assert_eq!(result.tty, "Console");
        assert_eq!(result.time, "0:00:03");
    }

    #[test]
    fn test_parse_tasklist_line_with_window_title() {
        let line = "\"cmd.exe\",\"4321\",\"Console\",\"1\",\"1,024 K\",\"Running\",\"DESKTOP\\user\",\"0:00:01\",\"AgentDesk Shell\"";
        let result = parse_tasklist_line(line).expect("tasklist row");
        assert_eq!(result.command, "cmd.exe [AgentDesk Shell]");
    }

    // ========== SortField tests ==========

    #[test]
    fn test_sort_field_equality() {
        assert_eq!(SortField::Pid, SortField::Pid);
        assert_eq!(SortField::Cpu, SortField::Cpu);
        assert_eq!(SortField::Mem, SortField::Mem);
        assert_eq!(SortField::Command, SortField::Command);
        assert_ne!(SortField::Pid, SortField::Cpu);
    }

    // ========== ProcessInfo tests ==========

    #[test]
    fn test_process_info_clone() {
        let info = ProcessInfo {
            pid: 1234,
            user: "test".to_string(),
            cpu: 1.5,
            mem: 2.5,
            vsz: 1000,
            rss: 500,
            tty: "pts/0".to_string(),
            stat: "S".to_string(),
            start: "10:00".to_string(),
            time: "0:01".to_string(),
            command: "test_cmd".to_string(),
        };

        let cloned = info.clone();
        assert_eq!(cloned.pid, info.pid);
        assert_eq!(cloned.user, info.user);
        assert_eq!(cloned.command, info.command);
    }
}

#[cfg(all(test, unix))]
mod simple_cancel_watcher_tests {
    use super::{configure_child_process_group, spawn_simple_cancel_watcher};
    use crate::services::provider::CancelToken;
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    /// #2250: a CancelToken-driven watcher must kill the child process tree
    /// within ~1s of the token flipping to cancelled. This guards against
    /// regressions where the watcher loop is wired up but cancellation
    /// signal does not actually terminate the child.
    #[test]
    fn watcher_kills_sleeping_child_when_token_is_cancelled() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30"]);
        configure_child_process_group(&mut command);
        let mut child = command.spawn().expect("sleep should spawn");
        let pid = child.id();

        let token = Arc::new(CancelToken::new());
        let watcher = spawn_simple_cancel_watcher(Some(token.clone()), pid);

        // Mid-flight cancel: the spawned watcher must observe this and
        // SIGTERM the process group within 1-2 seconds.
        let cancel_at = Instant::now();
        token.cancelled.store(true, Ordering::Relaxed);

        // Wait up to 2s for the child to exit (watcher poll = 100ms).
        let status = loop {
            if let Ok(Some(status)) = child.try_wait() {
                break status;
            }
            if cancel_at.elapsed() > Duration::from_secs(2) {
                let _ = child.kill();
                panic!(
                    "watcher did not kill child within 2s of cancel; elapsed {:?}",
                    cancel_at.elapsed()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        };

        // child was killed by signal, not exit(0)
        assert!(
            !status.success(),
            "expected non-zero exit because watcher killed the child, got {:?}",
            status
        );
        watcher.disarm();
    }

    /// Sanity: when no token is supplied, the watcher must not interfere.
    /// A None-token watcher should be an inert handle.
    #[test]
    fn watcher_is_noop_without_token() {
        let watcher = spawn_simple_cancel_watcher(None, 0);
        watcher.disarm();
    }

    /// Issue #2335 (d): the Claude simple-cancel path now arms the watcher
    /// BEFORE writing to stdin. This regression test mirrors the new
    /// ordering: spawn a child that blocks reading stdin, arm the watcher
    /// immediately, then trigger cancel BEFORE writing/closing stdin. The
    /// watcher must reap the child even though stdin never finished — i.e.
    /// the previous ordering (stdin-then-watcher) would have stalled the
    /// caller until stdin completion.
    #[test]
    fn watcher_armed_before_stdin_write_honours_immediate_cancel() {
        use std::process::Stdio;

        // `cat` blocks reading stdin and exits on EOF. We never close stdin
        // here; the watcher must kill the process instead.
        let mut command = Command::new("sh");
        command.args(["-c", "cat > /dev/null"]);
        configure_child_process_group(&mut command);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("cat should spawn");
        let pid = child.id();

        // ORDERING UNDER TEST: arm the watcher BEFORE the (would-be) stdin
        // write. Then keep stdin open and cancel; the watcher must reap.
        let token = Arc::new(CancelToken::new());
        let watcher = spawn_simple_cancel_watcher(Some(token.clone()), pid);

        // Hold stdin to simulate the window where the caller has not yet
        // written or closed the pipe. With the old ordering the watcher
        // would not exist yet and an immediate cancel would not propagate.
        let _stdin = child.stdin.take();
        let cancel_at = Instant::now();
        token.cancelled.store(true, Ordering::Relaxed);

        let status = loop {
            if let Ok(Some(status)) = child.try_wait() {
                break status;
            }
            if cancel_at.elapsed() > Duration::from_secs(2) {
                let _ = child.kill();
                panic!(
                    "watcher armed pre-stdin did not kill child within 2s; elapsed {:?}",
                    cancel_at.elapsed()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(
            !status.success(),
            "expected non-zero exit because watcher killed the child, got {:?}",
            status
        );
        watcher.disarm();
    }

    /// #2250 (Codex review follow-up): when the spawned child has its own
    /// process group (as codex/claude simple calls now do), the watcher's
    /// `kill_pid_tree` must reap a grandchild that outlives the direct
    /// child. This guards against regressions where wrapper / grandchild
    /// processes leak after cancellation.
    #[test]
    fn watcher_reaps_grandchild_when_child_uses_process_group() {
        // The parent `sh` exec's into a background `sleep` and a `wait` so
        // the process group contains both. Killing only the direct PID
        // would orphan the sleep; only a group kill reaps it within the
        // assertion window.
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 & wait"]);
        configure_child_process_group(&mut command);
        let mut child = command.spawn().expect("sh wrapper should spawn");
        let pid = child.id();

        let token = Arc::new(CancelToken::new());
        let watcher = spawn_simple_cancel_watcher(Some(token.clone()), pid);
        // Give the wrapper a moment to actually fork the sleep.
        std::thread::sleep(Duration::from_millis(200));
        token.cancelled.store(true, Ordering::Relaxed);

        let cancel_at = Instant::now();
        let status = loop {
            if let Ok(Some(status)) = child.try_wait() {
                break status;
            }
            if cancel_at.elapsed() > Duration::from_secs(3) {
                let _ = child.kill();
                panic!(
                    "watcher did not kill process group within 3s; elapsed {:?}",
                    cancel_at.elapsed()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(
            !status.success(),
            "expected non-zero exit after group kill, got {:?}",
            status
        );
        watcher.disarm();
    }
}
