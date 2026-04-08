//! Platform-aware binary resolution.
//!
//! Provides a single resolution contract for provider CLIs across macOS,
//! Linux, and Windows.

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(3);
const SHELL_ENV_DELIMITER: &str = "__AGENTDESK_SHELL_ENV__";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinaryResolution {
    pub requested_binary: String,
    pub resolved_path: Option<String>,
    pub canonical_path: Option<String>,
    pub source: Option<String>,
    pub attempts: Vec<String>,
    pub failure_kind: Option<String>,
    pub exec_path: Option<String>,
}

pub fn resolve_binary(name: &str) -> Option<String> {
    resolve_in_paths(name, std::env::var_os("PATH"), &current_dir_fallback())
        .map(|path| path.to_string_lossy().to_string())
}

pub fn resolve_binary_with_login_shell(name: &str) -> Option<String> {
    let cwd = current_dir_fallback();
    if let Some(path) = resolve_in_paths(name, std::env::var_os("PATH"), &cwd) {
        return Some(path.to_string_lossy().to_string());
    }
    if let Some(path) = resolve_in_paths(name, resolve_login_shell_path_os(), &cwd) {
        return Some(path.to_string_lossy().to_string());
    }
    resolve_in_paths(name, join_paths_lossy(standard_fallback_dirs()), &cwd)
        .map(|path| path.to_string_lossy().to_string())
}

pub fn resolve_provider_binary(provider: &str) -> BinaryResolution {
    let requested_binary = normalize_name(provider);
    let override_var = override_var_name(&requested_binary);
    let cwd = current_dir_fallback();
    let mut attempts = Vec::new();

    match std::env::var_os(&override_var).filter(|value| !os_value_is_empty(value)) {
        Some(raw_override) => {
            let expanded = expand_user_path(&raw_override);
            match resolve_candidate_path(&expanded, &cwd) {
                Ok(path) => {
                    attempts.push(format!(
                        "env_override:{}=found:{}",
                        override_var,
                        path.display()
                    ));
                    return finalize_resolution(
                        requested_binary,
                        path,
                        "env_override".to_string(),
                        attempts,
                    );
                }
                Err(error) => {
                    attempts.push(format!(
                        "env_override:{}=miss:{}:{}",
                        override_var,
                        expanded.display(),
                        error
                    ));
                    return BinaryResolution {
                        requested_binary,
                        resolved_path: None,
                        canonical_path: None,
                        source: None,
                        attempts,
                        failure_kind: Some(error),
                        exec_path: merged_runtime_path(),
                    };
                }
            }
        }
        None => attempts.push(format!("env_override:{}=unset", override_var)),
    }

    let current_path = std::env::var_os("PATH").filter(|value| !os_value_is_empty(value));
    match resolve_in_paths(&requested_binary, current_path.clone(), &cwd) {
        Some(path) => {
            attempts.push(format!("current_path=found:{}", path.display()));
            return finalize_resolution(
                requested_binary,
                path,
                "current_path".to_string(),
                attempts,
            );
        }
        None => attempts.push(if current_path.is_some() {
            "current_path=miss".to_string()
        } else {
            "current_path=unset".to_string()
        }),
    }

    let login_path = resolve_login_shell_path_os().filter(|value| !os_value_is_empty(value));
    match resolve_in_paths(&requested_binary, login_path.clone(), &cwd) {
        Some(path) => {
            attempts.push(format!("login_shell_path=found:{}", path.display()));
            return finalize_resolution(
                requested_binary,
                path,
                "login_shell_path".to_string(),
                attempts,
            );
        }
        None => attempts.push(if login_path.is_some() {
            "login_shell_path=miss".to_string()
        } else {
            "login_shell_path=unavailable".to_string()
        }),
    }

    let fallback_dirs = provider_fallback_dirs(&requested_binary);
    match resolve_in_paths(
        &requested_binary,
        join_paths_lossy(fallback_dirs.clone()),
        &cwd,
    ) {
        Some(path) => {
            attempts.push(format!("fallback_path=found:{}", path.display()));
            finalize_resolution(
                requested_binary,
                path,
                "fallback_path".to_string(),
                attempts,
            )
        }
        None => {
            attempts.push(format!("fallback_path=miss:{}dirs", fallback_dirs.len()));
            BinaryResolution {
                requested_binary,
                resolved_path: None,
                canonical_path: None,
                source: None,
                attempts,
                failure_kind: Some("not_found".to_string()),
                exec_path: merged_runtime_path(),
            }
        }
    }
}

pub fn resolve_login_shell_path() -> Option<String> {
    resolve_login_shell_path_os().map(|value| value.to_string_lossy().to_string())
}

pub fn merged_runtime_path() -> Option<String> {
    join_paths_lossy(runtime_path_entries()).map(|value| value.to_string_lossy().to_string())
}

pub fn apply_runtime_path(command: &mut Command) {
    if let Some(path) = merged_runtime_path() {
        command.env("PATH", path);
    }
}

pub fn augment_exec_path(command: &mut Command, binary_path: impl AsRef<Path>) {
    if let Some(path) = exec_path_for_binary(binary_path.as_ref()) {
        command.env("PATH", path);
    } else {
        apply_runtime_path(command);
    }
}

pub fn apply_binary_resolution(command: &mut Command, resolution: &BinaryResolution) {
    if let Some(path) = &resolution.exec_path {
        command.env("PATH", path);
    } else if let Some(path) = &resolution.resolved_path {
        augment_exec_path(command, path);
    } else {
        apply_runtime_path(command);
    }
}

pub fn prepare_provider_command(resolution: &BinaryResolution) -> Option<Command> {
    let resolved_path = resolution.resolved_path.as_ref()?;
    let mut command = Command::new(resolved_path);
    apply_binary_resolution(&mut command, resolution);
    Some(command)
}

pub fn probe_resolved_binary_version(
    binary_path: impl AsRef<OsStr>,
    resolution: &BinaryResolution,
) -> (Option<String>, Option<String>) {
    let mut command = Command::new(binary_path);
    apply_binary_resolution(&mut command, resolution);
    command.arg("--version");

    match command.output() {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if stdout.is_empty() {
                (None, Some("version_probe_empty".to_string()))
            } else {
                (Some(stdout), None)
            }
        }
        Ok(_) => (None, Some("version_probe_failed".to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            (None, Some("permission_denied".to_string()))
        }
        Err(_) => (None, Some("version_probe_spawn_failed".to_string())),
    }
}

pub async fn async_resolve_binary_with_login_shell(name: &str) -> Option<String> {
    let name = name.to_string();
    tokio::task::spawn_blocking(move || resolve_binary_with_login_shell(&name))
        .await
        .ok()
        .flatten()
}

fn finalize_resolution(
    requested_binary: String,
    resolved_path: PathBuf,
    source: String,
    attempts: Vec<String>,
) -> BinaryResolution {
    let canonical_path = std::fs::canonicalize(&resolved_path).ok();
    BinaryResolution {
        requested_binary,
        resolved_path: Some(resolved_path.to_string_lossy().to_string()),
        canonical_path: canonical_path
            .as_ref()
            .map(|path| path.to_string_lossy().to_string()),
        source: Some(source),
        attempts,
        failure_kind: None,
        exec_path: build_exec_path(&resolved_path, canonical_path.as_deref()),
    }
}

fn resolve_in_paths(
    binary_name: impl AsRef<OsStr>,
    paths: Option<OsString>,
    cwd: &Path,
) -> Option<PathBuf> {
    which::which_in(binary_name, paths, cwd).ok()
}

fn resolve_candidate_path(candidate: &Path, cwd: &Path) -> Result<PathBuf, String> {
    if candidate.exists() && !is_effectively_executable(candidate) {
        return Err("permission_denied".to_string());
    }
    which::which_in(candidate.as_os_str(), Option::<OsString>::None, cwd)
        .map_err(|error| error.to_string())
}

fn is_effectively_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return std::fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
    }

    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn build_exec_path(resolved_path: &Path, canonical_path: Option<&Path>) -> Option<String> {
    join_paths_lossy(exec_path_entries(resolved_path, canonical_path))
        .map(|value| value.to_string_lossy().to_string())
}

fn exec_path_for_binary(binary_path: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(binary_path).ok();
    build_exec_path(binary_path, canonical.as_deref())
}

fn runtime_path_entries() -> Vec<PathBuf> {
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();

    extend_split_paths(std::env::var_os("PATH"), &mut entries, &mut seen);
    extend_split_paths(resolve_login_shell_path_os(), &mut entries, &mut seen);
    for dir in standard_fallback_dirs() {
        push_unique_path(dir, &mut entries, &mut seen);
    }

    entries
}

fn exec_path_entries(resolved_path: &Path, canonical_path: Option<&Path>) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();

    if let Some(parent) = resolved_path.parent() {
        push_unique_path(parent.to_path_buf(), &mut entries, &mut seen);
    }
    if let Some(parent) = canonical_path.and_then(Path::parent) {
        push_unique_path(parent.to_path_buf(), &mut entries, &mut seen);
    }
    for entry in runtime_path_entries() {
        push_unique_path(entry, &mut entries, &mut seen);
    }

    entries
}

fn provider_fallback_dirs(provider: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen = BTreeSet::new();

    for dir in windows_provider_subdirs(provider) {
        push_unique_path(dir, &mut dirs, &mut seen);
    }
    for dir in standard_fallback_dirs() {
        push_unique_path(dir, &mut dirs, &mut seen);
    }

    dirs
}

#[cfg(windows)]
fn windows_provider_subdirs(provider: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen = BTreeSet::new();

    if let Some(home) = dirs::home_dir() {
        push_unique_path(
            home.join("AppData/Local/Programs").join(provider),
            &mut dirs,
            &mut seen,
        );
    }
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        push_unique_path(
            PathBuf::from(local_app_data)
                .join("Programs")
                .join(provider),
            &mut dirs,
            &mut seen,
        );
    }
    push_unique_path(
        PathBuf::from("C:/Program Files").join(provider),
        &mut dirs,
        &mut seen,
    );
    push_unique_path(
        PathBuf::from("C:/Program Files (x86)").join(provider),
        &mut dirs,
        &mut seen,
    );

    dirs
}

#[cfg(not(windows))]
fn windows_provider_subdirs(_provider: &str) -> Vec<PathBuf> {
    Vec::new()
}

fn standard_fallback_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen = BTreeSet::new();
    let home = dirs::home_dir();

    if let Some(home) = &home {
        push_unique_path(home.join(".local/bin"), &mut dirs, &mut seen);
        push_unique_path(home.join("bin"), &mut dirs, &mut seen);
        push_unique_path(home.join(".volta/bin"), &mut dirs, &mut seen);
        push_unique_path(home.join(".bun/bin"), &mut dirs, &mut seen);
        push_unique_path(home.join(".asdf/shims"), &mut dirs, &mut seen);
        push_unique_path(home.join(".npm-global/bin"), &mut dirs, &mut seen);
    }

    #[cfg(unix)]
    push_unique_path(PathBuf::from("/usr/local/bin"), &mut dirs, &mut seen);

    #[cfg(target_os = "macos")]
    {
        push_unique_path(PathBuf::from("/opt/homebrew/bin"), &mut dirs, &mut seen);
        if let Some(home) = &home {
            push_unique_path(home.join("Library/pnpm"), &mut dirs, &mut seen);
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(home) = &home {
            push_unique_path(home.join(".local/share/pnpm"), &mut dirs, &mut seen);
            push_unique_path(home.join(".local/share/mise/shims"), &mut dirs, &mut seen);
            push_unique_path(home.join(".local/share/rtx/shims"), &mut dirs, &mut seen);
        }
    }

    push_env_dir("PNPM_HOME", None, &mut dirs, &mut seen);
    push_env_dir("VOLTA_HOME", Some("bin"), &mut dirs, &mut seen);
    push_env_dir("BUN_INSTALL", Some("bin"), &mut dirs, &mut seen);
    push_env_dir("ASDF_DATA_DIR", Some("shims"), &mut dirs, &mut seen);
    push_env_dir("MISE_DATA_DIR", Some("shims"), &mut dirs, &mut seen);
    push_env_dir("RTX_DATA_DIR", Some("shims"), &mut dirs, &mut seen);
    push_env_dir("N_PREFIX", Some("bin"), &mut dirs, &mut seen);

    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            push_unique_path(PathBuf::from(appdata).join("npm"), &mut dirs, &mut seen);
        }
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            let root = PathBuf::from(local_app_data);
            push_unique_path(root.join("Volta/bin"), &mut dirs, &mut seen);
            push_unique_path(root.join("Programs"), &mut dirs, &mut seen);
        }
        if let Some(user_profile) = std::env::var_os("USERPROFILE") {
            push_unique_path(
                PathBuf::from(user_profile).join("scoop/shims"),
                &mut dirs,
                &mut seen,
            );
        }
        push_unique_path(
            PathBuf::from("C:/ProgramData/chocolatey/bin"),
            &mut dirs,
            &mut seen,
        );
    }

    dirs
}

#[cfg(unix)]
fn resolve_login_shell_path_os() -> Option<OsString> {
    static LOGIN_SHELL_PATH: OnceLock<Option<OsString>> = OnceLock::new();
    LOGIN_SHELL_PATH
        .get_or_init(resolve_login_shell_path_uncached)
        .clone()
}

#[cfg(not(unix))]
fn resolve_login_shell_path_os() -> Option<OsString> {
    None
}

#[cfg(unix)]
fn resolve_login_shell_path_uncached() -> Option<OsString> {
    let env_cmd = format!(
        "printf '%s' '{delimiter}'; command env; printf '%s' '{delimiter}'; exit",
        delimiter = SHELL_ENV_DELIMITER
    );

    for shell in login_shell_candidates() {
        let mut command = Command::new(&shell);
        command
            .args(["-ilc", &env_cmd])
            .env("DISABLE_AUTO_UPDATE", "true")
            .env("ZSH_TMUX_AUTOSTARTED", "true")
            .env("ZSH_TMUX_AUTOSTART", "false")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let Ok(mut child) = command.spawn() else {
            continue;
        };

        let deadline = Instant::now() + LOGIN_SHELL_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }

        let Ok(output) = child.wait_with_output() else {
            continue;
        };
        if !output.status.success() {
            continue;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let Some((_, after_start)) = stdout.split_once(SHELL_ENV_DELIMITER) else {
            continue;
        };
        let Some((env_block, _)) = after_start.split_once(SHELL_ENV_DELIMITER) else {
            continue;
        };
        if let Some(path) = env_block
            .lines()
            .find_map(|line| line.strip_prefix("PATH="))
        {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Some(OsString::from(trimmed));
            }
        }
    }

    None
}

#[cfg(unix)]
fn login_shell_candidates() -> Vec<PathBuf> {
    let mut shells = Vec::new();
    let mut seen = BTreeSet::new();

    if let Some(shell) = std::env::var_os("SHELL").filter(|value| !os_value_is_empty(value)) {
        push_unique_path(PathBuf::from(shell), &mut shells, &mut seen);
    }
    push_unique_path(PathBuf::from("/bin/zsh"), &mut shells, &mut seen);
    push_unique_path(PathBuf::from("/bin/bash"), &mut shells, &mut seen);

    shells
}

fn current_dir_fallback() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn override_var_name(provider: &str) -> String {
    let mut normalized = String::new();
    for ch in provider.chars() {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch.to_ascii_uppercase());
        } else {
            normalized.push('_');
        }
    }
    format!("AGENTDESK_{}_PATH", normalized)
}

fn normalize_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

fn os_value_is_empty(value: &OsStr) -> bool {
    value.to_string_lossy().trim().is_empty()
}

fn expand_user_path(raw: &OsStr) -> PathBuf {
    let raw = raw.to_string_lossy();
    if raw == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(raw.as_ref()));
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(raw.as_ref())
}

fn push_env_dir(
    env_name: &str,
    suffix: Option<&str>,
    entries: &mut Vec<PathBuf>,
    seen: &mut BTreeSet<String>,
) {
    let Some(value) = std::env::var_os(env_name).filter(|value| !os_value_is_empty(value)) else {
        return;
    };
    let mut path = expand_user_path(&value);
    if let Some(suffix) = suffix {
        path = path.join(suffix);
    }
    push_unique_path(path, entries, seen);
}

fn extend_split_paths(
    value: Option<OsString>,
    entries: &mut Vec<PathBuf>,
    seen: &mut BTreeSet<String>,
) {
    let Some(value) = value.filter(|value| !os_value_is_empty(value)) else {
        return;
    };
    for entry in std::env::split_paths(&value) {
        push_unique_path(entry, entries, seen);
    }
}

fn push_unique_path(path: PathBuf, entries: &mut Vec<PathBuf>, seen: &mut BTreeSet<String>) {
    let normalized = path.to_string_lossy().trim().to_string();
    if normalized.is_empty() || !seen.insert(normalized) {
        return;
    }
    entries.push(path);
}

fn join_paths_lossy(paths: Vec<PathBuf>) -> Option<OsString> {
    if paths.is_empty() {
        return None;
    }
    std::env::join_paths(paths).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    fn env_guard() -> MutexGuard<'static, ()> {
        crate::services::discord::runtime_store::lock_test_env()
    }

    #[cfg(unix)]
    fn write_executable(path: &Path) {
        write_executable_with_contents(path, "#!/bin/sh\nexit 0\n");
    }

    #[cfg(unix)]
    fn write_executable_with_contents(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, contents).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn resolve_binary_finds_known_tool() {
        let _guard = env_guard();
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
        let _guard = env_guard();
        #[cfg(unix)]
        assert!(resolve_binary_with_login_shell("ls").is_some());
        #[cfg(windows)]
        assert!(resolve_binary_with_login_shell("cmd.exe").is_some());
    }

    #[test]
    fn standard_fallback_dirs_include_common_entries() {
        let dirs = standard_fallback_dirs();

        #[cfg(unix)]
        assert!(
            dirs.iter()
                .any(|entry| entry == Path::new("/usr/local/bin"))
        );

        let volta_suffix = Path::new(".volta").join("bin");
        assert!(dirs.iter().any(|entry| entry.ends_with(&volta_suffix)));

        #[cfg(target_os = "macos")]
        assert!(
            dirs.iter()
                .any(|entry| entry == Path::new("/opt/homebrew/bin"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn provider_resolution_uses_windows_provider_program_subdir() {
        let _guard = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let provider = "agentdesk-test-provider";
        let provider_dir = temp.path().join("Programs").join(provider);
        let binary_path = provider_dir.join(format!("{provider}.exe"));
        let original_local_app_data = std::env::var_os("LOCALAPPDATA");
        let original_path = std::env::var_os("PATH");
        let override_var = override_var_name(provider);
        let original_override = std::env::var_os(&override_var);

        std::fs::create_dir_all(&provider_dir).unwrap();
        std::fs::write(&binary_path, b"stub").unwrap();

        unsafe {
            std::env::set_var("LOCALAPPDATA", temp.path());
            std::env::set_var("PATH", "");
            std::env::remove_var(&override_var);
        }

        let resolution = resolve_provider_binary(provider);

        assert_eq!(
            resolution.resolved_path,
            Some(binary_path.to_string_lossy().to_string())
        );
        assert_eq!(resolution.source.as_deref(), Some("fallback_path"));

        unsafe {
            match original_local_app_data {
                Some(value) => std::env::set_var("LOCALAPPDATA", value),
                None => std::env::remove_var("LOCALAPPDATA"),
            }
            match original_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
            match original_override {
                Some(value) => std::env::set_var(&override_var, value),
                None => std::env::remove_var(&override_var),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn provider_override_reloads_on_each_call() {
        let _guard = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("codex-first");
        let second = temp.path().join("codex-second");
        write_executable(&first);
        write_executable(&second);

        unsafe {
            std::env::set_var("AGENTDESK_CODEX_PATH", &first);
        }
        let first_resolution = resolve_provider_binary("codex");
        assert_eq!(
            first_resolution.resolved_path,
            Some(first.to_string_lossy().to_string())
        );
        assert_eq!(first_resolution.source.as_deref(), Some("env_override"));

        unsafe {
            std::env::set_var("AGENTDESK_CODEX_PATH", &second);
        }
        let second_resolution = resolve_provider_binary("codex");
        assert_eq!(
            second_resolution.resolved_path,
            Some(second.to_string_lossy().to_string())
        );
        assert_eq!(second_resolution.source.as_deref(), Some("env_override"));

        unsafe {
            std::env::remove_var("AGENTDESK_CODEX_PATH");
        }
    }

    #[cfg(unix)]
    #[test]
    fn provider_override_reports_permission_denied_for_non_executable_file() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("codex");
        std::fs::write(&provider, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&provider).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&provider, perms).unwrap();

        unsafe {
            std::env::set_var("AGENTDESK_CODEX_PATH", &provider);
        }
        let resolution = resolve_provider_binary("codex");

        assert_eq!(resolution.resolved_path, None);
        assert_eq!(
            resolution.failure_kind.as_deref(),
            Some("permission_denied")
        );
        assert!(
            resolution
                .attempts
                .iter()
                .any(|attempt| attempt.contains("permission_denied"))
        );

        unsafe {
            std::env::remove_var("AGENTDESK_CODEX_PATH");
        }
    }

    #[cfg(unix)]
    #[test]
    fn exec_path_includes_resolved_and_canonical_parent_dirs() {
        let _guard = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let real_dir = temp.path().join("real");
        let link_dir = temp.path().join("link");
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::create_dir_all(&link_dir).unwrap();

        let real_bin = real_dir.join("claude");
        let link_bin = link_dir.join("claude");
        write_executable(&real_bin);
        std::os::unix::fs::symlink(&real_bin, &link_bin).unwrap();

        unsafe {
            std::env::set_var("AGENTDESK_CLAUDE_PATH", &link_bin);
        }
        let resolution = resolve_provider_binary("claude");
        let exec_path = resolution.exec_path.unwrap();
        let parts = std::env::split_paths(&exec_path)
            .map(|entry| entry.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        let canonical_real_dir = std::fs::canonicalize(&real_dir).unwrap();
        let expected_link_dir = link_dir.to_string_lossy().to_string();
        let expected_real_dir = canonical_real_dir.to_string_lossy().to_string();

        assert_eq!(
            parts.first().map(String::as_str),
            Some(expected_link_dir.as_str())
        );
        assert_eq!(
            parts.get(1).map(String::as_str),
            Some(expected_real_dir.as_str())
        );

        unsafe {
            std::env::remove_var("AGENTDESK_CLAUDE_PATH");
        }
    }

    #[cfg(unix)]
    #[test]
    fn apply_binary_resolution_makes_sibling_helpers_visible_with_minimal_path() {
        let _guard = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("claude");
        let helper = temp.path().join("provider-helper");
        let original_path = std::env::var_os("PATH");

        write_executable_with_contents(&helper, "#!/bin/sh\nprintf 'helper:%s' \"$1\"\n");
        write_executable_with_contents(&provider, "#!/bin/sh\nprovider-helper \"$1\"\n");

        unsafe {
            std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            std::env::set_var("AGENTDESK_CLAUDE_PATH", &provider);
        }

        let resolution = resolve_provider_binary("claude");
        let mut command = Command::new(
            resolution
                .resolved_path
                .as_ref()
                .expect("resolved path should exist"),
        );
        apply_binary_resolution(&mut command, &resolution);
        let output = command.arg("ok").output().unwrap();

        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "helper:ok");
        assert_eq!(resolution.source.as_deref(), Some("env_override"));

        unsafe {
            std::env::remove_var("AGENTDESK_CLAUDE_PATH");
            match original_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
    }
}
