//! Profile table and the dedicated agy login home for the `agy-headless` test cell.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{checked_path, rejected};

pub(super) const AGY_LOGIN_MARKER: &str = "AGENTDESK_TEST_AGY_LOGIN";

/// Providers and the exact credential basenames (sorted) one profile admits.
#[derive(Debug)]
pub(super) struct Profile {
    pub(super) providers: &'static [&'static str],
    #[cfg(unix)]
    pub(super) credentials: &'static [&'static str],
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
};

const AGY_HEADLESS: Profile = Profile {
    providers: &["antigravity"],
    #[cfg(unix)]
    credentials: &[
        "announce_bot_token",
        "antigravity_bot_token",
        "notify_bot_token",
    ],
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
        Ok(Some(Self { home, cli }))
    }

    pub(super) fn provider_env(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("HOME".into(), self.home.to_string_lossy().into_owned()),
            (AGY_LOGIN_MARKER.into(), "1".into()),
        ])
    }

    /// agy reads MCP servers from its HOME; only an absent or empty server map may spawn.
    pub(super) fn verify_no_mcp_servers(&self) -> Result<(), String> {
        let path = self.home.join(".gemini/config/mcp_config.json");
        checked_path(&path)?;
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
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

#[cfg(all(test, unix))]
#[path = "agy_cell_tests.rs"]
pub(super) mod tests;
