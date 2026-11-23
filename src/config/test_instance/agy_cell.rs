//! Profile table and the dedicated agy login home for the `agy-headless` test cell.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{checked_path, rejected};

pub(super) const AGY_LOGIN_MARKER: &str = "AGENTDESK_TEST_AGY_LOGIN";
const GEMINI_API_KEY: &str = "gemini_api_key";

/// Providers and the exact credential basenames (sorted) one profile admits.
#[derive(Debug)]
pub(super) struct Profile {
    pub(super) providers: &'static [&'static str],
    #[cfg(unix)]
    pub(super) credentials: &'static [&'static str],
    /// Optional provider keys beside the bot tokens; they never take part in Discord identity.
    #[cfg(unix)]
    pub(super) provider_credentials: &'static [&'static str],
}

const TUI_LIVE: Profile = Profile {
    providers: &["claude", "codex"],
    #[cfg(unix)]
    credentials: &[
        "announce_bot_token",
        "claude_bot_token",
        "codex_bot_token",
        "notify_bot_token",
    ],
    #[cfg(unix)]
    provider_credentials: &[],
};

const AGY_HEADLESS: Profile = Profile {
    providers: &["antigravity"],
    #[cfg(unix)]
    credentials: &[
        "announce_bot_token",
        "antigravity_bot_token",
        "notify_bot_token",
    ],
    #[cfg(unix)]
    provider_credentials: &[GEMINI_API_KEY],
};

/// Declarations without the agy profile name keep the original two-provider contract.
pub(super) fn profile(manifest: &Value) -> &'static Profile {
    if manifest["profile"]["name"] == "agy-headless" {
        &AGY_HEADLESS
    } else {
        &TUI_LIVE
    }
}

#[derive(Debug)]
pub(super) struct AgyLogin {
    pub(super) home: PathBuf,
    pub(super) cli: PathBuf,
    api_key: PathBuf,
}

impl AgyLogin {
    /// The agy profile requires `<instances parent>/agy-login`; every other profile forbids it.
    pub(super) fn load(root: &Path, manifest: &Value) -> Result<Option<Self>, String> {
        let agy = std::ptr::eq(profile(manifest), &AGY_HEADLESS);
        let Some(value) = manifest.get("agy_login") else {
            return if agy { Err(rejected()) } else { Ok(None) };
        };
        let keys = ["mode", "login_home", "cli"];
        if !agy
            || manifest.get("claude_login").is_some()
            || value
                .as_object()
                .is_none_or(|v| v.len() != keys.len() || keys.iter().any(|k| !v.contains_key(*k)))
            || value["mode"] != "dedicated_home"
        {
            return Err(rejected());
        }
        let path = |key| value[key].as_str().map(PathBuf::from).ok_or_else(rejected);
        let (home, cli) = (path("login_home")?, path("cli")?);
        if root
            .parent()
            .and_then(Path::parent)
            .map(|dev| dev.join("agy-login"))
            != Some(home.clone())
        {
            return Err(rejected());
        }
        checked_path(&home)?;
        checked_path(&cli)?;
        let metadata = std::fs::symlink_metadata(&cli).map_err(|_| rejected())?;
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = true;
        if !metadata.is_file() || !executable {
            return Err(rejected());
        }
        let api_key = root.join("credential").join(GEMINI_API_KEY);
        Ok(Some(Self { home, cli, api_key }))
    }

    pub(super) fn provider_env(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("HOME".into(), self.home.to_string_lossy().into_owned()),
            (AGY_LOGIN_MARKER.into(), "1".into()),
        ])
    }

    /// The Gemini key for agy API-key mode, from the instance credential file when present.
    pub(super) fn api_key_env(&self) -> Result<Option<(&'static str, String)>, String> {
        Ok(read_private_credential(&self.api_key)?.map(|value| ("GEMINI_API_KEY", value)))
    }

    /// agy reads MCP servers from its HOME; only an absent or empty server map may spawn.
    pub(super) fn verify_no_mcp_servers(&self) -> Result<(), String> {
        let path = self.home.join(".gemini/config/mcp_config.json");
        checked_path(&path)?;
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            // agy itself creates this file empty on its first run.
            Ok(metadata) if metadata.is_file() && metadata.len() == 0 => return Ok(()),
            Ok(metadata) if metadata.is_file() && metadata.len() <= 1024 * 1024 => {}
            _ => return Err(rejected()),
        }
        let value: Value = serde_json::from_slice(&std::fs::read(&path).map_err(|_| rejected())?)
            .map_err(|_| rejected())?;
        match value.as_object().map(|object| object.get("mcpServers")) {
            Some(None) => Ok(()),
            Some(Some(servers)) if servers.as_object().is_some_and(|map| map.is_empty()) => Ok(()),
            _ => Err(rejected()),
        }
    }

    pub(super) fn overlay(&self) -> crate::services::provider_auth_profile::ProviderAuthOverlay {
        let provider = crate::services::provider::ProviderKind::Antigravity;
        crate::services::provider_auth_profile::ProviderAuthOverlay {
            profile_id: "test-antigravity".into(),
            home: Some(self.home.clone()),
            env: self.provider_env(),
            unset: crate::services::provider_auth_profile::provider_credential_env_keys(&provider)
                .iter()
                .map(|key| (*key).into())
                .collect(),
            provider,
        }
    }
}

/// Absent is `None`; anything but a private one-line regular file is refused.
#[cfg(unix)]
fn read_private_credential(path: &Path) -> Result<Option<String>, String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    checked_path(path)?;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        file => file.map_err(|_| rejected())?,
    };
    let metadata = file.metadata().map_err(|_| rejected())?;
    if !metadata.is_file()
        || metadata.mode() & 0o7777 != 0o600
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
    {
        return Err(rejected());
    }
    let mut value = String::new();
    file.take(4097)
        .read_to_string(&mut value)
        .map_err(|_| rejected())?;
    if value.is_empty()
        || value.len() > 4096
        || value.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(rejected());
    }
    Ok(Some(value))
}

#[cfg(not(unix))]
fn read_private_credential(_path: &Path) -> Result<Option<String>, String> {
    Ok(None)
}

#[cfg(all(test, unix))]
#[path = "agy_cell_tests.rs"]
pub(crate) mod tests;
