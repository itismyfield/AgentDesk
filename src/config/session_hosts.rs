//! `session_hosts`: the channels a Herdr endpoint runs. Read once at boot; an empty section
//! changes nothing.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Once, OnceLock};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use super::Config;
use crate::services::provider_hosting::channel_effective_tui_request;

const CHANNELS_KEY: &str = "session_hosts.herdr.channels";
// The removed Claude submit guard list: an old config or env may still name it.
const RETIRED_GUARD_KEY: &str = "session_hosts.claude_guard_channels";
const RETIRED_GUARD_ENV: &str = "AGENTDESK_CLAUDE_SUBMIT_GUARD_CHANNEL_IDS";

pub(crate) mod approval_store;
pub(crate) mod migration;
pub(crate) mod projection;
mod user_default;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointMode {
    #[default]
    LegacyDedicated,
    UserDefault,
}

impl EndpointMode {
    fn is_legacy(&self) -> bool {
        *self == Self::LegacyDedicated
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionHostsConfig {
    #[serde(default, skip_serializing_if = "HerdrSection::is_empty")]
    pub herdr: HerdrSection,
    /// Accepted only so an old config still boots; its value is never read.
    #[serde(default, rename = "claude_guard_channels", skip_serializing)]
    pub(crate) retired_guard: RetiredKey,
}

impl SessionHostsConfig {
    pub fn is_empty(&self) -> bool {
        self.herdr.is_empty()
    }
}

/// Whether a retired key was present; any value it held is skipped unread.
#[derive(Debug, Clone, Copy, Default, Eq)]
pub(crate) struct RetiredKey(bool);

// Adding or removing an ignored key changes no hosting, so it is never a restart-required edit.
impl PartialEq for RetiredKey {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl<'de> Deserialize<'de> for RetiredKey {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(deserializer).map(|_| Self(true))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HerdrSection {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub endpoints: BTreeMap<String, EndpointConfig>,
    /// Discord channel id to endpoint key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub channels: BTreeMap<String, String>,
}

impl HerdrSection {
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty() && self.channels.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointConfig {
    #[serde(default, skip_serializing_if = "EndpointMode::is_legacy")]
    pub mode: EndpointMode,
    /// The `cluster.instance_id` of the only node that may run this endpoint.
    pub execution_node: String,
    #[serde(default)]
    pub socket_path: PathBuf,
    #[serde(default)]
    pub herdr_home: PathBuf,
    #[serde(default)]
    pub herdr_session: String,
}

/// One configured endpoint, named by its config key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChannelEndpoint {
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
    channels: BTreeMap<u64, ChannelEndpoint>,
    local_node: Option<String>,
}

static RETIRED_WARNING: Once = Once::new();

// Config loading validates before install; both stages use this same resolution.
fn normalized(config: &Config) -> Result<Config> {
    let mut effective = config.clone();
    user_default::resolve(&mut effective, || {
        std::env::var_os("HOME").map(PathBuf::from)
    })?;
    Ok(effective)
}

impl BootSessionHosts {
    pub(crate) fn from_config(config: &Config) -> Result<Self> {
        let effective = normalized(config)?;
        let channels = validate_effective(&effective)?;
        let yaml = effective.session_hosts.retired_guard.0;
        let env = std::env::var_os(RETIRED_GUARD_ENV).is_some();
        if yaml || env {
            RETIRED_WARNING.call_once(|| {
                tracing::warn!(
                    key = RETIRED_GUARD_KEY,
                    env_name = RETIRED_GUARD_ENV,
                    yaml,
                    env,
                    "retired Claude submit guard setting ignored; remove it"
                );
            });
        }
        let local_node = effective.cluster.instance_id.as_deref().map(str::trim);
        Ok(Self {
            config: effective.session_hosts.clone(),
            channels,
            local_node: local_node.filter(|id| !id.is_empty()).map(str::to_owned),
        })
    }

    pub(crate) fn herdr_endpoint(&self, channel: u64) -> Option<&ChannelEndpoint> {
        self.channels.get(&channel)
    }

    pub(crate) fn local_node(&self) -> Option<&str> {
        self.local_node.as_deref()
    }

    pub(crate) fn channels(&self) -> &BTreeMap<u64, ChannelEndpoint> {
        &self.channels
    }

    pub(crate) fn config(&self) -> &SessionHostsConfig {
        &self.config
    }
}

/// Every endpoint complete with absolute paths, every channel a registered TUI on a known endpoint.
pub(crate) fn validate(config: &Config) -> Result<BTreeMap<u64, ChannelEndpoint>> {
    validate_effective(&normalized(config)?)
}

fn validate_effective(config: &Config) -> Result<BTreeMap<u64, ChannelEndpoint>> {
    let channels = projected_channels(&config.session_hosts)?;
    let adapter = crate::services::session_host::policy::peer::SUPPORTED;
    user_default_platform(config, adapter)?;
    for &channel in channels.keys() {
        bound_as_tui(config, CHANNELS_KEY, channel, &["claude", "codex"])?;
    }
    Ok(channels)
}

/// A user_default endpoint this node may execute needs a loaded-image adapter here; an endpoint
/// of another node is judged by that node's own boot validation.
fn user_default_platform(config: &Config, adapter: bool) -> Result<()> {
    if adapter {
        return Ok(());
    }
    let local = config.cluster.instance_id.as_deref().map(str::trim);
    let local = local.filter(|id| !id.is_empty());
    for (key, endpoint) in &config.session_hosts.herdr.endpoints {
        // An unknown node id cannot prove the endpoint runs elsewhere.
        let remote = local.is_some_and(|id| id != endpoint.execution_node.trim());
        ensure!(
            endpoint.mode != EndpointMode::UserDefault || remote,
            "session_hosts.herdr.endpoints.{key}.mode: user_default needs a loaded-image \
             adapter on its execution node and this platform has none; keep legacy_dedicated \
             (이 플랫폼 미지원)"
        );
    }
    Ok(())
}

fn projected_channels(hosts: &SessionHostsConfig) -> Result<BTreeMap<u64, ChannelEndpoint>> {
    let herdr = &hosts.herdr;
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
            endpoint.socket_path.is_absolute()
                || (endpoint.mode == EndpointMode::UserDefault
                    && endpoint.socket_path.as_os_str().is_empty()),
            "{at}.socket_path is not absolute"
        );
        ensure!(
            endpoint.herdr_home.is_absolute() || endpoint.mode == EndpointMode::UserDefault,
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
        let endpoint = ChannelEndpoint {
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

/// The channel must be bound to an agent, and every binding of it must run as a TUI of `providers`.
fn bound_as_tui(config: &Config, key: &str, channel: u64, providers: &[&str]) -> Result<()> {
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
        // The hosting resolver's own rule: a blank or unknown runtime falls back to tui_hosting.
        let tui = channel_effective_tui_request(config, binding, &provider);
        ensure!(
            providers.contains(&provider.as_str()) && tui,
            "{key}: channel {channel} is not a {} TUI channel",
            providers.join(" or ")
        );
    }
    ensure!(found, "{key}: channel {channel} is not registered");
    Ok(())
}

/// Migration metadata stays outside the boot configuration's equality contract.
#[derive(Debug, Clone)]
pub(crate) struct BootMigration {
    candidate: SessionHostsConfig,
    diagnostic: Option<migration::Diagnostic>,
    admission_stopped: bool,
}

impl BootMigration {
    pub(crate) fn restart_required(&self, config: &Config) -> bool {
        normalized(config).map_or(true, |effective| self.candidate != effective.session_hosts)
    }

    pub(crate) fn candidate(&self) -> &SessionHostsConfig {
        &self.candidate
    }

    pub(crate) fn migration(&self) -> Option<&migration::Diagnostic> {
        self.diagnostic.as_ref()
    }

    pub(crate) fn admission_stopped(&self) -> bool {
        self.admission_stopped
    }
}

static BOOT_MIGRATION: OnceLock<BootMigration> = OnceLock::new();
static BOOT: OnceLock<BootSessionHosts> = OnceLock::new();

#[cfg(test)]
pub(crate) fn install(config: &Config) -> Result<()> {
    let boot = BootSessionHosts::from_config(config)?;
    let _ = BOOT.set(boot);
    Ok(())
}

pub(crate) fn install_selected(selection: migration::HostsBootSelection) {
    let (hosts, migration) = selection.into_parts();
    BOOT.get_or_init(|| {
        let _ = BOOT_MIGRATION.set(migration);
        hosts
    });
}

/// The boot section, or `None` before install, which configures nothing.
pub(crate) fn with_boot<R>(read: impl FnOnce(Option<&BootSessionHosts>) -> R) -> R {
    #[cfg(test)]
    if let Some(forced) = FORCED.with(|forced| forced.borrow().clone()) {
        return read(Some(&forced));
    }
    read(BOOT.get())
}

pub(crate) fn with_boot_migration<R>(read: impl FnOnce(Option<&BootMigration>) -> R) -> R {
    #[cfg(test)]
    if FORCED.with(|forced| forced.borrow().is_some()) {
        return FORCED_MIGRATION.with(|forced| read(forced.borrow().as_ref()));
    }
    read(BOOT_MIGRATION.get())
}

pub(crate) fn herdr_endpoint(channel: u64) -> Option<ChannelEndpoint> {
    with_boot(|boot| boot.and_then(|boot| boot.herdr_endpoint(channel)).cloned())
}

/// Every Herdr-configured channel id as text, for SQL that must leave their rows alone.
pub(crate) fn configured_channel_ids() -> Vec<String> {
    let ids = |boot: &BootSessionHosts| boot.channels().keys().map(u64::to_string).collect();
    with_boot(|boot| boot.map_or_else(Vec::new, ids))
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
    static FORCED_MIGRATION: std::cell::RefCell<Option<BootMigration>> = const { std::cell::RefCell::new(None) };
}

/// Replaces the boot section on this thread until dropped.
#[cfg(test)]
pub(crate) struct ForcedSessionHosts(Option<BootSessionHosts>, Option<BootMigration>);

/// Installs a real selection's hosts and migration metadata on this thread until dropped.
#[cfg(all(test, unix))]
pub(crate) fn force_selected_for_test(
    selection: migration::HostsBootSelection,
) -> ForcedSessionHosts {
    let (hosts, metadata) = selection.into_parts();
    ForcedSessionHosts(
        FORCED.with(|forced| forced.replace(Some(hosts))),
        FORCED_MIGRATION.with(|forced| forced.replace(Some(metadata))),
    )
}

#[cfg(test)]
pub(crate) fn force_for_test(
    local_node: Option<&str>,
    channels: &[(u64, &str)],
) -> ForcedSessionHosts {
    let endpoint = |execution_node: &str| ChannelEndpoint {
        key: format!("{execution_node}-endpoint"),
        execution_node: execution_node.to_owned(),
        socket_path: "/adk/herdr/agentdesk.sock".into(),
        herdr_home: "/adk/herdr".into(),
        herdr_session: "agentdesk".into(),
    };
    let channels: BTreeMap<u64, ChannelEndpoint> = channels
        .iter()
        .map(|(channel, node)| (*channel, endpoint(node)))
        .collect();
    let mut config = SessionHostsConfig::default();
    for (channel, endpoint) in &channels {
        let written = EndpointConfig {
            mode: EndpointMode::LegacyDedicated,
            execution_node: endpoint.execution_node.clone(),
            socket_path: endpoint.socket_path.clone(),
            herdr_home: endpoint.herdr_home.clone(),
            herdr_session: endpoint.herdr_session.clone(),
        };
        let herdr = &mut config.herdr;
        herdr.endpoints.insert(endpoint.key.clone(), written);
        herdr
            .channels
            .insert(channel.to_string(), endpoint.key.clone());
    }
    let boot = BootSessionHosts {
        config,
        channels,
        local_node: local_node.map(str::to_owned),
    };
    ForcedSessionHosts(
        FORCED.with(|forced| forced.replace(Some(boot))),
        FORCED_MIGRATION.with(|forced| forced.take()),
    )
}

/// Fixes `channels` as the boot section of this whole process; only an isolated child may call it.
#[cfg(test)]
pub(crate) fn install_for_test(local_node: Option<&str>, channels: &[(u64, &str)]) {
    let forced = force_for_test(local_node, channels);
    let boot = FORCED
        .with(|forced| forced.borrow().clone())
        .expect("forced section");
    drop(forced);
    assert!(
        BOOT.set(boot).is_ok(),
        "the boot section is installed once per process"
    );
}

#[cfg(test)]
impl ForcedSessionHosts {
    pub(crate) fn with_mode(self, mode: EndpointMode) -> Self {
        FORCED.with(|forced| {
            if let Some(boot) = forced.borrow_mut().as_mut() {
                for endpoint in boot.config.herdr.endpoints.values_mut() {
                    endpoint.mode = mode;
                }
            }
        });
        self
    }
}

#[cfg(test)]
impl Drop for ForcedSessionHosts {
    fn drop(&mut self) {
        FORCED.with(|forced| *forced.borrow_mut() = self.0.take());
        FORCED_MIGRATION.with(|forced| *forced.borrow_mut() = self.1.take());
    }
}

#[cfg(test)]
#[path = "session_hosts_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "session_hosts/user_default_validator_tests.rs"]
mod user_default_validator_tests;
