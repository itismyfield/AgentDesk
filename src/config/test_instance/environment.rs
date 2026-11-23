use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use super::agy_cell::{self, AgyLogin};
use super::{checked_path, current, rejected, requested};

#[derive(Debug)]
pub(crate) struct Environment {
    api: String,
    claude_login: Option<ClaudeLogin>,
    agy_login: Option<AgyLogin>,
    #[cfg(unix)]
    pub(super) credentials: &'static [&'static str],
    pub(super) required: BTreeMap<String, String>,
    pub(super) flags: BTreeMap<String, String>,
}

impl Environment {
    pub(crate) fn load(root: &Path, manifest: &Value, config: &Value) -> Result<Self, String> {
        let id = manifest["instance_id"].as_str().ok_or_else(rejected)?;
        let port = manifest["api"]["port"].as_u64().ok_or_else(rejected)?;
        let api = format!("http://127.0.0.1:{port}");
        if !(18791..=18806).contains(&port)
            || manifest["api"]["host"] != "127.0.0.1"
            || manifest["api"]["self_url"] != api
            || config["server"]["host"] != "127.0.0.1"
            || config["server"]["port"] != port
            || config["mcp_servers"]
                .as_object()
                .is_none_or(|map| !map.is_empty())
            || config["memory"]["backend"] != "file"
            || config["memory"]["mcp"]["endpoint"] != ""
        {
            return Err(rejected());
        }
        let claude_login = ClaudeLogin::load(root, manifest)?;
        let agy_login = AgyLogin::load(root, manifest)?;
        let profile = agy_cell::profile(manifest);
        let mut required = BTreeMap::new();
        for (key, path) in [
            ("HOME", root.join("home")),
            ("TMPDIR", root.join("tmp")),
            ("XDG_CONFIG_HOME", root.join("xdg")),
            (
                "CLAUDE_CONFIG_DIR",
                claude_login
                    .as_ref()
                    .map(|v| v.config_dir.clone())
                    .unwrap_or(home_at(root, "claude")?),
            ),
            ("CODEX_HOME", home_at(root, "codex")?),
            ("AGENTDESK_ROOT_DIR", root.into()),
            (
                "AGENTDESK_CONFIG",
                crate::config::config_path::canonical_config_path(root),
            ),
            ("AGENTDESK_TEST_MANIFEST", root.join("manifest.json")),
            ("TMUX_TMPDIR", root.join("runtime/tmux")),
        ] {
            checked_path(&path)?;
            required.insert(key.into(), path.to_string_lossy().into_owned());
        }
        let worktree = Path::new(manifest["worktree"].as_str().ok_or_else(rejected)?);
        checked_path(worktree)?;
        for (key, value) in [
            ("AGENTDESK_API_URL", api.clone()),
            ("ADK_API_URL", api.clone()),
            (
                "AGENTDESK_DCSERVER_LABEL",
                format!("com.agentdesk.test.{id}"),
            ),
            (
                "AGENTDESK_RELEASE_TMUX_SESSION",
                format!("AdkTest-{id}-dcserver"),
            ),
            (
                "PATH",
                required_runtime_path(worktree, claude_login.is_some())?,
            ),
        ] {
            required.insert(key.into(), value);
        }
        if let Some(login) = &claude_login {
            required.insert(
                "AGENTDESK_CLAUDE_PATH".into(),
                login.cli.to_string_lossy().into_owned(),
            );
        }
        if let Some(login) = &agy_login {
            required.insert(
                "AGENTDESK_AGY_PATH".into(),
                login.cli.to_string_lossy().into_owned(),
            );
        }
        let org_path = root.join("config/org.yaml");
        checked_path(&org_path)?;
        if !std::fs::metadata(&org_path)
            .map_err(|_| rejected())?
            .is_file()
        {
            return Err(rejected());
        }
        let org: Value = serde_yaml::from_slice(&std::fs::read(&org_path).map_err(|_| rejected())?)
            .map_err(|_| rejected())?;
        if org["provider_auth_profiles"]
            .as_object()
            .is_none_or(|map| map.len() != profile.providers.len())
        {
            return Err(rejected());
        }
        for &provider in profile.providers {
            let profile = format!("test-{provider}");
            let def = &org["provider_auth_profiles"][&profile];
            let home = if let Some(login) = agy_login.as_ref().filter(|_| provider == "antigravity")
            {
                login.home.clone()
            } else if provider == "claude" {
                claude_login
                    .as_ref()
                    .map(|login| login.config_dir.clone())
                    .unwrap_or(home_at(root, provider)?)
            } else {
                home_at(root, provider)?
            };
            if def["provider"] != provider
                || def["home"] != home.to_string_lossy().as_ref()
                || def["env"].as_object().is_none_or(|map| !map.is_empty())
                || org["provider_auth_primary_profiles"][provider] != profile
                || org["agents"][provider]["auth_profile"] != profile
            {
                return Err(rejected());
            }
        }
        let flags: BTreeMap<String, String> =
            serde_json::from_value(manifest["applied_env"].clone()).map_err(|_| rejected())?;
        if flags.keys().any(|key| {
            !matches!(
                key.as_str(),
                "ADK_BUSY_INJECT"
                    | "ADK_QUEUED_HEAD_INJECT"
                    | "ADK_TURN_PRESENCE_TAIL"
                    | "ADK_INTAKE_ROUTING_MODE"
                    | "ADK_IMSG_OBLIGATIONS"
                    | "ADK_BUSY_INJECT_DISCORD_CHANNELS"
                    | "ADK_BUSY_INJECT_STASH_CHANNELS"
            )
        }) {
            return Err(rejected());
        }
        Ok(Self {
            api,
            claude_login,
            agy_login,
            #[cfg(unix)]
            credentials: profile.credentials,
            required,
            flags,
        })
    }

    /// The agy profile never starts without a supervisor-verified identity run.
    #[cfg(unix)]
    pub(super) fn requires_verified_run(&self) -> bool {
        self.agy_login.is_some()
    }

    pub(super) fn verify(&self) -> Result<(), String> {
        if self
            .required
            .iter()
            .any(|(key, value)| std::env::var(key).as_ref() != Ok(value))
            || std::env::vars_os().any(|(key, _)| forbidden(&key.to_string_lossy()))
        {
            return Err(rejected());
        }
        for key in [
            "HOME",
            "TMPDIR",
            "XDG_CONFIG_HOME",
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
        ] {
            checked_path(Path::new(&self.required[key]))?;
        }
        Ok(())
    }
}

fn required_runtime_path(worktree: &Path, claude_login: bool) -> Result<String, String> {
    let base = format!("{}/target/debug:/usr/bin:/bin", worktree.display());
    #[cfg(target_os = "macos")]
    if claude_login {
        use std::os::unix::fs::PermissionsExt;
        let source = ["/opt/homebrew/opt/tmux/bin", "/usr/local/opt/tmux/bin"]
            .into_iter()
            .map(Path::new)
            .find(|path| path.exists())
            .ok_or_else(rejected)?;
        let directory = std::fs::canonicalize(source).map_err(|_| rejected())?;
        let entries = std::fs::read_dir(&directory)
            .map_err(|_| rejected())?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| rejected())?;
        let binary = std::fs::symlink_metadata(directory.join("tmux")).map_err(|_| rejected())?;
        if entries.len() != 1
            || entries[0] != "tmux"
            || !binary.is_file()
            || binary.permissions().mode() & 0o111 == 0
        {
            return Err(rejected());
        }
        return Ok(format!("{base}:{}", directory.display()));
    }
    let _ = claude_login;
    Ok(base)
}

fn forbidden(key: &str) -> bool {
    matches!(
        key,
        "ANTHROPIC_API_KEY"
            | "OPENAI_API_KEY"
            | "XAI_API_KEY"
            | "DASHSCOPE_API_KEY"
            | "GEMINI_API_KEY"
            | "GOOGLE_API_KEY"
            | "NVIDIA_API_KEY"
            | "CLAUDE_CODE_OAUTH_TOKEN"
            | "BASH_ENV"
            | "ENV"
            | "ZDOTDIR"
            | "USERPROFILE"
            | "XDG_DATA_HOME"
            | "XDG_STATE_HOME"
            | "XDG_CACHE_HOME"
            | "XDG_RUNTIME_DIR"
            | "QWEN_HOME"
            | "GEMINI_CLI_HOME"
            | "GROK_HOME"
            | "OPENCODE_CONFIG"
    ) || key.starts_with("MEMENTO_")
        || key.starts_with("OP_")
        || key.starts_with("AGENT_OP_")
        || key.contains("DISCORD") && (key.ends_with("TOKEN") || key.ends_with("KEY"))
        || key.ends_with("_BOT_TOKEN")
        || key == "AGENTDESK_TOKEN"
}

fn scope() -> Result<Option<&'static Environment>, String> {
    let Some(instance) = current()? else {
        return Ok(None);
    };
    instance.environment.verify()?;
    Ok(Some(&instance.environment))
}

pub(crate) fn runtime_path() -> Result<Option<&'static str>, String> {
    Ok(scope()?.map(|snapshot| snapshot.required["PATH"].as_str()))
}

fn home_at(root: &Path, provider: &str) -> Result<PathBuf, String> {
    match provider {
        "claude" | "codex" => Ok(root
            .join("home/.adk/profiles")
            .join(provider)
            .join(format!("test-{provider}"))),
        _ => Err(rejected()),
    }
}

pub(crate) fn provider_home(provider: &str) -> Result<Option<PathBuf>, String> {
    let Some(snapshot) = scope()? else {
        return Ok(None);
    };
    if provider == "claude"
        && let Some(login) = &snapshot.claude_login
    {
        return Ok(Some(login.config_dir.clone()));
    }
    if provider == "antigravity" {
        let login = snapshot.agy_login.as_ref().ok_or_else(rejected)?;
        return Ok(Some(login.home.clone()));
    }
    home_at(&current()?.ok_or_else(rejected)?.root, provider).map(Some)
}

pub(crate) fn managed_profiles_root() -> Result<Option<PathBuf>, String> {
    Ok(scope()?.map(|env| Path::new(&env.required["HOME"]).join(".adk/profiles")))
}

pub(crate) fn validate_provider_home(provider: &str, path: &Path) -> Result<(), String> {
    if let Some(expected) = provider_home(provider)? {
        if path != expected {
            return Err(rejected());
        }
        checked_path(path)?;
    }
    Ok(())
}

pub(crate) fn validate_overlay(
    provider: &str,
    profile_id: &str,
    home: Option<&Path>,
    env: &BTreeMap<String, String>,
) -> Result<(), String> {
    let Some(expected) = provider_home(provider)? else {
        return Ok(());
    };
    if profile_id != format!("test-{provider}") || home != Some(expected.as_path()) {
        return Err(rejected());
    }
    let snapshot = scope()?.ok_or_else(rejected)?;
    if provider == "claude"
        && let Some(login) = scope()?.and_then(|s| s.claude_login.as_ref())
    {
        return if *env == login.provider_env() {
            Ok(())
        } else {
            Err(rejected())
        };
    }
    if let Some(login) = snapshot
        .agy_login
        .as_ref()
        .filter(|_| provider == "antigravity")
    {
        return if *env == login.provider_env() {
            Ok(())
        } else {
            Err(rejected())
        };
    }
    if env.iter().any(|(key, value)| {
        forbidden(key)
            || snapshot
                .required
                .get(key)
                .is_some_and(|expected| value != expected)
    }) {
        return Err(rejected());
    }
    validate_provider_home(provider, &expected)
}

pub(crate) fn configure_child_command(command: &mut Command) -> Result<(), String> {
    if let Some(snapshot) = scope()? {
        let extras: Vec<_> = command
            .get_envs()
            .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_owned())))
            .collect();
        let marked = |marker: &str| {
            extras
                .iter()
                .any(|(key, value)| key == marker && value == "1")
        };
        let agy = snapshot
            .agy_login
            .as_ref()
            .filter(|_| marked(agy_cell::AGY_LOGIN_MARKER));
        if let Some(login) = agy {
            login.verify_no_mcp_servers()?;
        }
        let provider = snapshot
            .claude_login
            .as_ref()
            .filter(|_| marked("AGENTDESK_TEST_CLAUDE_LOGIN"))
            .map(ClaudeLogin::provider_env)
            .or_else(|| agy.map(AgyLogin::provider_env));
        if extras.iter().any(|(key, value)| {
            forbidden(&key.to_string_lossy())
                || snapshot
                    .required
                    .get(key.to_string_lossy().as_ref())
                    .is_some_and(|expected| {
                        value != expected.as_str()
                            && provider
                                .as_ref()
                                .and_then(|v| v.get(&key.to_string_lossy().to_string()))
                                .is_none_or(|allowed| value != allowed.as_str())
                    })
        }) {
            return Err(rejected());
        }
        command
            .env_clear()
            .envs(extras)
            .envs(&snapshot.flags)
            .envs(&snapshot.required)
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        if let Some(provider) = provider {
            command.envs(provider);
        }
    }
    Ok(())
}

pub(crate) fn shell_environment() -> Result<String, String> {
    let Some(snapshot) = scope()? else {
        return Ok(String::new());
    };
    // A long-lived tmux server can retain credentials absent from this process.
    let mut script = String::from(
        "unset BASH_ENV ENV ZDOTDIR USERPROFILE XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME XDG_RUNTIME_DIR CLAUDE_CODE_OAUTH_TOKEN QWEN_HOME GEMINI_CLI_HOME GROK_HOME OPENCODE_CONFIG ANTHROPIC_API_KEY OPENAI_API_KEY XAI_API_KEY DASHSCOPE_API_KEY GEMINI_API_KEY GOOGLE_API_KEY NVIDIA_API_KEY MEMENTO_ACCESS_KEY OP_SERVICE_ACCOUNT_TOKEN OP_CONNECT_TOKEN AGENTDESK_TOKEN DISCORD_TOKEN DISCORD_BOT_TOKEN CLAUDE_BOT_TOKEN CODEX_BOT_TOKEN DRIVER_BOT_TOKEN TMUX TMUX_PANE\n",
    );
    script.push_str("for adk_instance_env_key in $(/usr/bin/env | /usr/bin/sed -n 's/^\\([A-Za-z_][A-Za-z0-9_]*\\)=.*/\\1/p'); do\ncase \"$adk_instance_env_key\" in MEMENTO_*|OP_*|AGENT_OP_*|*DISCORD*TOKEN|*DISCORD*KEY|*_BOT_TOKEN) unset \"$adk_instance_env_key\";; esac\ndone\nunset adk_instance_env_key\n");
    if snapshot.claude_login.is_some() {
        script.push_str("unset AGENTDESK_TEST_CLAUDE_LOGIN CLAUDE_SECURESTORAGE_CONFIG_DIR\n");
    }
    for (key, value) in snapshot.flags.iter().chain(&snapshot.required) {
        script.push_str(&format!(
            "export {key}={}\n",
            crate::services::process::shell_escape(value)
        ));
    }
    Ok(script)
}

pub(crate) fn validate_api_target(target: &str) -> Result<(), String> {
    if let Some(snapshot) = scope()?
        && snapshot.api != target
    {
        return Err(rejected());
    }
    Ok(())
}

pub(crate) fn http_agent() -> ureq::Agent {
    if requested() {
        ureq::AgentBuilder::new()
            .redirects(0)
            .try_proxy_from_env(false)
            .build()
    } else {
        ureq::Agent::new()
    }
}

pub(crate) fn refuse_service_management() -> Result<(), String> {
    if requested() { Err(rejected()) } else { Ok(()) }
}

pub(crate) fn validate_config(config: &crate::config::Config) -> Result<(), String> {
    if let Some(snapshot) = scope()?
        && (config.server.local_base_url() != snapshot.api
            || config.server.host != "127.0.0.1"
            || !config.mcp_servers.is_empty()
            || config
                .memory
                .as_ref()
                .is_none_or(|memory| memory.backend != "file" || !memory.mcp.endpoint.is_empty()))
    {
        return Err(rejected());
    }
    Ok(())
}

pub(crate) fn validate_path(path: &Path) -> Result<(), String> {
    if scope()?.is_some() {
        let instance = current()?.ok_or_else(rejected)?;
        if !path.starts_with(&instance.root) {
            return Err(rejected());
        }
        checked_path(path)?;
    }
    Ok(())
}

#[derive(Debug)]
struct ClaudeLogin {
    home: PathBuf,
    config_dir: PathBuf,
    cli: PathBuf,
    account_slot: u64,
    user: String,
}

impl ClaudeLogin {
    fn load(root: &Path, manifest: &Value) -> Result<Option<Self>, String> {
        let Some(value) = manifest.get("claude_login") else {
            return Ok(None);
        };
        let keys = ["mode", "account_slot", "login_home", "config_dir", "cli"];
        if value
            .as_object()
            .is_none_or(|v| v.len() != keys.len() || keys.iter().any(|k| !v.contains_key(*k)))
            || value["mode"] != "default_active"
            || !matches!(value["account_slot"].as_u64(), Some(2 | 3))
        {
            return Err(rejected());
        }
        let path = |key| value[key].as_str().map(PathBuf::from).ok_or_else(rejected);
        let home = path("login_home")?;
        let config_dir = path("config_dir")?;
        let cli = path("cli")?;
        if root.parent() != Some(home.join(".adk/dev/instances").as_path())
            || config_dir != home_at(root, "claude")?
            || !cli.starts_with(home.join(".local"))
        {
            return Err(rejected());
        }
        for p in [&home, &config_dir, &cli] {
            checked_path(p)?;
        }
        let user = Command::new("/usr/bin/id")
            .arg("-un")
            .env_clear()
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(rejected)?;
        Ok(Some(Self {
            home,
            config_dir,
            cli,
            account_slot: value["account_slot"].as_u64().ok_or_else(rejected)?,
            user: user.trim().to_owned(),
        }))
    }

    fn provider_env(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("HOME".into(), self.home.to_string_lossy().into_owned()),
            (
                "CLAUDE_CONFIG_DIR".into(),
                self.config_dir.to_string_lossy().into_owned(),
            ),
            ("USER".into(), self.user.clone()),
            ("LOGNAME".into(), self.user.clone()),
            ("CLAUDE_SECURESTORAGE_CONFIG_DIR".into(), String::new()),
            ("DISABLE_AUTOUPDATER".into(), "1".into()),
            ("AGENTDESK_TEST_CLAUDE_LOGIN".into(), "1".into()),
        ])
    }
}

pub(crate) fn claude_login_enabled() -> bool {
    scope()
        .ok()
        .flatten()
        .is_some_and(|v| v.claude_login.is_some())
}

pub(crate) fn resolve_profile(
    provider: &str,
    selected: Option<&str>,
) -> Result<Option<crate::services::provider_auth_profile::ProviderAuthOverlay>, String> {
    let Some(snapshot) = scope()? else {
        return Ok(None);
    };
    provider_home(provider)?;
    if selected.is_none() {
        return Err("test instance requires a named provider profile".into());
    }
    if let Some(login) = snapshot
        .agy_login
        .as_ref()
        .filter(|_| provider == "antigravity")
    {
        return if selected == Some("test-antigravity") {
            Ok(Some(login.overlay()))
        } else {
            Err(rejected())
        };
    }
    let Some(login) = snapshot
        .claude_login
        .as_ref()
        .filter(|_| provider == "claude")
    else {
        return Ok(None);
    };
    if selected != Some("test-claude") {
        return Err(rejected());
    }
    Ok(Some(
        crate::services::provider_auth_profile::ProviderAuthOverlay {
            profile_id: "test-claude".into(),
            provider: crate::services::provider::ProviderKind::Claude,
            home: Some(login.config_dir.clone()),
            env: login.provider_env(),
            unset: crate::services::provider_auth_profile::provider_credential_env_keys(
                &crate::services::provider::ProviderKind::Claude,
            )
            .iter()
            .map(|v| (*v).into())
            .collect(),
        },
    ))
}

pub(crate) fn provider_shell_environment(
    overlay: &crate::services::provider_auth_profile::ProviderAuthOverlay,
) -> Result<String, String> {
    let mut script = shell_environment()?;
    if overlay.provider == crate::services::provider::ProviderKind::Claude
        && let Some(login) = scope()?.and_then(|s| s.claude_login.as_ref())
    {
        claude_launch_admission()?;
        validate_overlay(
            "claude",
            &overlay.profile_id,
            overlay.home.as_deref(),
            &overlay.env,
        )?;
        for (key, value) in login.provider_env() {
            script.push_str(&format!(
                "export {key}={}\n",
                crate::services::process::shell_escape(&value)
            ));
        }
    }
    Ok(script)
}

pub(crate) fn claude_launch_args() -> Vec<String> {
    if !claude_login_enabled() {
        return Vec::new();
    }
    [
        "--strict-mcp-config",
        "--mcp-config",
        r#"{"mcpServers":{}}"#,
        "--setting-sources",
        "",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

pub(crate) fn claude_startup_prompt_accepted(endpoint: &str) -> bool {
    #[cfg(unix)]
    if let Ok(Some(snapshot)) = scope()
        && snapshot.api == endpoint
        && let Some(login) = &snapshot.claude_login
        && let Ok(directory) =
            crate::utils::secret_file::private_directory::PrivateDirectory::open(&login.config_dir)
        && let Ok(Some(bytes)) = directory.read("settings.json")
        && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
    {
        return value["skipDangerousModePermissionPrompt"].as_bool() == Some(true);
    }
    #[cfg(not(unix))]
    let _ = endpoint;
    false
}

pub(crate) fn claude_relay_command(
    config: &crate::services::claude_tui::hook_bundle::HookBundleConfig,
    event: &str,
    quote: fn(&str) -> String,
) -> String {
    let command = [
        quote(&config.agentdesk_exe),
        "claude-hook-relay".into(),
        "--endpoint".into(),
        quote(&config.endpoint),
        "--provider".into(),
        quote(&config.provider),
        "--event".into(),
        quote(event),
        "--session-id".into(),
        quote(&config.session_id),
    ]
    .join(" ");
    match scope() {
        Ok(Some(snapshot)) if snapshot.claude_login.is_some() => {
            if config.endpoint != snapshot.api || config.provider != "claude" {
                return "/usr/bin/false".into();
            }
            let mut prefix = String::from(
                "/usr/bin/env -u AGENTDESK_TEST_CLAUDE_LOGIN -u CLAUDE_SECURESTORAGE_CONFIG_DIR",
            );
            for (key, value) in snapshot.flags.iter().chain(&snapshot.required) {
                prefix.push_str(&format!(" {}", quote(&format!("{key}={value}"))));
            }
            format!("{prefix} {command}")
        }
        Err(_) => "/usr/bin/false".into(),
        _ => command,
    }
}

pub(crate) fn claude_login_presence() -> Option<bool> {
    if !claude_login_enabled() {
        return None;
    }
    Some(metadata_command_present(
        &mut claude_login_metadata_command(),
    ))
}

pub(crate) fn claude_login_metadata_command() -> Command {
    let mut command = Command::new(metadata_program());
    // No -w/-g: inspect existence, never request the secret value.
    command.args(["find-generic-password", "-s", "Claude Code-credentials"]);
    if let Ok(Some(snapshot)) = scope()
        && let Some(login) = &snapshot.claude_login
    {
        command.env("HOME", &login.home);
    }
    command
}

pub(crate) fn metadata_command_present(command: &mut Command) -> bool {
    bounded_success(command, std::time::Duration::from_secs(2))
}

fn bounded_success(command: &mut Command, timeout: std::time::Duration) -> bool {
    use std::process::Stdio;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

pub(crate) fn expected_profile_home(
    root: &Path,
    provider: &crate::services::provider::ProviderKind,
    profile: &str,
) -> PathBuf {
    if *provider == crate::services::provider::ProviderKind::Claude
        && profile == "test-claude"
        && let Ok(Some(snapshot)) = scope()
        && let Some(login) = &snapshot.claude_login
    {
        login.config_dir.clone()
    } else {
        crate::services::provider_auth_profile::extra_account_home_at(root, provider, profile)
    }
}

pub(crate) fn claude_login_enabled_checked() -> Result<bool, String> {
    Ok(scope()?.is_some_and(|snapshot| snapshot.claude_login.is_some()))
}

pub(crate) fn provider_resolution_allowed(provider: &str) -> Result<bool, String> {
    Ok(!provider.trim().eq_ignore_ascii_case("codex") || !claude_login_enabled_checked()?)
}

#[cfg(test)]
static METADATA_TEST_PROGRAM: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_metadata_program_for_tests(path: PathBuf) {
    *METADATA_TEST_PROGRAM.lock().unwrap() = Some(path);
}

fn metadata_program() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = METADATA_TEST_PROGRAM.lock().unwrap().clone() {
        return path;
    }
    PathBuf::from("/usr/bin/security")
}

const CLAUDE_ADMISSION: &str = r#"import json, os, signal, subprocess, sys

def run(argv, timeout, capture=False, env=None):
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE if capture else subprocess.DEVNULL,
        stderr=subprocess.DEVNULL, env=env, start_new_session=True)
    try:
        output, _ = child.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        try: os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError: pass
        child.communicate()
        raise
    return child.returncode, output

try:
    home, declared, metadata = sys.argv[1:]
    code, output = run([home+'/.local/bin/cswap', 'list', '--json'], 30, True,
        {'HOME': home, 'PATH': '/opt/homebrew/bin:/usr/bin:/bin'})
    slot = json.loads(output)['activeAccountNumber']
    if code or type(slot) is not int or slot not in (2, 3) or slot != int(declared):
        sys.exit(1)
    code, _ = run([metadata, 'find-generic-password', '-s', 'Claude Code-credentials'], 2,
        env={**os.environ, 'HOME': home})
    if code: sys.exit(1)
except Exception:
    sys.exit(1)
"#;

fn admission_command(login: &ClaudeLogin) -> Command {
    let mut command = Command::new("/usr/bin/python3");
    command
        .args(["-I", "-c", CLAUDE_ADMISSION])
        .arg(&login.home)
        .arg(login.account_slot.to_string())
        .arg(metadata_program());
    command
}

pub(crate) fn claude_launch_admission() -> Result<(), String> {
    if let Some(login) = scope()?.and_then(|snapshot| snapshot.claude_login.as_ref())
        && !bounded_success(
            &mut admission_command(login),
            std::time::Duration::from_secs(35),
        )
    {
        return Err("test instance Claude login admission refused".into());
    }
    Ok(())
}

pub(crate) fn claude_provider_exports() -> Result<String, String> {
    let Some(login) = scope()?.and_then(|snapshot| snapshot.claude_login.as_ref()) else {
        return Ok(String::new());
    };
    Ok(login
        .provider_env()
        .into_iter()
        .map(|(key, value)| {
            format!(
                "export {key}={}\n",
                crate::services::process::shell_escape(&value)
            )
        })
        .collect())
}

pub(crate) fn claude_pre_exec_admission() -> Result<String, String> {
    let Some(login) = scope()?.and_then(|snapshot| snapshot.claude_login.as_ref()) else {
        return Ok(String::new());
    };
    let command = admission_command(login);
    let line = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| crate::services::process::shell_escape(&part.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!("{line} || exit 1\n"))
}

pub(crate) fn admit_claude_wrapper(command: &mut Command) -> Result<(), String> {
    let snapshot = scope()?.ok_or_else(rejected)?;
    let login = snapshot.claude_login.as_ref().ok_or_else(rejected)?;
    let args = command.get_args().collect::<Vec<_>>();
    let option = |key: &str, expected: &str| {
        args.iter().filter(|arg| **arg == key).count() == 1
            && args
                .windows(2)
                .any(|pair| pair[0] == key && pair[1] == expected)
    };
    let settings = args
        .windows(2)
        .find(|pair| pair[0] == "--settings")
        .map(|pair| PathBuf::from(pair[1]))
        .ok_or_else(rejected)?;
    let instance = current()?.ok_or_else(rejected)?;
    if args
        .iter()
        .filter(|arg| **arg == "--strict-mcp-config")
        .count()
        != 1
        || !option("--setting-sources", "")
        || !option("--mcp-config", r#"{"mcpServers":{}}"#)
        || args.iter().filter(|arg| **arg == "--settings").count() != 1
        || !settings.starts_with(&instance.root)
        || std::fs::canonicalize(command.get_program()).map_err(|_| rejected())?
            != std::fs::canonicalize(&login.cli).map_err(|_| rejected())?
    {
        return Err(rejected());
    }
    checked_path(&settings)?;
    claude_launch_admission()?;
    for key in crate::services::provider_auth_profile::provider_credential_env_keys(
        &crate::services::provider::ProviderKind::Claude,
    ) {
        command.env_remove(key);
    }
    command.envs(login.provider_env());
    command.env("PATH", &snapshot.required["PATH"]);
    configure_child_command(command)
}

#[cfg(all(test, unix))]
pub(crate) fn login_fixture_for_tests() -> (tempfile::TempDir, PathBuf, PathBuf) {
    super::login_test_support::review_fixture()
}
