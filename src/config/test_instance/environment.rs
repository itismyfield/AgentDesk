use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use super::{checked_path, current, read_json, rejected, requested};

#[derive(Debug)]
pub(super) struct Environment {
    api: String,
    pub(super) required: BTreeMap<String, String>,
    pub(super) flags: BTreeMap<String, String>,
}

impl Environment {
    pub(super) fn load(root: &Path, manifest: &Value, config: &Value) -> Result<Self, String> {
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
        let mut required = BTreeMap::new();
        for (key, path) in [
            ("HOME", root.join("home")),
            ("TMPDIR", root.join("tmp")),
            ("XDG_CONFIG_HOME", root.join("xdg")),
            ("CLAUDE_CONFIG_DIR", home_at(root, "claude")?),
            ("CODEX_HOME", home_at(root, "codex")?),
            ("AGENTDESK_ROOT_DIR", root.into()),
            ("AGENTDESK_CONFIG", root.join("config/agentdesk.yaml")),
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
                format!("{}/target/debug:/usr/bin:/bin", worktree.display()),
            ),
        ] {
            required.insert(key.into(), value);
        }
        let org = read_json(&root.join("config/org.yaml"))?;
        if org["provider_auth_profiles"]
            .as_object()
            .is_none_or(|map| map.len() != 2)
        {
            return Err(rejected());
        }
        for provider in ["claude", "codex"] {
            let profile = format!("test-{provider}");
            let def = &org["provider_auth_profiles"][&profile];
            if def["provider"] != provider
                || def["home"] != home_at(root, provider)?.to_string_lossy().as_ref()
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
            required,
            flags,
        })
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
    let Some(_) = scope()? else { return Ok(None) };
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
        if extras.iter().any(|(key, value)| {
            forbidden(&key.to_string_lossy())
                || snapshot
                    .required
                    .get(key.to_string_lossy().as_ref())
                    .is_some_and(|expected| value != expected.as_str())
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
