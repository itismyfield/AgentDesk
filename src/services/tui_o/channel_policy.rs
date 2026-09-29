//! Validated writer membership is fixed for the lifetime of the process.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use anyhow::{Result, ensure};

use crate::config::Config;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::provider_hosting::RuntimeMode;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BootChannels {
    channels: BTreeSet<u64>,
    kinds: BTreeMap<u64, RuntimeHandoffKind>,
}

static BOOT: OnceLock<BootChannels> = OnceLock::new();

pub(crate) fn configured_channels(config: &Config) -> BTreeSet<u64> {
    config
        .tui_o
        .as_ref()
        .map(|config| config.writer.channels.clone())
        .unwrap_or_default()
}

impl BootChannels {
    pub(crate) fn validate(config: &Config) -> Result<Self> {
        let channels = configured_channels(config);
        ensure!(
            !channels.contains(&0),
            "tui_o.writer.channels rejects channel 0"
        );
        let mut kinds = BTreeMap::new();
        for agent in &config.agents {
            for (channel_provider, channel) in agent.channels.iter() {
                let Some(id) = channel.channel_id().and_then(|id| id.parse::<u64>().ok()) else {
                    continue;
                };
                if !channels.contains(&id) {
                    continue;
                }
                let provider = channel
                    .provider()
                    .unwrap_or_else(|| channel_provider.to_owned());
                let provider = provider.trim().to_ascii_lowercase();
                let kind = match provider.as_str() {
                    "claude" => RuntimeHandoffKind::ClaudeTui,
                    "codex" => RuntimeHandoffKind::CodexTui,
                    _ => anyhow::bail!(
                        "tui_o.writer.channels: channel {id} has non-TUI provider {provider}"
                    ),
                };
                let mut provider_configs = config
                    .providers
                    .iter()
                    .filter(|(key, _)| key.trim().eq_ignore_ascii_case(&provider));
                let provider_config = provider_configs.next().map(|(_, value)| value);
                ensure!(
                    provider_configs.next().is_none(),
                    "tui_o.writer.channels: channel {id} has ambiguous provider settings"
                );
                let channel_runtime = channel.runtime_mode_raw();
                let raw_runtime = channel_runtime
                    .as_deref()
                    .or_else(|| provider_config.and_then(|config| config.runtime.as_deref()));
                let tui = match raw_runtime {
                    Some(raw) => {
                        let mode = RuntimeMode::parse(raw).ok_or_else(|| {
                            anyhow::anyhow!(
                                "tui_o.writer.channels: channel {id} has invalid runtime {raw}"
                            )
                        })?;
                        mode == RuntimeMode::Tui
                    }
                    None => channel
                        .tui_hosting()
                        .or_else(|| provider_config.and_then(|config| config.tui_hosting))
                        .unwrap_or_else(|| crate::config::default_provider_tui_hosting(&provider)),
                };
                ensure!(tui, "tui_o.writer.channels: channel {id} is not TUI");
                if let Some(previous) = kinds.insert(id, kind) {
                    ensure!(
                        previous == kind,
                        "tui_o.writer.channels: channel {id} has conflicting providers"
                    );
                }
            }
        }
        for id in &channels {
            ensure!(
                kinds.contains_key(id),
                "tui_o.writer.channels: channel {id} is not registered"
            );
        }
        Ok(Self { channels, kinds })
    }

    pub(crate) fn channels(&self) -> &BTreeSet<u64> {
        &self.channels
    }

    pub(crate) fn kind(&self, channel: u64) -> Option<RuntimeHandoffKind> {
        self.kinds.get(&channel).copied()
    }
}

pub(crate) fn install(config: &Config) -> Result<()> {
    let candidate = BootChannels::validate(config)?;
    let installed = BOOT.get_or_init(|| candidate.clone());
    ensure!(
        installed == &candidate,
        "tui_o.writer.channels requires a process restart"
    );
    Ok(())
}

pub(crate) fn boot() -> Option<&'static BootChannels> {
    BOOT.get()
}

pub(crate) fn owns_output(
    enabled: bool,
    channels: &BTreeSet<u64>,
    channel: u64,
    kind: Option<RuntimeHandoffKind>,
) -> bool {
    enabled
        && channels.contains(&channel)
        && matches!(
            kind,
            Some(RuntimeHandoffKind::ClaudeTui | RuntimeHandoffKind::CodexTui)
        )
}

#[cfg(test)]
mod tests;
