use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use crate::services::provider::{CancelToken, cancel_requested};

/// Handle returned by [`spawn_simple_cancel_watcher`].
///
/// Drop or call [`SimpleCancelWatcher::disarm`] once the child has been
/// reaped to stop the polling thread. The watcher also snapshots the child
/// PID identity at spawn time so a late cancel after `wait_with_output` reaps
/// the child cannot send SIGTERM to a reused PID.
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
    let child_identity = ProcessIdentity::capture(child_pid);

    let handle = std::thread::Builder::new()
        .name("simple-cancel-watcher".to_string())
        .spawn(move || {
            let poll = Duration::from_millis(100);
            while armed_thread.load(Ordering::Relaxed) {
                if cancel_requested(Some(token.as_ref())) {
                    if armed_thread.load(Ordering::Relaxed) {
                        kill_pid_tree_if_identity_matches(child_pid, child_identity);
                    }
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

/// Opaque process-identity snapshot captured at SIGTERM dispatch time.
///
/// Used to detect PID reuse between SIGTERM and the 200ms-later SIGKILL
/// escalation. On Linux this stores the kernel `starttime` jiffies from
/// `/proc/<pid>/stat`; on macOS it stores the BSD-info start microseconds
/// from `proc_pidinfo(..PROC_PIDTBSDINFO..)`. On other platforms the
/// snapshot is `None` and verification is skipped (best-effort).
#[derive(Debug, Clone, Copy)]
pub struct ProcessIdentity {
    starttime: Option<u128>,
    #[cfg(target_os = "macos")]
    macos_lstart_hash: Option<u128>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessIdentityProbe {
    Same,
    GoneOrReused,
    ProbeError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessGroupProbe {
    Live,
    Gone,
    ProbeError,
}

impl ProcessIdentity {
    /// Capture identity for `pid` (best-effort; returns an empty snapshot on
    /// unsupported platforms or if the read fails).
    pub fn capture(pid: u32) -> Self {
        Self {
            starttime: read_process_starttime(pid),
            #[cfg(target_os = "macos")]
            macos_lstart_hash: read_lstart_hash_macos(pid),
        }
    }

    /// Returns true if the process at `pid` still matches this snapshot's
    /// identity (fail-closed for #2320).
    ///
    /// Three cases:
    /// 1. No snapshot was captured at all (unsupported platform): return
    ///    `true` so legacy escalation behaviour is preserved on platforms
    ///    where we cannot read identity anyway.
    /// 2. A snapshot exists and the current starttime equals it: `true`.
    /// 3. A snapshot exists but current starttime is unreadable or differs:
    ///    `false`. We must fail closed here — proceeding with SIGKILL on a
    ///    PID we cannot verify is exactly the unsafe path #2320 closes. The
    ///    target either already exited (SIGKILL is now a no-op or worse, a
    ///    PID-reuse hit) or its identity has changed.
    pub fn matches(&self, pid: u32) -> bool {
        if let Some(saved) = self.starttime {
            match read_process_starttime(pid) {
                Some(current) => return current == saved, // case 2/3a
                None => {
                    #[cfg(target_os = "macos")]
                    {
                        if let Some(saved_lstart) = self.macos_lstart_hash {
                            return read_lstart_hash_macos(pid) == Some(saved_lstart);
                        }
                    }
                    return false; // case 3b: fail closed
                }
            }
        }
        #[cfg(target_os = "macos")]
        {
            if let Some(saved_lstart) = self.macos_lstart_hash {
                return read_lstart_hash_macos(pid) == Some(saved_lstart);
            }
        }
        // Case 1: no baseline — defer to legacy behaviour.
        true
    }

    /// Three-state identity probe for replay barriers. Unlike [`Self::matches`],
    /// an unreadable identity for a still-present PID is `ProbeError`, not
    /// evidence that the original process exited.
    #[cfg(unix)]
    pub fn probe(&self, pid: u32) -> ProcessIdentityProbe {
        match unix_process_presence(pid) {
            ProcessGroupProbe::Gone => return ProcessIdentityProbe::GoneOrReused,
            ProcessGroupProbe::ProbeError => return ProcessIdentityProbe::ProbeError,
            ProcessGroupProbe::Live => {}
        }
        if let Some(saved) = self.starttime {
            return match read_process_starttime(pid) {
                Some(current) if current == saved => ProcessIdentityProbe::Same,
                Some(_) => ProcessIdentityProbe::GoneOrReused,
                None => {
                    #[cfg(target_os = "macos")]
                    if let Some(saved_lstart) = self.macos_lstart_hash {
                        return match read_lstart_hash_macos(pid) {
                            Some(current) if current == saved_lstart => ProcessIdentityProbe::Same,
                            Some(_) => ProcessIdentityProbe::GoneOrReused,
                            None => ProcessIdentityProbe::ProbeError,
                        };
                    }
                    ProcessIdentityProbe::ProbeError
                }
            };
        }
        #[cfg(target_os = "macos")]
        if let Some(saved_lstart) = self.macos_lstart_hash {
            return match read_lstart_hash_macos(pid) {
                Some(current) if current == saved_lstart => ProcessIdentityProbe::Same,
                Some(_) => ProcessIdentityProbe::GoneOrReused,
                None => ProcessIdentityProbe::ProbeError,
            };
        }
        ProcessIdentityProbe::ProbeError
    }

    pub(crate) fn from_persisted(starttime: Option<u128>, macos_lstart_hash: Option<u128>) -> Self {
        Self {
            starttime,
            #[cfg(target_os = "macos")]
            macos_lstart_hash,
        }
    }

    pub(crate) fn persisted_starttime(&self) -> Option<u128> {
        self.starttime
    }

    pub(crate) fn persisted_macos_lstart_hash(&self) -> Option<u128> {
        #[cfg(target_os = "macos")]
        {
            self.macos_lstart_hash
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    fn has_baseline(&self) -> bool {
        self.starttime.is_some() || {
            #[cfg(target_os = "macos")]
            {
                self.macos_lstart_hash.is_some()
            }
            #[cfg(not(target_os = "macos"))]
            {
                false
            }
        }
    }

    /// Test-only inspector for the captured snapshot. Used to verify that
    /// the identity reader actually produced a value on supported
    /// platforms (Linux/macOS) — without this hook, regression tests cannot
    /// distinguish "captured a real starttime" from "snapshot is empty and
    /// guard silently disabled".
    #[cfg(test)]
    pub(crate) fn raw_starttime(&self) -> Option<u128> {
        #[cfg(target_os = "macos")]
        {
            self.starttime.or(self.macos_lstart_hash)
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.starttime
        }
    }

    /// Test-only synthetic constructor used to pin the mismatch-skip path.
    #[cfg(test)]
    pub(crate) fn from_raw_for_test(starttime: Option<u128>) -> Self {
        Self {
            starttime,
            #[cfg(target_os = "macos")]
            macos_lstart_hash: None,
        }
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn unix_kill_zero_probe(target: libc::pid_t) -> ProcessGroupProbe {
    if unsafe { libc::kill(target, 0) } == 0 {
        return ProcessGroupProbe::Live;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => ProcessGroupProbe::Gone,
        Some(libc::EPERM) => ProcessGroupProbe::Live,
        _ => ProcessGroupProbe::ProbeError,
    }
}

#[cfg(unix)]
fn unix_process_presence(pid: u32) -> ProcessGroupProbe {
    unix_kill_zero_probe(pid as libc::pid_t)
}

#[cfg(unix)]
pub fn process_group_probe(pgid: u32) -> ProcessGroupProbe {
    unix_kill_zero_probe(-(pgid as libc::pid_t))
}

/// Cross-platform starttime reader returning a stable monotonic-ish value
/// per process. `None` means "cannot determine" (process gone, or platform
/// not supported).
fn read_process_starttime(pid: u32) -> Option<u128> {
    #[cfg(target_os = "linux")]
    {
        return get_process_starttime(pid as i32).map(|v| v as u128);
    }
    #[cfg(target_os = "macos")]
    {
        return read_starttime_macos(pid);
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn read_starttime_macos(pid: u32) -> Option<u128> {
    use std::mem::MaybeUninit;
    let mut info: MaybeUninit<libc::proc_bsdinfo> = MaybeUninit::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: proc_pidinfo writes up to `size` bytes into `info` when it
    // returns a positive value. We check the return code before reading.
    let ret = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr() as *mut libc::c_void,
            size,
        )
    };
    if ret <= 0 || ret < size {
        return None;
    }
    // SAFETY: proc_pidinfo wrote a full struct.
    let info = unsafe { info.assume_init() };
    Some((info.pbi_start_tvsec as u128) * 1_000_000 + info.pbi_start_tvusec as u128)
}

#[cfg(target_os = "macos")]
fn read_lstart_hash_macos(pid: u32) -> Option<u128> {
    use std::hash::{Hash, Hasher};

    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let lstart = String::from_utf8_lossy(&output.stdout);
    let lstart = lstart.trim();
    if lstart.is_empty() {
        return None;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    lstart.hash(&mut hasher);
    Some(hasher.finish() as u128)
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn pid_exists(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn process_group_exists(pgid: u32) -> bool {
    unsafe { libc::kill(-(pgid as libc::pid_t), 0) == 0 }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn pid_has_process_group(pid: u32) -> bool {
    unsafe { libc::getpgid(pid as libc::pid_t) >= 0 }
}

#[cfg(all(unix, target_os = "linux"))]
fn pid_is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            parse_linux_proc_stat_after_comm(&stat)
                .and_then(|rest| rest.split_whitespace().next())
                .map(str::to_string)
        })
        .is_some_and(|state| state == "Z")
}

#[cfg(all(unix, target_os = "macos"))]
#[allow(unsafe_code)]
fn pid_is_zombie(pid: u32) -> bool {
    use std::mem::MaybeUninit;
    let mut info: MaybeUninit<libc::proc_bsdinfo> = MaybeUninit::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let ret = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr() as *mut libc::c_void,
            size,
        )
    };
    if ret <= 0 || ret < size {
        return false;
    }
    let info = unsafe { info.assume_init() };
    info.pbi_status == libc::SZOMB
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn pid_is_zombie(_pid: u32) -> bool {
    false
}

#[cfg(all(unix, target_os = "linux"))]
fn parse_linux_proc_stat_after_comm(stat: &str) -> Option<&str> {
    stat.rsplit_once(") ").map(|(_, rest)| rest)
}

#[cfg(all(unix, target_os = "linux"))]
fn parse_linux_proc_stat_starttime(stat: &str) -> Option<u64> {
    let after_comm = parse_linux_proc_stat_after_comm(stat)?;
    after_comm
        .split_whitespace()
        .nth(19)
        .and_then(|field| field.parse().ok())
}

#[cfg(unix)]
fn should_escalate_process_group_after_grace(
    leader_exists: bool,
    leader_identity_matches: bool,
    group_exists_after_leader_exit: bool,
) -> bool {
    if leader_exists {
        leader_identity_matches
    } else {
        group_exists_after_leader_exit
    }
}

/// Kill a process tree by PID.
///
/// On Unix, sends SIGTERM to the process group (or PID), waits ~200ms, then
/// escalates to SIGKILL — but only after verifying the PID/PGID leader is
/// still the same process that received SIGTERM. This identity check closes
/// the PID-reuse race where the original child exits during the grace
/// window and the OS recycles its PID for an unrelated process before our
/// SIGKILL fires. See issue #2320.
#[allow(unsafe_code)]
pub fn kill_pid_tree(pid: u32) {
    #[cfg(unix)]
    {
        let identity = ProcessIdentity::capture(pid);
        let _ = kill_pid_tree_with_identity(pid, identity);
    }
    #[cfg(not(unix))]
    {
        let _ = kill_pid_tree_with_identity(pid);
    }
}

#[cfg(unix)]
pub(crate) fn kill_pid_tree_if_identity_matches(pid: u32, identity: ProcessIdentity) -> bool {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if !identity.has_baseline() {
        tracing::debug!(
            pid,
            "skip kill_pid_tree for simple cancel watcher because child PID identity was not captured"
        );
        return false;
    }
    if !identity.matches(pid) {
        tracing::debug!(
            pid,
            "skip kill_pid_tree for simple cancel watcher because child PID identity no longer matches"
        );
        return false;
    }
    kill_pid_tree_with_identity(pid, identity)
}

#[cfg(not(unix))]
pub(crate) fn kill_pid_tree_if_identity_matches(pid: u32, _identity: ProcessIdentity) -> bool {
    kill_pid_tree_with_identity(pid)
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn kill_pid_tree_with_identity(pid: u32, identity: ProcessIdentity) -> bool {
    unsafe {
        let ret = libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
        let mut signalled = ret == 0;
        if ret != 0 {
            // No process group (single PID fallback path).
            signalled = libc::kill(pid as libc::pid_t, libc::SIGTERM) == 0;
            std::thread::sleep(std::time::Duration::from_millis(200));
            if identity.matches(pid) {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        } else {
            std::thread::sleep(std::time::Duration::from_millis(200));
            // If the leader PID still exists, only fire SIGKILL when it still
            // matches the captured identity. A reused zombie PID is still a
            // PID-reuse hazard, not proof that the original leader exited. If
            // the leader PID fully disappeared during the grace window but the
            // group remains, the group still belongs to the original
            // cancellation target and needs SIGKILL cleanup.
            let leader_exists =
                pid_exists(pid) && pid_has_process_group(pid) && !pid_is_zombie(pid);
            if should_escalate_process_group_after_grace(
                leader_exists,
                leader_exists && identity.matches(pid),
                !leader_exists && process_group_exists(pid),
            ) {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }
        signalled
    }
}

#[cfg(not(unix))]
fn kill_pid_tree_with_identity(pid: u32) -> bool {
    std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output()
        .is_ok()
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

// #3034: Windows-only — reached solely from the tasklist branch of
// `get_process_list_result`; gated so the non-Windows build does not flag it.
#[cfg(target_os = "windows")]
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

// #3034: Windows-only — CSV splitter for tasklist output (see parse_tasklist_line).
#[cfg(target_os = "windows")]
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

// #3034: Windows-only — parses tasklist "Mem Usage" KB column (see parse_tasklist_line).
#[cfg(target_os = "windows")]
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
    parse_linux_proc_stat_starttime(&content)
}

#[cfg(all(test, unix))]
mod process_group_tests {
    use super::*;
    use std::io::Read;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn configured_process_group_kill_reaps_grandchild() {
        let temp = tempfile::tempdir().expect("tempdir");
        let grandchild_pid_path = temp.path().join("grandchild.pid");
        let script = format!(
            "sleep 100 & echo $! > {}; wait",
            shell_escape(&grandchild_pid_path.display().to_string())
        );

        let mut command = Command::new("sh");
        command.args(["-c", &script]);
        configure_child_process_group(&mut command);
        let mut child = command.spawn().expect("wrapper shell should spawn");
        let child_pid = child.id();
        let child_identity = ProcessIdentity::capture(child_pid);

        assert_eq!(child_identity.probe(child_pid), ProcessIdentityProbe::Same);
        assert_eq!(process_group_probe(child_pid), ProcessGroupProbe::Live);

        let grandchild_pid =
            wait_for_pid_file(&grandchild_pid_path).expect("wrapper should write grandchild pid");
        assert!(
            process_is_running(grandchild_pid),
            "grandchild should be alive before process-group kill"
        );

        kill_pid_tree(child_pid);
        let _ = child.wait();

        assert!(
            wait_until_not_running(grandchild_pid, Duration::from_secs(3)),
            "grandchild should be reaped by process-group kill"
        );
        assert_eq!(
            child_identity.probe(child_pid),
            ProcessIdentityProbe::GoneOrReused
        );
        assert_eq!(process_group_probe(child_pid), ProcessGroupProbe::Gone);
    }

    fn wait_for_pid_file(path: &std::path::Path) -> Option<u32> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(mut file) = std::fs::File::open(path) {
                let mut contents = String::new();
                if file.read_to_string(&mut contents).is_ok()
                    && let Ok(pid) = contents.trim().parse::<u32>()
                {
                    return Some(pid);
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_until_not_running(pid: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !process_is_running(pid) {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[allow(unsafe_code)]
    fn process_is_running(pid: u32) -> bool {
        (unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }) && !super::pid_is_zombie(pid)
    }
}

#[cfg(all(test, unix))]
mod simple_cancel_watcher_tests {
    use super::{
        configure_child_process_group, should_escalate_process_group_after_grace,
        spawn_simple_cancel_watcher,
    };
    use crate::services::provider::CancelToken;
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    #[test]
    fn group_escalation_requires_identity_match_while_leader_pid_exists() {
        assert!(
            !should_escalate_process_group_after_grace(
                true,  /* leader_exists */
                false, /* leader_identity_matches */
                true,  /* group_exists_after_leader_exit */
            ),
            "an existing leader PID with mismatched identity must not use the leader-exit path"
        );
        assert!(should_escalate_process_group_after_grace(
            true,  /* leader_exists */
            true,  /* leader_identity_matches */
            false, /* group_exists_after_leader_exit */
        ));
        assert!(should_escalate_process_group_after_grace(
            false, /* leader_exists */
            false, /* leader_identity_matches */
            true,  /* group_exists_after_leader_exit */
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_proc_stat_parser_handles_comm_with_spaces_and_parens() {
        let stat =
            "123 (worker ) with spaces) Z 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 424242 20";

        assert_eq!(
            super::parse_linux_proc_stat_after_comm(stat),
            Some("Z 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 424242 20")
        );
        assert_eq!(super::parse_linux_proc_stat_starttime(stat), Some(424242));
    }

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

    #[test]
    fn kill_pid_tree_kills_group_when_leader_exits_but_descendant_ignores_sigterm() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let descendant_pid_path = temp.path().join("descendant.pid");

        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(
                r#"
                trap 'exit 0' TERM
                sh -c 'trap "" TERM; printf "%s\n" "$$" > "$DESCENDANT_PID_FILE"; exec sleep 60' &
                while :; do sleep 1; done
                "#,
            )
            .env("DESCENDANT_PID_FILE", &descendant_pid_path);
        configure_child_process_group(&mut command);
        let mut child = command.spawn().expect("leader shell should spawn");
        let leader_pid = child.id();

        let descendant_pid = {
            let started_at = Instant::now();
            loop {
                if let Ok(raw) = std::fs::read_to_string(&descendant_pid_path) {
                    let trimmed = raw.trim();
                    if !trimmed.is_empty() {
                        break trimmed.parse::<u32>().expect("descendant pid");
                    }
                }
                if started_at.elapsed() > Duration::from_secs(2) {
                    let _ = child.kill();
                    panic!("descendant did not write pid file within 2s");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        };

        assert!(
            process_is_running(descendant_pid),
            "descendant should be alive before kill"
        );
        super::kill_pid_tree(leader_pid);

        let deadline = Instant::now() + Duration::from_secs(2);
        while process_is_running(descendant_pid) {
            if Instant::now() >= deadline {
                force_kill_process_for_test(descendant_pid);
                let _ = child.kill();
                panic!("descendant pid {descendant_pid} survived leader-exit group SIGKILL path");
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        let _ = child.wait();
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

    /// #2320: identity capture must yield a *real, non-empty* snapshot on
    /// Linux/macOS — otherwise the SIGKILL guard silently disables itself
    /// and the fix regresses to pre-#2320 behaviour. We assert
    /// `raw_starttime().is_some()` rather than only `matches()` because
    /// `matches()` returns `true` on the no-baseline path, which would let
    /// a broken reader pass the test.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn process_identity_captures_real_starttime_on_supported_platforms() {
        use super::ProcessIdentity;

        let mut child = Command::new("sh")
            .args(["-c", "sleep 5"])
            .spawn()
            .expect("sleep should spawn");
        let pid = child.id();

        let identity = ProcessIdentity::capture(pid);
        assert!(
            identity.raw_starttime().is_some(),
            "Linux/macOS reader must produce a non-empty starttime; \
             empty snapshot would silently disable the #2320 guard"
        );
        assert!(
            identity.matches(pid),
            "captured identity must match the live PID"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// #2320: when the original child has exited, an identity-bearing
    /// snapshot must fail closed (`matches() == false`) — `kill_pid_tree`
    /// relies on this to skip the SIGKILL that would otherwise target a
    /// potentially recycled PID.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn process_identity_fails_closed_when_pid_no_longer_exists() {
        use super::ProcessIdentity;

        let mut child = Command::new("sh")
            .args(["-c", "true"])
            .spawn()
            .expect("true should spawn");
        let pid = child.id();
        let identity = ProcessIdentity::capture(pid);
        // Reap the child so the kernel releases its PID slot.
        let _ = child.wait();
        // Spin briefly to ensure the kernel reflects the exit.
        for _ in 0..20 {
            if super::read_process_starttime(pid).is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            !identity.matches(pid),
            "after the original PID's process has gone, matches() must \
             return false so SIGKILL is skipped"
        );
    }

    /// #2320: a synthetic mismatch must skip the SIGKILL path. Pins the
    /// recycled-PID safety invariant without depending on a real PID-reuse
    /// race (which is non-deterministic to provoke in tests).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn process_identity_mismatch_returns_false_for_live_pid() {
        use super::ProcessIdentity;

        let mut child = Command::new("sh")
            .args(["-c", "sleep 5"])
            .spawn()
            .expect("sleep should spawn");
        let pid = child.id();

        // Construct a snapshot whose starttime is deliberately wrong.
        let live = ProcessIdentity::capture(pid);
        let bogus =
            ProcessIdentity::from_raw_for_test(live.raw_starttime().map(|s| s.wrapping_add(1)));
        assert!(
            !bogus.matches(pid),
            "different starttime for same PID must be treated as PID reuse"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// #2385: SimpleCancelWatcher captures the child identity at spawn time
    /// and must skip the *initial* SIGTERM when that snapshot no longer
    /// matches the PID. This pins the PID-reuse guard before any signal is
    /// sent; #2502 separately covers the post-SIGTERM SIGKILL escalation.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn simple_cancel_identity_guard_skips_initial_sigterm_for_mismatched_pid() {
        use super::ProcessIdentity;

        let mut child = Command::new("sh")
            .args(["-c", "sleep 5"])
            .spawn()
            .expect("sleep should spawn");
        let pid = child.id();

        let live = ProcessIdentity::capture(pid);
        let bogus =
            ProcessIdentity::from_raw_for_test(live.raw_starttime().map(|s| s.wrapping_add(1)));

        assert!(
            !super::kill_pid_tree_if_identity_matches(pid, bogus),
            "mismatched identity must skip kill_pid_tree before SIGTERM"
        );
        assert!(
            process_is_running(pid),
            "mismatched identity path must not signal the live process"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[allow(unsafe_code)]
    fn process_is_running(pid: u32) -> bool {
        (unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }) && !super::pid_is_zombie(pid)
    }

    #[allow(unsafe_code)]
    fn force_kill_process_for_test(pid: u32) {
        let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
}
