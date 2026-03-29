//! Platform-aware binary resolution.
//!
//! Replaces direct `which`/`where` and `bash -lc "which X"` calls with a
//! unified API that works across macOS, Linux, and Windows.

use std::collections::BTreeSet;
use std::process::Command;
use std::sync::OnceLock;

/// Resolve a binary by name using the platform's standard lookup mechanism.
///
/// - **Unix**: `which <name>`
/// - **Windows**: `where.exe <name>`
///
/// Returns `None` if the binary is not found on PATH.
pub fn resolve_binary(name: &str) -> Option<String> {
    #[cfg(unix)]
    let output = Command::new("which").arg(name).output();
    #[cfg(windows)]
    let output = Command::new("where.exe").arg(name).output();

    output.ok().filter(|o| o.status.success()).and_then(|o| {
        let path = String::from_utf8_lossy(&o.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if path.is_empty() { None } else { Some(path) }
    })
}

/// Resolve a binary using a login shell to pick up the user's full PATH.
///
/// Useful for non-interactive contexts (e.g., launchd services, SSH sessions)
/// where `~/.profile` / `~/.zshrc` are not sourced.
///
/// - **Unix**: Tries `zsh -lc "which <name>"`, then `bash -lc "which <name>"`
/// - **Windows**: Falls back to `resolve_binary` (login shell not applicable)
pub fn resolve_binary_with_login_shell(name: &str) -> Option<String> {
    // Fast path: try standard PATH first
    if let Some(path) = resolve_binary(name) {
        return Some(path);
    }

    #[cfg(unix)]
    {
        let which_cmd = format!("which {}", name);
        for shell in &["zsh", "bash"] {
            if let Ok(output) = Command::new(shell).args(["-lc", &which_cmd]).output() {
                if output.status.success() {
                    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !path.is_empty() {
                        return Some(path);
                    }
                }
            }
        }
    }

    #[cfg(windows)]
    {
        // On Windows, try PowerShell Get-Command as fallback
        if let Ok(output) = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "(Get-Command {} -ErrorAction SilentlyContinue).Source",
                    name
                ),
            ])
            .output()
        {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    return Some(path);
                }
            }
        }
    }

    None
}

fn merge_path_entries(primary: &str, secondary: &str) -> Option<String> {
    let mut seen = BTreeSet::new();
    let mut merged = Vec::new();
    for raw in [primary, secondary] {
        for entry in std::env::split_paths(raw) {
            let normalized = entry.to_string_lossy().trim().to_string();
            if normalized.is_empty() || !seen.insert(normalized.clone()) {
                continue;
            }
            merged.push(entry);
        }
    }
    std::env::join_paths(merged)
        .ok()
        .map(|value| value.to_string_lossy().to_string())
}

#[cfg(unix)]
fn resolve_login_shell_path_uncached() -> Option<String> {
    for shell in &["zsh", "bash"] {
        if let Ok(output) = Command::new(shell)
            .args(["-lc", r#"printf '%s' "$PATH""#])
            .output()
        {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    return Some(path);
                }
            }
        }
    }
    None
}

#[cfg(not(unix))]
fn resolve_login_shell_path_uncached() -> Option<String> {
    None
}

pub fn resolve_login_shell_path() -> Option<String> {
    static LOGIN_SHELL_PATH: OnceLock<Option<String>> = OnceLock::new();
    LOGIN_SHELL_PATH
        .get_or_init(resolve_login_shell_path_uncached)
        .clone()
}

pub fn merged_runtime_path() -> Option<String> {
    let current = std::env::var("PATH")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let login_shell = resolve_login_shell_path().filter(|value| !value.trim().is_empty());
    match (current, login_shell) {
        (Some(current), Some(login_shell)) => merge_path_entries(&login_shell, &current),
        (Some(current), None) => Some(current),
        (None, Some(login_shell)) => Some(login_shell),
        (None, None) => None,
    }
}

pub fn apply_runtime_path(command: &mut Command) {
    if let Some(path) = merged_runtime_path() {
        command.env("PATH", path);
    }
}

/// Async version of `resolve_binary_with_login_shell`.
///
/// Runs the full resolution chain (which/where → login shell → known paths)
/// on a blocking thread so it can be used from async contexts.
pub async fn async_resolve_binary_with_login_shell(name: &str) -> Option<String> {
    let name = name.to_string();
    tokio::task::spawn_blocking(move || resolve_binary_with_login_shell(&name))
        .await
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_binary_finds_known_tool() {
        // `ls` exists on all Unix, `cmd.exe` on Windows
        #[cfg(unix)]
        assert!(resolve_binary("ls").is_some());
        #[cfg(windows)]
        assert!(resolve_binary("cmd.exe").is_some());
    }

    #[test]
    fn resolve_binary_returns_none_for_missing() {
        assert!(resolve_binary("__nonexistent_binary_12345__").is_none());
    }

    #[test]
    fn resolve_with_login_shell_finds_known_tool() {
        #[cfg(unix)]
        assert!(resolve_binary_with_login_shell("ls").is_some());
        #[cfg(windows)]
        assert!(resolve_binary_with_login_shell("cmd.exe").is_some());
    }

    #[test]
    fn merge_path_entries_dedupes_and_preserves_order() {
        #[cfg(unix)]
        let joined = merge_path_entries("/opt/homebrew/bin:/usr/bin", "/usr/bin:/bin").unwrap();
        #[cfg(windows)]
        let joined = merge_path_entries(r"C:\Tools;C:\Windows", r"C:\Windows;C:\Bin").unwrap();

        let parts = std::env::split_paths(&joined)
            .map(|entry| entry.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        #[cfg(unix)]
        assert_eq!(parts, vec!["/opt/homebrew/bin", "/usr/bin", "/bin"]);
        #[cfg(windows)]
        assert_eq!(parts, vec![r"C:\Tools", r"C:\Windows", r"C:\Bin"]);
    }
}
