//! Validated writer membership is fixed for the lifetime of the process; on the O home each
//! selected channel also carries this process's adoption state.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use std::time::Instant;

use anyhow::{Result, ensure};

use crate::config::Config;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::provider_hosting::RuntimeMode;
use crate::services::tui_o::alarm::AlarmRouter;
use crate::services::tui_o::writer::WriterAlarm;

mod adoption;
#[cfg(test)]
pub(crate) use adoption::stored;
pub(crate) use adoption::{Adoption, Candidate, Site};

#[derive(Clone, Debug, Default)]
pub(crate) struct BootChannels {
    channels: BTreeSet<u64>,
    kinds: BTreeMap<u64, RuntimeHandoffKind>,
    site: Site,
    configured_id: Option<String>,
    candidates: BTreeMap<u64, Candidate>,
}

/// Membership as configured; the adoption states are process state, not configuration.
impl PartialEq for BootChannels {
    fn eq(&self, other: &Self) -> bool {
        (&self.channels, &self.kinds, &self.site, &self.configured_id)
            == (
                &other.channels,
                &other.kinds,
                &other.site,
                &other.configured_id,
            )
    }
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
        let (site, configured_id) = site(config, !channels.is_empty())?;
        Ok(Self {
            channels,
            kinds,
            site,
            configured_id,
            candidates: BTreeMap::new(),
        })
    }

    /// Starts each selected channel's adoption. The store is read only for an enabled writer with
    /// a non-empty list; a non-home node adopts nothing and only reports local store state.
    fn seeded(
        mut self,
        enabled: bool,
        stored: impl FnOnce(&BTreeSet<u64>) -> BTreeMap<u64, Adoption>,
    ) -> Self {
        if !enabled || self.channels.is_empty() {
            return self;
        }
        let states = stored(&self.channels);
        match &self.site {
            Site::Home => {
                let candidate = |(channel, state)| (channel, Candidate::new(state));
                self.candidates = states.into_iter().map(candidate).collect();
            }
            Site::Foreign { home } => {
                let detail = format!("non-home node ignores its local O store; O home is {home}");
                let alarm = WriterAlarm::Halted { detail };
                let alarms = AlarmRouter::for_process(None, None);
                for (&channel, _) in states.iter().filter(|(_, s)| **s != Adoption::Pending) {
                    alarms.raise_at(channel, &alarm, Instant::now());
                }
            }
        }
        self
    }

    pub(crate) fn channels(&self) -> &BTreeSet<u64> {
        &self.channels
    }

    pub(crate) fn kind(&self, channel: u64) -> Option<RuntimeHandoffKind> {
        self.kinds.get(&channel).copied()
    }

    pub(crate) fn site(&self) -> &Site {
        &self.site
    }

    /// `cluster.instance_id` when clustering is on: the id the home judgement used.
    pub(crate) fn configured_id(&self) -> Option<&str> {
        self.configured_id.as_deref()
    }

    /// The adoption of a selected channel; none off the home or while the writer is off.
    pub(crate) fn candidate(&self, channel: u64) -> Option<&Candidate> {
        self.candidates.get(&channel)
    }

    /// Every selected channel starts in `state`, as `seeded` would leave it on the home.
    #[cfg(test)]
    pub(crate) fn adopted(mut self, state: Adoption) -> Self {
        self.candidates = self
            .channels
            .iter()
            .map(|&c| (c, Candidate::new(state)))
            .collect();
        self
    }

    #[cfg(test)]
    pub(crate) fn foreign(mut self, home: &str) -> Self {
        self.site = Site::Foreign { home: home.into() };
        self.candidates.clear();
        self
    }
}

/// The O home is `cluster.gateway_preferred_instance_id`; without clustering this node is it.
/// A clustered node selecting channels must name both ids, or no node could tell it is home.
fn site(config: &Config, selects: bool) -> Result<(Site, Option<String>)> {
    let cluster = &config.cluster;
    if !cluster.enabled {
        return Ok((Site::Home, None));
    }
    let trimmed = |id: &Option<String>| {
        let id = id.as_deref().map(str::trim).filter(|id| !id.is_empty());
        id.map(str::to_owned)
    };
    let (home, local) = (
        trimmed(&cluster.gateway_preferred_instance_id),
        trimmed(&cluster.instance_id),
    );
    let (Some(home), Some(local)) = (home, local) else {
        ensure!(
            !selects,
            "tui_o.writer.channels needs cluster.instance_id and cluster.gateway_preferred_instance_id"
        );
        return Ok((Site::Home, None));
    };
    let site = if home == local {
        Site::Home
    } else {
        Site::Foreign { home }
    };
    Ok((site, Some(local)))
}

pub(crate) fn install(config: &Config) -> Result<()> {
    let candidate = BootChannels::validate(config)?;
    let installed = BOOT.get_or_init(|| {
        let stored = |channels: &BTreeSet<u64>| {
            adoption::stored(crate::config::runtime_root().as_deref(), channels)
        };
        candidate
            .clone()
            .seeded(super::cutover::writer_enabled(), stored)
    });
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
