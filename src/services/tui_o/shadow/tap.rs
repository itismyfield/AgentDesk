use serde::{Deserialize, Serialize};

use crate::services::tui_o::repost::config::RepostConfig;

/// The `tui_o:` config section; absent means every output-track feature is off.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiOConfig {
    pub writer: WriterChannelsConfig,
    pub turn: crate::services::tui_o::turn_mode::TurnConfig,
    /// Operator channel for writer alarms; absent keeps them to health and logs.
    pub alert_channel_id: Option<u64>,
    #[serde(skip_serializing_if = "RepostConfig::is_default")]
    pub repost: RepostConfig,
    /// Codex history adoption at boot. Off (the default) installs no observation runtime.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub codex_history_adoption: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WriterChannelsConfig {
    pub channels: std::collections::BTreeSet<u64>,
    /// Selects every `agents[].channels` binding that resolves to a Claude or Codex TUI.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub all_tui: bool,
    /// Reserve fresh Claude channels at boot for adoption after a source is bound.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub late_adopt: bool,
    /// Channels allowed to close drained historical source FDs, without selecting a writer.
    #[serde(skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub dormant_channels: std::collections::BTreeSet<u64>,
}
