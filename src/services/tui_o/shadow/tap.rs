use serde::{Deserialize, Serialize};

/// The `tui_o:` config section; an unset writer selection, late adoption and turn mode default on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiOConfig {
    pub writer: WriterChannelsConfig,
    pub tool_cards: ToolCardsConfig,
    pub turn: crate::services::tui_o::turn_mode::TurnConfig,
    /// Operator channel for writer alarms; absent keeps them to health and logs.
    pub alert_channel_id: Option<u64>,
}

impl TuiOConfig {
    /// The section as configured, or its defaults when the config has none.
    pub(crate) fn of(config: &crate::config::Config) -> &TuiOConfig {
        Self::or_default(config.tui_o.as_ref())
    }

    pub(crate) fn or_default(section: Option<&TuiOConfig>) -> &TuiOConfig {
        static DEFAULT: std::sync::LazyLock<TuiOConfig> =
            std::sync::LazyLock::new(Default::default);
        section.unwrap_or(&DEFAULT)
    }

    /// Unset flags written as their effective values, so stating a default is not a change.
    pub(crate) fn resolved(&self) -> TuiOConfig {
        let mut resolved = self.clone();
        resolved.writer.all_tui = Some(self.writer.all_tui());
        resolved.writer.late_adopt = Some(self.writer.late_adopt());
        resolved.turn.all_owned = Some(self.turn.all_owned());
        resolved
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolCardsConfig {
    pub enabled: bool,
}

impl Default for ToolCardsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WriterChannelsConfig {
    pub channels: std::collections::BTreeSet<u64>,
    /// Selects every `agents[].channels` binding that resolves to a Claude or Codex TUI; unset
    /// means on unless `channels` lists the selection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub all_tui: Option<bool>,
    /// Reserve fresh Claude channels at boot for adoption after a source is bound; unset means on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub late_adopt: Option<bool>,
    /// Channels allowed to close drained historical source FDs, without selecting a writer.
    #[serde(skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub dormant_channels: std::collections::BTreeSet<u64>,
}

impl WriterChannelsConfig {
    pub(crate) fn all_tui(&self) -> bool {
        self.all_tui.unwrap_or(self.channels.is_empty())
    }

    pub(crate) fn late_adopt(&self) -> bool {
        self.late_adopt.unwrap_or(true)
    }
}
