//! Platform-aware process dump capture.
//!
//! Abstracts `sample` (macOS), `gcore`/`/proc` (Linux), and `procdump` (Windows)
//! behind a single API for hang detection diagnostics.

use std::process::Command;

/// Capture a diagnostic dump of the given process for post-mortem analysis.
///
/// - **macOS**: Uses `sample <pid> <duration> -f <output_path>`
/// - **Linux**: Reads `/proc/<pid>/stack` (no extra tool needed)
/// - **Windows**: Attempts `procdump -accepteula -ma <pid> <output_path>` (best-effort)
///
/// Returns `Ok(())` if the dump was written (or at least attempted).
/// Returns `Err` if the dump mechanism is unavailable.
pub fn capture_process_dump(pid: u32, output_path: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let status = Command::new("sample")
            .args([&pid.to_string(), "1", "-f", output_path])
            .status()
            .map_err(|e| format!("Failed to run sample: {}", e))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("sample exited with code {:?}", status.code()))
        }
    }

    #[cfg(target_os = "linux")]
    {
        // Try /proc/<pid>/stack first (kernel stack, requires root or same user)
        let stack_path = format!("/proc/{}/stack", pid);
        match std::fs::read_to_string(&stack_path) {
            Ok(stack) => {
                // Also grab /proc/<pid>/status for thread info
                let status_info =
                    std::fs::read_to_string(format!("/proc/{}/status", pid)).unwrap_or_default();
                let combined = format!(
                    "=== Process {} stack ===\n{}\n=== Process {} status ===\n{}",
                    pid, stack, pid, status_info
                );
                std::fs::write(output_path, combined)
                    .map_err(|e| format!("Failed to write dump: {}", e))?;
                Ok(())
            }
            Err(_) => {
                // Fallback: try gcore if available.
                // Note: gcore writes to `<prefix>.<pid>`, so we strip the extension
                // from output_path and let gcore append the PID suffix, then rename
                // to the caller's expected path.
                let gcore_prefix = output_path.trim_end_matches(".txt");
                let status = Command::new("gcore")
                    .args(["-o", gcore_prefix, &pid.to_string()])
                    .status()
                    .map_err(|e| format!("Neither /proc stack nor gcore available: {}", e))?;
                if status.success() {
                    // gcore writes to <prefix>.<pid> — rename to expected path
                    let actual = format!("{}.{}", gcore_prefix, pid);
                    let _ = std::fs::rename(&actual, output_path);
                    Ok(())
                } else {
                    Err("gcore failed".to_string())
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        // Best-effort: procdump is a Sysinternals tool, may not be installed
        let status = Command::new("procdump")
            .args(["-accepteula", "-ma", &pid.to_string(), output_path])
            .status();
        match status {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(format!("procdump exited with code {:?}", s.code())),
            Err(_) => {
                // procdump not available — write a stub diagnostic
                let info = format!(
                    "Process dump not available on this Windows system (PID {}).\n\
                     Install procdump from Sysinternals for full diagnostics.",
                    pid
                );
                std::fs::write(output_path, info)
                    .map_err(|e| format!("Failed to write stub dump: {}", e))?;
                Ok(())
            }
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (pid, output_path);
        Err("Process dump not supported on this platform".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_dump_for_self_does_not_panic() {
        let pid = std::process::id();
        let tmp = std::env::temp_dir().join("adk-dump-test.txt");
        // This may fail (permissions, tool not available) but should not panic
        let _ = capture_process_dump(pid, &tmp.display().to_string());
        let _ = std::fs::remove_file(&tmp);
    }
}
