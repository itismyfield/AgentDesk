//! Validated writer membership is fixed for the lifetime of the process; on the O home each
//! selected or locally committed channel also carries this process's adoption state, and off it
//! each selected channel keeps its local store state for a delegated home.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use std::time::Instant;

use anyhow::{Context, Result, ensure};

use crate::config::{AgentChannel, Config};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::provider_hosting::RuntimeMode;
use crate::services::tui_o::alarm::AlarmRouter;
use crate::services::tui_o::writer::WriterAlarm;

mod adoption;
pub(crate) use adoption::{Adoption, Candidate, Site};
#[cfg(test)]
pub(crate) use adoption::{
    body_check::{BodyCheck, SinkOp},
    stored,
};

#[derive(Clone, Debug, Default)]
pub(crate) struct BootChannels {
    /// The selection and, on the home, each committed channel the selection left out.
    channels: BTreeSet<u64>,
    selected: BTreeSet<u64>,
    kinds: BTreeMap<u64, RuntimeHandoffKind>,
    site: Site,
    configured_id: Option<String>,
    candidates: BTreeMap<u64, Candidate>,
    /// Off the home, each selected channel's local store state, kept so a delegated home can open
    /// here without a restart. Holding one opens nothing: the channel's home gate decides.
    standby: BTreeMap<u64, Candidate>,
}

/// Membership as configured; the adoption states and kept committed channels are process state.
impl PartialEq for BootChannels {
    fn eq(&self, other: &Self) -> bool {
        fn selected(boot: &BootChannels) -> impl PartialEq + '_ {
            let kinds = boot.kinds.iter().filter(|(c, _)| boot.selected.contains(c));
            let kinds: Vec<_> = kinds.collect();
            (&boot.selected, kinds, &boot.site, &boot.configured_id)
        }
        selected(self) == selected(other)
    }
}

static BOOT: OnceLock<BootChannels> = OnceLock::new();

/// The channels the config selects: the explicit list, or with `all_tui` every TUI binding.
pub(crate) fn configured_channels(config: &Config) -> BTreeSet<u64> {
    let Some(writer) = config.tui_o.as_ref().map(|config| &config.writer) else {
        return BTreeSet::new();
    };
    if !writer.all_tui {
        return writer.channels.clone();
    }
    all_tui_kinds(config)
        .map(|kinds| kinds.into_keys().collect())
        .unwrap_or_default()
}

/// The TUI kind `channel` runs as. The inner error names why it is not a TUI; the outer one is a
/// binding whose kind cannot be told.
fn tui_kind(
    config: &Config,
    key: &str,
    id: u64,
    channel_provider: &str,
    channel: &AgentChannel,
) -> Result<std::result::Result<RuntimeHandoffKind, String>> {
    let provider = channel
        .provider()
        .unwrap_or_else(|| channel_provider.to_owned());
    let provider = provider.trim().to_ascii_lowercase();
    let kind = match provider.as_str() {
        "claude" => RuntimeHandoffKind::ClaudeTui,
        "codex" => RuntimeHandoffKind::CodexTui,
        _ => {
            return Ok(Err(format!(
                "{key}: channel {id} has non-TUI provider {provider}"
            )));
        }
    };
    let mut provider_configs = config
        .providers
        .iter()
        .filter(|(key, _)| key.trim().eq_ignore_ascii_case(&provider));
    let provider_config = provider_configs.next().map(|(_, value)| value);
    ensure!(
        provider_configs.next().is_none(),
        "{key}: channel {id} has ambiguous provider settings"
    );
    let channel_runtime = channel.runtime_mode_raw();
    let raw_runtime = channel_runtime
        .as_deref()
        .or_else(|| provider_config.and_then(|config| config.runtime.as_deref()));
    let tui = match raw_runtime {
        Some(raw) => {
            let mode = RuntimeMode::parse(raw)
                .ok_or_else(|| anyhow::anyhow!("{key}: channel {id} has invalid runtime {raw}"))?;
            mode == RuntimeMode::Tui
        }
        None => channel
            .tui_hosting()
            .or_else(|| provider_config.and_then(|config| config.tui_hosting))
            .unwrap_or_else(|| crate::config::default_provider_tui_hosting(&provider)),
    };
    Ok(if tui {
        Ok(kind)
    } else {
        Err(format!("{key}: channel {id} is not TUI"))
    })
}

fn agent_bindings(config: &Config) -> impl Iterator<Item = (u64, &str, &AgentChannel)> {
    let bindings = config.agents.iter().flat_map(|agent| agent.channels.iter());
    bindings.filter_map(|(provider, channel)| {
        let id = channel.channel_id()?.parse::<u64>().ok()?;
        Some((id, provider, channel))
    })
}

/// Kinds of the listed channels; each must be registered as a TUI with one provider.
fn listed_kinds(
    config: &Config,
    key: &str,
    channels: &BTreeSet<u64>,
) -> Result<BTreeMap<u64, RuntimeHandoffKind>> {
    let mut kinds = BTreeMap::new();
    for (id, channel_provider, channel) in agent_bindings(config) {
        if !channels.contains(&id) {
            continue;
        }
        let kind =
            tui_kind(config, key, id, channel_provider, channel)?.map_err(anyhow::Error::msg)?;
        if let Some(previous) = kinds.insert(id, kind) {
            ensure!(
                previous == kind,
                "{key}: channel {id} has conflicting providers"
            );
        }
    }
    for id in channels {
        ensure!(
            kinds.contains_key(id),
            "{key}: channel {id} is not registered"
        );
    }
    Ok(kinds)
}

/// Every binding that resolves to a TUI; the rest are skipped, not rejected. A channel bound as a
/// TUI and as something else is a conflict.
fn all_tui_kinds(config: &Config) -> Result<BTreeMap<u64, RuntimeHandoffKind>> {
    const KEY: &str = "tui_o.writer.all_tui";
    let (mut kinds, mut skipped) = (BTreeMap::new(), BTreeSet::new());
    for (id, channel_provider, channel) in agent_bindings(config) {
        let Ok(kind) = tui_kind(config, KEY, id, channel_provider, channel)? else {
            skipped.insert(id);
            continue;
        };
        if let Some(previous) = kinds.insert(id, kind) {
            ensure!(
                previous == kind,
                "{KEY}: channel {id} has conflicting providers"
            );
        }
    }
    if let Some(id) = skipped.iter().find(|id| kinds.contains_key(id)) {
        anyhow::bail!("{KEY}: channel {id} has conflicting providers");
    }
    ensure!(!kinds.contains_key(&0), "{KEY} rejects channel 0");
    Ok(kinds)
}

impl BootChannels {
    pub(crate) fn validate(config: &Config) -> Result<Self> {
        let writer = config.tui_o.as_ref().map(|config| &config.writer);
        let kinds = if writer.is_some_and(|writer| writer.all_tui) {
            ensure!(
                writer.is_some_and(|writer| writer.channels.is_empty()),
                "tui_o.writer.all_tui cannot be combined with a non-empty tui_o.writer.channels"
            );
            all_tui_kinds(config)?
        } else {
            let channels = configured_channels(config);
            ensure!(
                !channels.contains(&0),
                "tui_o.writer.channels rejects channel 0"
            );
            listed_kinds(config, "tui_o.writer.channels", &channels)?
        };
        let channels: BTreeSet<u64> = kinds.keys().copied().collect();
        let (site, configured_id) = site(config, !channels.is_empty())?;
        Ok(Self {
            selected: channels.clone(),
            channels,
            kinds,
            site,
            configured_id,
            candidates: BTreeMap::new(),
            standby: BTreeMap::new(),
        })
    }

    /// Starts each selected channel's adoption. The store is read only for an enabled writer with
    /// a non-empty selection. The home also keeps every channel its store committed; a non-home
    /// node adopts nothing, reports local store state and keeps it as standby.
    pub(crate) fn seeded(
        mut self,
        enabled: bool,
        config: &Config,
        stored: impl FnOnce(&BTreeSet<u64>, bool) -> std::io::Result<BTreeMap<u64, Adoption>>,
    ) -> Result<Self> {
        if !enabled || self.selected.is_empty() {
            return Ok(self);
        }
        let alarms = AlarmRouter::for_process(None, None);
        match &self.site {
            Site::Home => {
                let states = stored(&self.selected, true)?;
                let kept: BTreeSet<u64> = states
                    .keys()
                    .filter(|channel| !self.selected.contains(channel))
                    .copied()
                    .collect();
                let kinds = listed_kinds(config, "o_store", &kept)
                    .context("a channel committed to O here left the writer selection")?;
                for &channel in &kept {
                    tracing::warn!(
                        channel,
                        "[tui_o] committed channel missing from writer selection"
                    );
                    alarms.raise_at(channel, &WriterAlarm::SelectionMissing, Instant::now());
                }
                self.kinds.extend(kinds);
                self.channels.extend(kept);
                let candidate = |(channel, state)| (channel, Candidate::new(state));
                self.candidates = states.into_iter().map(candidate).collect();
            }
            Site::Foreign { home } => {
                let states = stored(&self.selected, false)?;
                let detail = format!("non-home node ignores its local O store; O home is {home}");
                let alarm = WriterAlarm::Halted { detail };
                for (&channel, _) in states.iter().filter(|(_, s)| **s != Adoption::Pending) {
                    alarms.raise_at(channel, &alarm, Instant::now());
                }
                let standby = |(channel, state)| (channel, Candidate::new(state));
                self.standby = states.into_iter().map(standby).collect();
            }
        }
        Ok(self)
    }

    /// Every channel O may own here: the selection plus, on the home, its kept committed channels.
    pub(crate) fn channels(&self) -> &BTreeSet<u64> {
        &self.channels
    }

    /// The channels the boot config selected, for restart-required comparison.
    pub(crate) fn selected(&self) -> &BTreeSet<u64> {
        &self.selected
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

    /// A selected channel's local store state off the home; only a home gate makes it count.
    pub(crate) fn standby(&self, channel: u64) -> Option<&Candidate> {
        self.standby.get(&channel)
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
        self.channels = self.selected.clone();
        self.candidates.clear();
        self
    }

    /// Every selected channel kept as standby in `state`, as `seeded` would leave it off the home.
    #[cfg(test)]
    pub(crate) fn standing_by(mut self, state: Adoption) -> Self {
        let standby = |&channel: &u64| (channel, Candidate::new(state));
        self.standby = self.selected.iter().map(standby).collect();
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
    let installed = match BOOT.get() {
        Some(installed) => installed,
        None => {
            let stored = |channels: &BTreeSet<u64>, committed: bool| {
                let root = crate::config::runtime_root();
                if committed {
                    adoption::stored_with_committed(root.as_deref(), channels)
                } else {
                    Ok(adoption::stored(root.as_deref(), channels))
                }
            };
            let enabled = super::cutover::writer_enabled();
            let seeded = candidate.clone().seeded(enabled, config, stored)?;
            BOOT.get_or_init(|| seeded)
        }
    };
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
