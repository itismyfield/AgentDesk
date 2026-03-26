//! Platform-aware shell command execution.
//!
//! Abstracts `bash -c` (Unix) vs `cmd /C` (Windows) behind a unified API.

use std::process::{Command, Output};

/// Execute a shell command string using the platform's default shell.
///
/// - **Unix**: `bash -c "<cmd>"`
/// - **Windows**: `cmd.exe /C "<cmd>"`
pub fn shell_command(cmd: &str) -> Result<Output, String> {
    #[cfg(unix)]
    let result = Command::new("bash").args(["-c", cmd]).output();
    #[cfg(windows)]
    let result = Command::new("cmd.exe").args(["/C", cmd]).output();

    result.map_err(|e| format!("Failed to execute shell command: {}", e))
}

/// Async version of `shell_command`.
pub async fn async_shell_command(cmd: &str) -> Result<Output, String> {
    #[cfg(unix)]
    let result = tokio::process::Command::new("bash")
        .args(["-c", cmd])
        .output()
        .await;
    #[cfg(windows)]
    let result = tokio::process::Command::new("cmd.exe")
        .args(["/C", cmd])
        .output()
        .await;

    result.map_err(|e| format!("Failed to execute shell command: {}", e))
}

/// Build a `Command` for the platform shell, ready for further customization.
///
/// Returns a `Command` set up as `bash -c <cmd>` (Unix) or `cmd.exe /C <cmd>` (Windows).
/// Caller can add `.stdin()`, `.stdout()`, `.current_dir()`, etc.
pub fn shell_command_builder(cmd: &str) -> Command {
    #[cfg(unix)]
    {
        let mut c = Command::new("bash");
        c.args(["-c", cmd]);
        c
    }
    #[cfg(windows)]
    {
        let mut c = Command::new("cmd.exe");
        c.args(["/C", cmd]);
        c
    }
}

/// Build a tokio `Command` for the platform shell.
pub fn async_shell_command_builder(cmd: &str) -> tokio::process::Command {
    #[cfg(unix)]
    {
        let mut c = tokio::process::Command::new("bash");
        c.args(["-c", cmd]);
        c
    }
    #[cfg(windows)]
    {
        let mut c = tokio::process::Command::new("cmd.exe");
        c.args(["/C", cmd]);
        c
    }
}

// ── Common shell utilities ─────────────────────────────────────────

/// Get the short hostname of the current machine.
///
/// Equivalent to `hostname -s` on Unix.  Falls back to "localhost" on failure.
pub fn hostname_short() -> String {
    Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "localhost".to_string())
}

/// Get the current HEAD commit hash from a git repo directory.
///
/// Returns `None` if git is unavailable or the directory is not a repo.
pub fn git_head_commit(repo_dir: &str) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_command_echo_works() {
        #[cfg(unix)]
        let output = shell_command("echo hello").unwrap();
        #[cfg(windows)]
        let output = shell_command("echo hello").unwrap();

        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("hello"));
    }

    #[test]
    fn shell_command_builder_works() {
        #[cfg(unix)]
        let cmd_str = "echo test123";
        #[cfg(windows)]
        let cmd_str = "echo test123";

        let output = shell_command_builder(cmd_str).output().unwrap();
        assert!(output.status.success());
    }
}
