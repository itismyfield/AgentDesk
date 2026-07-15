use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use crate::services::claude_gateway_proxy::ClaudeGatewayProxyEnv;
use crate::services::claude_tui::hook_bundle::{HookBundleConfig, write_claude_hook_settings};
use crate::services::process::shell_escape;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeTuiSessionFiles {
    pub hook_settings_path: PathBuf,
    pub launch_script_path: PathBuf,
}

impl ClaudeTuiSessionFiles {
    pub fn cleanup_best_effort(&self) {
        let _ = std::fs::remove_file(&self.hook_settings_path);
        let _ = std::fs::remove_file(&self.launch_script_path);
    }
}

#[derive(Debug)]
struct PersistedContinuationSession {
    session_id: String,
    recorded_at: Instant,
}

static PERSISTED_CONTINUATION_SESSIONS: LazyLock<
    Mutex<HashMap<String, PersistedContinuationSession>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));
const PERSISTED_CONTINUATION_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

fn continuation_persistence_cached(tmux_session_name: &str, session_id: &str) -> bool {
    let now = Instant::now();
    let mut persisted = PERSISTED_CONTINUATION_SESSIONS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    persisted.retain(|_, entry| {
        now.duration_since(entry.recorded_at) <= PERSISTED_CONTINUATION_CACHE_TTL
    });
    persisted
        .get(tmux_session_name)
        .is_some_and(|entry| entry.session_id == session_id)
}

fn remember_persisted_continuation(tmux_session_name: &str, session_id: &str) {
    PERSISTED_CONTINUATION_SESSIONS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(
            tmux_session_name.to_string(),
            PersistedContinuationSession {
                session_id: session_id.to_string(),
                recorded_at: Instant::now(),
            },
        );
}

/// Persist a Claude continuation cutover into the AgentDesk-owned artifacts
/// that survive dcserver restarts (#4423).
///
/// Claude can keep the live TUI pane while changing the provider session UUID.
/// Updating only the in-memory transcript binding lets the launch script's old
/// UUID win again after rehydration. Both artifacts are therefore repaired:
/// the launch script selects the new transcript on the next pane restart, and
/// the hook settings address the same identity. A partially completed repair is
/// accepted and converges on the next call.
pub(crate) fn persist_claude_continuation_session(
    tmux_session_name: &str,
    new_session_id: &str,
) -> Result<bool, String> {
    let tmux_session_name = tmux_session_name.trim();
    let new_session_id = new_session_id.trim();
    if tmux_session_name.is_empty() {
        return Err("Claude continuation tmux session name is required".to_string());
    }
    if uuid::Uuid::parse_str(new_session_id).is_err() {
        return Err("Claude continuation session id must be a UUID".to_string());
    }
    if continuation_persistence_cached(tmux_session_name, new_session_id) {
        return Ok(false);
    }
    let hook_settings_path = crate::services::tmux_common::resolve_session_temp_path(
        tmux_session_name,
        crate::services::tmux_common::CLAUDE_TUI_HOOK_SETTINGS_TEMP_EXT,
    )
    .ok_or_else(|| format!("missing Claude hook settings for {tmux_session_name}"))?;
    let launch_script_path = crate::services::tmux_common::resolve_session_temp_path(
        tmux_session_name,
        crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
    )
    .ok_or_else(|| format!("missing Claude launch script for {tmux_session_name}"))?;
    let changed = persist_claude_continuation_session_files(
        &ClaudeTuiSessionFiles {
            hook_settings_path: PathBuf::from(hook_settings_path),
            launch_script_path: PathBuf::from(launch_script_path),
        },
        new_session_id,
    )?;
    // Cache only a verified success. Any read/parse/write/rollback failure is
    // deliberately left uncached so the next mismatched hook retries it.
    remember_persisted_continuation(tmux_session_name, new_session_id);
    Ok(changed)
}

fn uuid_after(content: &str, marker: &str) -> Vec<String> {
    let mut remainder = content;
    let mut values = Vec::new();
    while let Some(index) = remainder.find(marker) {
        remainder = &remainder[index + marker.len()..];
        remainder = remainder.strip_prefix('\'').unwrap_or(remainder);
        let value = &remainder[..remainder
            .find(|character: char| !(character.is_ascii_hexdigit() || character == '-'))
            .unwrap_or(remainder.len())];
        if uuid::Uuid::parse_str(value).is_ok() && !values.iter().any(|existing| existing == value)
        {
            values.push(value.to_string());
        }
        remainder = &remainder[value.len()..];
    }
    values
}

fn single_artifact_session_id(
    label: &str,
    content: &str,
    markers: &[&str],
) -> Result<String, String> {
    let mut values = Vec::new();
    for marker in markers {
        for value in uuid_after(content, marker) {
            if !values.contains(&value) {
                values.push(value);
            }
        }
    }
    match values.as_slice() {
        [value] => Ok(value.clone()),
        [] => Err(format!(
            "{label} contains no generated Claude session selector"
        )),
        _ => Err(format!(
            "{label} contains multiple Claude session selectors"
        )),
    }
}

fn atomic_replace_preserving_permissions(path: &Path, data: &str) -> Result<(), String> {
    let permissions = fs::metadata(path)
        .map_err(|error| format!("stat {}: {error}", path.display()))?
        .permissions();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temp_path = path.with_file_name(format!(
        ".{file_name}.{}.continuation.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|error| format!("create {}: {error}", temp_path.display()))?;
        file.write_all(data.as_bytes())
            .map_err(|error| format!("write {}: {error}", temp_path.display()))?;
        file.sync_all()
            .map_err(|error| format!("sync {}: {error}", temp_path.display()))?;
        fs::set_permissions(&temp_path, permissions)
            .map_err(|error| format!("chmod {}: {error}", temp_path.display()))?;
        replace_existing_file(&temp_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

#[cfg(unix)]
fn replace_existing_file(temp_path: &Path, path: &Path) -> Result<(), String> {
    fs::rename(temp_path, path).map_err(|error| format!("replace {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn replace_existing_file(temp_path: &Path, path: &Path) -> Result<(), String> {
    // Claude TUI hosting requires tmux and is Unix-only. Keep tests and shared
    // compilation functional on Windows, where std::fs::rename cannot replace
    // an existing file, even though that platform cannot execute this path.
    fs::remove_file(path).map_err(|error| format!("remove {}: {error}", path.display()))?;
    fs::rename(temp_path, path).map_err(|error| format!("replace {}: {error}", path.display()))
}

fn persist_claude_continuation_session_files(
    files: &ClaudeTuiSessionFiles,
    new_session_id: &str,
) -> Result<bool, String> {
    let original_launch = fs::read_to_string(&files.launch_script_path).map_err(|error| {
        format!(
            "read Claude launch script {}: {error}",
            files.launch_script_path.display()
        )
    })?;
    let original_settings = fs::read_to_string(&files.hook_settings_path).map_err(|error| {
        format!(
            "read Claude hook settings {}: {error}",
            files.hook_settings_path.display()
        )
    })?;
    let launch_session_id = single_artifact_session_id(
        "Claude launch script",
        &original_launch,
        &["'--session-id' ", "'--resume' "],
    )?;
    serde_json::from_str::<serde_json::Value>(&original_settings)
        .map_err(|error| format!("Claude hook settings are invalid JSON: {error}"))?;
    let settings_session_id = single_artifact_session_id(
        "Claude hook settings",
        &original_settings,
        &["--session-id "],
    )?;
    // A process crash can leave either artifact one continuation hop ahead of
    // the other. The caller has already authenticated the live payload against
    // the tmux binding, so converge each generated selector independently.
    let launch = (launch_session_id != new_session_id)
        .then(|| original_launch.replace(&launch_session_id, new_session_id));
    let settings = (settings_session_id != new_session_id)
        .then(|| original_settings.replace(&settings_session_id, new_session_id));
    if launch.is_none() && settings.is_none() {
        return Ok(false);
    }
    if let Some(updated) = settings.as_deref() {
        serde_json::from_str::<serde_json::Value>(updated)
            .map_err(|error| format!("updated Claude hook settings are invalid JSON: {error}"))?;
    }
    if let Some(updated) = launch.as_deref()
        && (!updated.contains(new_session_id) || updated.contains(&launch_session_id))
    {
        return Err("updated Claude launch script failed session-id validation".to_string());
    }

    // Repair the launch selector first. If the settings write then fails, put
    // the original launch script back; either partial ordering is also safe at
    // runtime because hook_server routes a mismatched payload through whichever
    // provider UUID is currently registered.
    if let Some(updated) = launch.as_deref() {
        atomic_replace_preserving_permissions(&files.launch_script_path, updated)?;
    }
    if let Some(updated) = settings.as_deref()
        && let Err(error) =
            atomic_replace_preserving_permissions(&files.hook_settings_path, updated)
    {
        if launch.is_some()
            && let Err(rollback_error) =
                atomic_replace_preserving_permissions(&files.launch_script_path, &original_launch)
        {
            return Err(format!(
                "{error}; additionally failed to roll back Claude launch script: {rollback_error}"
            ));
        }
        return Err(error);
    }
    Ok(true)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeTuiLaunchConfig {
    pub tmux_session_name: String,
    pub working_dir: PathBuf,
    pub claude_bin: PathBuf,
    pub agentdesk_exe: PathBuf,
    pub hook_endpoint: String,
    pub session_id: String,
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    pub resume: bool,
    /// #3166: provider-specific auto-compact threshold
    /// (`context_compact_percent_claude`), resolved by the caller via
    /// `fetch_context_thresholds`. When `Some(pct)` with `pct > 0` the launch
    /// script exports `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=pct` so BOTH fresh and
    /// `--resume` TUI spawns honour the configured override. Mirrors the
    /// `p > 0` guard used by the non-TUI tmux/process spawn paths.
    pub compact_percent: Option<u64>,
    pub(crate) gateway_proxy_env: ClaudeGatewayProxyEnv,
}

impl ClaudeTuiLaunchConfig {
    pub fn session_files(&self) -> ClaudeTuiSessionFiles {
        ClaudeTuiSessionFiles {
            hook_settings_path: PathBuf::from(crate::services::tmux_common::session_temp_path(
                &self.tmux_session_name,
                crate::services::tmux_common::CLAUDE_TUI_HOOK_SETTINGS_TEMP_EXT,
            )),
            launch_script_path: PathBuf::from(crate::services::tmux_common::session_temp_path(
                &self.tmux_session_name,
                crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
            )),
        }
    }
}

pub fn prepare_claude_tui_launch(
    config: &ClaudeTuiLaunchConfig,
) -> Result<ClaudeTuiSessionFiles, String> {
    let files = config.session_files();
    let result = (|| {
        crate::services::tui_prompt_dedupe::register_provider_session(
            "claude",
            &config.session_id,
            &config.tmux_session_name,
        );
        write_claude_hook_settings(
            &files.hook_settings_path,
            &HookBundleConfig {
                endpoint: config.hook_endpoint.clone(),
                provider: "claude".to_string(),
                session_id: config.session_id.clone(),
                agentdesk_exe: config.agentdesk_exe.display().to_string(),
            },
        )?;
        write_launch_script(&files.launch_script_path, config, &files.hook_settings_path)
    })();
    if let Err(error) = result {
        files.cleanup_best_effort();
        return Err(error);
    }
    Ok(files)
}

pub fn build_claude_tui_args(
    config: &ClaudeTuiLaunchConfig,
    hook_settings_path: &Path,
) -> Vec<String> {
    let mut args = vec!["--dangerously-skip-permissions".to_string()];
    if config.resume {
        args.push("--resume".to_string());
    } else {
        args.push("--session-id".to_string());
    }
    args.push(config.session_id.clone());

    args.push("--settings".to_string());
    args.push(hook_settings_path.display().to_string());

    if let Some(model) = config.model.as_deref().filter(|value| !value.is_empty()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(system_prompt) = config
        .system_prompt
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        args.push("--append-system-prompt".to_string());
        args.push(system_prompt.to_string());
    }
    args
}

fn write_launch_script(
    path: &Path,
    config: &ClaudeTuiLaunchConfig,
    hook_settings_path: &Path,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!("create TUI launch script dir {}: {error}", parent.display())
        })?;
    }
    let args = build_claude_tui_args(config, hook_settings_path)
        .into_iter()
        .map(|arg| shell_escape(&arg))
        .collect::<Vec<_>>()
        .join(" ");
    // Neutralise the Claude CLI's auto-resume prompt
    // (PY6 = "Continue from where you left off.") that gets prepended
    // every time the TUI deserialises a transcript whose last user
    // message ended that turn.
    //
    // The CLI reads this via `process.env.CLAUDE_CODE_RESUME_PROMPT || …`.
    // Setting the variable to an empty string is silently ignored
    // (JavaScript `||` treats `""` as falsy), so we previously used a
    // single space (#2719) — truthy enough to bypass the fallback and
    // visually invisible.
    //
    // #2730 follow-up: the single-space value backfired. Claude CLI
    // *submits* the resume-prompt verbatim as a user turn on every
    // resume, and the JSONL transcript records it as
    // `{"type":"text","text":" "}`. Anthropic's API then rejects every
    // subsequent request with `400 messages: text content blocks must
    // contain non-whitespace text` because the cached conversation
    // history now contains a whitespace-only block. Reproduced reliably
    // by E-17 (SETUP turn after a `--resume` spawn always returned 400).
    //
    // We need a value that is (1) JS-truthy, (2) non-whitespace per the
    // Anthropic API's content-block validator, and (3) visually minimal.
    // Use an ASCII underscore `_` — single byte, single character,
    // unambiguously non-whitespace.
    //
    // TODO(#2718-upstream): if the Claude CLI moves PY6 from `||` to
    // nullish coalescing (`??`) or exposes a flag to skip the auto-resume
    // prompt entirely, the placeholder semantics here should be
    // revisited. Track upstream PY6 changes and reflect them in this
    // comment + the unit tests.
    // #3166: export the configured Claude auto-compact override so TUI-hosted
    // sessions (fresh AND `--resume`) honour `context_compact_percent_claude`.
    // Gated on `pct > 0`, mirroring the non-TUI spawn paths
    // (`build_tmux_launch_env_lines` / `execute_streaming_local_process`); a 0
    // or absent value leaves the var unset so the Claude CLI keeps its default.
    let compact_export = match config.compact_percent.filter(|&pct| pct > 0) {
        Some(pct) => format!("export CLAUDE_AUTOCOMPACT_PCT_OVERRIDE={pct}\n"),
        None => String::new(),
    };
    let mut gateway_exports = String::new();
    config
        .gateway_proxy_env
        .append_shell_env(&mut gateway_exports);
    let script = format!(
        "#!/bin/bash\n\
         cd {cwd}\n\
         export CLAUDE_CODE_RESUME_PROMPT=\"_\"\n\
         {compact_export}\
         {gateway_exports}\
         exec {claude_bin} {args}\n",
        cwd = shell_escape(&config.working_dir.display().to_string()),
        claude_bin = shell_escape(&config.claude_bin.display().to_string()),
        compact_export = compact_export,
        gateway_exports = gateway_exports,
        args = args,
    );
    std::fs::write(path, script)
        .map_err(|error| format!("write TUI launch script {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> ClaudeTuiLaunchConfig {
        ClaudeTuiLaunchConfig {
            tmux_session_name: "AgentDesk-claude-test".to_string(),
            working_dir: PathBuf::from("/tmp/project dir"),
            claude_bin: PathBuf::from("/usr/local/bin/claude"),
            agentdesk_exe: PathBuf::from("/usr/local/bin/agentdesk"),
            hook_endpoint: "http://127.0.0.1:49152".to_string(),
            session_id: "01234567-89ab-cdef-0123-456789abcdef".to_string(),
            system_prompt: Some("system prompt".to_string()),
            model: Some("sonnet".to_string()),
            resume: false,
            compact_percent: None,
            gateway_proxy_env: crate::services::claude_gateway_proxy::launch_env_for_test(
                false,
                "http://127.0.0.1:10100",
                true,
            ),
        }
    }

    #[test]
    fn tui_args_do_not_use_print_mode() {
        let config = sample_config();
        let args = build_claude_tui_args(&config, Path::new("/tmp/settings.json"));

        assert!(!args.iter().any(|arg| arg == "-p" || arg == "--print"));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--session-id", "01234567-89ab-cdef-0123-456789abcdef"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--settings", "/tmp/settings.json"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--model", "sonnet"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--append-system-prompt", "system prompt"])
        );
    }

    #[test]
    fn tui_args_resume_existing_session_by_id() {
        let mut config = sample_config();
        config.resume = true;

        let args = build_claude_tui_args(&config, Path::new("/tmp/settings.json"));

        assert!(
            args.windows(2)
                .any(|pair| pair == ["--resume", "01234567-89ab-cdef-0123-456789abcdef"])
        );
        assert!(!args.iter().any(|arg| arg == "--session-id"));
    }

    #[test]
    fn tui_args_omit_model_when_provider_default_is_requested() {
        let mut config = sample_config();
        config.model = None;

        let args = build_claude_tui_args(&config, Path::new("/tmp/settings.json"));

        assert!(!args.iter().any(|arg| arg == "--model"));
    }

    #[test]
    fn prepare_launch_writes_settings_and_script() {
        let dir = tempfile::tempdir().unwrap();
        let config = sample_config();
        let hook_settings_path = dir.path().join("settings.json");
        let launch_script_path = dir.path().join("launch.sh");

        write_claude_hook_settings(
            &hook_settings_path,
            &HookBundleConfig {
                endpoint: config.hook_endpoint.clone(),
                provider: "claude".to_string(),
                session_id: config.session_id.clone(),
                agentdesk_exe: config.agentdesk_exe.display().to_string(),
            },
        )
        .unwrap();
        write_launch_script(&launch_script_path, &config, &hook_settings_path).unwrap();

        let settings = std::fs::read_to_string(&hook_settings_path).unwrap();
        let script = std::fs::read_to_string(&launch_script_path).unwrap();
        assert!(settings.contains("claude-hook-relay"));
        assert!(script.contains("exec '/usr/local/bin/claude'"));
        assert!(!script.contains(" -p "));
        // Neutralise the Claude CLI's auto-resume prompt
        // (PY6 = "Continue from where you left off.") so it does not
        // re-prepend a steering meta user message every time the TUI
        // deserialises its transcript at turn start.
        //
        // #2718: the env var must be a *truthy* placeholder. Setting it
        // to "" is silently ignored by PY6 because of its
        // `process.env.CLAUDE_CODE_RESUME_PROMPT || "Continue..."` pattern
        // — empty strings are falsy in JS and the default fallback wins.
        //
        // #2730 follow-up: the placeholder must ALSO be non-whitespace
        // per the Anthropic API's content-block validator. A single
        // space (the previous choice) was truthy but whitespace-only,
        // and Claude CLI submits the resume prompt verbatim — leaving a
        // whitespace block in the transcript history that the API then
        // rejects with `400 messages: text content blocks must contain
        // non-whitespace text` on every subsequent request. Use `_` so
        // the recorded turn satisfies both the JS truthy check and the
        // API's non-whitespace check.
        assert!(
            script.contains("export CLAUDE_CODE_RESUME_PROMPT=\"_\""),
            "launch script must export CLAUDE_CODE_RESUME_PROMPT to a JS-truthy AND non-whitespace placeholder so the API does not reject subsequent turns"
        );
        assert!(
            !script.contains("export CLAUDE_CODE_RESUME_PROMPT=\"\""),
            "empty-string placeholder is falsy and silently ignored by PY6"
        );
        assert!(
            !script.contains("export CLAUDE_CODE_RESUME_PROMPT=\" \""),
            "single-space placeholder is truthy but whitespace-only; #2730 reproduced this poisoning the API conversation history"
        );
    }

    #[test]
    fn launch_script_exports_compact_and_gates_gateway_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = sample_config();
        config.compact_percent = Some(60);
        config.gateway_proxy_env = crate::services::claude_gateway_proxy::launch_env_for_test(
            true,
            "http://proxy.example/it's-ready",
            true,
        );
        let launch_script_path = dir.path().join("launch.sh");

        write_launch_script(
            &launch_script_path,
            &config,
            Path::new("/tmp/settings.json"),
        )
        .unwrap();

        let script = std::fs::read_to_string(&launch_script_path).unwrap();
        assert!(script.contains("export CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=60\n"));
        assert!(
            script.contains("export ANTHROPIC_BASE_URL='http://proxy.example/it'\\''s-ready'\n")
        );
        assert!(script.contains("export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1\n"));
        assert!(!script.contains("CLAUDE_CODE_EXTENDED_CACHE_TTL"));
        let export_pos = script.find("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE").unwrap();
        let exec_pos = script.find("exec ").unwrap();
        assert!(
            export_pos < exec_pos,
            "compact override must be exported before exec"
        );

        config.gateway_proxy_env = crate::services::claude_gateway_proxy::launch_env_for_test(
            false,
            "http://foreign.example",
            true,
        );
        write_launch_script(
            &launch_script_path,
            &config,
            Path::new("/tmp/settings.json"),
        )
        .unwrap();
        let disabled_script = std::fs::read_to_string(&launch_script_path).unwrap();
        assert!(disabled_script.contains("export CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=60\n"));
        assert!(disabled_script.contains("unset ANTHROPIC_BASE_URL\n"));
        assert!(disabled_script.contains("unset CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY\n"));
    }

    #[test]
    fn launch_script_resume_exports_compact_override() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = sample_config();
        config.resume = true;
        config.compact_percent = Some(60);
        let launch_script_path = dir.path().join("launch.sh");

        write_launch_script(
            &launch_script_path,
            &config,
            Path::new("/tmp/settings.json"),
        )
        .unwrap();

        let script = std::fs::read_to_string(&launch_script_path).unwrap();
        assert!(script.contains("export CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=60\n"));
    }

    #[test]
    fn launch_script_omits_compact_override_when_absent_or_zero() {
        let dir = tempfile::tempdir().unwrap();
        for value in [None, Some(0)] {
            let mut config = sample_config();
            config.compact_percent = value;
            let launch_script_path = dir.path().join("launch.sh");
            write_launch_script(
                &launch_script_path,
                &config,
                Path::new("/tmp/settings.json"),
            )
            .unwrap();
            let script = std::fs::read_to_string(&launch_script_path).unwrap();
            assert!(
                !script.contains("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"),
                "value {value:?} must not export the override, got:\n{script}"
            );
        }
    }

    #[test]
    fn session_files_cleanup_best_effort_removes_settings_and_script() {
        let dir = tempfile::tempdir().unwrap();
        let files = ClaudeTuiSessionFiles {
            hook_settings_path: dir.path().join("settings.json"),
            launch_script_path: dir.path().join("launch.sh"),
        };
        std::fs::write(&files.hook_settings_path, "{}").unwrap();
        std::fs::write(&files.launch_script_path, "#!/bin/bash\n").unwrap();

        files.cleanup_best_effort();
        files.cleanup_best_effort();

        assert!(!files.hook_settings_path.exists());
        assert!(!files.launch_script_path.exists());
    }

    #[test]
    fn successful_continuation_persistence_is_cached_per_target_uuid() {
        let tmux = format!("tmux-4423-persist-cache-{}", uuid::Uuid::new_v4());
        let session = uuid::Uuid::new_v4().to_string();
        let later_session = uuid::Uuid::new_v4().to_string();
        assert!(!continuation_persistence_cached(&tmux, &session));
        remember_persisted_continuation(&tmux, &session);
        assert!(continuation_persistence_cached(&tmux, &session));
        assert!(
            !continuation_persistence_cached(&tmux, &later_session),
            "a later continuation UUID must still trigger artifact convergence"
        );
    }

    #[test]
    fn continuation_cutover_rewrites_both_artifacts_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let old_session_id = uuid::Uuid::new_v4().to_string();
        let new_session_id = uuid::Uuid::new_v4().to_string();
        let files = ClaudeTuiSessionFiles {
            hook_settings_path: dir.path().join("settings.json"),
            launch_script_path: dir.path().join("launch.sh"),
        };
        let mut config = sample_config();
        config.session_id = old_session_id.clone();
        write_claude_hook_settings(
            &files.hook_settings_path,
            &HookBundleConfig {
                endpoint: config.hook_endpoint.clone(),
                provider: "claude".to_string(),
                session_id: old_session_id.clone(),
                agentdesk_exe: config.agentdesk_exe.display().to_string(),
            },
        )
        .unwrap();
        write_launch_script(
            &files.launch_script_path,
            &config,
            &files.hook_settings_path,
        )
        .unwrap();

        assert!(persist_claude_continuation_session_files(&files, &new_session_id).unwrap());
        let launch = fs::read_to_string(&files.launch_script_path).unwrap();
        let settings = fs::read_to_string(&files.hook_settings_path).unwrap();
        assert!(!launch.contains(&old_session_id));
        assert!(launch.contains(&new_session_id));
        assert!(!settings.contains(&old_session_id));
        assert!(settings.contains(&new_session_id));
        serde_json::from_str::<serde_json::Value>(&settings).unwrap();
        assert!(
            !persist_claude_continuation_session_files(&files, &new_session_id).unwrap(),
            "an already durable cutover must not rewrite artifacts again"
        );

        let third_session_id = uuid::Uuid::new_v4().to_string();
        assert!(
            persist_claude_continuation_session_files(&files, &third_session_id).unwrap(),
            "a later continuation must advance from the artifact's current UUID, not the original query UUID"
        );
        let launch = fs::read_to_string(&files.launch_script_path).unwrap();
        let settings = fs::read_to_string(&files.hook_settings_path).unwrap();
        assert!(!launch.contains(&new_session_id));
        assert!(launch.contains(&third_session_id));
        assert!(!settings.contains(&new_session_id));
        assert!(settings.contains(&third_session_id));
    }

    #[test]
    fn continuation_cutover_repairs_a_partial_artifact_update() {
        let dir = tempfile::tempdir().unwrap();
        let old_session_id = uuid::Uuid::new_v4().to_string();
        let new_session_id = uuid::Uuid::new_v4().to_string();
        let files = ClaudeTuiSessionFiles {
            hook_settings_path: dir.path().join("settings.json"),
            launch_script_path: dir.path().join("launch.sh"),
        };
        fs::write(
            &files.launch_script_path,
            format!("exec claude '--resume' '{new_session_id}'\n"),
        )
        .unwrap();
        fs::write(
            &files.hook_settings_path,
            serde_json::to_string(&serde_json::json!({
                "command": format!("agentdesk claude-hook-relay --session-id '{old_session_id}'")
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(persist_claude_continuation_session_files(&files, &new_session_id).unwrap());
        assert!(
            fs::read_to_string(&files.hook_settings_path)
                .unwrap()
                .contains(&new_session_id)
        );
        assert!(
            fs::read_to_string(&files.launch_script_path)
                .unwrap()
                .contains(&new_session_id)
        );

        let later_session_id = uuid::Uuid::new_v4().to_string();
        fs::write(
            &files.hook_settings_path,
            serde_json::to_string(&serde_json::json!({
                "command": format!("agentdesk claude-hook-relay --session-id '{old_session_id}'")
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(
            persist_claude_continuation_session_files(&files, &later_session_id).unwrap(),
            "a later hop must converge even when a prior crash left two older selectors"
        );
        assert!(
            fs::read_to_string(&files.hook_settings_path)
                .unwrap()
                .contains(&later_session_id)
        );
        assert!(
            fs::read_to_string(&files.launch_script_path)
                .unwrap()
                .contains(&later_session_id)
        );
    }

    #[test]
    fn prepare_launch_cleans_settings_when_script_write_fails() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_host = std::env::var_os("HOSTNAME");
        let root = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", root.path());
            std::env::set_var("HOSTNAME", "issue-2143-host");
        }

        let mut config = sample_config();
        config.tmux_session_name = format!("issue-2143-{}", uuid::Uuid::new_v4());
        let files = config.session_files();
        std::fs::create_dir_all(files.launch_script_path.parent().unwrap()).unwrap();
        std::fs::create_dir(&files.launch_script_path).unwrap();

        let error = prepare_claude_tui_launch(&config).unwrap_err();

        assert!(error.contains("write TUI launch script"));
        assert!(
            !files.hook_settings_path.exists(),
            "prepare failure must not leave hook settings behind"
        );

        let _ = std::fs::remove_dir_all(&files.launch_script_path);
        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
        match previous_host {
            Some(value) => unsafe { std::env::set_var("HOSTNAME", value) },
            None => unsafe { std::env::remove_var("HOSTNAME") },
        }
    }
}
