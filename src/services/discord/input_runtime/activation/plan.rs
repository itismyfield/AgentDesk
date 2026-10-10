use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail, ensure};

use crate::config::Config;
use crate::services::agent_protocol::RuntimeHandoffKind;
#[cfg(test)]
use crate::services::tui_input::transition::mutant;
use crate::services::tui_o::channel_policy::BootChannels;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum InputMode {
    #[default]
    Legacy,
    Ledger,
}

// Kept separate from TurnConfig until the boot collector and selector land together.
#[derive(Clone, Debug, Default)]
pub(crate) struct InputSelection {
    pub(crate) mode: InputMode,
    pub(crate) channels: Option<BTreeSet<u64>>,
}

impl InputSelection {
    pub(crate) fn validate(&self, config: &Config) -> Result<BTreeMap<u64, &'static str>> {
        if self.mode == InputMode::Legacy {
            return Ok(BTreeMap::new());
        }
        let channels = match &self.channels {
            Some(channels) => channels.clone(),
            #[cfg(test)]
            None if mutant("g2-selection-missing-list") => {
                BootChannels::validate(config)?.selected().clone()
            }
            None => bail!("tui_o.turn.input ledger requires explicit input_channels"),
        };
        if channels.is_empty() {
            return Ok(BTreeMap::new());
        }
        ensure!(!channels.contains(&0), "input_channels rejects channel 0");
        let writer = BootChannels::validate(config)?;
        let turn = config.tui_o.as_ref().map(|config| &config.turn);
        let mut selected = BTreeMap::new();
        for channel in channels {
            ensure!(
                turn.is_some_and(|turn| turn.selects(channel)),
                "input_channels channel {channel} is outside the O turn selection"
            );
            let provider = match writer.kind(channel) {
                Some(RuntimeHandoffKind::ClaudeTui) => "claude",
                Some(RuntimeHandoffKind::CodexTui) => "codex",
                _ => bail!(
                    "input_channels channel {channel} is outside the supported O writer selection"
                ),
            };
            selected.insert(channel, provider);
        }
        Ok(selected)
    }
}
