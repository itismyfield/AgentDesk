//! `session_hosts`: the channels a Herdr endpoint runs. Read once at boot; an empty section
//! leaves every channel on the host it had before.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use super::Config;
use crate::services::provider_hosting::RuntimeMode;

const CHANNELS_KEY: &str = "session_hosts.herdr.channels";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionHostsConfig {
    #[serde(default, skip_serializing_if = "HerdrHostsConfig::is_empty")]
    pub herdr: HerdrHostsConfig,
}

impl SessionHostsConfig {
    pub fn is_empty(&self) -> bool {
        self.herdr.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HerdrHostsConfig {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub endpoints: BTreeMap<String, HerdrEndpointConfig>,
    /// Discord channel id to endpoint key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub channels: BTreeMap<String, String>,
}

impl HerdrHostsConfig {
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty() && self.channels.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HerdrEndpointConfig {
    /// The `cluster.instance_id` of the only node that may run this endpoint.
    pub execution_node: String,
    pub socket_path: PathBuf,
    pub herdr_home: PathBuf,
    pub herdr_session: String,
}

/// One configured endpoint, named by its config key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HerdrEndpoint {
    pub key: String,
    pub execution_node: String,
    pub socket_path: PathBuf,
    pub herdr_home: PathBuf,
    pub herdr_session: String,
}

/// The validated section and this node's id, as the process booted with them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BootSessionHosts {
    config: SessionHostsConfig,
    channels: BTreeMap<u64, HerdrEndpoint>,
    local_node: Option<String>,
}

impl BootSessionHosts {
    pub(crate) fn from_config(config: &Config) -> Result<Self> {
        let local_node = config.cluster.instance_id.as_deref().map(str::trim);
        Ok(Self {
            config: config.session_hosts.clone(),
            channels: validate(config)?,
            local_node: local_node.filter(|id| !id.is_empty()).map(str::to_owned),
        })
    }

    pub(crate) fn herdr_endpoint(&self, channel: u64) -> Option<&HerdrEndpoint> {
        self.channels.get(&channel)
    }

    pub(crate) fn local_node(&self) -> Option<&str> {
        self.local_node.as_deref()
    }

    pub(crate) fn channels(&self) -> &BTreeMap<u64, HerdrEndpoint> {
        &self.channels
    }

    pub(crate) fn config(&self) -> &SessionHostsConfig {
        &self.config
    }
}

/// Every endpoint complete with absolute paths, every channel a registered TUI on a known endpoint.
pub(crate) fn validate(config: &Config) -> Result<BTreeMap<u64, HerdrEndpoint>> {
    let herdr = &config.session_hosts.herdr;
    for (key, endpoint) in &herdr.endpoints {
        let at = format!("session_hosts.herdr.endpoints.{key}");
        let filled = |value: &str| !value.trim().is_empty();
        ensure!(
            filled(key),
            "session_hosts.herdr.endpoints has an empty key"
        );
        ensure!(
            filled(&endpoint.execution_node),
            "{at}.execution_node is empty"
        );
        ensure!(
            filled(&endpoint.herdr_session),
            "{at}.herdr_session is empty"
        );
        ensure!(
            endpoint.socket_path.is_absolute(),
            "{at}.socket_path is not absolute"
        );
        ensure!(
            endpoint.herdr_home.is_absolute(),
            "{at}.herdr_home is not absolute"
        );
    }
    let mut channels = BTreeMap::new();
    for (raw, key) in &herdr.channels {
        let channel = raw.trim().parse::<u64>().ok().filter(|id| *id != 0);
        let Some(channel) = channel else {
            anyhow::bail!("{CHANNELS_KEY}: {raw:?} is not a channel id");
        };
        let Some(endpoint) = herdr.endpoints.get(key) else {
            anyhow::bail!("{CHANNELS_KEY}: channel {channel} names unknown endpoint {key:?}");
        };
        bound_as_tui(config, channel)?;
        let endpoint = HerdrEndpoint {
            key: key.clone(),
            execution_node: endpoint.execution_node.trim().to_owned(),
            socket_path: endpoint.socket_path.clone(),
            herdr_home: endpoint.herdr_home.clone(),
            herdr_session: endpoint.herdr_session.clone(),
        };
        ensure!(
            channels.insert(channel, endpoint).is_none(),
            "{CHANNELS_KEY}: channel {channel} is listed twice"
        );
    }
    Ok(channels)
}

/// The channel must be bound to an agent, and every binding of it must run as a Claude or Codex TUI.
fn bound_as_tui(config: &Config, channel: u64) -> Result<()> {
    let bindings = config.agents.iter().flat_map(|agent| agent.channels.iter());
    let mut found = false;
    for (provider_key, binding) in bindings {
        if binding.channel_id().and_then(|id| id.parse::<u64>().ok()) != Some(channel) {
            continue;
        }
        found = true;
        let provider = binding
            .provider()
            .unwrap_or_else(|| provider_key.to_owned());
        let provider = provider.trim().to_ascii_lowercase();
        let provider_config = config
            .providers
            .iter()
            .find(|(key, _)| key.trim().eq_ignore_ascii_case(&provider))
            .map(|(_, value)| value);
        let runtime = binding
            .runtime_mode_raw()
            .or_else(|| provider_config.and_then(|config| config.runtime.clone()));
        let tui = match runtime {
            Some(raw) => RuntimeMode::parse(&raw) == Some(RuntimeMode::Tui),
            None => binding
                .tui_hosting()
                .or_else(|| provider_config.and_then(|config| config.tui_hosting))
                .unwrap_or_else(|| super::default_provider_tui_hosting(&provider)),
        };
        ensure!(
            matches!(provider.as_str(), "claude" | "codex") && tui,
            "{CHANNELS_KEY}: channel {channel} is not a Claude or Codex TUI channel"
        );
    }
    ensure!(found, "{CHANNELS_KEY}: channel {channel} is not registered");
    Ok(())
}

static BOOT: OnceLock<BootSessionHosts> = OnceLock::new();

/// Fixes the section for the process lifetime; a later install keeps the first one.
pub(crate) fn install(config: &Config) -> Result<()> {
    let boot = BootSessionHosts::from_config(config)?;
    let _ = BOOT.set(boot);
    Ok(())
}

/// The boot section, or `None` before install, which configures nothing.
pub(crate) fn with_boot<R>(read: impl FnOnce(Option<&BootSessionHosts>) -> R) -> R {
    #[cfg(test)]
    if let Some(forced) = FORCED.with(|forced| forced.borrow().clone()) {
        return read(Some(&forced));
    }
    read(BOOT.get())
}

pub(crate) fn herdr_endpoint(channel: u64) -> Option<HerdrEndpoint> {
    with_boot(|boot| boot.and_then(|boot| boot.herdr_endpoint(channel)).cloned())
}

pub(crate) fn local_node() -> Option<String> {
    with_boot(|boot| {
        boot.and_then(BootSessionHosts::local_node)
            .map(str::to_owned)
    })
}

#[cfg(test)]
thread_local! {
    static FORCED: std::cell::RefCell<Option<BootSessionHosts>> = const { std::cell::RefCell::new(None) };
}

/// Replaces the boot section on this thread until dropped.
#[cfg(test)]
pub(crate) struct ForcedSessionHosts(Option<BootSessionHosts>);

#[cfg(test)]
pub(crate) fn force_for_test(
    local_node: Option<&str>,
    channels: &[(u64, &str)],
) -> ForcedSessionHosts {
    let endpoint = |execution_node: &str| HerdrEndpoint {
        key: format!("{execution_node}-endpoint"),
        execution_node: execution_node.to_owned(),
        socket_path: "/adk/herdr/agentdesk.sock".into(),
        herdr_home: "/adk/herdr".into(),
        herdr_session: "agentdesk".into(),
    };
    let boot = BootSessionHosts {
        config: SessionHostsConfig::default(),
        channels: channels
            .iter()
            .map(|(channel, node)| (*channel, endpoint(node)))
            .collect(),
        local_node: local_node.map(str::to_owned),
    };
    ForcedSessionHosts(FORCED.with(|forced| forced.replace(Some(boot))))
}

#[cfg(test)]
impl Drop for ForcedSessionHosts {
    fn drop(&mut self) {
        FORCED.with(|forced| *forced.borrow_mut() = self.0.take());
    }
}

#[cfg(test)]
#[path = "session_hosts_tests.rs"]
mod tests;
