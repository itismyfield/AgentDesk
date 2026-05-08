//! Platform-aware binary resolution.
//!
//! Provides a single resolution contract for provider CLIs across macOS,
//! Linux, and Windows.

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::runtime_layout::expand_user_path;

const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(3);
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const VERSION_PROBE_MAX_OUTPUT_BYTES: usize = 8 * 1024;
const SHELL_ENV_DELIMITER: &str = "__AGENTDESK_SHELL_ENV__";

thread_local! {
    static ACTIVE_PROVIDER_CONTEXTS: RefCell<Vec<crate::services::provider_cli::ProviderExecutionContext>> =
        const { RefCell::new(Vec::new()) };
}

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

pub fn git_binary() -> &'static OsString {
    static GIT_BINARY: OnceLock<OsString> = OnceLock::new();
    GIT_BINARY.get_or_init(|| {
        for key in ["AGENTDESK_TEST_GIT", "AGENTDESK_GIT"] {
            if let Some(configured) = std::env::var_os(key).filter(|value| !value.is_empty()) {
                return configured;
            }
        }
        #[cfg(windows)]
        {
            if let Ok(output) = Command::new(r"C:\Windows\System32\where.exe")
                .arg("git")
                .output()
            {
                if output.status.success() {
                    if let Some(path) = String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .map(str::trim)
                        .find(|line| !line.is_empty())
                    {
                        return path.into();
                    }
                }
            }
            for candidate in [
                r"C:\Program Files\Git\cmd\git.exe",
                r"C:\Program Files\Git\bin\git.exe",
                r"C:\Program Files (x86)\Git\cmd\git.exe",
                r"C:\Program Files (x86)\Git\bin\git.exe",
            ] {
                if Path::new(candidate).exists() {
                    return candidate.into();
                }
            }
            "git.exe".into()
        }
        #[cfg(not(windows))]
        {
            "git".into()
        }
    })
}

pub fn git_command() -> Command {
    Command::new(git_binary())
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
    if let Some(ctx) = active_provider_context(provider) {
        return resolve_provider_binary_for_context(&ctx);
    }
    resolve_provider_binary_legacy(provider)
}

fn resolve_provider_binary_legacy(provider: &str) -> BinaryResolution {
    let requested_binary = normalize_name(provider);
    let override_var = override_var_name(&requested_binary);
    let cwd = current_dir_fallback();
    let mut attempts = Vec::new();

    match std::env::var_os(&override_var).filter(|value| !os_value_is_empty(value)) {
        Some(raw_override) => {
            let expanded = expand_user_path(&raw_override.to_string_lossy())
                .unwrap_or_else(|| PathBuf::from(&raw_override));
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

struct ProviderContextScope;

impl ProviderContextScope {
    fn push(ctx: crate::services::provider_cli::ProviderExecutionContext) -> Self {
        ACTIVE_PROVIDER_CONTEXTS.with(|contexts| contexts.borrow_mut().push(ctx));
        Self
    }
}

impl Drop for ProviderContextScope {
    fn drop(&mut self) {
        ACTIVE_PROVIDER_CONTEXTS.with(|contexts| {
            contexts.borrow_mut().pop();
        });
    }
}

pub fn with_provider_execution_context<T>(
    ctx: crate::services::provider_cli::ProviderExecutionContext,
    run: impl FnOnce() -> T,
) -> T {
    let _scope = ProviderContextScope::push(ctx);
    run()
}

fn active_provider_context(
    provider: &str,
) -> Option<crate::services::provider_cli::ProviderExecutionContext> {
    ACTIVE_PROVIDER_CONTEXTS.with(|contexts| {
        contexts
            .borrow()
            .iter()
            .rev()
            .find(|ctx| ctx.provider.eq_ignore_ascii_case(provider))
            .cloned()
    })
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

fn drain_limited_output<R>(mut reader: R) -> Vec<u8>
where
    R: Read + Send + 'static,
{
    let mut output = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let remaining = VERSION_PROBE_MAX_OUTPUT_BYTES.saturating_sub(output.len());
                if remaining > 0 {
                    output.extend_from_slice(&buf[..n.min(remaining)]);
                }
            }
            Err(_) => break,
        }
    }
    output
}

pub fn probe_resolved_binary_version(
    binary_path: impl AsRef<OsStr>,
    resolution: &BinaryResolution,
) -> (Option<String>, Option<String>) {
    let mut command = Command::new(binary_path);
    apply_binary_resolution(&mut command, resolution);
    command.arg("--version");
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return (None, Some("permission_denied".to_string()));
        }
        Err(_) => return (None, Some("version_probe_spawn_failed".to_string())),
    };
    let stdout_reader = child
        .stdout
        .take()
        .map(|reader| std::thread::spawn(move || drain_limited_output(reader)));
    let stderr_reader = child
        .stderr
        .take()
        .map(|reader| std::thread::spawn(move || drain_limited_output(reader)));

    let deadline = Instant::now() + VERSION_PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                drop(stderr_reader);
                drop(stdout_reader);
                return (None, Some("version_probe_timeout".to_string()));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                drop(stderr_reader);
                drop(stdout_reader);
                return (None, Some("version_probe_failed".to_string()));
            }
        }
    };

    let stdout = stdout_reader
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    let _ = stderr_reader.and_then(|reader| reader.join().ok());

    if status.success() {
        let stdout = String::from_utf8_lossy(&stdout).trim().to_string();
        if stdout.is_empty() {
            (None, Some("version_probe_empty".to_string()))
        } else {
            (Some(stdout), None)
        }
    } else {
        (None, Some("version_probe_failed".to_string()))
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

fn push_env_dir(
    env_name: &str,
    suffix: Option<&str>,
    entries: &mut Vec<PathBuf>,
    seen: &mut BTreeSet<String>,
) {
    let Some(value) = std::env::var_os(env_name).filter(|value| !os_value_is_empty(value)) else {
        return;
    };
    let mut path =
        expand_user_path(&value.to_string_lossy()).unwrap_or_else(|| PathBuf::from(&value));
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

/// Context-aware resolver (PR-2).
///
/// Resolution order:
/// 1. Per-agent channel override in registry (`agent_overrides`)
/// 2. Current registry channel
/// 3. Legacy env-override / PATH / login-shell / fallback (unchanged behaviour)
pub fn resolve_provider_binary_for_context(
    ctx: &crate::services::provider_cli::ProviderExecutionContext,
) -> BinaryResolution {
    let provider = normalize_name(&ctx.provider);
    if let Some(root) = crate::config::runtime_root() {
        if let Ok(Some(registry)) = crate::services::provider_cli::io::load_registry(&root) {
            if let Some(channels) = registry.providers.get(&provider) {
                // 1. Per-agent override → named channel
                let channel_name = ctx
                    .agent_id
                    .as_deref()
                    .and_then(|id| registry.agent_channel(&provider, id))
                    .unwrap_or("current");

                let selected_channel = match channel_name {
                    "candidate" => channels.candidate.as_ref(),
                    "default" => channels.default.as_ref(),
                    "previous" => channels.previous.as_ref(),
                    _ => channels.current.as_ref(),
                };

                if let Some(channel) = selected_channel {
                    if let Some(resolution) =
                        registry_channel_resolution(ctx, &provider, channel_name, channel)
                    {
                        return resolution;
                    }
                }

                if channel_name != "current" {
                    if let Some(channel) = channels.current.as_ref() {
                        if let Some(resolution) =
                            registry_channel_resolution(ctx, &provider, "current", channel)
                        {
                            return resolution;
                        }
                    }
                }
            }
        }
    }
    // 3. Fall back to legacy resolver.
    resolve_provider_binary_legacy(&provider)
}

fn registry_channel_resolution(
    ctx: &crate::services::provider_cli::ProviderExecutionContext,
    requested_binary: &str,
    channel_name: &str,
    channel: &crate::services::provider_cli::ProviderCliChannel,
) -> Option<BinaryResolution> {
    let cwd = current_dir_fallback();
    let expanded = expand_user_path(&channel.path).unwrap_or_else(|| PathBuf::from(&channel.path));
    let resolved_path = resolve_candidate_path(&expanded, &cwd).ok()?;
    let resolution = finalize_resolution(
        requested_binary.to_string(),
        resolved_path,
        format!("registry:{channel_name}"),
        vec![format!("registry:{channel_name}=found:{}", channel.path)],
    );
    record_context_launch_artifact(ctx, &resolution, channel_name, &channel.version);
    Some(resolution)
}

fn record_context_launch_artifact(
    ctx: &crate::services::provider_cli::ProviderExecutionContext,
    resolution: &BinaryResolution,
    channel_name: &str,
    cli_version: &str,
) {
    let Some(session_key) = ctx
        .session_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    let Some(root) = crate::config::runtime_root() else {
        return;
    };
    let Some(cli_path) = resolution.resolved_path.clone() else {
        return;
    };
    let canonical_path = resolution
        .canonical_path
        .clone()
        .unwrap_or_else(|| cli_path.clone());

    let artifact = crate::services::provider_cli::LaunchArtifact {
        provider: resolution.requested_binary.clone(),
        agent_id: ctx.agent_id.clone(),
        channel_id: ctx.channel_id.clone(),
        session_key: Some(session_key.to_string()),
        channel: channel_name.to_string(),
        cli_path,
        canonical_path,
        cli_version: cli_version.to_string(),
        process_id: None,
        tmux_session: ctx.tmux_session.clone(),
        launched_at: chrono::Utc::now(),
    };

    let _ = crate::services::provider_cli::io::save_launch_artifact(&root, &artifact);
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    fn env_guard() -> MutexGuard<'static, ()> {
        crate::services::discord::runtime_store::lock_test_env()
    }

    struct RuntimeRootOverrideGuard {
        previous: Option<PathBuf>,
    }

    impl RuntimeRootOverrideGuard {
        fn set(path: &Path) -> Self {
            let previous = crate::config::current_test_runtime_root_override();
            crate::config::set_test_runtime_root_override(Some(path.to_path_buf()));
            Self { previous }
        }
    }

    impl Drop for RuntimeRootOverrideGuard {
        fn drop(&mut self) {
            crate::config::set_test_runtime_root_override(self.previous.take());
        }
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

    fn registry_channel(path: &Path) -> crate::services::provider_cli::ProviderCliChannel {
        crate::services::provider_cli::ProviderCliChannel {
            path: path.to_string_lossy().to_string(),
            canonical_path: std::fs::canonicalize(path)
                .unwrap()
                .to_string_lossy()
                .to_string(),
            version: "test-version".to_string(),
            version_output: None,
            source: "test".to_string(),
            checked_at: chrono::Utc::now(),
            evidence: Default::default(),
        }
    }

    #[test]
    fn resolve_binary_finds_known_tool() {
        let _guard = env_guard();
        #[cfg(unix)]
        assert!(resolve_binary("ls").is_some());
        #[cfg(windows)]
        assert!(resolve_binary("cmd.exe").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn scoped_context_prefers_agent_override_over_discord_channel_name() {
        let _guard = env_guard();
        let root = tempfile::tempdir().unwrap();
        let _root_guard = RuntimeRootOverrideGuard::set(root.path());
        let bin_dir = root.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let current = bin_dir.join("codex-current");
        let candidate = bin_dir.join("codex-candidate");
        write_executable(&current);
        write_executable(&candidate);

        let mut registry = crate::services::provider_cli::registry::ProviderCliRegistry::default();
        let mut channels = crate::services::provider_cli::registry::ProviderChannels {
            current: Some(registry_channel(&current)),
            candidate: Some(registry_channel(&candidate)),
            ..Default::default()
        };
        channels
            .agent_overrides
            .insert("codex-agent".to_string(), "candidate".to_string());
        registry.providers.insert("codex".to_string(), channels);
        crate::services::provider_cli::io::save_registry(root.path(), &registry).unwrap();

        let ctx = crate::services::provider_cli::ProviderExecutionContext {
            provider: "codex".to_string(),
            agent_id: Some("codex-agent".to_string()),
            channel_id: Some("123".to_string()),
            session_key: Some("session-1".to_string()),
            tmux_session: Some("agentdesk-codex-agent-sandbox".to_string()),
            channel_name: Some("agent-sandbox".to_string()),
            execution_mode: Some("discord_turn".to_string()),
        };

        let resolution = with_provider_execution_context(ctx, || resolve_provider_binary("codex"));

        assert_eq!(resolution.source.as_deref(), Some("registry:candidate"));
        assert_eq!(
            resolution.resolved_path.as_deref(),
            Some(candidate.to_string_lossy().as_ref())
        );
        let artifact =
            crate::services::provider_cli::io::load_launch_artifact(root.path(), "session-1")
                .unwrap()
                .unwrap();
        assert_eq!(artifact.provider, "codex");
        assert_eq!(artifact.agent_id.as_deref(), Some("codex-agent"));
        assert_eq!(artifact.channel, "candidate");
        assert_eq!(
            artifact.tmux_session.as_deref(),
            Some("agentdesk-codex-agent-sandbox")
        );
        assert_eq!(
            artifact.canonical_path,
            std::fs::canonicalize(&candidate)
                .unwrap()
                .to_string_lossy()
                .to_string()
        );
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
    fn context_resolver_normalizes_provider_for_registry_lookup() {
        use crate::services::provider_cli::registry::{
            ProviderChannels, ProviderCliChannel, ProviderCliRegistry,
        };
        use chrono::Utc;

        let _guard = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let _runtime_root = RuntimeRootOverrideGuard::set(temp.path());
        let registry_path = temp.path().join("registry-codex");
        write_executable(&registry_path);

        let mut registry = ProviderCliRegistry::default();
        registry.providers.insert(
            "codex".to_string(),
            ProviderChannels {
                current: Some(ProviderCliChannel {
                    path: registry_path.to_string_lossy().to_string(),
                    canonical_path: registry_path.to_string_lossy().to_string(),
                    version: "registry".to_string(),
                    version_output: None,
                    source: "test".to_string(),
                    checked_at: Utc::now(),
                    evidence: Default::default(),
                }),
                ..Default::default()
            },
        );
        crate::services::provider_cli::io::save_registry(temp.path(), &registry).unwrap();

        let resolution = resolve_provider_binary_for_context(
            &crate::services::provider_cli::ProviderExecutionContext::for_provider("CoDeX"),
        );

        assert_eq!(
            resolution.resolved_path.as_deref(),
            Some(registry_path.to_string_lossy().as_ref())
        );
        assert_eq!(resolution.source.as_deref(), Some("registry:current"));
    }

    #[cfg(unix)]
    #[test]
    fn context_resolver_falls_back_when_registry_path_is_not_executable() {
        use crate::services::provider_cli::registry::{
            ProviderChannels, ProviderCliChannel, ProviderCliRegistry,
        };
        use chrono::Utc;
        use std::os::unix::fs::PermissionsExt;

        let _guard = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let _runtime_root = RuntimeRootOverrideGuard::set(temp.path());
        let invalid_registry_path = temp.path().join("registry-codex");
        let fallback_path = temp.path().join("env-codex");
        std::fs::write(&invalid_registry_path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&invalid_registry_path)
            .unwrap()
            .permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&invalid_registry_path, perms).unwrap();
        write_executable(&fallback_path);

        let mut registry = ProviderCliRegistry::default();
        registry.providers.insert(
            "codex".to_string(),
            ProviderChannels {
                current: Some(ProviderCliChannel {
                    path: invalid_registry_path.to_string_lossy().to_string(),
                    canonical_path: invalid_registry_path.to_string_lossy().to_string(),
                    version: "registry".to_string(),
                    version_output: None,
                    source: "test".to_string(),
                    checked_at: Utc::now(),
                    evidence: Default::default(),
                }),
                ..Default::default()
            },
        );
        crate::services::provider_cli::io::save_registry(temp.path(), &registry).unwrap();

        unsafe {
            std::env::set_var("AGENTDESK_CODEX_PATH", &fallback_path);
        }
        let resolution = resolve_provider_binary_for_context(
            &crate::services::provider_cli::ProviderExecutionContext::for_provider("codex"),
        );

        assert_eq!(
            resolution.resolved_path.as_deref(),
            Some(fallback_path.to_string_lossy().as_ref())
        );
        assert_eq!(resolution.source.as_deref(), Some("env_override"));

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

    #[cfg(unix)]
    #[test]
    fn version_probe_caps_large_stdout() {
        let _guard = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("codex");
        write_executable_with_contents(
            &provider,
            "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 1000 ]; do printf 'abcdefghijklmnopqrst'; i=$((i + 1)); done\n",
        );
        let resolution = BinaryResolution {
            requested_binary: "codex".to_string(),
            resolved_path: Some(provider.to_string_lossy().to_string()),
            canonical_path: None,
            source: Some("env_override".to_string()),
            attempts: Vec::new(),
            failure_kind: None,
            exec_path: None,
        };

        let (version, error) = probe_resolved_binary_version(&provider, &resolution);

        assert_eq!(error, None);
        let version = version.expect("version stdout should be captured");
        assert_eq!(version.len(), VERSION_PROBE_MAX_OUTPUT_BYTES);
        assert!(version.starts_with("abcdefghijklmnopqrst"));
    }
}
